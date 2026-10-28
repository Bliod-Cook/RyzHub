//! Embeddings come straight from the provider (the gateway proxies no embeddings) under the
//! gateway's credential policy and usage ledger.
use crate::{
    app::App,
    error::Code,
    llm::credentials,
    store::{context_in, effective_policy},
};
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use uuid::Uuid;

/// Stored vectors have this many dimensions.
pub const DIMENSIONS: usize = 1024;

/// Vectors for `texts` from the configured provider.
pub async fn embed(
    app: &App,
    org: Uuid,
    user: Uuid,
    texts: Vec<String>,
) -> anyhow::Result<Vec<Vec<f32>>> {
    if app.config().search.embedding_provider == "local" {
        #[cfg(feature = "local-embedding")]
        return local(app, texts).await;
        #[cfg(not(feature = "local-embedding"))]
        anyhow::bail!(Code::TemporarilyUnavailable);
    }
    remote(app, org, user, texts).await
}

/// An OpenAI-compatible `/v1/embeddings` endpoint.
async fn remote(
    app: &App,
    org: Uuid,
    user: Uuid,
    texts: Vec<String>,
) -> anyhow::Result<Vec<Vec<f32>>> {
    let config = app.config();
    let cfg = &config.search;
    ensure!(!cfg.embedding_url.is_empty(), Code::TemporarilyUnavailable);
    if texts.is_empty() {
        return Ok(vec![]);
    }
    let mut tx = app.store.tx(org).await?;
    context_in(&mut tx, user, org).await?;
    let policy = effective_policy(&mut tx, &config.policy, org).await?;
    ensure!(
        policy
            .models
            .as_ref()
            .is_none_or(|models| models.contains(&cfg.embedding_model)),
        Code::PolicyDenied
    );
    // A dedicated key, else the member's or organization's OpenAI key, else the platform's.
    let member = if cfg.embedding_key_env.is_empty() {
        credentials::member_key(app, &mut tx, org, user, &policy, "openai").await?
    } else {
        None
    };
    let (credential, secret) = match member {
        Some((id, key)) => (Some(id), key),
        None => {
            ensure!(
                policy.allow_platform_credentials != Some(false),
                Code::PlatformCredentialDenied
            );
            let env = if cfg.embedding_key_env.is_empty() {
                &config.llm.responses_key_env
            } else {
                &cfg.embedding_key_env
            };
            (
                None,
                std::env::var(env).context(Code::CredentialUnavailable)?,
            )
        }
    };
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO usage_records(id,org_id,user_id,model,credential_id,provider)
         VALUES($1,$2,$3,$4,$5,'openai')",
    )
    .bind(id)
    .bind(org)
    .bind(user)
    .bind(&cfg.embedding_model)
    .bind(credential)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let result = async {
        let url = format!("{}/v1/embeddings", cfg.embedding_url.trim_end_matches('/'));
        let request = json!({"model": cfg.embedding_model, "input": texts,
            "dimensions": DIMENSIONS, "encoding_format": "float"});
        let response = app
            .http
            .upstream
            .post(url)
            .bearer_auth(secret)
            .json(&request)
            .send()
            .await?
            .error_for_status()?;
        let mut stream = response.bytes_stream();
        use futures_util::StreamExt;
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk?);
            ensure!(bytes.len() <= 8 * 1024 * 1024, Code::ContentTooLarge);
        }
        let value: Value = serde_json::from_slice(&bytes)?;
        Ok::<_, anyhow::Error>((vectors(&value, texts.len())?, value["usage"].clone()))
    }
    .await;
    let mut tx = app.store.tx(org).await?;
    let usage = result.as_ref().map(|(_, u)| u.clone()).unwrap_or(json!({}));
    sqlx::query(
        "UPDATE usage_records SET status=$3,raw_usage=$4,input_tokens=$5,estimated=$6,
         finished_at=now() WHERE org_id=$1 AND id=$2",
    )
    .bind(org)
    .bind(id)
    .bind(if result.is_ok() {
        "completed"
    } else {
        "failed"
    })
    .bind(&usage)
    .bind(usage["prompt_tokens"].as_i64().unwrap_or(0))
    .bind(!usage["prompt_tokens"].is_number())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result?.0)
}

/// The vectors of an embeddings response for `count` inputs, in input order, at unit length.
fn vectors(response: &Value, count: usize) -> anyhow::Result<Vec<Vec<f32>>> {
    let mut rows = response["data"]
        .as_array()
        .context("embedding data missing")?
        .clone();
    rows.sort_by_key(|v| v["index"].as_u64().unwrap_or(u64::MAX));
    ensure!(rows.len() == count, "embedding count mismatch");
    rows.iter()
        .enumerate()
        .map(|(i, v)| {
            ensure!(
                v["index"].as_u64() == Some(i as u64),
                "embedding index mismatch"
            );
            // Longer vectors (Gemini's are Matryoshka-trained, 3072 wide) keep their meaning
            // when cut.
            truncated(serde_json::from_value(v["embedding"].clone())?, DIMENSIONS)
        })
        .collect()
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

#[cfg(feature = "local-embedding")]
static LOCAL_MODEL: tokio::sync::OnceCell<std::sync::Arc<fastembed::Qwen3TextEmbedding>> =
    tokio::sync::OnceCell::const_new();
#[cfg(feature = "local-embedding")]
static LOCAL_TURN: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// Qwen3-Embedding in this process, on candle (CPU). The model loads on first use; texts are
/// embedded one batch at a time, off the async runtime. Vectors are cut to [`DIMENSIONS`]
/// (Qwen3 embeddings are trained to keep their meaning when truncated) and renormalized.
/// Nothing leaves the server, so no credential, policy model list or usage record applies.
#[cfg(feature = "local-embedding")]
async fn local(app: &App, texts: Vec<String>) -> anyhow::Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(vec![]);
    }
    let name = app.config().search.local_embedding_model.clone();
    let model = LOCAL_MODEL
        .get_or_try_init(|| async move {
            tokio::task::spawn_blocking(move || {
                fastembed::Qwen3TextEmbedding::from_hf(
                    &name,
                    &candle_core::Device::Cpu,
                    candle_core::DType::F32,
                    1024,
                )
                .map(std::sync::Arc::new)
                .map_err(|error| anyhow::anyhow!("loading {name}: {error}"))
            })
            .await?
        })
        .await
        .context(Code::TemporarilyUnavailable)?
        .clone();
    let _turn = LOCAL_TURN.acquire().await?;
    let vectors = tokio::task::spawn_blocking(move || model.embed(&texts))
        .await?
        .map_err(|error| anyhow::anyhow!("embedding: {error}"))?;
    vectors
        .into_iter()
        .map(|v| truncated(v, DIMENSIONS))
        .collect()
}

/// `vector` cut to `dimensions` and scaled back to unit length.
pub fn truncated(mut vector: Vec<f32>, dimensions: usize) -> anyhow::Result<Vec<f32>> {
    ensure!(vector.len() >= dimensions, "embedding too short");
    vector.truncate(dimensions);
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    ensure!(norm.is_finite() && norm > 0.0, "invalid embedding");
    Ok(vector.into_iter().map(|v| v / norm).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncated_vectors_keep_unit_length() {
        let v = truncated(vec![3.0, 4.0, 12.0], 2).unwrap();
        assert_eq!(v, vec![0.6, 0.8]);
        assert!(truncated(vec![1.0], 2).is_err());
        assert!(truncated(vec![0.0, 0.0], 2).is_err());
    }

    #[test]
    fn longer_provider_vectors_are_cut_to_the_stored_dimensions() {
        // Gemini embeddings come with 3072 dimensions whatever `dimensions` asks for.
        let response = |len: usize| {
            let mut embedding = vec![0.0f32; len];
            embedding[0] = 3.0;
            embedding[1] = 4.0;
            embedding[len - 1] = 12.0;
            json!({"data": [{"index": 0, "embedding": embedding}]})
        };
        let vector = &vectors(&response(3072), 1).unwrap()[0];
        assert_eq!(vector.len(), DIMENSIONS);
        assert_eq!(&vector[..2], &[0.6, 0.8]);
        assert!(vectors(&response(DIMENSIONS - 1), 1).is_err());
    }

    /// Downloads Qwen3-Embedding-0.6B (about 1.2 GB) into `HF_HOME` on first run.
    #[cfg(feature = "local-embedding")]
    #[test]
    #[ignore = "downloads the embedding model"]
    fn local_model_embeds_meaning() {
        let model = fastembed::Qwen3TextEmbedding::from_hf(
            "Qwen/Qwen3-Embedding-0.6B",
            &candle_core::Device::Cpu,
            candle_core::DType::F32,
            1024,
        )
        .unwrap();
        let vectors = model
            .embed(&[
                "How do I reset my password?",
                "忘记密码怎么重置",
                "Volcanoes erupt lava.",
            ])
            .unwrap();
        let vectors: Vec<Vec<f32>> = vectors
            .into_iter()
            .map(|v| truncated(v, DIMENSIONS).unwrap())
            .collect();
        assert!(vectors.iter().all(|v| v.len() == DIMENSIONS));
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        assert!(dot(&vectors[0], &vectors[1]) > dot(&vectors[0], &vectors[2]));
    }
}
