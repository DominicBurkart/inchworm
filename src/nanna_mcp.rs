//! [`Nanna`] over MCP: spawns `nanna mcp-serve` and drives `assign_task` as a
//! task-augmented `tools/call` (MCP Tasks extension, protocol `2025-11-25`).
//!
//! Layers, bottom up: [`Transport`] moves JSON messages (a child process's
//! stdio in production, an in-memory fake in tests); [`Rpc`] does JSON-RPC
//! request/response matching on top of it; [`run_task`] is the protocol
//! logic; [`McpNanna`] owns the process and the timeout.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::nanna::{Nanna, NannaError, TaskOutput, TaskRequest};

const PROTOCOL_VERSION: &str = "2025-11-25";
/// JSON-RPC "invalid params": nanna uses it for every refused submission.
const INVALID_PARAMS: i32 = -32602;
/// Poll interval when the server advertises none.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    #[error("io: {0}")]
    Io(String),
    #[error("server closed the connection")]
    Closed,
    #[error("timed out")]
    Timeout,
}

/// A bidirectional stream of JSON-RPC messages, one `Value` per message.
pub trait Transport {
    fn send(&mut self, message: &Value) -> Result<(), TransportError>;
    /// Next message from the server; `Timeout` once `deadline` passes.
    fn recv(&mut self, deadline: Instant) -> Result<Value, TransportError>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RpcError {
    #[error("rpc error {code}: {message}")]
    Rpc { code: i32, message: String },
    #[error("{0}")]
    Transport(TransportError),
    #[error("protocol: {0}")]
    Protocol(String),
}

impl From<TransportError> for RpcError {
    fn from(e: TransportError) -> Self {
        RpcError::Transport(e)
    }
}

/// JSON-RPC 2.0 client: matches responses to requests by id.
pub struct Rpc<T> {
    transport: T,
    next_id: u64,
}

impl<T: Transport> Rpc<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            next_id: 0,
        }
    }

    pub fn request(
        &mut self,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> Result<Value, RpcError> {
        self.next_id += 1;
        let id = self.next_id;
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.transport.send(&message)?;
        loop {
            let reply = self.transport.recv(deadline)?;
            // Notifications and replies to other requests are not ours.
            if reply.get("id") != Some(&json!(id)) || reply.get("method").is_some() {
                continue;
            }
            if let Some(error) = reply.get("error") {
                return Err(RpcError::Rpc {
                    code: error["code"]
                        .as_i64()
                        .and_then(|c| i32::try_from(c).ok())
                        .unwrap_or(0),
                    message: error["message"].as_str().unwrap_or_default().to_string(),
                });
            }
            return reply
                .get("result")
                .cloned()
                .ok_or_else(|| RpcError::Protocol(format!("{method}: no result or error")));
        }
    }

    pub fn notify(&mut self, method: &str, params: Value) -> Result<(), RpcError> {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        Ok(self.transport.send(&message)?)
    }
}

/// How long to wait between `tasks/get` polls.
fn poll_interval(task: &Value, cap: Duration) -> Duration {
    let advertised = task["pollInterval"].as_u64().map(Duration::from_millis);
    advertised.unwrap_or(DEFAULT_POLL_INTERVAL).min(cap)
}

fn map_rpc(error: RpcError) -> NannaError {
    match error {
        RpcError::Rpc { code, message } if code == INVALID_PARAMS => NannaError::Refused(message),
        other => NannaError::Transport(other.to_string()),
    }
}

/// Text of the first text content block of a `CallToolResult`.
fn result_text(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Maps a completed task's `CallToolResult` to a [`TaskOutput`].
fn parse_output(result: &Value) -> Result<TaskOutput, NannaError> {
    let text = result_text(result);
    if result["isError"].as_bool().unwrap_or(false) {
        return Err(NannaError::Failed(text));
    }
    // Nanna encodes its TaskResult as JSON text; fall back to the raw text.
    let Ok(body) = serde_json::from_str::<Value>(&text) else {
        return Ok(TaskOutput {
            summary: text,
            patch: None,
        });
    };
    let patch = body["changes_patch"]
        .as_str()
        .filter(|p| !p.is_empty())
        .map(str::to_string);
    Ok(TaskOutput {
        summary: body["result_summary"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        patch,
    })
}

/// Initialize, submit the task, poll until terminal, and fetch the result.
/// `max_poll` caps the wait between polls.
pub fn run_task<T: Transport>(
    rpc: &mut Rpc<T>,
    request: &TaskRequest,
    deadline: Instant,
    max_poll: Duration,
) -> Result<TaskOutput, NannaError> {
    let init = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {"tasks": {"list": {}, "cancel": {}}},
        "clientInfo": {"name": "inchworm", "version": env!("CARGO_PKG_VERSION")},
    });
    rpc.request("initialize", init, deadline).map_err(map_rpc)?;
    rpc.notify("notifications/initialized", json!({}))
        .map_err(map_rpc)?;

    let call = json!({
        "name": "assign_task",
        "arguments": {
            "identity": request.identity,
            "description": request.description,
            "repo_path": request.repo_path.to_string_lossy(),
            "branch": request.branch,
        },
        "task": {},
    });
    let created = rpc.request("tools/call", call, deadline).map_err(map_rpc)?;
    let task_id = created["task"]["taskId"]
        .as_str()
        .ok_or_else(|| NannaError::Transport("CreateTaskResult missing task.taskId".into()))?
        .to_string();

    let status_message = loop {
        let params = json!({"taskId": task_id});
        let task = rpc
            .request("tasks/get", params, deadline)
            .map_err(map_rpc)?;
        match task["status"].as_str() {
            Some("working") => {
                let wait = poll_interval(&task, max_poll);
                let left = deadline.saturating_duration_since(Instant::now());
                std::thread::sleep(wait.min(left));
                if Instant::now() >= deadline {
                    return Err(NannaError::Transport(TransportError::Timeout.to_string()));
                }
            }
            Some("cancelled") => return Err(NannaError::Cancelled),
            Some("completed" | "failed") => {
                break task["statusMessage"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
            }
            other => {
                return Err(NannaError::Transport(format!(
                    "unexpected task status {other:?}"
                )));
            }
        }
    };

    let result = rpc
        .request("tasks/result", json!({"taskId": task_id}), deadline)
        .map_err(|e| match e {
            // Nanna may report a failed task as an rpc error instead.
            RpcError::Rpc { message, .. } if !status_message.is_empty() => {
                NannaError::Failed(format!("{status_message}: {message}"))
            }
            other => map_rpc(other),
        })?;
    parse_output(&result)
}

/// Child process speaking newline-delimited JSON on stdio. A reader thread
/// forwards stdout lines so `recv` can honour a deadline. The child is killed
/// on drop.
pub struct ChildTransport {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<Result<String, String>>,
}

impl ChildTransport {
    pub fn spawn(command: &PathBuf, args: &[String]) -> Result<Self, TransportError> {
        let mut child = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| TransportError::Io(format!("spawning {}: {e}", command.display())))?;
        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let done = line.is_err();
                if tx.send(line.map_err(|e| e.to_string())).is_err() || done {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            lines,
        })
    }
}

impl Transport for ChildTransport {
    fn send(&mut self, message: &Value) -> Result<(), TransportError> {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(|e| TransportError::Io(e.to_string()))
    }

    fn recv(&mut self, deadline: Instant) -> Result<Value, TransportError> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = match self.lines.recv_timeout(left) {
                Ok(line) => line.map_err(TransportError::Io)?,
                Err(RecvTimeoutError::Timeout) => return Err(TransportError::Timeout),
                Err(RecvTimeoutError::Disconnected) => return Err(TransportError::Closed),
            };
            if line.trim().is_empty() {
                continue;
            }
            return serde_json::from_str(&line)
                .map_err(|e| TransportError::Io(format!("invalid json from server: {e}")));
        }
    }
}

impl Drop for ChildTransport {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs each task on a fresh `nanna mcp-serve` child process.
#[derive(Debug, Clone)]
pub struct McpNanna {
    command: PathBuf,
    args: Vec<String>,
    timeout: Duration,
    max_poll: Duration,
}

impl Default for McpNanna {
    fn default() -> Self {
        Self {
            command: PathBuf::from("nanna"),
            args: vec!["mcp-serve".into()],
            timeout: Duration::from_secs(60 * 60),
            max_poll: DEFAULT_POLL_INTERVAL,
        }
    }
}

impl McpNanna {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the server command line (default: `nanna mcp-serve`).
    pub fn command(mut self, command: impl Into<PathBuf>, args: Vec<String>) -> Self {
        self.command = command.into();
        self.args = args;
        self
    }

    /// Wall-clock limit for one whole task, handshake included.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Upper bound on the wait between status polls.
    pub fn max_poll_interval(mut self, interval: Duration) -> Self {
        self.max_poll = interval;
        self
    }
}

impl Nanna for McpNanna {
    fn run(&self, request: &TaskRequest) -> Result<TaskOutput, NannaError> {
        let deadline = Instant::now() + self.timeout;
        let transport = ChildTransport::spawn(&self.command, &self.args)
            .map_err(|e| NannaError::Transport(e.to_string()))?;
        let mut rpc = Rpc::new(transport);
        run_task(&mut rpc, request, deadline, self.max_poll)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// In-memory server: `handler` maps each client message to replies.
    struct Fake<F> {
        handler: F,
        out: VecDeque<Value>,
        seen: Vec<Value>,
    }

    impl<F: FnMut(&Value) -> Vec<Value>> Fake<F> {
        fn new(handler: F) -> Self {
            Self {
                handler,
                out: VecDeque::new(),
                seen: Vec::new(),
            }
        }
    }

    impl<F: FnMut(&Value) -> Vec<Value>> Transport for Fake<F> {
        fn send(&mut self, message: &Value) -> Result<(), TransportError> {
            self.seen.push(message.clone());
            self.out.extend((self.handler)(message));
            Ok(())
        }

        fn recv(&mut self, _: Instant) -> Result<Value, TransportError> {
            self.out.pop_front().ok_or(TransportError::Closed)
        }
    }

    fn ok(message: &Value, result: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": message["id"], "result": result})
    }

    fn err(message: &Value, code: i32, text: &str) -> Value {
        json!({"jsonrpc": "2.0", "id": message["id"], "error": {"code": code, "message": text}})
    }

    fn request() -> TaskRequest {
        TaskRequest {
            identity: "implementer".into(),
            description: "fix the bug".into(),
            repo_path: "/repo".into(),
            branch: "main".into(),
        }
    }

    /// Server whose task reports `statuses` in turn, then `result` for
    /// tasks/result.
    fn server(statuses: Vec<&'static str>, result: Value) -> impl FnMut(&Value) -> Vec<Value> {
        let mut statuses: VecDeque<_> = statuses.into();
        move |m| match m["method"].as_str() {
            Some("initialize") => vec![ok(m, json!({"protocolVersion": PROTOCOL_VERSION}))],
            Some("tools/call") => vec![ok(
                m,
                json!({"task": {"taskId": "t1", "status": "working"}}),
            )],
            Some("tasks/get") => {
                let status = if statuses.len() > 1 {
                    statuses.pop_front().unwrap()
                } else {
                    statuses[0]
                };
                let task = json!({"taskId": "t1", "status": status, "pollInterval": 1, "statusMessage": "boom"});
                vec![ok(m, task)]
            }
            Some("tasks/result") => vec![ok(m, result.clone())],
            _ => vec![],
        }
    }

    fn drive<F: FnMut(&Value) -> Vec<Value>>(
        handler: F,
    ) -> (Result<TaskOutput, NannaError>, Vec<Value>) {
        let mut rpc = Rpc::new(Fake::new(handler));
        let deadline = Instant::now() + Duration::from_secs(5);
        let out = run_task(&mut rpc, &request(), deadline, Duration::from_millis(1));
        (out, rpc.transport.seen)
    }

    fn call_result(text: &str, is_error: bool) -> Value {
        json!({"content": [{"type": "text", "text": text}], "isError": is_error})
    }

    #[test]
    fn completed_task_maps_summary_and_patch() {
        let body = json!({"result_summary": "done", "changes_patch": "diff --git"}).to_string();
        let (out, seen) = drive(server(
            vec!["working", "working", "completed"],
            call_result(&body, false),
        ));
        assert_eq!(
            out,
            Ok(TaskOutput {
                summary: "done".into(),
                patch: Some("diff --git".into())
            })
        );
        let methods: Vec<_> = seen.iter().map(|m| m["method"].as_str().unwrap()).collect();
        assert_eq!(
            methods,
            [
                "initialize",
                "notifications/initialized",
                "tools/call",
                "tasks/get",
                "tasks/get",
                "tasks/get",
                "tasks/result"
            ]
        );
        let call = &seen[2]["params"];
        assert_eq!(call["name"], "assign_task");
        assert_eq!(call["task"], json!({}));
        assert_eq!(
            call["arguments"],
            json!({"identity": "implementer", "description": "fix the bug", "repo_path": "/repo", "branch": "main"})
        );
    }

    #[test]
    fn empty_or_null_patch_is_none_and_plain_text_is_summary() {
        let body = json!({"result_summary": "s", "changes_patch": ""}).to_string();
        let (out, _) = drive(server(vec!["completed"], call_result(&body, false)));
        assert_eq!(out.unwrap().patch, None);
        let (out, _) = drive(server(vec!["completed"], call_result("just text", false)));
        assert_eq!(out.unwrap().summary, "just text");
    }

    #[test]
    fn failed_task_is_failed() {
        let (out, _) = drive(server(vec!["failed"], call_result("it broke", true)));
        assert_eq!(out, Err(NannaError::Failed("it broke".into())));
    }

    #[test]
    fn failed_status_with_rpc_error_result_is_failed() {
        let mut inner = server(vec!["failed"], json!(null));
        let (out, _) = drive(move |m| {
            if m["method"] == "tasks/result" {
                vec![err(m, -32000, "gone")]
            } else {
                inner(m)
            }
        });
        assert_eq!(out, Err(NannaError::Failed("boom: gone".into())));
    }

    #[test]
    fn cancelled_task_is_cancelled() {
        let (out, seen) = drive(server(vec!["working", "cancelled"], json!(null)));
        assert_eq!(out, Err(NannaError::Cancelled));
        assert!(seen.iter().all(|m| m["method"] != "tasks/result"));
    }

    #[test]
    fn invalid_params_on_submit_is_refused() {
        let (out, _) = drive(|m| match m["method"].as_str() {
            Some("initialize") => vec![ok(m, json!({}))],
            Some("tools/call") => vec![err(m, INVALID_PARAMS, "unknown identity")],
            _ => vec![],
        });
        assert_eq!(out, Err(NannaError::Refused("unknown identity".into())));
    }

    #[test]
    fn other_rpc_errors_and_closed_streams_are_transport() {
        let (out, _) = drive(|m| match m["method"].as_str() {
            Some("initialize") => vec![err(m, -32601, "nope")],
            _ => vec![],
        });
        assert!(matches!(out, Err(NannaError::Transport(e)) if e.contains("-32601")));
        let (out, _) = drive(|_| vec![]);
        assert!(matches!(out, Err(NannaError::Transport(_))));
    }

    #[test]
    fn missing_task_id_and_unknown_status_are_transport() {
        let (out, _) = drive(|m| match m["method"].as_str() {
            Some("tools/call") => vec![ok(m, json!({}))],
            _ => vec![ok(m, json!({}))],
        });
        assert!(matches!(out, Err(NannaError::Transport(e)) if e.contains("taskId")));
        let (out, _) = drive(server(vec!["input_required"], json!(null)));
        assert!(matches!(out, Err(NannaError::Transport(e)) if e.contains("input_required")));
    }

    #[test]
    fn rpc_skips_notifications_and_foreign_replies() {
        let mut rpc = Rpc::new(Fake::new(|m| {
            vec![
                json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {}}),
                json!({"jsonrpc": "2.0", "id": 99, "result": "other"}),
                ok(m, json!("mine")),
            ]
        }));
        let deadline = Instant::now() + Duration::from_secs(1);
        assert_eq!(rpc.request("x", json!({}), deadline), Ok(json!("mine")));
    }

    #[test]
    fn working_forever_hits_the_deadline() {
        let mut rpc = Rpc::new(Fake::new(server(vec!["working"], json!(null))));
        let deadline = Instant::now() + Duration::from_millis(30);
        let out = run_task(&mut rpc, &request(), deadline, Duration::from_millis(5));
        assert!(matches!(out, Err(NannaError::Transport(e)) if e.contains("timed out")));
    }

    #[test]
    fn poll_interval_uses_server_value_capped() {
        let task = json!({"pollInterval": 500});
        assert_eq!(
            poll_interval(&task, Duration::from_secs(2)),
            Duration::from_millis(500)
        );
        assert_eq!(
            poll_interval(&task, Duration::from_millis(10)),
            Duration::from_millis(10)
        );
        assert_eq!(
            poll_interval(&json!({}), Duration::from_secs(9)),
            DEFAULT_POLL_INTERVAL
        );
    }

    #[cfg(unix)]
    mod process {
        use super::*;

        /// A shell "server" answering the fixed request sequence
        /// initialize, tools/call, tasks/get, tasks/result.
        const SCRIPT: &str = r#"
while read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) echo '{"jsonrpc":"2.0","id":'$id',"result":{}}' ;;
    *'"method":"tools/call"'*) echo '{"jsonrpc":"2.0","id":'$id',"result":{"task":{"taskId":"t"}}}' ;;
    *'"method":"tasks/get"'*) echo '{"jsonrpc":"2.0","id":'$id',"result":{"status":"completed"}}' ;;
    *'"method":"tasks/result"'*) echo '{"jsonrpc":"2.0","id":'$id',"result":{"isError":false,"content":[{"type":"text","text":"{\"result_summary\":\"ok\",\"changes_patch\":\"p\"}"}]}}' ;;
  esac
done
"#;

        fn sh(script: &str) -> McpNanna {
            McpNanna::new()
                .command("sh", vec!["-c".into(), script.into()])
                .timeout(Duration::from_secs(10))
                .max_poll_interval(Duration::from_millis(5))
        }

        #[test]
        fn end_to_end_against_fake_server_process() {
            let out = sh(SCRIPT).run(&request());
            assert_eq!(
                out,
                Ok(TaskOutput {
                    summary: "ok".into(),
                    patch: Some("p".into())
                })
            );
        }

        #[test]
        fn server_that_exits_is_transport_error() {
            let out = sh("exit 0").run(&request());
            assert!(matches!(out, Err(NannaError::Transport(_))));
        }

        #[test]
        fn silent_server_times_out() {
            let out = sh("sleep 30")
                .timeout(Duration::from_millis(100))
                .run(&request());
            assert!(matches!(out, Err(NannaError::Transport(e)) if e.contains("timed out")));
        }

        #[test]
        fn missing_binary_is_transport_error() {
            let nanna = McpNanna::new().command("/nonexistent/nanna", vec![]);
            assert!(matches!(
                nanna.run(&request()),
                Err(NannaError::Transport(_))
            ));
        }
    }
}
