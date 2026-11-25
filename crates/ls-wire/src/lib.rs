//! Wire-level constants, discovery, pinned-TLS Connect JSON transport, DTOs,
//! and incremental framing for AntigravityCN's direct `language_server` integration.
//!
//! Protocol framing, error classification, and bounds align with
//! `docs/engine-contract.md` and the verified 2.35.0 baseline.

pub mod client;
pub mod discovery;
pub mod dto;
pub mod framing;

pub use client::{
    AgentStateStream, DEFAULT_CONNECT_TIMEOUT, DEFAULT_HEALTH_TIMEOUT,
    DEFAULT_STREAM_CONNECT_TIMEOUT, DEFAULT_UNARY_TIMEOUT, LsClient,
};
pub use discovery::{
    DiscoveredEndpoint, DiscoveryReport, DiscoverySelectError, ProcessPortInspector,
    SkippedInstance, WindowsProcessPortInspector, default_discovery_dir,
    default_install_resources_dir, discover_instances, discover_instances_with, extract_asar_entry,
    extract_installed_package_version, extract_pinned_cert_pem, load_pinned_cert_pem,
    select_endpoint,
};
pub use dto::{
    AskQuestionAnswer, AuthSummary, InteractionReply, ModelCatalog, ModelEntry, PageBounds,
    PageUpdateRequest, PermissionScope, ReadonlyCheckReport, SecurityBoundaryReport,
    SendMessageOptions, StartCascadeRequest, StopScope, StreamSubscribeRequest, redact_value,
};
pub use framing::{ConnectFrameDecoder, DecodeFailure, FrameError, encode_connect_frame};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// Official Antigravity language_server version pinned for the initial attached release.
pub const TARGET_LS_VERSION: &str = "2.35.0";

/// Connect RPC service name on the official local HTTPS loopback endpoint.
pub const LS_SERVICE_NAME: &str = "exa.language_server_pb.LanguageServerService";

/// Verified trajectory verbosity used by official PROD_UI streaming subscriptions.
pub const PROD_UI_VERBOSITY: &str = "CLIENT_TRAJECTORY_VERBOSITY_PROD_UI";

/// Maximum Connect envelope payload bytes accepted from a stream frame (8 MiB).
pub const MAX_CONNECT_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Maximum unary JSON response size accepted from a single RPC call (16 MiB).
pub const MAX_UNARY_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Mandatory three-way error classification for transport and command execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransportErrorCategory {
    /// Request was never transmitted; safe for the user to retry.
    NotSent,
    /// Server explicitly rejected the request (e.g., 401, invalid_argument).
    Rejected,
    /// Transmission started but response outcome is unknown; never auto-resend.
    OutcomeUnknown,
}

/// Verified attached official instance identity (never contains CSRF tokens).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceSummary {
    pub pid: u32,
    pub https_port: u16,
    pub ls_version: String,
}

/// Structured wire-layer error carrying the mandatory three-way classification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireError {
    pub category: TransportErrorCategory,
    pub code: String,
    pub http_status: Option<u16>,
    pub message: String,
    pub detail: Option<Value>,
}

impl WireError {
    pub fn not_sent(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            category: TransportErrorCategory::NotSent,
            code: code.into(),
            http_status: None,
            message: message.into(),
            detail: None,
        }
    }

    pub fn rejected(
        code: impl Into<String>,
        http_status: Option<u16>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            category: TransportErrorCategory::Rejected,
            code: code.into(),
            http_status,
            message: message.into(),
            detail: None,
        }
    }

    pub fn outcome_unknown(
        code: impl Into<String>,
        http_status: Option<u16>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            category: TransportErrorCategory::OutcomeUnknown,
            code: code.into(),
            http_status,
            message: message.into(),
            detail: None,
        }
    }
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{:?}:{}] {}", self.category, self.code, self.message)
    }
}

impl std::error::Error for WireError {}
