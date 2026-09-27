use anyhow::{Context, bail, ensure};
use chrono::{DateTime, Utc};
use prost::Message;
use ryzhub_domain::{Action, OrgContext, Principal, Resource, Role, authorize};
use ryzhub_proto::{event_envelope::Event, frame::Body, *};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

pub type Tx<'a> = Transaction<'a, Postgres>;
#[derive(Clone)]
pub struct Store {
    pub pool: PgPool,
}
#[derive(Clone, Debug)]
pub struct Auth {
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub expires_at: DateTime<Utc>,
    pub is_instance_admin: bool,
}
#[derive(Clone, Debug)]
pub struct Lease {
    pub org_id: Uuid,
    pub conversation_id: Uuid,
    pub owner_id: Uuid,
    pub epoch: i64,
}
impl Lease {
    pub fn channel(&self) -> String {
        format!("conv:{}", self.conversation_id)
    }
}
#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct Conversation {
    pub id: Uuid,
    pub org_id: Uuid,
    pub creator_id: Uuid,
    pub mode: String,
    pub title: String,
    pub state: String,
    pub project_id: Option<Uuid>,
    pub run_config: serde_json::Value,
}
#[derive(sqlx::FromRow)]
struct ItemRow {
    id: Uuid,
    revision: i64,
    kind: String,
    state: String,
    text: String,
    first_seq: i64,
    last_seq: i64,
    run_id: Option<Uuid>,
}
impl From<ItemRow> for Item {
    fn from(i: ItemRow) -> Self {
        Self {
            id: i.id.to_string(),
            revision: i.revision as u64,
            kind: i.kind,
            state: i.state,
            text: i.text,
            first_seq: i.first_seq as u64,
            last_seq: i.last_seq as u64,
            run_id: i.run_id.map(|v| v.to_string()).unwrap_or_default(),
        }
    }
}
#[derive(sqlx::FromRow)]
pub struct ActionRow {
    pub id: Uuid,
    pub run_id: Uuid,
    pub run_epoch: i64,
    pub summary: String,
    pub options: Vec<String>,
    pub status: String,
    pub resolved_device: String,
    pub expires_at: DateTime<Utc>,
}
impl From<ActionRow> for PendingAction {
    fn from(a: ActionRow) -> Self {
        Self {
            id: a.id.to_string(),
            run_id: a.run_id.to_string(),
            run_epoch: a.run_epoch as u64,
            summary: a.summary,
            options: a.options,
            status: a.status,
            resolved_device: a.resolved_device,
            expires_at_ms: a.expires_at.timestamp_millis(),
        }
    }
}

pub fn hash_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}
pub fn new_token() -> String {
    use base64::Engine;
    let bytes: [u8; 32] = rand::random();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
impl Store {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(20)
            .connect(url)
            .await?;
        let privileged: bool = sqlx::query_scalar(
            "SELECT rolsuper OR rolbypassrls FROM pg_roles WHERE rolname=current_user",
        )
        .fetch_one(&pool)
        .await?;
        ensure!(
            !privileged,
            "runtime DATABASE_URL must use a non-superuser, non-BYPASSRLS role"
        );
        Ok(Self { pool })
    }
    pub async fn tx(&self, org: Uuid) -> anyhow::Result<Tx<'_>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT set_config('app.org_id',$1,true)")
            .bind(org.to_string())
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }
    pub async fn authenticate(&self, token: &str) -> anyhow::Result<Auth> {
        let row = sqlx::query("SELECT s.id,s.user_id,s.expires_at,u.is_instance_admin FROM auth_sessions s JOIN users u ON u.id=s.user_id WHERE token_hash=$1 AND s.revoked_at IS NULL AND s.expires_at>now() AND u.status='active'")
            .bind(hash_token(token)).fetch_optional(&self.pool).await?.context("UNAUTHORIZED")?;
        Ok(Auth {
            session_id: row.get("id"),
            user_id: row.get("user_id"),
            expires_at: row.get("expires_at"),
            is_instance_admin: row.get("is_instance_admin"),
        })
    }
    pub async fn auth_by_session(&self, id: Uuid) -> anyhow::Result<Auth> {
        let row = sqlx::query("SELECT s.id,s.user_id,s.expires_at,u.is_instance_admin FROM auth_sessions s JOIN users u ON u.id=s.user_id WHERE s.id=$1 AND s.revoked_at IS NULL AND s.expires_at>now() AND u.status='active'")
            .bind(id).fetch_optional(&self.pool).await?.context("UNAUTHORIZED")?;
        Ok(Auth {
            session_id: row.get("id"),
            user_id: row.get("user_id"),
            expires_at: row.get("expires_at"),
            is_instance_admin: row.get("is_instance_admin"),
        })
    }
    pub async fn context(&self, user: Uuid, org: Uuid) -> anyhow::Result<OrgContext> {
        let mut tx = self.tx(org).await?;
        let ctx = context_in(&mut tx, user, org).await?;
        tx.commit().await?;
        Ok(ctx)
    }
    pub async fn conversation(&self, ctx: &OrgContext, id: Uuid) -> anyhow::Result<Conversation> {
        let mut tx = self.tx(ctx.org_id).await?;
        let c = conversation_in(&mut tx, ctx, id).await?;
        tx.commit().await?;
        Ok(c)
    }
    pub async fn create_conversation(
        &self,
        ctx: &OrgContext,
        title: &str,
        mode: &str,
    ) -> anyhow::Result<Conversation> {
        self.create_configured(ctx, title, mode, serde_json::json!({}), None, None)
            .await
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn create_configured(
        &self,
        ctx: &OrgContext,
        title: &str,
        mode: &str,
        mut settings: serde_json::Value,
        project: Option<Uuid>,
        config: Option<&crate::config::Config>,
    ) -> anyhow::Result<Conversation> {
        ensure!(
            matches!(mode, "" | "chat" | "work") && !title.trim().is_empty() && title.len() <= 256,
            "INVALID_CONVERSATION"
        );
        let mut tx = self.tx(ctx.org_id).await?;
        let ctx = context_in(&mut tx, ctx.user_id, ctx.org_id).await?;
        authorize(
            &Principal::User(ctx.clone()),
            Action::UseConversation,
            &Resource {
                org_id: ctx.org_id,
                id: ctx.org_id,
                owner_user_id: Some(ctx.user_id),
                target_role: None,
            },
        )?;
        if project.is_some()
            && let Some(config) = config
        {
            let org_policy: Option<serde_json::Value> = sqlx::query_scalar(
                "SELECT policy FROM org_policies WHERE org_id=$1 ORDER BY version DESC LIMIT 1",
            )
            .bind(ctx.org_id)
            .fetch_optional(&mut *tx)
            .await?;
            ensure!(
                config
                    .policy
                    .restrict(&serde_json::from_value(
                        org_policy.unwrap_or(serde_json::json!({}))
                    )?)
                    .allow_projects
                    != Some(false),
                "POLICY_DENIED"
            );
        }
        let mut resolved_mode = mode.to_owned();
        if let Some(id) = project {
            let defaults:serde_json::Value=sqlx::query_scalar("SELECT defaults FROM projects WHERE org_id=$1 AND id=$2 AND creator_id=$3 AND state='active' FOR UPDATE").bind(ctx.org_id).bind(id).bind(ctx.user_id).fetch_optional(&mut *tx).await?.context("FORBIDDEN")?;
            if resolved_mode.is_empty() {
                resolved_mode = defaults["mode"].as_str().unwrap_or("chat").into();
            }
            let mut merged = defaults.as_object().context("INVALID_DEFAULTS")?.clone();
            let default_mode = merged.remove("mode");
            let request = settings.as_object().context("INVALID_CONFIG")?;
            // Defaults describe one path. A different mode, harness or model
            // replaces the parts of that path it implies instead of mixing them.
            let other_mode = !mode.is_empty()
                && default_mode
                    .as_ref()
                    .and_then(|m| m.as_str())
                    .unwrap_or("chat")
                    != mode;
            if other_mode || request.contains_key("harness") {
                merged.remove("harness");
                merged.remove("model");
                merged.remove("protocol");
            }
            if request.contains_key("model") {
                merged.remove("protocol");
            }
            for (k, v) in request {
                merged.insert(k.clone(), v.clone());
            }
            settings = serde_json::Value::Object(merged);
        }
        let mode = if resolved_mode.is_empty() {
            "chat"
        } else {
            &resolved_mode
        };
        ensure!(matches!(mode, "chat" | "work"), "INVALID_MODE");
        if let Some(config) = config {
            let c = crate::run_config::RunConfig::resolve(settings, mode, config)?;
            c.validate(&mut tx, ctx.org_id, config).await?;
            settings = serde_json::to_value(c)?;
        }
        let id = Uuid::now_v7();
        let c = sqlx::query_as::<_,Conversation>("INSERT INTO conversations(id,org_id,creator_id,title,mode,run_config,project_id) VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING id,org_id,creator_id,title,mode,state,project_id,run_config")
            .bind(id).bind(ctx.org_id).bind(ctx.user_id).bind(title).bind(mode).bind(settings).bind(project).fetch_one(&mut *tx).await?;
        sqlx::query("INSERT INTO channels(id,org_id,kind,conversation_id,writer_epoch) VALUES($1,$2,'conv',$3,0)").bind(format!("conv:{id}")).bind(ctx.org_id).bind(id).execute(&mut *tx).await?;
        append(
            &mut tx,
            ctx.org_id,
            &format!("user:{}:{}", ctx.org_id, ctx.user_id),
            None,
            "",
            Event::ItemUpserted(Item {
                id: id.to_string(),
                revision: 1,
                kind: "conversation".into(),
                state: "active".into(),
                text: title.into(),
                ..Default::default()
            }),
        )
        .await?;
        audit(
            &mut tx,
            &ctx,
            "conversation.create",
            &id.to_string(),
            "allowed",
        )
        .await?;
        tx.commit().await?;
        Ok(c)
    }
    pub async fn list_conversations(&self, ctx: &OrgContext) -> anyhow::Result<Vec<Conversation>> {
        let mut tx = self.tx(ctx.org_id).await?;
        context_in(&mut tx, ctx.user_id, ctx.org_id).await?;
        let list = sqlx::query_as("SELECT id,org_id,creator_id,title,mode,state,project_id,run_config FROM conversations WHERE org_id=$1 AND creator_id=$2 AND state IN ('active','archived') ORDER BY created_at DESC LIMIT 200")
            .bind(ctx.org_id).bind(ctx.user_id).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        Ok(list)
    }
    pub async fn authorize_channel(&self, user: Uuid, channel: &str) -> anyhow::Result<OrgContext> {
        let parts: Vec<_> = channel.split(':').collect();
        let org = match parts.as_slice() {
            ["conv", id] => {
                let conv = Uuid::parse_str(id)?;
                let org: Option<Uuid> =
                    sqlx::query_scalar("SELECT resolve_conversation_org($1,$2)")
                        .bind(user)
                        .bind(conv)
                        .fetch_one(&self.pool)
                        .await?;
                org.context("FORBIDDEN")?
            }
            ["user", org, id] => {
                ensure!(Uuid::parse_str(id)? == user, "FORBIDDEN");
                Uuid::parse_str(org)?
            }
            ["org", org] => Uuid::parse_str(org)?,
            _ => bail!("INVALID_CHANNEL"),
        };
        let ctx = self.context(user, org).await?;
        authorize(
            &Principal::User(ctx.clone()),
            Action::Subscribe,
            &Resource {
                org_id: org,
                id: org,
                owner_user_id: Some(user),
                target_role: None,
            },
        )?;
        Ok(ctx)
    }
    pub async fn accept(
        &self,
        ctx: &OrgContext,
        conv: Uuid,
        device: &str,
        cmd: &CommandEnvelope,
    ) -> anyhow::Result<Option<Frame>> {
        let cid = Uuid::parse_str(&cmd.cid).context("INVALID_CID")?;
        ensure!(cmd.command.is_some(), "INVALID_COMMAND");
        let mut tx = self.tx(ctx.org_id).await?;
        let ctx = context_in(&mut tx, ctx.user_id, ctx.org_id).await?;
        conversation_in(&mut tx, &ctx, conv).await?;
        sqlx::query("INSERT INTO commands(id,user_id,cid,org_id,conversation_id,device_id,payload) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(user_id,cid) DO NOTHING")
            .bind(Uuid::now_v7()).bind(ctx.user_id).bind(cid).bind(ctx.org_id).bind(conv).bind(device).bind(cmd.encode_to_vec()).execute(&mut *tx).await?;
        let row=sqlx::query("SELECT conversation_id,payload,result FROM commands WHERE org_id=$1 AND user_id=$2 AND cid=$3")
            .bind(ctx.org_id).bind(ctx.user_id).bind(cid).fetch_optional(&mut *tx).await?.context("CID_REUSED")?;
        ensure!(
            row.get::<Uuid, _>("conversation_id") == conv
                && row.get::<Vec<u8>, _>("payload") == cmd.encode_to_vec(),
            "CID_REUSED"
        );
        let bytes: Option<Vec<u8>> = row.get("result");
        tx.commit().await?;
        bytes
            .map(|b| Frame::decode(b.as_slice()).map_err(Into::into))
            .transpose()
    }
    pub async fn command_result(
        &self,
        ctx: &OrgContext,
        cid: Uuid,
    ) -> anyhow::Result<Option<Frame>> {
        let mut tx = self.tx(ctx.org_id).await?;
        let bytes: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT result FROM commands WHERE org_id=$1 AND user_id=$2 AND cid=$3",
        )
        .bind(ctx.org_id)
        .bind(ctx.user_id)
        .bind(cid)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        bytes
            .map(|b| Frame::decode(b.as_slice()).map_err(Into::into))
            .transpose()
    }
    pub async fn replay(
        &self,
        ctx: &OrgContext,
        channel: &str,
        since: Option<u64>,
    ) -> anyhow::Result<(Vec<Frame>, u64)> {
        self.authorize_channel(ctx.user_id, channel).await?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SELECT set_config('app.org_id',$1,true)")
            .bind(ctx.org_id.to_string())
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query(
            "SELECT last_seq,kind,conversation_id FROM channels WHERE org_id=$1 AND id=$2",
        )
        .bind(ctx.org_id)
        .bind(channel)
        .fetch_one(&mut *tx)
        .await?;
        let head: i64 = row.get("last_seq");
        let kind: String = row.get("kind");
        let min: Option<i64> =
            sqlx::query_scalar("SELECT min(seq) FROM events WHERE org_id=$1 AND channel_id=$2")
                .bind(ctx.org_id)
                .bind(channel)
                .fetch_one(&mut *tx)
                .await?;
        let retained = since.is_some_and(|s| {
            s <= head as u64
                && (s == head as u64 || min.is_some_and(|m| s.saturating_add(1) >= m as u64))
        });
        let mut frames = vec![];
        if retained {
            let payloads:Vec<Vec<u8>>=sqlx::query_scalar("SELECT payload FROM events WHERE org_id=$1 AND channel_id=$2 AND seq>$3 AND seq<=$4 ORDER BY seq")
                .bind(ctx.org_id).bind(channel).bind(since.unwrap() as i64).bind(head).fetch_all(&mut *tx).await?;
            for bytes in payloads {
                frames.push(frame(
                    channel,
                    Body::Event(EventEnvelope::decode(bytes.as_slice())?),
                ));
            }
        } else {
            let (items, actions) = if kind == "conv" {
                let conv: Uuid = row.get("conversation_id");
                let items:Vec<ItemRow>=sqlx::query_as("SELECT id,revision,kind,state,text,first_seq,last_seq,run_id FROM items WHERE org_id=$1 AND conversation_id=$2 ORDER BY first_seq DESC LIMIT 100").bind(ctx.org_id).bind(conv).fetch_all(&mut *tx).await?;
                let actions:Vec<ActionRow>=sqlx::query_as("SELECT a.id,a.run_id,a.run_epoch,a.summary,a.options,a.status,a.resolved_device,a.expires_at FROM pending_actions a JOIN runs r ON r.org_id=a.org_id AND r.id=a.run_id WHERE a.org_id=$1 AND r.conversation_id=$2 AND a.status='pending'").bind(ctx.org_id).bind(conv).fetch_all(&mut *tx).await?;
                (
                    items.into_iter().map(Into::into).collect(),
                    actions.into_iter().map(Into::into).collect(),
                )
            } else if kind == "user" {
                let rows:Vec<Conversation>=sqlx::query_as("SELECT id,org_id,creator_id,title,mode,state,project_id,run_config FROM conversations WHERE org_id=$1 AND creator_id=$2 AND state IN ('active','archived') ORDER BY created_at DESC LIMIT 200").bind(ctx.org_id).bind(ctx.user_id).fetch_all(&mut *tx).await?;
                (
                    rows.into_iter()
                        .map(|c| Item {
                            id: c.id.to_string(),
                            revision: 1,
                            kind: "conversation".into(),
                            state: c.state,
                            text: c.title,
                            ..Default::default()
                        })
                        .collect(),
                    vec![],
                )
            } else {
                (vec![], vec![])
            };
            let mut runs = Vec::new();
            let mut read_states = Vec::new();
            if kind == "conv" {
                let conv: Uuid = row.get("conversation_id");
                let rows:Vec<(Uuid,String,i64,bool)>=sqlx::query_as("SELECT id,state,exec_epoch,read_only FROM runs WHERE org_id=$1 AND conversation_id=$2 ORDER BY started_at DESC LIMIT 50").bind(ctx.org_id).bind(conv).fetch_all(&mut *tx).await?;
                runs = rows
                    .into_iter()
                    .map(|(id, state, epoch, read_only)| RunState {
                        read_only,
                        run_id: id.to_string(),
                        state,
                        exec_epoch: epoch as u64,
                    })
                    .collect();
            }
            if kind == "user" {
                let rows:Vec<(Uuid,i64)>=sqlx::query_as("SELECT conversation_id,read_seq FROM read_states WHERE org_id=$1 AND user_id=$2").bind(ctx.org_id).bind(ctx.user_id).fetch_all(&mut *tx).await?;
                read_states = rows
                    .into_iter()
                    .map(|(id, seq)| ReadState {
                        user_id: ctx.user_id.to_string(),
                        conversation_id: id.to_string(),
                        read_seq: seq as u64,
                    })
                    .collect();
            }
            frames.push(frame(
                channel,
                Body::Snapshot(Snapshot {
                    head_seq: head as u64,
                    items,
                    pending_actions: actions,
                    runs,
                    read_states,
                }),
            ));
        }
        tx.commit().await?;
        Ok((frames, head as u64))
    }
    pub async fn history(
        &self,
        ctx: &OrgContext,
        conv: Uuid,
        before: i64,
    ) -> anyhow::Result<(Vec<Item>, bool)> {
        const PAGE: usize = 100;
        self.conversation(ctx, conv).await?;
        let mut tx = self.tx(ctx.org_id).await?;
        // One extra row tells whether anything older remains.
        let mut items:Vec<ItemRow>=sqlx::query_as("SELECT id,revision,kind,state,text,first_seq,last_seq,run_id FROM items WHERE org_id=$1 AND conversation_id=$2 AND first_seq<$3 ORDER BY first_seq DESC LIMIT $4")
            .bind(ctx.org_id).bind(conv).bind(before).bind(PAGE as i64 + 1).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        let more = items.len() > PAGE;
        items.truncate(PAGE);
        Ok((items.into_iter().map(Into::into).collect(), more))
    }
    pub async fn claim(
        &self,
        org: Uuid,
        conv: Uuid,
        owner: Uuid,
        ttl_ms: u64,
    ) -> anyhow::Result<Option<Lease>> {
        let mut tx = self.tx(org).await?;
        let row=sqlx::query("UPDATE channels SET owner_id=$3,writer_epoch=writer_epoch+1,lease_until=clock_timestamp()+($4::bigint * interval '1 millisecond') WHERE org_id=$1 AND conversation_id=$2 AND (lease_until IS NULL OR lease_until<clock_timestamp()) RETURNING writer_epoch")
            .bind(org).bind(conv).bind(owner).bind(ttl_ms as i64).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Ok(None);
        };
        let epoch: i64 = row.get("writer_epoch");
        let lease = Lease {
            org_id: org,
            conversation_id: conv,
            owner_id: owner,
            epoch,
        };
        sqlx::query("UPDATE conversations SET owner_epoch=$3 WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(conv)
            .bind(epoch)
            .execute(&mut *tx)
            .await?;
        let runs:Vec<(Uuid,String,bool)>=sqlx::query_as("UPDATE runs SET exec_epoch=$3 WHERE org_id=$1 AND conversation_id=$2 AND state IN ('running','requires_action') RETURNING id,state,read_only").bind(org).bind(conv).bind(epoch).fetch_all(&mut *tx).await?;
        for (run, state, read_only) in runs {
            append(
                &mut tx,
                org,
                &lease.channel(),
                Some(&lease),
                "",
                Event::RunStateChanged(RunState {
                    read_only,
                    run_id: run.to_string(),
                    state,
                    exec_epoch: epoch as u64,
                }),
            )
            .await?;
            let actions:Vec<ActionRow>=sqlx::query_as("UPDATE pending_actions SET run_epoch=$3 WHERE org_id=$1 AND run_id=$2 AND status='pending' RETURNING id,run_id,run_epoch,summary,options,status,resolved_device,expires_at").bind(org).bind(run).bind(epoch).fetch_all(&mut *tx).await?;
            for a in actions {
                append(
                    &mut tx,
                    org,
                    &lease.channel(),
                    Some(&lease),
                    "",
                    Event::PendingActionChanged(a.into()),
                )
                .await?;
            }
        }
        tx.commit().await?;
        Ok(Some(lease))
    }
    pub async fn renew(&self, lease: &Lease, ttl: u64) -> anyhow::Result<()> {
        let mut tx = self.tx(lease.org_id).await?;
        let n=sqlx::query!("UPDATE channels SET lease_until=clock_timestamp()+($5::bigint * interval '1 millisecond') WHERE org_id=$1 AND id=$2 AND writer_epoch=$3 AND owner_id=$4 AND lease_until>clock_timestamp()",lease.org_id,lease.channel(),lease.epoch,lease.owner_id,ttl as i64).execute(&mut *tx).await?.rows_affected();
        ensure!(n == 1, "FENCED");
        tx.commit().await?;
        Ok(())
    }
    pub async fn unpublished(
        &self,
        org: Uuid,
        channel: &str,
    ) -> anyhow::Result<Vec<EventEnvelope>> {
        let mut tx = self.tx(org).await?;
        let bytes:Vec<Vec<u8>>=sqlx::query_scalar("SELECT e.payload FROM events e JOIN channels c ON c.org_id=e.org_id AND c.id=e.channel_id WHERE e.org_id=$1 AND e.channel_id=$2 AND e.seq>c.published_seq ORDER BY e.seq LIMIT 256").bind(org).bind(channel).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        bytes
            .into_iter()
            .map(|b| EventEnvelope::decode(b.as_slice()).map_err(Into::into))
            .collect()
    }
    pub async fn published(
        &self,
        org: Uuid,
        channel: &str,
        seq: u64,
        lease: Option<&Lease>,
    ) -> anyhow::Result<()> {
        let mut tx = self.tx(org).await?;
        let n=sqlx::query("UPDATE channels SET published_seq=greatest(published_seq,$3) WHERE org_id=$1 AND id=$2 AND (($4::bigint IS NULL AND writer_epoch IS NULL) OR (writer_epoch=$4 AND owner_id=$5 AND lease_until>clock_timestamp()))")
            .bind(org).bind(channel).bind(seq as i64).bind(lease.map(|l|l.epoch)).bind(lease.map(|l|l.owner_id)).execute(&mut *tx).await?.rows_affected();
        ensure!(n == 1, "FENCED");
        tx.commit().await?;
        Ok(())
    }
}

pub async fn context_in(tx: &mut Tx<'_>, user: Uuid, org: Uuid) -> anyhow::Result<OrgContext> {
    let role:Option<String>=sqlx::query_scalar("SELECT m.role FROM memberships m JOIN organizations o ON o.id=m.org_id JOIN users u ON u.id=m.user_id WHERE m.org_id=$1 AND m.user_id=$2 AND o.state='active' AND u.status='active'").bind(org).bind(user).fetch_optional(&mut **tx).await?;
    let role = match role.as_deref() {
        Some("owner") => Role::Owner,
        Some("admin") => Role::Admin,
        Some("member") => Role::Member,
        _ => bail!("FORBIDDEN"),
    };
    Ok(OrgContext {
        org_id: org,
        user_id: user,
        role,
    })
}
pub async fn conversation_in(
    tx: &mut Tx<'_>,
    ctx: &OrgContext,
    id: Uuid,
) -> anyhow::Result<Conversation> {
    let c:Conversation=sqlx::query_as("SELECT id,org_id,creator_id,title,mode,state,project_id,run_config FROM conversations WHERE org_id=$1 AND id=$2 AND state IN ('active','archived')").bind(ctx.org_id).bind(id).fetch_optional(&mut **tx).await?.context("FORBIDDEN")?;
    authorize(
        &Principal::User(ctx.clone()),
        Action::UseConversation,
        &Resource {
            org_id: c.org_id,
            id: c.id,
            owner_user_id: Some(c.creator_id),
            target_role: None,
        },
    )?;
    Ok(c)
}
pub async fn audit(
    tx: &mut Tx<'_>,
    ctx: &OrgContext,
    action: &str,
    resource: &str,
    result: &str,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO audit_logs(id,org_id,actor_id,actor_kind,action,resource,result) VALUES($1,$2,$3,'user',$4,$5,$6)").bind(Uuid::now_v7()).bind(ctx.org_id).bind(ctx.user_id).bind(action).bind(resource).bind(result).execute(&mut **tx).await?;
    Ok(())
}
/// Caller transaction contains the projection, command result, sequence allocation and outbox.
/// The channel row lock plus epoch/lease CAS fences every durable write.
pub async fn append(
    tx: &mut Tx<'_>,
    org: Uuid,
    channel: &str,
    lease: Option<&Lease>,
    cid: &str,
    mut event: Event,
) -> anyhow::Result<EventEnvelope> {
    let seq:Option<i64>=sqlx::query_scalar!("UPDATE channels SET last_seq=last_seq+1 WHERE org_id=$1 AND id=$2 AND (($3::bigint IS NULL AND writer_epoch IS NULL) OR (writer_epoch=$3 AND owner_id=$4 AND lease_until>clock_timestamp())) RETURNING last_seq", org,channel,lease.map(|l|l.epoch),lease.map(|l|l.owner_id)).fetch_optional(&mut **tx).await?;
    let seq = seq.context("FENCED")?;
    if let Event::ItemUpserted(item) = &mut event {
        item.last_seq = seq as u64;
        if item.first_seq == 0 {
            item.first_seq = seq as u64;
        }
        if let Some(lease) = lease {
            sqlx::query("INSERT INTO items(id,org_id,conversation_id,run_id,revision,kind,state,text,first_seq,last_seq) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT(id) DO UPDATE SET revision=EXCLUDED.revision,state=EXCLUDED.state,text=EXCLUDED.text,last_seq=EXCLUDED.last_seq WHERE items.org_id=EXCLUDED.org_id AND items.revision<EXCLUDED.revision")
                .bind(Uuid::parse_str(&item.id)?).bind(org).bind(lease.conversation_id).bind(Uuid::parse_str(&item.run_id).ok()).bind(item.revision as i64).bind(&item.kind).bind(&item.state).bind(&item.text).bind(item.first_seq as i64).bind(seq).execute(&mut **tx).await?;
        }
    }
    let kind = match &event {
        Event::ItemUpserted(_) => "item_upserted",
        Event::PendingActionChanged(_) => "pending_action_changed",
        Event::RunStateChanged(_) => "run_state_changed",
        Event::ReadStateChanged(_) => "read_state_changed",
    };
    let envelope = EventEnvelope {
        seq: seq as u64,
        cause_cid: cid.into(),
        event: Some(event),
    };
    sqlx::query("INSERT INTO events(channel_id,seq,org_id,kind,cause_cid,payload) VALUES($1,$2,$3,$4,$5,$6)").bind(channel).bind(seq).bind(org).bind(kind).bind(Uuid::parse_str(cid).ok()).bind(envelope.encode_to_vec()).execute(&mut **tx).await?;
    sqlx::query("SELECT pg_notify('ryzhub_outbox',$1)")
        .bind(channel)
        .execute(&mut **tx)
        .await?;
    Ok(envelope)
}
