use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
}

impl TaskDone {
    pub fn has_changes(&self) -> bool {
        let non_empty = |p: &Option<String>| p.as_deref().is_some_and(|s| !s.trim().is_empty());
        non_empty(&self.changes_patch)
            || non_empty(&self.format_patch)
            || !self.files_modified.is_empty()
    }

    fn from_wire(raw: Value) -> TaskDone {
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

pub const MAX_NOISE_LINES_PER_EXCHANGE: u64 = 1000;

pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

const INTERRUPT_SLICE: Duration = Duration::from_millis(100);

const CANCEL_ON_DROP_READ_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnOptions {
    pub bin: PathBuf,
    pub state_dir: PathBuf,
    pub config_dir: Option<PathBuf>,
}

impl SpawnOptions {
    pub fn new(bin: impl Into<PathBuf>, state_dir: impl Into<PathBuf>) -> SpawnOptions {
        SpawnOptions {
            bin: bin.into(),
            state_dir: state_dir.into(),
            config_dir: None,
        }
    }
}

pub fn state_dir_for(state_db: &Path) -> PathBuf {
    state_db
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
        .join("nanna")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnEnv {
    pub queue_file: PathBuf,
    pub set: Vec<(&'static str, PathBuf)>,
    pub remove: Vec<&'static str>,
}

fn non_empty(lookup: &dyn Fn(&str) -> Option<OsString>, key: &str) -> Option<OsString> {
    lookup(key).filter(|value| !value.is_empty())
}

/// ```
/// use std::ffi::OsString;
/// use inchworm::nanna_client::{spawn_env, SpawnOptions};
///
/// let options = SpawnOptions::new("nanna", "/state/inchworm/nanna");
/// let lookup = |key: &str| (key == "HOME").then(|| OsString::from("/home/me"));
/// let env = spawn_env(&options, "1-2-3", &lookup).unwrap();
/// assert_eq!(env.queue_file.to_str(), Some("/state/inchworm/nanna/queue-1-2-3.jsonl"));
/// let get = |name: &str| env.set.iter().find(|(k, _)| *k == name).map(|(_, v)| v.clone());
/// assert_eq!(get("NANNA_BUDGET_PATH").unwrap().to_str(), Some("/home/me/.local/state/nanna/budget.json"));
/// assert!(env.remove.contains(&"RUST_LOG"));
/// ```
pub fn spawn_env(
    options: &SpawnOptions,
    unique: &str,
    lookup: &dyn Fn(&str) -> Option<OsString>,
) -> Result<SpawnEnv, ClientError> {
    let queue_file = options.state_dir.join(format!("queue-{unique}.jsonl"));
    let shared_dir = || -> Result<PathBuf, ClientError> {
        non_empty(lookup, "NANNA_QUEUE_PATH")
            .and_then(|queue| PathBuf::from(queue).parent().map(Path::to_path_buf))
            .or_else(|| {
                non_empty(lookup, "HOME").map(|home| PathBuf::from(home).join(".local/state/nanna"))
            })
            .ok_or_else(|| {
                ClientError::Protocol(
                    "cannot locate nanna's shared state: set HOME or NANNA_QUEUE_PATH".into(),
                )
            })
    };
    let mut set = vec![("NANNA_QUEUE_PATH", queue_file.clone())];
    for (key, file) in [
        ("NANNA_LEASE_PATH", "leases.jsonl"),
        ("NANNA_ESCALATION_PATH", "escalations.jsonl"),
        ("NANNA_BUDGET_PATH", "budget.json"),
    ] {
        let path = match non_empty(lookup, key) {
            Some(explicit) => PathBuf::from(explicit),
            None => shared_dir()?.join(file),
        };
        set.push((key, path));
    }
    if let Some(config_dir) = &options.config_dir {
        set.push(("NANNA_CONFIG_DIR", config_dir.clone()));
    }
    Ok(SpawnEnv {
        queue_file,
        set,
        remove: vec!["RUST_LOG"],
    })
}

fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!(
        "{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn build_command(options: &SpawnOptions, env: &SpawnEnv) -> Command {
    let mut command = Command::new(&options.bin);
    command
        .arg("mcp-serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    for key in &env.remove {
        command.env_remove(key);
    }
    for (key, value) in &env.set {
        command.env(key, value);
    }
    command
}

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
    noise_lines: u64,
    queue_file: Option<PathBuf>,
    active_task: Option<String>,
    shutdown_grace: Duration,
    interrupt: Option<Arc<AtomicBool>>,
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
            noise_lines: 0,
            queue_file: None,
            active_task: None,
            shutdown_grace: SHUTDOWN_GRACE,
            interrupt: None,
        }
    }

    pub fn set_interrupt(&mut self, flag: Arc<AtomicBool>) {
        self.interrupt = Some(flag);
    }

    fn interrupted(&self) -> bool {
        self.interrupt
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
    }

    pub fn is_usable(&self) -> bool {
        !self.dead && !self.interrupted()
    }

    fn pause(&self, total: Duration) {
        let end = Instant::now() + total;
        loop {
            let now = Instant::now();
            if self.interrupted() || now >= end {
                return;
            }
            std::thread::sleep((end - now).min(INTERRUPT_SLICE));
        }
    }

    pub fn set_shutdown_grace(&mut self, grace: Duration) {
        self.shutdown_grace = grace;
    }

    /// ```
    /// use std::io::BufReader;
    /// use inchworm::nanna_client::NannaClient;
    ///
    /// let replies = concat!(
    ///     "2026-11-10T12:00:00Z ERROR something nanna logged to stdout\n",
    ///     r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25"}}"#, "\n",
    /// );
    /// let mut client = NannaClient::new(BufReader::new(replies.as_bytes()), std::io::sink());
    /// client.initialize().unwrap();
    /// assert_eq!(client.noise_lines(), 1);
    /// ```
    pub fn noise_lines(&self) -> u64 {
        self.noise_lines
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

    pub fn spawn(options: &SpawnOptions) -> Result<NannaClient, ClientError> {
        NannaClient::spawn_with_lookup(options, &|key| std::env::var_os(key))
    }

    pub fn spawn_with_lookup(
        options: &SpawnOptions,
        lookup: &dyn Fn(&str) -> Option<OsString>,
    ) -> Result<NannaClient, ClientError> {
        let env = spawn_env(options, &unique_suffix(), lookup)?;
        std::fs::create_dir_all(&options.state_dir)?;
        let _ = std::fs::remove_file(&env.queue_file);
        let mut child = build_command(options, &env).spawn()?;
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
        client.queue_file = Some(env.queue_file);
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
        let mut noise_in_exchange = 0u64;
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
            let Ok(message) = serde_json::from_str::<Value>(trimmed) else {
                self.noise_lines += 1;
                noise_in_exchange += 1;
                if noise_in_exchange > MAX_NOISE_LINES_PER_EXCHANGE {
                    return Err(ClientError::Protocol(format!(
                        "nanna sent more than {MAX_NOISE_LINES_PER_EXCHANGE} non-JSON lines without answering"
                    )));
                }
                continue;
            };
            if message.get("method").is_some() || message.get("id") != Some(&json!(id)) {
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
                let info = TaskInfo::from_wire(task)?;
                if !info.state.is_terminal() {
                    self.active_task = Some(info.task_id.clone());
                }
                Ok(Submitted::Accepted(info))
            }
            Err(ClientError::Rpc {
                message,
                failure_kind,
                ..
            }) => Ok(Submitted::Refused(NannaFailure {
                failure_kind,
                error_type: None,
                message,
            })),
            Err(other) => Err(other),
        }
    }

    fn settle_if_terminal(&mut self, info: TaskInfo) -> TaskInfo {
        if info.state.is_terminal() && self.active_task.as_deref() == Some(info.task_id.as_str()) {
            self.active_task = None;
        }
        info
    }

    pub fn get(&mut self, task_id: &str) -> Result<TaskInfo, ClientError> {
        let value = self.request("tasks/get", json!({"taskId": task_id}))?;
        let info = TaskInfo::from_wire(&value)?;
        Ok(self.settle_if_terminal(info))
    }

    pub fn cancel(&mut self, task_id: &str) -> Result<TaskInfo, ClientError> {
        let value = self.request("tasks/cancel", json!({"taskId": task_id}))?;
        let info = TaskInfo::from_wire(&value)?;
        Ok(self.settle_if_terminal(info))
    }

    pub fn result(&mut self, task_id: &str, state: TaskState) -> Result<TaskOutcome, ClientError> {
        let value = self.request("tasks/result", json!({"taskId": task_id}))?;
        if self.active_task.as_deref() == Some(task_id) {
            self.active_task = None;
        }
        parse_result(state, &value)
    }

    fn cancel_or_collect(&mut self, task_id: &str) -> Result<TaskOutcome, ClientError> {
        if self.cancel(task_id).is_ok() {
            return Ok(TaskOutcome::TimedOut);
        }
        Ok(match self.get(task_id) {
            Ok(info) if info.state.is_terminal() => self
                .result(task_id, info.state)
                .unwrap_or(TaskOutcome::TimedOut),
            _ => TaskOutcome::TimedOut,
        })
    }

    /// ```
    /// use std::io::BufReader;
    /// use inchworm::nanna_client::NannaClient;
    ///
    /// let mut client = NannaClient::new(BufReader::new(&b""[..]), std::io::sink());
    /// assert!(!client.is_dead());
    /// assert!(client.initialize().is_err());
    /// assert!(client.is_dead());
    /// ```
    pub fn is_dead(&self) -> bool {
        self.dead
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
            if self.interrupted() {
                let _ = self.cancel(&current.task_id);
                return Ok(TaskOutcome::Cancelled);
            }
            let now = Instant::now();
            if now >= deadline {
                return self.cancel_or_collect(&current.task_id);
            }
            self.pause(Duration::from_millis(current.poll_interval_ms).min(deadline - now));
            current = self.get(&current.task_id)?;
        }
        self.result(&current.task_id, current.state)
    }
}

impl Drop for NannaClient {
    fn drop(&mut self) {
        if let Some(task_id) = self.active_task.take()
            && !self.dead
        {
            self.read_timeout = CANCEL_ON_DROP_READ_TIMEOUT;
            let _ = self.request("tasks/cancel", json!({"taskId": task_id}));
        }
        self.writer = Box::new(io::sink());
        if let Some(mut child) = self.child.take() {
            let deadline = Instant::now() + self.shutdown_grace;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        if let Some(queue_file) = self.queue_file.take() {
            let _ = std::fs::remove_file(queue_file);
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

fn split_diagnostics(text: &str) -> (String, Option<Value>) {
    let text = text.replace("\r\n", "\n");
    let mut end = text.len();
    while let Some(at) = text[..end].rfind("\n\n{") {
        if let Ok(diagnostics) = serde_json::from_str::<Value>(&text[at + 2..]) {
            return (text[..at].to_string(), Some(diagnostics));
        }
        end = at;
    }
    (text, None)
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
    let (head, diagnostics) = split_diagnostics(&text);
    let diagnostics_type = diagnostics.and_then(|d| {
        d.get("error_type")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    NannaFailure {
        failure_kind: string_at("/failure_kind"),
        error_type: string_at("/error_type").or(diagnostics_type),
        message: string_at("/error").unwrap_or(head),
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
    Ok(TaskOutcome::Completed(TaskDone::from_wire(raw)))
}

#[cfg(test)]
pub(crate) mod fake;

#[cfg(test)]
mod tests;
