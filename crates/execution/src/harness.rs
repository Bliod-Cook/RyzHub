//! The platform's harness protocol.
//!
//! The control plane speaks only these types. The guest translates them to and from each
//! harness's native protocol (see `native`), so a harness upgrade changes the guest adapter,
//! never the Owner.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Settings that apply to the next turn.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TurnSettings {
    pub model: String,
    pub reasoning: String,
    /// `ask`, `allow_edits` or `full_auto`.
    pub approval: String,
    pub plan: bool,
}

/// A request from the Owner to the harness.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessRequest {
    StartThread {
        settings: TurnSettings,
        instructions: String,
    },
    ResumeThread {
        thread_id: String,
        settings: TurnSettings,
        instructions: String,
    },
    /// Continue a copy of `thread_id` truncated after `last_turn_id` / `message_id`.
    ForkThread {
        thread_id: String,
        last_turn_id: Option<String>,
        message_id: Option<String>,
        settings: TurnSettings,
        instructions: String,
    },
    /// Session-level settings for harnesses that hold them between turns.
    Configure {
        settings: TurnSettings,
        instructions: String,
    },
    StartTurn {
        thread_id: String,
        text: String,
        settings: TurnSettings,
    },
    SteerTurn {
        thread_id: String,
        turn_id: String,
        text: String,
    },
    InterruptTurn {
        thread_id: String,
        turn_id: String,
    },
    Compact {
        thread_id: String,
    },
    ListModels,
}

/// Normalized result of a completed [`HarnessRequest`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HarnessResult {
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub models: Vec<ModelInfo>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelInfo {
    pub id: String,
    pub reasoning_levels: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Assistant,
    Reasoning,
    Plan,
    Command,
    FileChange,
    ToolCall,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemPhase {
    Started,
    Completed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalCategory {
    Command,
    FileChange,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FileChange {
    pub path: String,
    pub change: String,
    pub diff: String,
}

/// Structured content of a timeline item.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ItemContent {
    /// Display text.
    pub text: String,
    /// Tool or command name.
    pub name: Option<String>,
    pub arguments_json: Option<String>,
    pub result_json: Option<String>,
    pub output: Option<String>,
    pub file_changes: Vec<FileChange>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Question {
    pub id: String,
    pub question: String,
    /// Harness-specific option list, passed through for display.
    pub options: serde_json::Value,
}

/// A durable event from the harness, journaled by the guest in order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessEvent {
    TurnStarted {
        turn_id: String,
    },
    TurnCompleted {
        turn_id: Option<String>,
        status: TurnStatus,
    },
    Item {
        phase: ItemPhase,
        turn_id: Option<String>,
        native_id: String,
        kind: ItemKind,
        content: ItemContent,
        /// Native message id, for forking after this message.
        message_id: Option<String>,
    },
    ApprovalRequested {
        request_id: String,
        turn_id: Option<String>,
        category: ApprovalCategory,
        summary: String,
    },
    QuestionRequested {
        request_id: String,
        turn_id: Option<String>,
        questions: Vec<Question>,
    },
    Capabilities {
        commands: serde_json::Value,
        models: Vec<ModelInfo>,
    },
    /// The harness process ended; requests in flight have an unknown outcome.
    Exited {
        exit_code: Option<i32>,
        error: Option<String>,
    },
}

impl HarnessEvent {
    pub fn turn_id(&self) -> Option<&str> {
        match self {
            Self::TurnStarted { turn_id } => Some(turn_id),
            Self::TurnCompleted { turn_id, .. }
            | Self::Item { turn_id, .. }
            | Self::ApprovalRequested { turn_id, .. }
            | Self::QuestionRequested { turn_id, .. } => turn_id.as_deref(),
            Self::Capabilities { .. } | Self::Exited { .. } => None,
        }
    }
}

/// The user's decision on an approval or question.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessAnswer {
    Approval { allow: bool },
    Question { answers: BTreeMap<String, String> },
}
