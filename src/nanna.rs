//! The seam between inchworm and nanna. Inchworm never talks to the underlying
//! harness; every coding agent is a nanna identity run as an MCP task.

use std::path::PathBuf;

/// One task for a nanna identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRequest {
    /// Name of the nanna identity (agent card) to run under.
    pub identity: String,
    /// Natural-language task description.
    pub description: String,
    /// Local checkout the task works in.
    pub repo_path: PathBuf,
    /// Branch or ref to base the worktree on.
    pub branch: String,
}

/// Terminal result of a task that ran to completion.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskOutput {
    pub summary: String,
    /// Patch produced by the task, if any.
    pub patch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NannaError {
    #[error("nanna transport: {0}")]
    Transport(String),
    #[error("nanna refused the task: {0}")]
    Refused(String),
    #[error("task failed: {0}")]
    Failed(String),
    #[error("task cancelled")]
    Cancelled,
}

/// Runs a task to completion (blocking).
pub trait Nanna {
    fn run(&self, request: &TaskRequest) -> Result<TaskOutput, NannaError>;
}
