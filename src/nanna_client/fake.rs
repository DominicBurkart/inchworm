use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use serde_json::{Value, json};

use super::NannaClient;

pub fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/nanna/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap()
}

#[derive(Clone)]
pub enum Scenario {
    Success { result: Value },
    Failed { result: Value },
    Cancelled { result: Value },
    Refused { error: Value },
    Hang,
    FinishedBeforeCancel { result: Value },
    ForgottenBeforeCancel,
    RejectsCancelStillWorking,
}

impl Scenario {
    pub fn success() -> Scenario {
        Scenario::Success {
            result: fixture("result_success"),
        }
    }

    pub fn success_with_patch(patch: &str, files: &[&str]) -> Scenario {
        let mut result = fixture("result_success");
        let text = result["content"][0]["text"].as_str().unwrap().to_string();
        let mut body: Value = serde_json::from_str(&text).unwrap();
        body["changes_patch"] = json!(patch);
        body["files_modified"] = json!(files);
        result["content"][0]["text"] = json!(body.to_string());
        Scenario::Success { result }
    }

    pub fn success_with_format_patch(patch: &str, files: &[&str]) -> Scenario {
        let mut result = fixture("result_success");
        let text = result["content"][0]["text"].as_str().unwrap().to_string();
        let mut body: Value = serde_json::from_str(&text).unwrap();
        body["changes_patch"] = Value::Null;
        body["format_patch"] = json!(patch);
        body["files_modified"] = json!(files);
        result["content"][0]["text"] = json!(body.to_string());
        Scenario::Success { result }
    }

    pub fn success_empty() -> Scenario {
        let mut result = fixture("result_success");
        let text = result["content"][0]["text"].as_str().unwrap().to_string();
        let mut body: Value = serde_json::from_str(&text).unwrap();
        body["changes_patch"] = Value::Null;
        body["files_modified"] = json!([]);
        result["content"][0]["text"] = json!(body.to_string());
        Scenario::Success { result }
    }

    pub fn finished_before_cancel() -> Scenario {
        Scenario::FinishedBeforeCancel {
            result: fixture("result_success"),
        }
    }

    pub fn failed(fixture_name: &str) -> Scenario {
        Scenario::Failed {
            result: fixture(fixture_name),
        }
    }

    pub fn failed_with_message(message: &str) -> Scenario {
        let mut result = fixture("result_failed_legacy");
        let text = result["content"][0]["text"].as_str().unwrap().to_string();
        let diagnostics = text.split_once("\n\n").unwrap().1.to_string();
        result["content"][0]["text"] = json!(format!("{message}\n\n{diagnostics}"));
        Scenario::Failed { result }
    }

    pub fn refused(fixture_name: &str) -> Scenario {
        Scenario::Refused {
            error: fixture(fixture_name),
        }
    }
}

pub type Requests = Arc<Mutex<Vec<Value>>>;

pub struct FakeNanna {
    pub requests: Requests,
    handle: Option<JoinHandle<()>>,
}

impl FakeNanna {
    pub fn settle(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    pub fn assigned(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == "tools/call")
            .map(|r| r["params"]["arguments"].clone())
            .collect()
    }

    pub fn methods(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r["method"].as_str().unwrap_or_default().to_string())
            .collect()
    }
}

pub fn connect(scenarios: Vec<Scenario>) -> (NannaClient, FakeNanna) {
    let (server_in, client_out) = std::io::pipe().unwrap();
    let (client_in, server_out) = std::io::pipe().unwrap();
    let requests: Requests = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&requests);
    let handle = std::thread::spawn(move || serve(server_in, server_out, scenarios, log));
    let client = NannaClient::new(BufReader::new(client_in), client_out);
    (
        client,
        FakeNanna {
            requests,
            handle: Some(handle),
        },
    )
}

struct Task {
    scenario: Scenario,
    gets: usize,
    cancelled: bool,
    cancel_rejected: bool,
}

fn serve(
    input: std::io::PipeReader,
    mut output: std::io::PipeWriter,
    scenarios: Vec<Scenario>,
    log: Requests,
) {
    let mut scenarios = scenarios.into_iter();
    let mut tasks: HashMap<String, Task> = HashMap::new();
    let mut reply = |id: &Value, outcome: Result<Value, Value>| {
        let noise = json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {}});
        let body = match outcome {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        };
        let _ = writeln!(output, "\n{noise}\n{body}");
    };
    for line in BufReader::new(input).lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = serde_json::from_str(&line).unwrap();
        log.lock().unwrap().push(request.clone());
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let task_id = request["params"]["taskId"]
            .as_str()
            .unwrap_or("")
            .to_string();
        match request["method"].as_str().unwrap() {
            "initialize" => reply(&id, Ok(fixture("initialize_result"))),
            "tools/call" => match scenarios.next() {
                None => reply(&id, Err(json!({"code": -32602, "message": "no scenario"}))),
                Some(Scenario::Refused { error }) => reply(&id, Err(error)),
                Some(scenario) => {
                    let new_id = format!("task-{}", tasks.len() + 1);
                    let mut created = fixture("create_task_result");
                    created["task"]["taskId"] = json!(new_id);
                    tasks.insert(
                        new_id,
                        Task {
                            scenario,
                            gets: 0,
                            cancelled: false,
                            cancel_rejected: false,
                        },
                    );
                    reply(&id, Ok(created));
                }
            },
            "tasks/get" => match tasks.get_mut(&task_id) {
                None => reply(
                    &id,
                    Err(json!({"code": -32602, "message": "Task not found"})),
                ),
                Some(task)
                    if task.cancel_rejected
                        && matches!(task.scenario, Scenario::ForgottenBeforeCancel) =>
                {
                    reply(
                        &id,
                        Err(json!({"code": -32602, "message": "Task not found"})),
                    )
                }
                Some(task) => {
                    task.gets += 1;
                    let name = if task.cancelled {
                        "task_cancelled"
                    } else if matches!(task.scenario, Scenario::FinishedBeforeCancel { .. }) {
                        if task.cancel_rejected {
                            "task_completed"
                        } else {
                            "task_working"
                        }
                    } else if task.gets < 2 {
                        "task_working"
                    } else {
                        match task.scenario {
                            Scenario::Success { .. } => "task_completed",
                            Scenario::Failed { .. } => "task_failed",
                            Scenario::Cancelled { .. } => "task_cancelled",
                            _ => "task_working",
                        }
                    };
                    let mut wire = fixture(name);
                    wire["taskId"] = json!(task_id);
                    reply(&id, Ok(wire));
                }
            },
            "tasks/cancel" => match tasks.get_mut(&task_id) {
                None => reply(
                    &id,
                    Err(json!({"code": -32602, "message": "Task not found"})),
                ),
                Some(task)
                    if matches!(
                        task.scenario,
                        Scenario::FinishedBeforeCancel { .. }
                            | Scenario::ForgottenBeforeCancel
                            | Scenario::RejectsCancelStillWorking
                    ) =>
                {
                    task.cancel_rejected = true;
                    reply(
                        &id,
                        Err(json!({"code": -32602, "message": "Task already in a terminal state"})),
                    )
                }
                Some(task) => {
                    task.cancelled = true;
                    let mut wire = fixture("task_cancelled");
                    wire["taskId"] = json!(task_id);
                    reply(&id, Ok(wire));
                }
            },
            "tasks/result" => match tasks.get(&task_id) {
                None => reply(
                    &id,
                    Err(json!({"code": -32602, "message": "Task not found"})),
                ),
                Some(task) if task.cancelled => reply(&id, Ok(fixture("result_cancelled_legacy"))),
                Some(task) => match &task.scenario {
                    Scenario::Success { result }
                    | Scenario::Failed { result }
                    | Scenario::Cancelled { result }
                    | Scenario::FinishedBeforeCancel { result } => reply(&id, Ok(result.clone())),
                    _ => reply(&id, Ok(fixture("result_cancelled_legacy"))),
                },
            },
            _ => reply(
                &id,
                Err(json!({"code": -32601, "message": "method not found"})),
            ),
        }
    }
}
