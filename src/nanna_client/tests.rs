use std::path::PathBuf;
use std::time::Duration;

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
fn garbage_and_malformed_responses_are_errors() {
    let mut client = NannaClient::new(BufReader::new(&b"not json\n"[..]), io::sink());
    assert!(matches!(
        client.initialize().unwrap_err(),
        ClientError::Json(_)
    ));

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

#[test]
fn spawn_runs_a_binary_with_mcp_serve_and_talks_to_it() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("nanna");
    std::fs::write(
        &script,
        "#!/bin/sh\n[ \"$1\" = mcp-serve ] || exit 9\nread line\nprintf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-11-25\"}}'\nread line\nsleep 5\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut client = loop {
        match NannaClient::spawn(&script) {
            Err(ClientError::Io(err)) if err.kind() == io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(Duration::from_millis(20));
            }
            other => break other.unwrap(),
        }
    };
    assert_eq!(
        client.initialize().unwrap()["protocolVersion"],
        PROTOCOL_VERSION
    );
}

#[test]
fn spawn_of_a_missing_binary_is_an_io_error() {
    let err = NannaClient::spawn(Path::new("/definitely/not/nanna"))
        .err()
        .unwrap();
    assert!(matches!(err, ClientError::Io(_)));
}

#[test]
#[ignore = "needs the real nanna binary at ~/.local/bin/nanna"]
fn real_binary_speaks_the_pinned_protocol() {
    let home = std::env::var("HOME").unwrap();
    let bin = std::env::var("INCHWORM_NANNA_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(&home).join(".local/bin/nanna"));
    let mut client = NannaClient::spawn(&bin).unwrap();
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
fn malformed_json_and_read_errors_are_fatal_to_the_connection() {
    let mut client = NannaClient::new(BufReader::new(&b"not json\n"[..]), io::sink());
    assert!(matches!(
        client.initialize().unwrap_err(),
        ClientError::Json(_)
    ));
    assert!(matches!(
        client.initialize().unwrap_err(),
        ClientError::Dead
    ));

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
