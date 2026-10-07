//! RyzHub control plane.
//!
//! - [`http`] and [`ws`]: the client-facing API (REST and WebSocket).
//! - [`owner`]: one actor per active conversation; orders commands and publishes events.
//! - [`runs`], [`chat`], [`work`]: run lifecycle and the two engines that drive runs.
//! - [`llm`], [`mcp`]: the model and tool gateways, the only paths out to providers.
//! - [`sandbox`], [`workspace`]: execution nodes, checkpoints and restores.
//! - [`store`]: PostgreSQL access under row-level security.

/// `anyhow::ensure!` for a stable [`error::Code`] that works in both `anyhow::Result` and
/// `ApiResult` functions.
macro_rules! require {
    ($condition:expr, $code:expr $(,)?) => {
        if !$condition {
            return Err($code.into());
        }
    };
}

/// Column lists shared by every query that loads a row type in `store::rows`. They expand to
/// string literals, so they compose with `concat!` into static SQL.
macro_rules! item_columns {
    () => {
        "id,revision,kind,state,text,first_seq,last_seq,run_id,truncated,detail"
    };
}
macro_rules! action_columns {
    () => {
        "id,run_id,run_epoch,kind,summary,options,status,resolved_device,expires_at"
    };
}
macro_rules! conversation_columns {
    () => {
        "id,org_id,creator_id,title,mode,state,project_id,run_config,temporary,\
         (extract(epoch FROM pinned_at)*1000)::bigint AS pinned_at_ms"
    };
}
/// A run's start and end in Unix milliseconds, as `RunState` carries them.
macro_rules! run_times {
    () => {
        "(extract(epoch FROM started_at)*1000)::bigint,(extract(epoch FROM ended_at)*1000)::bigint"
    };
}

pub mod app;
pub mod attachments;
pub mod chat;
pub mod config;
pub mod conversation_commands;
pub mod error;
pub mod http;
pub mod llm;
pub mod maintenance;
pub mod mcp;
pub mod memory;
pub mod owner;
pub mod projects;
pub mod provider;
pub mod retention;
pub mod revocation;
pub mod run_config;
pub mod runs;
pub mod sandbox;
pub mod search;
pub mod store;
pub mod work;
pub mod workspace;
pub mod ws;
