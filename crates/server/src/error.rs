//! Stable error codes shared by REST responses, WebSocket rejections and run failures.
//!
//! A [`Code`] is the only error text that crosses a trust boundary. Everything else is an
//! internal error: it is logged with full context and reported as `INTERNAL_ERROR`.
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

macro_rules! codes {
    ($($name:ident = $text:literal => $status:ident,)*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Code { $($name,)* }
        impl Code {
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$name => $text,)* }
            }
            pub const fn status(self) -> StatusCode {
                match self { $(Self::$name => StatusCode::$status,)* }
            }
            pub fn parse(text: &str) -> Option<Self> {
                match text { $($text => Some(Self::$name),)* _ => None }
            }
        }
    };
}

codes! {
    // Identity and access.
    Unauthorized = "UNAUTHORIZED" => UNAUTHORIZED,
    InvalidCredentials = "INVALID_CREDENTIALS" => UNAUTHORIZED,
    Forbidden = "FORBIDDEN" => FORBIDDEN,
    RateLimited = "RATE_LIMITED" => TOO_MANY_REQUESTS,
    PolicyDenied = "POLICY_DENIED" => BAD_REQUEST,
    ModelDenied = "MODEL_DENIED" => FORBIDDEN,
    PlatformCredentialDenied = "PLATFORM_CREDENTIAL_DENIED" => FORBIDDEN,
    CredentialUnavailable = "CREDENTIAL_UNAVAILABLE" => SERVICE_UNAVAILABLE,
    CredentialRevoked = "CREDENTIAL_REVOKED" => SERVICE_UNAVAILABLE,
    // Invalid requests.
    InvalidInput = "INVALID_INPUT" => BAD_REQUEST,
    InvalidConversation = "INVALID_CONVERSATION" => BAD_REQUEST,
    InvalidMode = "INVALID_MODE" => BAD_REQUEST,
    InvalidMessage = "INVALID_MESSAGE" => BAD_REQUEST,
    InvalidCid = "INVALID_CID" => BAD_REQUEST,
    InvalidCommand = "INVALID_COMMAND" => BAD_REQUEST,
    InvalidChannel = "INVALID_CHANNEL" => BAD_REQUEST,
    CidReused = "CID_REUSED" => CONFLICT,
    UnsupportedCommand = "UNSUPPORTED_COMMAND" => BAD_REQUEST,
    InvalidDecision = "INVALID_DECISION" => BAD_REQUEST,
    InvalidAction = "INVALID_ACTION" => BAD_REQUEST,
    InvalidReadSeq = "INVALID_READ_SEQ" => BAD_REQUEST,
    InvalidConfig = "INVALID_CONFIG" => BAD_REQUEST,
    InvalidHarness = "INVALID_HARNESS" => BAD_REQUEST,
    InvalidReasoning = "INVALID_REASONING" => BAD_REQUEST,
    InvalidApprovalMode = "INVALID_APPROVAL_MODE" => BAD_REQUEST,
    InvalidProtocol = "INVALID_PROTOCOL" => BAD_REQUEST,
    ModelNotRoutable = "MODEL_NOT_ROUTABLE" => BAD_REQUEST,
    ModelUnsupported = "MODEL_UNSUPPORTED" => BAD_REQUEST,
    PlanUnsupported = "PLAN_UNSUPPORTED" => BAD_REQUEST,
    CompactUnsupported = "COMPACT_UNSUPPORTED" => BAD_REQUEST,
    HarnessProtocolImmutable = "HARNESS_PROTOCOL_IMMUTABLE" => BAD_REQUEST,
    InvalidProject = "INVALID_PROJECT" => BAD_REQUEST,
    InvalidProjectFile = "INVALID_PROJECT_FILE" => BAD_REQUEST,
    InvalidDefaults = "INVALID_DEFAULTS" => BAD_REQUEST,
    MemoryModeImmutable = "MEMORY_MODE_IMMUTABLE" => BAD_REQUEST,
    ProjectFileLimit = "PROJECT_FILE_LIMIT" => BAD_REQUEST,
    ProjectDeleted = "PROJECT_DELETED" => BAD_REQUEST,
    RestoreExpired = "RESTORE_EXPIRED" => BAD_REQUEST,
    InvalidFork = "INVALID_FORK" => BAD_REQUEST,
    ForkPointUnavailable = "FORK_POINT_UNAVAILABLE" => BAD_REQUEST,
    WorkOnly = "WORK_ONLY" => BAD_REQUEST,
    UseWorkspaceArchive = "USE_WORKSPACE_ARCHIVE" => BAD_REQUEST,
    InvalidArguments = "INVALID_ARGUMENTS" => BAD_REQUEST,
    InvalidToolArguments = "INVALID_TOOL_ARGUMENTS" => BAD_REQUEST,
    InvalidTool = "INVALID_TOOL" => BAD_REQUEST,
    MethodUnsupported = "METHOD_UNSUPPORTED" => BAD_REQUEST,
    RequestIdRequired = "REQUEST_ID_REQUIRED" => BAD_REQUEST,
    RequestIdReused = "REQUEST_ID_REUSED" => CONFLICT,
    // Conflicting state.
    Fenced = "FENCED" => CONFLICT,
    ConversationArchived = "CONVERSATION_ARCHIVED" => CONFLICT,
    ConversationBusy = "CONVERSATION_BUSY" => CONFLICT,
    RunBusy = "RUN_BUSY" => CONFLICT,
    RunBusyUseSteer = "RUN_BUSY_USE_STEER" => CONFLICT,
    NoActiveRun = "NO_ACTIVE_RUN" => CONFLICT,
    ActionAlreadyResolved = "ACTION_ALREADY_RESOLVED" => CONFLICT,
    WorkspaceBusy = "WORKSPACE_BUSY" => CONFLICT,
    WorkspaceRecoveryRequired = "WORKSPACE_RECOVERY_REQUIRED" => CONFLICT,
    OrgConcurrencyLimit = "ORG_CONCURRENCY_LIMIT" => CONFLICT,
    NoCheckpoint = "NO_CHECKPOINT" => CONFLICT,
    CheckpointIncompatible = "CHECKPOINT_INCOMPATIBLE" => CONFLICT,
    OldWriterNotFenced = "OLD_WRITER_NOT_FENCED" => CONFLICT,
    // Availability.
    Busy = "BUSY" => SERVICE_UNAVAILABLE,
    OwnerUnavailable = "OWNER_UNAVAILABLE" => SERVICE_UNAVAILABLE,
    TemporarilyUnavailable = "TEMPORARILY_UNAVAILABLE" => SERVICE_UNAVAILABLE,
    ModelGatewayDisabled = "MODEL_GATEWAY_DISABLED" => SERVICE_UNAVAILABLE,
    SandboxDisabled = "SANDBOX_DISABLED" => SERVICE_UNAVAILABLE,
    // Upstream model and tool calls.
    UpstreamUnavailable = "UPSTREAM_UNAVAILABLE" => BAD_GATEWAY,
    UpstreamStreamError = "UPSTREAM_STREAM_ERROR" => BAD_GATEWAY,
    UpstreamStreamInterrupted = "UPSTREAM_STREAM_INTERRUPTED" => BAD_GATEWAY,
    ModelRequestFailed = "MODEL_REQUEST_FAILED" => BAD_GATEWAY,
    ModelAuthenticationFailed = "MODEL_AUTHENTICATION_FAILED" => BAD_GATEWAY,
    ModelConnectionFailed = "MODEL_CONNECTION_FAILED" => BAD_GATEWAY,
    ModelStreamInterrupted = "MODEL_STREAM_INTERRUPTED" => BAD_GATEWAY,
    SandboxIdentityRequired = "SANDBOX_IDENTITY_REQUIRED" => UNAUTHORIZED,
    ToolDenied = "TOOL_DENIED" => FORBIDDEN,
    ApprovalDeniedOrExpired = "APPROVAL_DENIED_OR_EXPIRED" => FORBIDDEN,
    OutcomeUnknownDoNotRetry = "OUTCOME_UNKNOWN_DO_NOT_RETRY" => CONFLICT,
    McpUpstreamFailed = "MCP_UPSTREAM_FAILED" => BAD_GATEWAY,
    // Terminal run failures shown in the timeline.
    LoopLimitReached = "LOOP_LIMIT_REACHED" => CONFLICT,
    TooManyToolCalls = "TOO_MANY_TOOL_CALLS" => BAD_GATEWAY,
    ReadOnlyToolRequestRejected = "READ_ONLY_TOOL_REQUEST_REJECTED" => BAD_GATEWAY,
    ContentTooLarge = "CONTENT_TOO_LARGE" => BAD_GATEWAY,
    PtyUnsupported = "PTY_UNSUPPORTED" => BAD_REQUEST,
    NativeTurnFailed = "NATIVE_TURN_FAILED" => BAD_GATEWAY,
    NativeOutcomeUnknown = "NATIVE_OUTCOME_UNKNOWN" => BAD_GATEWAY,
    CheckpointRecoveryRpo = "CHECKPOINT_RECOVERY_RPO" => CONFLICT,
    AuthorizationOrActionExpired = "AUTHORIZATION_OR_ACTION_EXPIRED" => FORBIDDEN,
    // WebSocket protocol.
    IncompatibleProtocol = "INCOMPATIBLE_PROTOCOL" => BAD_REQUEST,
    UnexpectedFrame = "UNEXPECTED_FRAME" => BAD_REQUEST,
    InternalError = "INTERNAL_ERROR" => INTERNAL_SERVER_ERROR,
}

impl Code {
    /// The same request may succeed later without any client-side change.
    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::Busy
                | Self::OwnerUnavailable
                | Self::TemporarilyUnavailable
                | Self::RateLimited
                | Self::UpstreamUnavailable
                | Self::WorkspaceBusy
                | Self::ConversationBusy
        )
    }
}

impl std::fmt::Display for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for Code {}

/// The stable code carried anywhere in an error chain.
pub fn code_of(error: &anyhow::Error) -> Option<Code> {
    // anyhow's own downcast sees codes attached with `.context()`, including on an `Option`
    // (which wraps them in a display adapter the plain source chain cannot see through).
    error.downcast_ref::<Code>().copied().or_else(|| {
        error
            .chain()
            .find_map(|cause| cause.downcast_ref::<Code>().copied())
    })
}

/// `error` unchanged if it already carries a code, otherwise with `code` attached.
pub fn or_code(error: anyhow::Error, code: Code) -> anyhow::Error {
    if code_of(&error).is_some() {
        error
    } else {
        error.context(code)
    }
}

pub fn is_code(error: &anyhow::Error, code: Code) -> bool {
    code_of(error) == Some(code)
}

pub fn is_fenced(error: &anyhow::Error) -> bool {
    is_code(error, Code::Fenced)
}

/// The code a client may see. Internal errors are logged here, once, with full context.
pub fn public_code(error: &anyhow::Error) -> Code {
    code_of(error).unwrap_or_else(|| {
        tracing::error!(error = format!("{error:#}"), "internal error");
        Code::InternalError
    })
}

/// Axum error: stable code in the body, HTTP status from the code table.
pub struct ApiError(pub anyhow::Error);
impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(error: E) -> Self {
        Self(error.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = public_code(&self.0);
        (code.status(), Json(json!({"error": code.as_str()}))).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn codes_survive_context_and_round_trip_through_text() {
        let error = Err::<(), _>(Code::Fenced)
            .context("while renewing")
            .unwrap_err();
        assert!(is_fenced(&error));
        assert_eq!(Code::parse("WORKSPACE_BUSY"), Some(Code::WorkspaceBusy));
        assert_eq!(Code::WorkspaceBusy.to_string(), "WORKSPACE_BUSY");
        assert_eq!(
            public_code(&anyhow::anyhow!("disk full")),
            Code::InternalError
        );
        let wrapped = None::<()>.context(Code::Forbidden).unwrap_err();
        assert_eq!(code_of(&wrapped), Some(Code::Forbidden));
    }
}
