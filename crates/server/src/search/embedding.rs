//! Real embedding requests share the gateway's credential policy and usage ledger.
use crate::{
    app::App,
    error::Code,
    llm::credentials,
    store::{context_in, effective_policy},
};
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use uuid::Uuid;

pub trait EmbeddingProvider: Send + Sync {
    fn embed(
        &self,
        app: &App,
        org: Uuid,
        user: Uuid,
        texts: Vec<String>,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<Vec<f32>>>> + Send;
}
pub struct OpenAiEmbedding;
impl EmbeddingProvider for OpenAiEmbedding {
    async fn embed(
        &self,
        app: &App,
        org: Uuid,
        user: Uuid,
        texts: Vec<String>,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        let cfg = &app.config.search;
        ensure!(!cfg.embedding_url.is_empty(), Code::TemporarilyUnavailable);
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let mut tx = app.store.tx(org).await?;
        context_in(&mut tx, user, org).await?;
        let policy = effective_policy(&mut tx, &app.config.policy, org).await?;
        ensure!(
            policy
                .models
                .as_ref()
                .is_none_or(|models| models.contains(&cfg.embedding_model)),
            Code::PolicyDenied
        );
        let (credential, secret) = if cfg.embedding_key_env.is_empty() {
            credentials::resolve(app, &mut tx, org, user, None, &policy, "openai").await?
        } else {
            ensure!(
                policy.allow_platform_credentials != Some(false),
                Code::PlatformCredentialDenied
            );
            (
                None,
                std::env::var(&cfg.embedding_key_env).context(Code::CredentialUnavailable)?,
            )
        };
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO usage_records(id,org_id,user_id,model,credential_id,provider) VALUES($1,$2,$3,$4,$5,'openai')")
            .bind(id).bind(org).bind(user).bind(&cfg.embedding_model).bind(credential).execute(&mut *tx).await?;
        tx.commit().await?;
        let result=async {
            let response=app.http.upstream.post(format!("{}/v1/embeddings",cfg.embedding_url.trim_end_matches('/')))
                .bearer_auth(secret).json(&json!({"model":cfg.embedding_model,"input":texts,"dimensions":1024,"encoding_format":"float"}))
                .send().await?.error_for_status()?;
            let mut stream=response.bytes_stream();
            use futures_util::StreamExt;
            let mut bytes=Vec::new();
            while let Some(chunk)=stream.next().await { bytes.extend_from_slice(&chunk?); ensure!(bytes.len()<=8*1024*1024,Code::ContentTooLarge); }
            let value:Value=serde_json::from_slice(&bytes)?;
            let mut rows=value["data"].as_array().context("embedding data missing")?.clone();
            rows.sort_by_key(|v|v["index"].as_u64().unwrap_or(u64::MAX));
            ensure!(rows.len()==texts.len(),"embedding count mismatch");
            let vectors=rows.iter().enumerate().map(|(i,v)| {
                ensure!(v["index"].as_u64()==Some(i as u64),"embedding index mismatch");
                let vector:Vec<f32>=serde_json::from_value(v["embedding"].clone())?;
                ensure!(vector.len()==1024 && vector.iter().all(|v|v.is_finite() && v.abs() <= 65504.0)
                    && vector.iter().any(|v|*v!=0.0),"invalid embedding");
                Ok(vector)
            }).collect::<anyhow::Result<Vec<_>>>()?;
            Ok::<_,anyhow::Error>((vectors,value["usage"].clone()))
        }.await;
        let mut tx = app.store.tx(org).await?;
        let usage = result.as_ref().map(|(_, u)| u.clone()).unwrap_or(json!({}));
        sqlx::query("UPDATE usage_records SET status=$3,raw_usage=$4,input_tokens=$5,estimated=$6,finished_at=now() WHERE org_id=$1 AND id=$2")
            .bind(org).bind(id).bind(if result.is_ok(){"completed"}else{"failed"}).bind(&usage)
            .bind(usage["prompt_tokens"].as_i64().unwrap_or(0)).bind(!usage["prompt_tokens"].is_number()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(result?.0)
    }
}
pub fn vector_literal(v: &[f32]) -> String {
    format!(
        "[{}]",
        v.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}
/// Character windows bound UTF-8 safely, with 12.5% overlap (~512-800 multilingual tokens).
pub fn chunks(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = vec![];
    let mut start = 0;
    while start < chars.len() {
        let end = (start + 1800).min(chars.len());
        out.push(chars[start..end].iter().collect());
        if end == chars.len() {
            break;
        }
        start = end - 225;
    }
    out
}
