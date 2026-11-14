use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use thiserror::Error;

pub const PROTOCOL_VERSION: &str = "2025-11-25";

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("io error talking to nanna: {0}")]
    Io(#[from] io::Error),
    #[error("nanna sent invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("nanna closed the connection before answering")]
    Eof,
    #[error("nanna sent nothing for {0:?}")]
    ReadTimeout(Duration),
    #[error("nanna connection is unusable after an earlier failure")]
    Dead,
    #[error("nanna protocol violation: {0}")]
    Protocol(String),
    #[error("nanna rpc error {code}: {message}")]
    Rpc {
        code: i64,
        message: String,
        failure_kind: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignRequest {
    pub description: String,
    pub repo_path: PathBuf,
    pub identity: String,
    pub branch: String,
}

impl AssignRequest {
    pub fn arguments(&self) -> Value {
        json!({
            "description": self.description,
            "repo_path": self.repo_path,
            "identity": self.identity,
            "branch": self.branch,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Working,
    Completed,
    Failed,
    Cancelled,
}

impl TaskState {
    pub fn is_terminal(self) -> bool {
        self != TaskState::Working
    }

    fn parse(text: &str) -> Result<TaskState, ClientError> {
        match text {
            "working" | "input_required" => Ok(TaskState::Working),
            "completed" => Ok(TaskState::Completed),
            "failed" => Ok(TaskState::Failed),
            "cancelled" => Ok(TaskState::Cancelled),
            other => Err(ClientError::Protocol(format!(
                "unknown task status `{other}`"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskInfo {
    pub task_id: String,
    pub state: TaskState,
    pub status_message: Option<String>,
    pub poll_interval_ms: u64,
}

impl TaskInfo {
    fn from_wire(value: &Value) -> Result<TaskInfo, ClientError> {
        let task_id = value
            .get("taskId")
            .and_then(Value::as_str)
            .ok_or_else(|| ClientError::Protocol("task is missing taskId".into()))?
            .to_string();
        let status = value
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| ClientError::Protocol("task is missing status".into()))?;
        Ok(TaskInfo {
            task_id,
            state: TaskState::parse(status)?,
            status_message: value
                .get("statusMessage")
                .and_then(Value::as_str)
                .map(str::to_string),
            poll_interval_ms: value
                .get("pollInterval")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_POLL_MS),
        })
    }
}

const DEFAULT_POLL_MS: u64 = 2000;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NannaFailure {
    pub failure_kind: Option<String>,
    pub error_type: Option<String>,
    pub message: String,
    pub diagnostics_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskDone {
    pub result_summary: String,
    pub changes_patch: Option<String>,
    pub format_patch: Option<String>,
    pub files_modified: Vec<String>,
    pub denial_count: usize,
    pub iterations: u64,
    pub model_used: String,
    pub raw: Value,
    pub result_text: String,
}

impl TaskDone {
    /// ```
    /// use inchworm::nanna_client::parse_result;
    /// use inchworm::nanna_client::TaskState;
    /// use serde_json::json;
    ///
    /// let body = json!({"qa_summary": {"endpoint_checks_failed": 0, "console_errors": 2}}).to_string();
    /// let value = json!({"isError": false, "content": [{"type": "text", "text": body}]});
    /// let outcome = parse_result(TaskState::Completed, &value).unwrap();
    /// let inchworm::nanna_client::TaskOutcome::Completed(done) = outcome else { panic!() };
    /// assert_eq!(done.qa_ok(), Some(false));
    /// ```
    pub fn qa_ok(&self) -> Option<bool> {
        let qa = self.raw.get("qa_summary")?.as_object()?;
        let failed = [
            "endpoint_checks_failed",
            "browser_steps_failed",
            "console_errors",
        ]
        .iter()
        .any(|key| qa.get(*key).and_then(Value::as_u64).unwrap_or(0) > 0);
        Some(!failed)
    }

    pub fn has_changes(&self) -> bool {
        let non_empty = |p: &Option<String>| p.as_deref().is_some_and(|s| !s.trim().is_empty());
        non_empty(&self.changes_patch)
            || non_empty(&self.format_patch)
            || !self.files_modified.is_empty()
    }

    fn from_wire(raw: Value, result_text: String) -> TaskDone {
        let text = |key: &str| {
            raw.get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        TaskDone {
            result_summary: text("result_summary").unwrap_or_default(),
            changes_patch: text("changes_patch"),
            format_patch: text("format_patch"),
            files_modified: raw
                .get("files_modified")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            denial_count: raw
                .get("denial_count")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .or_else(|| raw.get("denials").and_then(Value::as_array).map(Vec::len))
                .unwrap_or(0),
            iterations: raw.get("iterations").and_then(Value::as_u64).unwrap_or(0),
            model_used: text("model_used").unwrap_or_default(),
            raw,
            result_text,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TaskOutcome {
    Completed(TaskDone),
    Failed(NannaFailure),
    Cancelled,
    TimedOut,
    Refused(NannaFailure),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Submitted {
    Accepted(TaskInfo),
    Refused(NannaFailure),
}

pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(120);

fn spawn_reader(mut reader: impl BufRead + Send + 'static) -> Receiver<io::Result<String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut open = true;
        while open {
            let mut line = String::new();
            let message = reader.read_line(&mut line).map(|_| line);
            open = matches!(&message, Ok(line) if !line.is_empty());
            open &= tx.send(message).is_ok();
        }
    });
    rx
}

pub struct NannaClient {
    lines: Receiver<io::Result<String>>,
    writer: Box<dyn Write + Send>,
    child: Option<Child>,
    next_id: i64,
    read_timeout: Duration,
    dead: bool,
}

impl NannaClient {
    pub fn new(reader: impl BufRead + Send + 'static, writer: impl Write + Send + 'static) -> Self {
        NannaClient {
            lines: spawn_reader(reader),
            writer: Box::new(writer),
            child: None,
            next_id: 0,
            read_timeout: DEFAULT_READ_TIMEOUT,
            dead: false,
        }
    }

    /// ```
    /// use std::io::BufReader;
    /// use std::time::Duration;
    /// use inchworm::nanna_client::{ClientError, NannaClient};
    ///
    /// let (silent, _keep_open) = std::io::pipe().unwrap();
    /// let mut client = NannaClient::new(BufReader::new(silent), std::io::sink());
    /// client.set_read_timeout(Duration::from_millis(20));
    /// assert!(matches!(client.initialize(), Err(ClientError::ReadTimeout(_))));
    /// assert!(matches!(client.initialize(), Err(ClientError::Dead)));
    /// ```
    pub fn set_read_timeout(&mut self, timeout: Duration) {
        self.read_timeout = timeout;
    }

    pub fn spawn(nanna_bin: &Path) -> Result<NannaClient, ClientError> {
        let mut child = Command::new(nanna_bin)
            .arg("mcp-serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ClientError::Protocol("child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ClientError::Protocol("child has no stdout".into()))?;
        let mut client = NannaClient::new(BufReader::new(stdout), stdin);
        client.child = Some(child);
        Ok(client)
    }

    fn send(&mut self, message: &Value) -> Result<(), ClientError> {
        let mut line = serde_json::to_vec(message)?;
        line.push(b'\n');
        self.writer.write_all(&line)?;
        self.writer.flush()?;
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        if self.dead {
            return Err(ClientError::Dead);
        }
        let result = self.exchange(method, params);
        if matches!(
            result,
            Err(ClientError::ReadTimeout(_)
                | ClientError::Eof
                | ClientError::Io(_)
                | ClientError::Json(_))
        ) {
            self.dead = true;
        }
        result
    }

    fn exchange(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        loop {
            let line = self.lines.recv_timeout(self.read_timeout).map_err(|err| {
                if err == RecvTimeoutError::Timeout {
                    ClientError::ReadTimeout(self.read_timeout)
                } else {
                    ClientError::Eof
                }
            })??;
            if line.is_empty() {
                return Err(ClientError::Eof);
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let message: Value = serde_json::from_str(trimmed)?;
            if message.get("id") != Some(&json!(id)) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(rpc_error(error));
            }
            return message.get("result").cloned().ok_or_else(|| {
                ClientError::Protocol("response has neither result nor error".into())
            });
        }
    }

    /// ```
    /// use std::io::BufReader;
    /// use inchworm::nanna_client::NannaClient;
    ///
    /// let reply = br#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25"}}"#;
    /// let mut client = NannaClient::new(BufReader::new(&reply[..]), std::io::sink());
    /// let init = client.initialize().unwrap();
    /// assert_eq!(init["protocolVersion"], "2025-11-25");
    /// ```
    pub fn initialize(&mut self) -> Result<Value, ClientError> {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tasks": {"list": {}, "cancel": {}}},
                "clientInfo": {"name": "inchworm", "version": env!("CARGO_PKG_VERSION")},
            }),
        )?;
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}))?;
        Ok(result)
    }

    pub fn assign_task(&mut self, request: &AssignRequest) -> Result<Submitted, ClientError> {
        let params = json!({"name": "assign_task", "arguments": request.arguments(), "task": {}});
        match self.request("tools/call", params) {
            Ok(result) => {
                let task = result
                    .get("task")
                    .ok_or_else(|| ClientError::Protocol("CreateTaskResult missing task".into()))?;
                Ok(Submitted::Accepted(TaskInfo::from_wire(task)?))
            }
            Err(ClientError::Rpc {
                message,
                failure_kind,
                ..
            }) => Ok(Submitted::Refused(NannaFailure {
                failure_kind,
                error_type: None,
                message,
                diagnostics_text: None,
            })),
            Err(other) => Err(other),
        }
    }

    pub fn get(&mut self, task_id: &str) -> Result<TaskInfo, ClientError> {
        let value = self.request("tasks/get", json!({"taskId": task_id}))?;
        TaskInfo::from_wire(&value)
    }

    pub fn cancel(&mut self, task_id: &str) -> Result<TaskInfo, ClientError> {
        let value = self.request("tasks/cancel", json!({"taskId": task_id}))?;
        TaskInfo::from_wire(&value)
    }

    pub fn result(&mut self, task_id: &str, state: TaskState) -> Result<TaskOutcome, ClientError> {
        let value = self.request("tasks/result", json!({"taskId": task_id}))?;
        parse_result(state, &value)
    }

    /// ```
    /// use std::io::BufReader;
    /// use std::path::PathBuf;
    /// use std::time::Duration;
    /// use inchworm::nanna_client::{AssignRequest, NannaClient, TaskOutcome};
    ///
    /// let replies = concat!(
    ///     r#"{"jsonrpc":"2.0","id":1,"result":{"task":{"taskId":"t","status":"working"}}}"#, "\n",
    ///     r#"{"jsonrpc":"2.0","id":2,"result":{"taskId":"t","status":"completed"}}"#, "\n",
    ///     r#"{"jsonrpc":"2.0","id":3,"result":{"isError":false,"content":[{"type":"text","text":"{\"result_summary\":\"done\"}"}]}}"#, "\n",
    /// );
    /// let mut client = NannaClient::new(BufReader::new(replies.as_bytes()), std::io::sink());
    /// let request = AssignRequest {
    ///     description: "d".into(),
    ///     repo_path: PathBuf::from("/repo"),
    ///     identity: "rust-implementer".into(),
    ///     branch: "main".into(),
    /// };
    /// let outcome = client.run_task(&request, Duration::from_secs(5)).unwrap();
    /// assert!(matches!(outcome, TaskOutcome::Completed(done) if done.result_summary == "done"));
    /// ```
    pub fn run_task(
        &mut self,
        request: &AssignRequest,
        timeout: Duration,
    ) -> Result<TaskOutcome, ClientError> {
        let deadline = Instant::now() + timeout;
        let info = match self.assign_task(request)? {
            Submitted::Accepted(info) => info,
            Submitted::Refused(failure) => return Ok(TaskOutcome::Refused(failure)),
        };
        let mut current = info;
        while !current.state.is_terminal() {
            let now = Instant::now();
            if now >= deadline {
                self.cancel(&current.task_id)?;
                return Ok(TaskOutcome::TimedOut);
            }
            let pause = Duration::from_millis(current.poll_interval_ms).min(deadline - now);
            std::thread::sleep(pause);
            current = match self.get(&current.task_id) {
                Ok(next) => next,
                Err(err) => {
                    self.cancel_after_timeout(&current.task_id, &err);
                    return Err(err);
                }
            };
        }
        match self.result(&current.task_id, current.state) {
            Err(err) => {
                self.cancel_after_timeout(&current.task_id, &err);
                Err(err)
            }
            done => done,
        }
    }

    fn cancel_after_timeout(&mut self, task_id: &str, err: &ClientError) {
        if matches!(err, ClientError::ReadTimeout(_)) {
            self.next_id += 1;
            let id = self.next_id;
            let _ = self.send(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tasks/cancel",
                "params": {"taskId": task_id}
            }));
        }
    }
}

impl Drop for NannaClient {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn rpc_error(error: &Value) -> ClientError {
    ClientError::Rpc {
        code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        failure_kind: error
            .pointer("/data/failure_kind")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

fn result_text(value: &Value) -> String {
    value
        .pointer("/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn parse_failure(value: &Value) -> NannaFailure {
    let text = result_text(value);
    let structured = value.get("structuredContent");
    let string_at = |pointer: &str| {
        structured
            .and_then(|s| s.pointer(pointer))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let (head, tail) = match text.split_once("\n\n") {
        Some((head, tail)) => (head.to_string(), Some(tail)),
        None => (text.clone(), None),
    };
    let diagnostics_text = tail
        .filter(|tail| serde_json::from_str::<Value>(tail).is_ok())
        .map(str::to_string);
    let diagnostics_type = diagnostics_text
        .as_deref()
        .and_then(|tail| serde_json::from_str::<Value>(tail).ok())
        .and_then(|d| {
            d.get("error_type")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    NannaFailure {
        failure_kind: string_at("/failure_kind"),
        error_type: string_at("/error_type").or(diagnostics_type),
        message: string_at("/error").unwrap_or(head),
        diagnostics_text,
    }
}

/// ```
/// use inchworm::nanna_client::{parse_result, TaskOutcome, TaskState};
///
/// let wire = serde_json::json!({
///     "isError": true,
///     "structuredContent": {"failure_kind": "cancelled"},
///     "content": [{"type": "text", "text": "Task was cancelled after 2 iterations."}]
/// });
/// assert_eq!(parse_result(TaskState::Failed, &wire).unwrap(), TaskOutcome::Cancelled);
/// ```
pub fn parse_result(state: TaskState, value: &Value) -> Result<TaskOutcome, ClientError> {
    let is_error = value
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let cancelled_kind = value
        .pointer("/structuredContent/failure_kind")
        .and_then(Value::as_str)
        == Some("cancelled");
    if state == TaskState::Cancelled || cancelled_kind {
        return Ok(TaskOutcome::Cancelled);
    }
    if is_error || state == TaskState::Failed {
        return Ok(TaskOutcome::Failed(parse_failure(value)));
    }
    let text = result_text(value);
    let raw: Value = serde_json::from_str(&text)
        .map_err(|e| ClientError::Protocol(format!("task result is not JSON: {e}")))?;
    Ok(TaskOutcome::Completed(TaskDone::from_wire(raw, text)))
}

#[cfg(test)]
pub(crate) mod fake;

#[cfg(test)]
mod tests;
