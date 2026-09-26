//! Native driver: durable command queue and projection/cursor are one SQLite value.

use futures_util::{SinkExt, StreamExt};
use prost::Message;
use ryzhub_core::Engine;
use ryzhub_proto::{frame::Body, *};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode},
};
use std::{str::FromStr, time::Duration};
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message as WsMessage, client::IntoClientRequest},
};

pub struct Cache {
    pool: SqlitePool,
    partition: String,
}
impl Cache {
    pub async fn open(path: &str, user: &str, org: &str) -> anyhow::Result<Self> {
        let options = SqliteConnectOptions::from_str(path)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePool::connect_with(options).await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS client_state(partition TEXT PRIMARY KEY,state TEXT NOT NULL)").execute(&pool).await?;
        Ok(Self {
            pool,
            partition: format!("{user}:{org}"),
        })
    }
    pub async fn load(&self) -> anyhow::Result<Engine> {
        let json: Option<String> =
            sqlx::query_scalar("SELECT state FROM client_state WHERE partition=?")
                .bind(&self.partition)
                .fetch_optional(&self.pool)
                .await?;
        Ok(match json {
            Some(s) => serde_json::from_str(&s)?,
            None => Engine::default(),
        })
    }
    pub async fn save(&self, engine: &Engine) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO client_state(partition,state) VALUES(?,?) ON CONFLICT(partition) DO UPDATE SET state=excluded.state").bind(&self.partition).bind(serde_json::to_string(engine)?).execute(&self.pool).await?;
        Ok(())
    }
}
pub enum Input {
    Command {
        channel: String,
        command: CommandEnvelope,
    },
    NetworkChanged,
    Stop,
}
pub struct Driver {
    pub url: String,
    pub token: String,
    pub device_id: String,
    pub cache: Cache,
}
impl Driver {
    pub async fn run(
        &self,
        mut input: mpsc::Receiver<Input>,
        output: mpsc::Sender<Engine>,
    ) -> anyhow::Result<()> {
        let mut engine = self.cache.load().await?;
        let mut attempt = 0;
        loop {
            let mut request = self.url.as_str().into_client_request()?;
            request
                .headers_mut()
                .insert("Authorization", format!("Bearer {}", self.token).parse()?);
            request
                .headers_mut()
                .insert("Sec-WebSocket-Protocol", SUBPROTOCOL.parse()?);
            let connection =
                tokio::time::timeout(Duration::from_secs(10), connect_async(request)).await;
            if let Ok(Ok((mut socket, _))) = connection {
                socket
                    .send(WsMessage::Binary(
                        frame(
                            "",
                            Body::Hello(Hello {
                                protocol_major: 1,
                                device_id: self.device_id.clone(),
                                resume_token: engine.resume_token.clone(),
                                ..Default::default()
                            }),
                        )
                        .encode_to_vec()
                        .into(),
                    ))
                    .await?;
                let mut retry = tokio::time::interval(Duration::from_secs(5));
                let mut last_received = tokio::time::Instant::now();
                loop {
                    tokio::select! {
                        received=socket.next()=>{
                            last_received=tokio::time::Instant::now();
                            let bytes=match received {Some(Ok(WsMessage::Binary(b)))=>b,Some(Ok(WsMessage::Ping(_)))=>continue,_=>break};
                            let f=Frame::decode(bytes)?;
                            let mut outbound=vec![];
                            if matches!(f.body,Some(Body::Welcome(_))){attempt=0;outbound.extend(engine.reconnect());}
                            let effects=engine.apply(f);outbound.extend(effects.outbound);
                            self.cache.save(&engine).await?;
                            for f in outbound {if socket.send(WsMessage::Binary(f.encode_to_vec().into())).await.is_err(){break;}}
                            let _=output.try_send(engine.clone());
                        }
                        _=retry.tick()=>{
                        if last_received.elapsed()>Duration::from_secs(55){break;}
                        for f in engine.pending_commands.values() {let _=socket.send(WsMessage::Binary(f.encode_to_vec().into())).await;}
                    }
                    command=input.recv()=>match command {
                            Some(Input::Command{channel,command})=>{
                                let f=engine.enqueue(channel,command);self.cache.save(&engine).await?;
                                if socket.send(WsMessage::Binary(f.encode_to_vec().into())).await.is_err(){break;}
                            }
                            Some(Input::NetworkChanged)=>break,
                            Some(Input::Stop)|None=>{let _=socket.close(None).await;return Ok(());}
                        }
                    }
                }
            }
            let random = uuid::Uuid::now_v7().as_u128() as u64;
            let delay = Duration::from_millis(ryzhub_core::reconnect_delay_ms(attempt, random));
            attempt += 1;
            tokio::select! {
                _=tokio::time::sleep(delay)=>{},
                command=input.recv()=>match command {
                    Some(Input::Command{channel,command})=>{engine.enqueue(channel,command);self.cache.save(&engine).await?;}
                    Some(Input::NetworkChanged)=>{},
                    Some(Input::Stop)|None=>return Ok(()),
                }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn sqlite_cache_reopens_pending_command_and_isolates_organizations() -> anyhow::Result<()>
    {
        let file = std::env::temp_dir().join(format!("ryzhub-cache-{}.db", uuid::Uuid::now_v7()));
        let cache = Cache::open(file.to_str().unwrap(), "alice", "org-a").await?;
        let mut state = Engine::default();
        state.subscribe("conv:x".into());
        state.enqueue(
            "conv:x".into(),
            CommandEnvelope {
                cid: "same-id".into(),
                command: Some(command_envelope::Command::Cancel(Cancel {})),
            },
        );
        cache.save(&state).await?;
        let other = Cache::open(file.to_str().unwrap(), "alice", "org-b").await?;
        assert!(other.load().await?.channels.is_empty());
        let restored = cache.load().await?;
        assert!(restored.pending_commands.contains_key("same-id"));
        cache.pool.close().await;
        other.pool.close().await;
        std::fs::remove_file(file)?;
        Ok(())
    }
}
