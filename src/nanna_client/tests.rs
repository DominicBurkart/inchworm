use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;

use super::fake::{Scenario, connect, fixture};
use super::*;

fn request(description: &str) -> AssignRequest {
    AssignRequest {
        description: description.into(),
        repo_path: PathBuf::from("/repo"),
        identity: "rust-implementer".into(),
        branch: "main".into(),
    }
}

const WAIT: Duration = Duration::from_secs(10);

#[test]
fn initialize_handshake_returns_server_info_and_sends_initialized() {
    let (mut client, mut fake) = connect(vec![]);
    let init = client.initialize().unwrap();
    assert_eq!(init["serverInfo"]["name"], "nanna");
    drop(client);
    fake.settle();
    assert_eq!(fake.methods(), ["initialize", "notifications/initialized"]);
    let first = &fake.requests.lock().unwrap()[0];
    assert_eq!(first["params"]["protocolVersion"], PROTOCOL_VERSION);
    assert_eq!(first["params"]["clientInfo"]["name"], "inchworm");
}

#[test]
fn assign_task_sends_the_pinned_wire_shape() {
    let (mut client, mut fake) = connect(vec![Scenario::success()]);
    client.initialize().unwrap();
    let submitted = client.assign_task(&request("do it")).unwrap();
    assert!(matches!(submitted, Submitted::Accepted(info) if info.state == TaskState::Working));
    drop(client);
    fake.settle();
    let calls: Vec<_> = fake
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r["method"] == "tools/call")
        .cloned()
        .collect();
    assert_eq!(
        calls[0]["params"],
        json!({
            "name": "assign_task",
            "arguments": {
                "description": "do it",
                "repo_path": "/repo",
                "identity": "rust-implementer",
                "branch": "main"
            },
            "task": {}
        })
    );
}

#[test]
fn successful_task_yields_the_parsed_result() {
    let (mut client, _fake) = connect(vec![Scenario::success()]);
    client.initialize().unwrap();
    let outcome = client.run_task(&request("x"), WAIT).unwrap();
    let TaskOutcome::Completed(done) = outcome else {
        panic!("expected completed, got {outcome:?}");
    };
    assert_eq!(done.result_summary, "Added a doc comment to f.");
    assert_eq!(done.files_modified, ["src/lib.rs"]);
    assert!(done.changes_patch.unwrap().contains("diff --git"));
    assert_eq!(done.format_patch, None);
    assert_eq!(done.denial_count, 0);
    assert_eq!(done.iterations, 4);
    assert_eq!(done.model_used, "gemma4:e4b");
}

#[test]
fn failed_task_with_legacy_text_recovers_the_error_type() {
    let (mut client, _fake) = connect(vec![Scenario::failed("result_failed_legacy")]);
    client.initialize().unwrap();
    let outcome = client.run_task(&request("x"), WAIT).unwrap();
    assert_eq!(
        outcome,
        TaskOutcome::Failed(NannaFailure {
            failure_kind: None,
            error_type: Some("MaxIterationsExceeded".into()),
            message: "Maximum iterations (100) exceeded".into(),
        })
    );
}

fn failed_wire(text: &str) -> serde_json::Value {
    json!({"isError": true, "content": [{"type": "text", "text": text}]})
}

#[test]
fn a_multi_paragraph_failure_message_keeps_all_paragraphs_and_recovers_the_error_type() {
    let diagnostics = "{\n  \"error_type\": \"ProtectedPathViolation\",\n  \"denials\": []\n}";
    let text = format!("first paragraph\n\nsecond paragraph\n\n{diagnostics}");
    let outcome = parse_result(TaskState::Failed, &failed_wire(&text)).unwrap();
    assert_eq!(
        outcome,
        TaskOutcome::Failed(NannaFailure {
            failure_kind: None,
            error_type: Some("ProtectedPathViolation".into()),
            message: "first paragraph\n\nsecond paragraph".into(),
        })
    );
}

#[test]
fn a_failure_without_a_diagnostics_block_keeps_its_whole_text() {
    let text = "Task was interrupted\n\nsee the operator log";
    let outcome = parse_result(TaskState::Failed, &failed_wire(text)).unwrap();
    assert_eq!(
        outcome,
        TaskOutcome::Failed(NannaFailure {
            failure_kind: None,
            error_type: None,
            message: text.into(),
        })
    );
}

#[test]
fn a_message_that_itself_contains_a_json_looking_block_is_not_mistaken_for_diagnostics() {
    let text = "went wrong\n\n{not json at all}\n\n{\"error_type\": \"StateError\"}";
    let outcome = parse_result(TaskState::Failed, &failed_wire(text)).unwrap();
    assert_eq!(
        outcome,
        TaskOutcome::Failed(NannaFailure {
            failure_kind: None,
            error_type: Some("StateError".into()),
            message: "went wrong\n\n{not json at all}".into(),
        })
    );
}

proptest::proptest! {
    #[test]
    fn the_message_in_front_of_the_diagnostics_block_always_round_trips(message in "[a-z \n{}]{0,40}") {
        let text = format!("{message}\n\n{{\"error_type\": \"StateError\"}}");
        let outcome = parse_result(TaskState::Failed, &failed_wire(&text)).unwrap();
        let TaskOutcome::Failed(failure) = outcome else { panic!("expected a failure") };
        proptest::prop_assert_eq!(failure.error_type.as_deref(), Some("StateError"));
        proptest::prop_assert_eq!(failure.message, message);
    }
}

#[test]
fn failed_task_with_structured_content_carries_failure_kind() {
    let (mut client, _fake) = connect(vec![Scenario::failed("result_failed_structured")]);
    client.initialize().unwrap();
    let outcome = client.run_task(&request("x"), WAIT).unwrap();
    assert_eq!(
        outcome,
        TaskOutcome::Failed(NannaFailure {
            failure_kind: Some("max_iterations".into()),
            error_type: Some("MaxIterationsExceeded".into()),
            message: "Maximum iterations (100) exceeded".into(),
        })
    );
}

#[test]
fn refusals_are_synchronous_and_carry_the_kind_when_present() {
    for (name, kind) in [
        ("assign_refused_legacy", None),
        ("assign_refused_structured", Some("spawn_refused")),
        ("assign_invalid_structured", Some("invalid_request")),
    ] {
        let (mut client, mut fake) = connect(vec![Scenario::refused(name)]);
        client.initialize().unwrap();
        let outcome = client.run_task(&request("x"), WAIT).unwrap();
        let TaskOutcome::Refused(failure) = outcome else {
            panic!("{name}: expected refusal, got {outcome:?}");
        };
        assert_eq!(failure.failure_kind.as_deref(), kind, "{name}");
        assert!(!failure.message.is_empty());
        drop(client);
        fake.settle();
        assert!(!fake.methods().contains(&"tasks/get".to_string()));
    }
}

#[test]
fn cancelled_task_is_reported_as_cancelled() {
    for name in ["result_cancelled_legacy", "result_cancelled_structured"] {
        let (mut client, _fake) = connect(vec![Scenario::Cancelled {
            result: fixture(name),
        }]);
        client.initialize().unwrap();
        assert_eq!(
            client.run_task(&request("x"), WAIT).unwrap(),
            TaskOutcome::Cancelled,
            "{name}"
        );
    }
}

#[test]
fn a_hanging_task_is_cancelled_when_the_timeout_passes() {
    let (mut client, mut fake) = connect(vec![Scenario::Hang]);
    client.initialize().unwrap();
    let outcome = client
        .run_task(&request("x"), Duration::from_millis(30))
        .unwrap();
    assert_eq!(outcome, TaskOutcome::TimedOut);
    drop(client);
    fake.settle();
    assert!(fake.methods().contains(&"tasks/cancel".to_string()));
}

#[test]
fn a_task_that_finishes_as_the_timeout_fires_returns_its_result() {
    let (mut client, mut fake) = connect(vec![Scenario::finished_before_cancel()]);
    client.initialize().unwrap();
    let outcome = client
        .run_task(&request("x"), Duration::from_millis(30))
        .unwrap();
    assert!(
        matches!(&outcome, TaskOutcome::Completed(done) if !done.result_summary.is_empty()),
        "{outcome:?}"
    );
    drop(client);
    fake.settle();
    let methods = fake.methods();
    let cancel = methods.iter().position(|m| m == "tasks/cancel").unwrap();
    assert_eq!(methods[cancel + 1], "tasks/get");
    assert_eq!(methods[cancel + 2], "tasks/result");
}

#[test]
fn a_cancel_rejected_for_a_task_nanna_has_forgotten_is_still_a_timeout() {
    let (mut client, mut fake) = connect(vec![Scenario::ForgottenBeforeCancel]);
    client.initialize().unwrap();
    let outcome = client
        .run_task(&request("x"), Duration::from_millis(30))
        .unwrap();
    assert_eq!(outcome, TaskOutcome::TimedOut);
    assert!(!client.is_dead());
    drop(client);
    fake.settle();
    let methods = fake.methods();
    let cancel = methods.iter().position(|m| m == "tasks/cancel").unwrap();
    assert_eq!(methods[cancel + 1], "tasks/get");
    assert!(!methods.contains(&"tasks/result".to_string()));
}

#[test]
fn a_rejected_cancel_for_a_task_still_working_is_still_a_timeout() {
    let (mut client, _fake) = connect(vec![Scenario::RejectsCancelStillWorking]);
    client.initialize().unwrap();
    let outcome = client
        .run_task(&request("x"), Duration::from_millis(30))
        .unwrap();
    assert_eq!(outcome, TaskOutcome::TimedOut);
}

#[test]
fn a_connection_that_dies_during_the_cancel_is_a_timeout_and_the_client_reports_dead() {
    let reply = concat!(
        r#"{"jsonrpc":"2.0","id":1,"result":{"task":{"taskId":"t","status":"working","pollInterval":60000}}}"#,
        "\n"
    );
    let mut client = NannaClient::new(BufReader::new(reply.as_bytes()), io::sink());
    client.set_read_timeout(Duration::from_millis(50));
    let outcome = client.run_task(&request("x"), Duration::ZERO).unwrap();
    assert_eq!(outcome, TaskOutcome::TimedOut);
    assert!(client.is_dead());
}

#[test]
fn a_connection_that_dies_while_polling_is_still_an_error_for_the_caller() {
    let reply = concat!(
        r#"{"jsonrpc":"2.0","id":1,"result":{"task":{"taskId":"t","status":"working","pollInterval":1}}}"#,
        "\n"
    );
    let mut client = NannaClient::new(BufReader::new(reply.as_bytes()), io::sink());
    let err = client.run_task(&request("x"), WAIT).unwrap_err();
    assert!(matches!(err, ClientError::Eof), "{err:?}");
    assert!(client.is_dead());
}

#[test]
fn a_server_initiated_message_that_reuses_our_id_is_not_taken_as_the_response() {
    let replies = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"sampling/createMessage","params":{}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":1,"method":"notifications/message","params":{}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25"}}"#,
        "\n",
    );
    let mut client = NannaClient::new(BufReader::new(replies.as_bytes()), io::sink());
    let init = client.initialize().unwrap();
    assert_eq!(init["protocolVersion"], PROTOCOL_VERSION);
}

#[test]
fn an_interrupt_cancels_the_running_task_and_reports_it_cancelled() {
    let (mut client, mut fake) = connect(vec![Scenario::Hang]);
    client.initialize().unwrap();
    let flag = Arc::new(AtomicBool::new(true));
    client.set_interrupt(Arc::clone(&flag));
    let outcome = client.run_task(&request("x"), WAIT).unwrap();
    assert_eq!(outcome, TaskOutcome::Cancelled);
    assert!(!client.is_dead());
    drop(client);
    fake.settle();
    assert!(fake.methods().contains(&"tasks/cancel".to_string()));
}

#[test]
fn an_interrupt_raised_while_waiting_is_noticed_within_a_poll_slice() {
    let (mut client, _fake) = connect(vec![Scenario::Hang]);
    client.initialize().unwrap();
    let flag = Arc::new(AtomicBool::new(false));
    client.set_interrupt(Arc::clone(&flag));
    let raiser = {
        let flag = Arc::clone(&flag);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            flag.store(true, Ordering::SeqCst);
        })
    };
    let started = Instant::now();
    let outcome = client.run_task(&request("x"), WAIT).unwrap();
    raiser.join().unwrap();
    assert_eq!(outcome, TaskOutcome::Cancelled);
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn an_interrupted_client_is_not_usable_for_another_attempt() {
    let (mut client, _fake) = connect(vec![]);
    let flag = Arc::new(AtomicBool::new(false));
    client.set_interrupt(Arc::clone(&flag));
    assert!(client.is_usable());
    flag.store(true, Ordering::SeqCst);
    assert!(!client.is_usable());
    assert!(!client.is_dead());
}

#[test]
fn a_fresh_client_is_not_dead() {
    let (client, _fake) = connect(vec![]);
    assert!(!client.is_dead());
}

#[test]
fn rpc_errors_on_other_methods_surface_with_code_and_kind() {
    let (mut client, _fake) = connect(vec![]);
    client.initialize().unwrap();
    let err = client.get("nope").unwrap_err();
    assert!(
        matches!(err, ClientError::Rpc { code: -32602, .. }),
        "{err:?}"
    );
    assert!(client.cancel("nope").is_err());
    assert!(client.result("nope", TaskState::Failed).is_err());
}

#[test]
fn eof_before_a_response_is_an_error() {
    let mut client = NannaClient::new(BufReader::new(&b""[..]), io::sink());
    assert!(matches!(client.initialize().unwrap_err(), ClientError::Eof));
}

#[test]
fn malformed_responses_are_protocol_errors() {
    let neither = b"{\"jsonrpc\":\"2.0\",\"id\":1}\n";
    let mut client = NannaClient::new(BufReader::new(&neither[..]), io::sink());
    assert!(matches!(
        client.initialize().unwrap_err(),
        ClientError::Protocol(_)
    ));

    let no_task = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n";
    let mut client = NannaClient::new(BufReader::new(&no_task[..]), io::sink());
    assert!(matches!(
        client.assign_task(&request("x")).unwrap_err(),
        ClientError::Protocol(_)
    ));

    let bad_status =
        b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"taskId\":\"t\",\"status\":\"weird\"}}\n";
    let mut client = NannaClient::new(BufReader::new(&bad_status[..]), io::sink());
    assert!(matches!(
        client.get("t").unwrap_err(),
        ClientError::Protocol(_)
    ));

    let no_id = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"status\":\"working\"}}\n";
    let mut client = NannaClient::new(BufReader::new(&no_id[..]), io::sink());
    assert!(matches!(
        client.get("t").unwrap_err(),
        ClientError::Protocol(_)
    ));

    let no_status = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"taskId\":\"t\"}}\n";
    let mut client = NannaClient::new(BufReader::new(&no_status[..]), io::sink());
    assert!(matches!(
        client.get("t").unwrap_err(),
        ClientError::Protocol(_)
    ));
}

#[test]
fn transport_errors_during_assign_are_not_mistaken_for_refusals() {
    let mut client = NannaClient::new(BufReader::new(&b""[..]), io::sink());
    assert!(matches!(
        client.assign_task(&request("x")).unwrap_err(),
        ClientError::Eof
    ));
}

#[test]
fn completed_result_that_is_not_json_is_a_protocol_error() {
    let wire = json!({"isError": false, "content": [{"type": "text", "text": "plain"}]});
    assert!(matches!(
        parse_result(TaskState::Completed, &wire).unwrap_err(),
        ClientError::Protocol(_)
    ));
}

#[test]
fn failed_state_without_is_error_still_fails() {
    let wire = json!({"content": [{"type": "text", "text": "boom"}]});
    let outcome = parse_result(TaskState::Failed, &wire).unwrap();
    assert_eq!(
        outcome,
        TaskOutcome::Failed(NannaFailure {
            failure_kind: None,
            error_type: None,
            message: "boom".into()
        })
    );
}

#[test]
fn task_done_detects_changes_from_any_output() {
    let done = |patch: Option<&str>, format: Option<&str>, files: &[&str]| {
        TaskDone::from_wire(json!({
            "result_summary": "s",
            "changes_patch": patch,
            "format_patch": format,
            "files_modified": files,
            "denials": [{"x": 1}],
        }))
    };
    assert!(!done(None, None, &[]).has_changes());
    assert!(!done(Some("  \n"), Some(""), &[]).has_changes());
    assert!(done(Some("diff"), None, &[]).has_changes());
    assert!(done(None, Some("From abc"), &[]).has_changes());
    assert!(done(None, None, &["a"]).has_changes());
    assert_eq!(done(None, None, &[]).denial_count, 1);
}

fn script_options(dir: &Path, body: &str) -> SpawnOptions {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("nanna");
    std::fs::write(
        &script,
        format!("#!/bin/sh\n[ \"$1\" = mcp-serve ] || exit 9\n{body}"),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    SpawnOptions::new(script, dir.join("state"))
}

fn spawn_script(options: &SpawnOptions, home: &Path) -> NannaClient {
    let home = home.as_os_str().to_os_string();
    let lookup = move |key: &str| (key == "HOME").then(|| home.clone());
    loop {
        match NannaClient::spawn_with_lookup(options, &lookup) {
            Err(ClientError::Io(err)) if err.kind() == io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(Duration::from_millis(20));
            }
            other => break other.unwrap(),
        }
    }
}

const HANDSHAKE: &str = "read line\nprintf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-11-25\"}}'\nread line\n";

#[test]
fn spawn_runs_a_binary_with_mcp_serve_and_talks_to_it() {
    let dir = tempfile::tempdir().unwrap();
    let options = script_options(dir.path(), &format!("{HANDSHAKE}exec cat >/dev/null\n"));
    let mut client = spawn_script(&options, dir.path());
    assert_eq!(
        client.initialize().unwrap()["protocolVersion"],
        PROTOCOL_VERSION
    );
}

#[test]
fn spawn_of_a_missing_binary_is_an_io_error() {
    let dir = tempfile::tempdir().unwrap();
    let options = SpawnOptions::new("/definitely/not/nanna", dir.path().join("state"));
    let err = NannaClient::spawn_with_lookup(&options, &|_| Some("/h".into()))
        .err()
        .unwrap();
    assert!(matches!(err, ClientError::Io(_)));
}

fn lookup_from<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
    move |key| {
        pairs
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| OsString::from(v))
    }
}

fn setting(env: &SpawnEnv, key: &str) -> Option<String> {
    env.set
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.to_str().unwrap().to_string())
}

#[test]
fn the_queue_is_unique_and_private_while_shared_state_keeps_nannas_defaults() {
    let options = SpawnOptions::new("nanna", "/state/inchworm/nanna");
    let env = spawn_env(&options, "7-8-9", &lookup_from(&[("HOME", "/home/me")])).unwrap();
    assert_eq!(
        setting(&env, "NANNA_QUEUE_PATH").as_deref(),
        Some("/state/inchworm/nanna/queue-7-8-9.jsonl")
    );
    assert_eq!(
        env.queue_file,
        PathBuf::from("/state/inchworm/nanna/queue-7-8-9.jsonl")
    );
    assert_eq!(
        setting(&env, "NANNA_LEASE_PATH").as_deref(),
        Some("/home/me/.local/state/nanna/leases.jsonl")
    );
    assert_eq!(
        setting(&env, "NANNA_ESCALATION_PATH").as_deref(),
        Some("/home/me/.local/state/nanna/escalations.jsonl")
    );
    assert_eq!(
        setting(&env, "NANNA_BUDGET_PATH").as_deref(),
        Some("/home/me/.local/state/nanna/budget.json")
    );
    assert_eq!(setting(&env, "NANNA_CONFIG_DIR"), None);
    assert_eq!(env.remove, ["RUST_LOG"]);
}

#[test]
fn an_inherited_queue_path_only_locates_the_shared_files_and_is_never_reused() {
    let options = SpawnOptions::new("nanna", "/state/inchworm/nanna");
    let inherited = [
        ("NANNA_QUEUE_PATH", "/data/nanna/queue.jsonl"),
        ("HOME", "/home/me"),
    ];
    let env = spawn_env(&options, "u", &lookup_from(&inherited)).unwrap();
    assert_eq!(
        setting(&env, "NANNA_QUEUE_PATH").as_deref(),
        Some("/state/inchworm/nanna/queue-u.jsonl")
    );
    assert_eq!(
        setting(&env, "NANNA_BUDGET_PATH").as_deref(),
        Some("/data/nanna/budget.json")
    );
}

#[test]
fn explicit_shared_paths_are_kept_and_empty_values_count_as_unset() {
    let options = SpawnOptions::new("nanna", "/s");
    let inherited = [
        ("NANNA_LEASE_PATH", "/x/leases"),
        ("NANNA_ESCALATION_PATH", ""),
        ("NANNA_BUDGET_PATH", "/x/budget"),
        ("HOME", "/home/me"),
    ];
    let env = spawn_env(&options, "u", &lookup_from(&inherited)).unwrap();
    assert_eq!(
        setting(&env, "NANNA_LEASE_PATH").as_deref(),
        Some("/x/leases")
    );
    assert_eq!(
        setting(&env, "NANNA_ESCALATION_PATH").as_deref(),
        Some("/home/me/.local/state/nanna/escalations.jsonl")
    );
    assert_eq!(
        setting(&env, "NANNA_BUDGET_PATH").as_deref(),
        Some("/x/budget")
    );
}

#[test]
fn with_every_shared_path_explicit_no_home_is_needed() {
    let options = SpawnOptions::new("nanna", "/s");
    let inherited = [
        ("NANNA_LEASE_PATH", "/x/l"),
        ("NANNA_ESCALATION_PATH", "/x/e"),
        ("NANNA_BUDGET_PATH", "/x/b"),
    ];
    assert!(spawn_env(&options, "u", &lookup_from(&inherited)).is_ok());
}

#[test]
fn without_home_or_a_queue_path_the_shared_state_cannot_be_located() {
    let options = SpawnOptions::new("nanna", "/s");
    let err = spawn_env(&options, "u", &lookup_from(&[("HOME", "")])).unwrap_err();
    assert!(
        matches!(err, ClientError::Protocol(ref m) if m.contains("HOME or NANNA_QUEUE_PATH")),
        "{err:?}"
    );
}

#[test]
fn a_config_dir_is_passed_only_when_given() {
    let mut options = SpawnOptions::new("nanna", "/s");
    options.config_dir = Some(PathBuf::from("/cfg/nanna"));
    let env = spawn_env(&options, "u", &lookup_from(&[("HOME", "/h")])).unwrap();
    assert_eq!(
        setting(&env, "NANNA_CONFIG_DIR").as_deref(),
        Some("/cfg/nanna")
    );
}

#[test]
fn the_command_removes_rust_log_and_sets_the_isolated_environment() {
    let options = SpawnOptions::new("nanna", "/s");
    let env = spawn_env(&options, "u", &lookup_from(&[("HOME", "/h")])).unwrap();
    let command = build_command(&options, &env);
    let envs: Vec<_> = command
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_str().unwrap().to_string(),
                v.map(|v| v.to_str().unwrap().to_string()),
            )
        })
        .collect();
    assert!(envs.contains(&("RUST_LOG".to_string(), None)), "{envs:?}");
    assert!(envs.contains(&(
        "NANNA_QUEUE_PATH".to_string(),
        Some("/s/queue-u.jsonl".to_string())
    )));
    assert_eq!(command.get_args().collect::<Vec<_>>(), ["mcp-serve"]);
}

#[test]
fn every_spawn_gets_its_own_queue_file_name() {
    let names: std::collections::HashSet<_> = (0..50).map(|_| unique_suffix()).collect();
    assert_eq!(names.len(), 50);
}

#[test]
fn the_child_sees_the_isolated_environment_and_the_queue_file_is_removed_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("report");
    let body = format!(
        "{{ echo \"queue=$NANNA_QUEUE_PATH\"; echo \"lease=$NANNA_LEASE_PATH\"; echo \"rust_log=${{RUST_LOG-unset}}\"; }} > {report}\ntouch \"$NANNA_QUEUE_PATH\"\n{HANDSHAKE}exec cat >/dev/null\n",
        report = report.display()
    );
    let options = script_options(dir.path(), &body);
    let mut client = spawn_script(&options, dir.path());
    client.initialize().unwrap();
    let text = std::fs::read_to_string(&report).unwrap();
    let queue = text
        .lines()
        .find_map(|l| l.strip_prefix("queue="))
        .unwrap()
        .to_string();
    assert!(
        queue.starts_with(options.state_dir.to_str().unwrap()),
        "{text}"
    );
    assert!(Path::new(&queue).exists());
    assert!(
        text.contains(&format!(
            "lease={}/.local/state/nanna/leases.jsonl",
            dir.path().display()
        )),
        "{text}"
    );
    assert!(text.contains("rust_log=unset"), "{text}");
    drop(client);
    assert!(!Path::new(&queue).exists());
}

#[test]
fn a_stale_queue_file_with_the_same_name_is_removed_before_spawning() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let options = SpawnOptions::new("/definitely/not/nanna", &state);
    let env = spawn_env(&options, "fixed", &lookup_from(&[("HOME", "/h")])).unwrap();
    std::fs::write(&env.queue_file, "stale").unwrap();
    assert!(NannaClient::spawn_with_lookup(&options, &|_| Some("/h".into())).is_err());
    assert!(state.exists());
}

fn pid_alive(pid: &str) -> bool {
    Path::new("/proc").join(pid).exists()
}

#[test]
fn dropping_the_client_lets_a_well_behaved_nanna_exit_on_stdin_eof() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let body = format!(
        "echo $$ > {}\n{HANDSHAKE}exec cat >/dev/null\n",
        pidfile.display()
    );
    let options = script_options(dir.path(), &body);
    let mut client = spawn_script(&options, dir.path());
    client.initialize().unwrap();
    client.set_shutdown_grace(Duration::from_secs(30));
    let pid = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .to_string();
    let started = Instant::now();
    drop(client);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert!(!pid_alive(&pid));
}

#[test]
fn dropping_the_client_kills_a_nanna_that_ignores_stdin_eof_after_the_grace_period() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let body = format!(
        "echo $$ > {}\n{HANDSHAKE}exec sleep 60\n",
        pidfile.display()
    );
    let options = script_options(dir.path(), &body);
    let mut client = spawn_script(&options, dir.path());
    client.initialize().unwrap();
    client.set_shutdown_grace(Duration::from_millis(150));
    let pid = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .to_string();
    let started = Instant::now();
    drop(client);
    assert!(started.elapsed() >= Duration::from_millis(150));
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    assert!(!pid_alive(&pid));
}

#[test]
fn dropping_a_client_with_a_running_task_cancels_it_first() {
    let (mut client, mut fake) = connect(vec![Scenario::Hang]);
    client.initialize().unwrap();
    assert!(matches!(
        client.assign_task(&request("x")).unwrap(),
        Submitted::Accepted(_)
    ));
    drop(client);
    fake.settle();
    assert_eq!(
        fake.methods().last().map(String::as_str),
        Some("tasks/cancel")
    );
}

#[test]
fn a_finished_task_is_not_cancelled_on_drop() {
    let (mut client, mut fake) = connect(vec![Scenario::success()]);
    client.initialize().unwrap();
    let outcome = client.run_task(&request("x"), WAIT).unwrap();
    assert!(matches!(outcome, TaskOutcome::Completed(_)));
    drop(client);
    fake.settle();
    assert!(!fake.methods().contains(&"tasks/cancel".to_string()));
}

#[test]
fn a_cancelled_task_is_not_cancelled_again_on_drop() {
    let (mut client, mut fake) = connect(vec![Scenario::Hang]);
    client.initialize().unwrap();
    let Submitted::Accepted(info) = client.assign_task(&request("x")).unwrap() else {
        panic!("expected acceptance");
    };
    client.cancel(&info.task_id).unwrap();
    drop(client);
    fake.settle();
    let cancels = fake
        .methods()
        .iter()
        .filter(|m| *m == "tasks/cancel")
        .count();
    assert_eq!(cancels, 1);
}

#[test]
fn a_dead_connection_does_not_try_to_cancel_on_drop() {
    let (mut client, mut fake) = connect(vec![Scenario::Hang]);
    client.initialize().unwrap();
    let Submitted::Accepted(_) = client.assign_task(&request("x")).unwrap() else {
        panic!("expected acceptance");
    };
    client.set_read_timeout(Duration::from_millis(10));
    client.dead = true;
    drop(client);
    fake.settle();
    assert!(!fake.methods().contains(&"tasks/cancel".to_string()));
}

#[test]
#[ignore = "needs the real nanna binary at ~/.local/bin/nanna"]
fn real_binary_speaks_the_pinned_protocol() {
    let home = std::env::var("HOME").unwrap();
    let bin = std::env::var("INCHWORM_NANNA_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(&home).join(".local/bin/nanna"));
    let state = tempfile::tempdir().unwrap();
    let options = SpawnOptions::new(bin, state.path().join("nanna"));
    let mut client = NannaClient::spawn(&options).unwrap();
    let init = client.initialize().unwrap();
    assert_eq!(init["protocolVersion"], PROTOCOL_VERSION);
    assert!(init["capabilities"]["tasks"]["requests"]["tools"]["call"].is_object());

    let mut bad = request("x");
    bad.identity = "definitely-not-an-identity".into();
    bad.repo_path = std::env::temp_dir();
    match client.assign_task(&bad).unwrap() {
        Submitted::Refused(failure) => {
            assert!(failure.message.contains("spawn refused"), "{failure:?}")
        }
        other => panic!("expected refusal, got {other:?}"),
    }
    assert!(matches!(
        client.get("nope").unwrap_err(),
        ClientError::Rpc { .. }
    ));
}

#[test]
fn a_silent_server_times_out_each_read_and_marks_the_client_dead() {
    let (silent, _keep_open) = std::io::pipe().unwrap();
    let mut client = NannaClient::new(BufReader::new(silent), io::sink());
    client.set_read_timeout(Duration::from_millis(30));
    let err = client.initialize().unwrap_err();
    assert!(matches!(err, ClientError::ReadTimeout(_)), "{err:?}");
    assert!(err.to_string().contains("nothing for"));
    assert!(matches!(client.get("t").unwrap_err(), ClientError::Dead));
}

#[test]
fn a_hung_server_fails_run_task_instead_of_blocking_forever() {
    let (silent, _keep_open) = std::io::pipe().unwrap();
    let mut client = NannaClient::new(BufReader::new(silent), io::sink());
    client.set_read_timeout(Duration::from_millis(30));
    let err = client.run_task(&request("x"), WAIT).unwrap_err();
    assert!(matches!(err, ClientError::ReadTimeout(_)), "{err:?}");
}

#[test]
fn non_json_lines_on_stdout_are_skipped_not_fatal() {
    let replies = concat!(
        "2026-11-10T12:00:00Z ERROR harness: disk is slow\n",
        "\n",
        "42\n",
        "[1,2]\n",
        "\"just a string\"\n",
        r#"{"level":"error","msg":"json log line without an id"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":99,"result":{"somebody":"else"}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25"}}"#,
        "\n",
    );
    let mut client = NannaClient::new(BufReader::new(replies.as_bytes()), io::sink());
    let init = client.initialize().unwrap();
    assert_eq!(init["protocolVersion"], PROTOCOL_VERSION);
    assert_eq!(client.noise_lines(), 1);
}

#[test]
fn garbage_then_end_of_stream_is_a_closed_connection_not_a_json_error() {
    let mut client = NannaClient::new(BufReader::new(&b"not json\n"[..]), io::sink());
    assert!(matches!(client.initialize().unwrap_err(), ClientError::Eof));
    assert_eq!(client.noise_lines(), 1);
    assert!(matches!(
        client.initialize().unwrap_err(),
        ClientError::Dead
    ));
}

#[test]
fn a_flood_of_noise_without_an_answer_is_a_protocol_error() {
    let flood = "log line\n".repeat(MAX_NOISE_LINES_PER_EXCHANGE as usize + 5);
    let mut client = NannaClient::new(BufReader::new(std::io::Cursor::new(flood)), io::sink());
    let err = client.initialize().unwrap_err();
    assert!(
        matches!(err, ClientError::Protocol(ref m) if m.contains("non-JSON")),
        "{err:?}"
    );
    assert_eq!(client.noise_lines(), MAX_NOISE_LINES_PER_EXCHANGE + 1);
}

#[test]
fn read_errors_are_fatal_to_the_connection() {
    struct Broken;
    impl io::Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("boom"))
        }
    }
    let mut client = NannaClient::new(BufReader::new(Broken), io::sink());
    assert!(matches!(
        client.initialize().unwrap_err(),
        ClientError::Io(_)
    ));
}
