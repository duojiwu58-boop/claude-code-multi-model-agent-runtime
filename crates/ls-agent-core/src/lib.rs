//! Reusable owned LS engine; no GUI runtime, FFI queue, or product build script.
use serde::{Deserialize, Serialize};
pub mod engine;
pub mod ls_host;
mod data_root;
pub mod permissions;
mod auto_approval;
pub mod interaction;
pub mod state_store;
pub mod presentation;
pub use engine::*;
pub use interaction::*;
/// Orthogonal session execution status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionExecutionStatus {
    Idle,
    Sending,
    Running,
    WaitingInteraction,
    Stopping,
    Unknown,
}

/// Explicit task outcome disposition (separate from `fullyIdle` and assistant output presence).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskDisposition {
    None,
    Completed,
    Failed,
    Canceled,
    Unconfirmed,
}
