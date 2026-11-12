use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn good_repo(root: &Path) {
    std::fs::create_dir_all(root.join(".git")).unwrap();
    write(
        root,
        ".inchworm/flows.toml",
        "[[flow]]\nname = \"issue\"\ntrigger = { kind = \"label\", label = \"inchworm\" }\n[[flow.step]]\nname = \"implement\"\nidentity = \"rust-implementer\"\n[flow.outcome]\nmode = \"draft_pr\"\n",
    );
    write(
        root,
        ".nanna/protected.toml",
        "extra = [\".inchworm/**\"]\n",
    );
    write(root, ".nanna/prompt.md", "prompt");
}

fn config(dir: &Path, repo: &Path) -> std::path::PathBuf {
    let path = dir.join("config.toml");
    write(
        dir,
        "config.toml",
        &format!(
            "nanna_bin = \"nanna\"\n[[repo]]\nslug = \"o/r\"\npath = \"{}\"\n",
            repo.display()
        ),
    );
    path
}

#[test]
fn validate_exits_zero_for_a_complete_repo() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    good_repo(&repo);
    let config = config(dir.path(), &repo);
    Command::cargo_bin("inchworm")
        .unwrap()
        .args(["validate", "--config"])
        .arg(&config)
        .assert()
        .success()
        .stdout(predicates_contains("0 error(s), 0 warning(s)"));
}

#[test]
fn validate_exits_one_when_a_repo_has_errors() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    good_repo(&repo);
    std::fs::remove_file(repo.join(".nanna/protected.toml")).unwrap();
    let config = config(dir.path(), &repo);
    Command::cargo_bin("inchworm")
        .unwrap()
        .args(["validate", "--config"])
        .arg(&config)
        .assert()
        .code(1)
        .stdout(predicates_contains("error: [o/r]"));
}

#[test]
fn validate_exits_two_when_the_config_is_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("inchworm")
        .unwrap()
        .args(["validate", "--config"])
        .arg(dir.path().join("missing.toml"))
        .assert()
        .code(2)
        .stderr(predicates_contains("cannot read"));
}

#[test]
fn unknown_subcommands_are_rejected() {
    Command::cargo_bin("inchworm")
        .unwrap()
        .arg("bogus")
        .assert()
        .failure();
}

fn predicates_contains(needle: &'static str) -> impl predicates::Predicate<[u8]> {
    predicates::str::contains(needle).from_utf8()
}

struct Fakes {
    dir: tempfile::TempDir,
    repo: std::path::PathBuf,
    config: std::path::PathBuf,
}

impl Fakes {
    fn bin(&self) -> std::path::PathBuf {
        self.dir.path().join("bin")
    }

    fn set(&self, name: &str, value: &str) {
        std::fs::write(self.bin().join(name), value).unwrap();
    }
}

fn script(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    let mut body_name = path.clone().into_os_string();
    body_name.push(".body");
    std::fs::write(&body_name, body).unwrap();
    let _ = std::fs::remove_file(&path);
    std::os::unix::fs::symlink(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/bin/shim"),
        &path,
    )
    .unwrap();
}

fn fakes() -> Fakes {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    good_repo(&repo);
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for name in ["nanna", "gh"] {
        std::os::unix::fs::symlink(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/bin/").to_string() + name,
            bin.join(name),
        )
        .unwrap();
    }
    std::fs::write(
        bin.join("fixtures"),
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/nanna"),
    )
    .unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "nanna_bin = \"{}\"\nstate_db = \"{}\"\n[[repo]]\nslug = \"o/r\"\npath = \"{}\"\n",
            bin.join("nanna").display(),
            dir.path().join("state/state.db").display(),
            repo.display()
        ),
    )
    .unwrap();
    Fakes { dir, repo, config }
}

fn inchworm(fakes: &Fakes) -> Command {
    let mut command = Command::cargo_bin("inchworm").unwrap();
    let path = format!(
        "{}:{}",
        fakes.bin().display(),
        std::env::var("PATH").unwrap()
    );
    command.env("PATH", path);
    command
}

#[test]
fn online_validate_passes_when_identities_and_labels_resolve() {
    let fakes = fakes();
    inchworm(&fakes)
        .args(["validate", "--online", "--config"])
        .arg(&fakes.config)
        .assert()
        .success()
        .stdout(predicates_contains("0 error(s), 0 warning(s)"));
}

#[test]
fn online_validate_reports_unknown_identities_and_missing_labels() {
    let fakes = fakes();
    fakes.set("identities", "reviewer");
    fakes.set("labels", r#"[{"name":"inchworm"}]"#);
    inchworm(&fakes)
        .args(["validate", "--online", "--config"])
        .arg(&fakes.config)
        .assert()
        .code(1)
        .stdout(predicates_contains(
            "identity `rust-implementer` does not resolve",
        ))
        .stdout(predicates_contains(
            "label `inchworm:running` does not exist",
        ));
}

const PLAN_LIMIT: &str =
    "gh: Upgrade to GitHub Pro or make this repository public to enable this feature. (HTTP 403)\n";

fn require_build_check(fakes: &Fakes, outcome: &str) {
    let flows = std::fs::read_to_string(fakes.repo.join(".inchworm/flows.toml")).unwrap();
    let flows = flows.replace(
        "[flow.outcome]\nmode = \"draft_pr\"",
        &format!("[[flow.judge]]\nkind = \"nanna_clean\"\n[flow.outcome]\n{outcome}\nrequired_checks = [\"build\"]"),
    );
    write(&fakes.repo, ".inchworm/flows.toml", &flows);
}

#[test]
fn online_validate_warns_when_branch_protection_is_plan_limited_for_draft_pr() {
    let fakes = fakes();
    require_build_check(&fakes, "mode = \"draft_pr\"");
    fakes.set("protection_error", PLAN_LIMIT);
    inchworm(&fakes)
        .args(["validate", "--online", "--config"])
        .arg(&fakes.config)
        .assert()
        .success()
        .stdout(predicates_contains("0 error(s), 1 warning(s)"))
        .stdout(predicates_contains("Upgrade to GitHub Pro"));
}

#[test]
fn online_validate_keeps_plan_limited_branch_protection_an_error_for_merge_when() {
    let fakes = fakes();
    require_build_check(&fakes, "mode = \"merge_when\"");
    fakes.set("protection_error", PLAN_LIMIT);
    inchworm(&fakes)
        .args(["validate", "--online", "--config"])
        .arg(&fakes.config)
        .assert()
        .code(1)
        .stdout(predicates_contains("Upgrade to GitHub Pro"));
}

#[test]
fn online_validate_keeps_other_branch_protection_failures_errors_for_draft_pr() {
    let fakes = fakes();
    require_build_check(&fakes, "mode = \"draft_pr\"");
    fakes.set("protection_error", "gh: Bad credentials (HTTP 401)\n");
    inchworm(&fakes)
        .args(["validate", "--online", "--config"])
        .arg(&fakes.config)
        .assert()
        .code(1)
        .stdout(predicates_contains("HTTP 401"));
}

#[test]
fn build_images_runs_nix_build_and_reports_failures() {
    let fakes = fakes();
    let bin = fakes.dir.path().join("bin");
    script(
        &bin,
        "nix",
        "#!/bin/sh\necho \"$@\" > \"$NIX_ARGS\"\nexit 0\n",
    );
    let args_file = fakes.dir.path().join("nix-args");
    inchworm(&fakes)
        .env("NIX_ARGS", &args_file)
        .args(["validate", "--build-images", "--config"])
        .arg(&fakes.config)
        .assert()
        .success();
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert_eq!(
        recorded.trim(),
        format!(
            "build path:{}#devContainerImage --no-link",
            fakes.repo.display()
        )
    );

    script(
        &bin,
        "nix",
        "#!/bin/sh\necho 'attribute missing' >&2\nexit 1\n",
    );
    inchworm(&fakes)
        .args(["validate", "--build-images", "--config"])
        .arg(&fakes.config)
        .assert()
        .code(1)
        .stdout(predicates_contains(
            "devContainerImage does not build: attribute missing",
        ));
}

#[test]
fn run_dispatches_the_flow_and_records_the_pr() {
    let fakes = fakes();
    inchworm(&fakes)
        .args(["run", "o/r", "5", "--config"])
        .arg(&fakes.config)
        .assert()
        .success()
        .stdout(predicates_contains("pr_open after 1 attempt(s)"))
        .stdout(predicates_contains("pull/9"));
    inchworm(&fakes)
        .args(["run", "o/r", "5", "--config"])
        .arg(&fakes.config)
        .assert()
        .success()
        .stdout(predicates_contains("run 2 for o/r#5"));
}

#[test]
fn run_rejects_a_pr_that_touches_inchworm_files() {
    let fakes = fakes();
    fakes.set("files", "src/lib.rs\n.inchworm/flows.toml");
    inchworm(&fakes)
        .args(["run", "o/r", "5", "--config"])
        .arg(&fakes.config)
        .assert()
        .code(1)
        .stdout(predicates_contains("failure: protected_path_violation"));
}

#[test]
fn run_reports_a_failed_task_with_exit_one() {
    let fakes = fakes();
    fakes.set("state", "task_failed");
    fakes.set("result", "result_failed_structured");
    inchworm(&fakes)
        .args(["run", "o/r", "5", "--config"])
        .arg(&fakes.config)
        .assert()
        .code(1)
        .stdout(predicates_contains("failure: max_iterations"));
}

#[test]
fn run_accepts_a_repo_path_and_a_flow_name_and_rejects_unknown_ones() {
    let fakes = fakes();
    inchworm(&fakes)
        .args(["run"])
        .arg(&fakes.repo)
        .args(["5", "--flow", "issue", "--config"])
        .arg(&fakes.config)
        .assert()
        .success();
    inchworm(&fakes)
        .args(["run", "x/y", "5", "--config"])
        .arg(&fakes.config)
        .assert()
        .code(2)
        .stderr(predicates_contains("not in the config"));
    inchworm(&fakes)
        .args(["run", "o/r", "6", "--flow", "nope", "--config"])
        .arg(&fakes.config)
        .assert()
        .code(2)
        .stderr(predicates_contains("flow nope not found"));
}

#[test]
fn run_fails_cleanly_when_nanna_cannot_start_or_flows_are_missing() {
    let fakes = fakes();
    std::fs::write(
        &fakes.config,
        format!(
            "nanna_bin = \"/no/such/nanna\"\nstate_db = \"{}\"\n[[repo]]\nslug = \"o/r\"\npath = \"{}\"\n",
            fakes.dir.path().join("s.db").display(),
            fakes.repo.display()
        ),
    )
    .unwrap();
    inchworm(&fakes)
        .args(["run", "o/r", "5", "--config"])
        .arg(&fakes.config)
        .assert()
        .code(2)
        .stderr(predicates_contains("cannot start"));
    std::fs::remove_file(fakes.repo.join(".inchworm/flows.toml")).unwrap();
    inchworm(&fakes)
        .args(["run", "o/r", "5", "--config"])
        .arg(&fakes.config)
        .assert()
        .code(2)
        .stderr(predicates_contains("cannot read"));
}

#[test]
fn daemon_stops_cleanly_on_sigterm() {
    let fakes = fakes();
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("inchworm"))
        .args(["daemon", "--config"])
        .arg(&fakes.config)
        .env(
            "PATH",
            format!(
                "{}:{}",
                fakes.bin().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert!(child.try_wait().unwrap().is_none(), "daemon exited early");
    let killed = std::process::Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "daemon ignored SIGTERM"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(status.success(), "{status:?}");
}

fn ollama_fixture(score: u8) -> (String, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = stream.read(&mut chunk).unwrap_or(0);
                request.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&request).to_string();
                if let Some(split) = text.find("\r\n\r\n") {
                    let length = text[..split]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= split + 4 + length {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let inner = format!("{{\"score\": {score}, \"reason\": \"tidy\"}}");
            let body = serde_json::json!({"response": inner}).to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    (url, handle)
}

#[test]
fn end_to_end_dry_run_issue_to_judged_draft_pr_and_stats() {
    let fakes = fakes();
    let (ollama, _server) = ollama_fixture(9);
    write(
        &fakes.repo,
        ".inchworm/flows.toml",
        r#"[[flow]]
name = "issue"
trigger = { kind = "label", label = "inchworm" }
[[flow.step]]
name = "implement"
identity = "rust-implementer"
[[flow.judge]]
kind = "diff"
max_lines = 50
forbid_paths = [".github/**"]
[[flow.judge]]
kind = "nanna_clean"
[[flow.judge]]
kind = "llm"
rubric = ".inchworm/rubric.md"
model = "gemma4:e4b"
min_score = 7
[flow.outcome]
mode = "draft_pr"
required_checks = ["build"]
"#,
    );
    write(
        &fakes.repo,
        ".inchworm/rubric.md",
        "Prefer small, tidy changes.",
    );
    let mut text = std::fs::read_to_string(&fakes.config).unwrap();
    text = format!("ollama_url = \"{ollama}\"\n{text}");
    std::fs::write(&fakes.config, text).unwrap();
    fakes.set(
        "checks",
        "{\"name\":\"build\",\"status\":\"completed\",\"conclusion\":\"success\"}",
    );
    fakes.set("protection", "build\n");

    inchworm(&fakes)
        .args(["validate", "--online", "--config"])
        .arg(&fakes.config)
        .assert()
        .success();

    inchworm(&fakes)
        .args(["run", "o/r", "5", "--config"])
        .arg(&fakes.config)
        .assert()
        .success()
        .stdout(predicates_contains("pr_open after 1 attempt(s)"))
        .stdout(predicates_contains("pull/9"));

    inchworm(&fakes)
        .args(["stats", "--window", "all", "--config"])
        .arg(&fakes.config)
        .assert()
        .success()
        .stdout(predicates_contains("o/r"))
        .stdout(predicates_contains("100.0%"));
    inchworm(&fakes)
        .args(["status", "--config"])
        .arg(&fakes.config)
        .assert()
        .success()
        .stdout(predicates_contains("pr_open"));
    inchworm(&fakes)
        .args(["failures", "--config"])
        .arg(&fakes.config)
        .assert()
        .success()
        .stdout(predicates_contains("no failures recorded"));

    let store = inchworm::store::Store::open(&fakes.dir.path().join("state/state.db")).unwrap();
    let run = store.recent_runs(1).unwrap().remove(0);
    assert_eq!(run.state, inchworm::model::RunState::PrOpen);
    let attempt = &store.attempts(run.id).unwrap()[0];
    assert_eq!(attempt.state, inchworm::store::AttemptState::Succeeded);
    let verdicts = store.verdicts(attempt.id).unwrap();
    let names: Vec<_> = verdicts
        .iter()
        .map(|v| (v.judge.as_str(), v.passed))
        .collect();
    assert_eq!(
        names,
        [("diff", true), ("nanna_clean", true), ("llm", true)]
    );
    assert_eq!(verdicts[2].score, Some(9));
    let rates = store.success_rates(i64::MAX / 2, None).unwrap();
    assert_eq!((rates[0].total, rates[0].succeeded), (1, 1));
}

#[test]
fn end_to_end_with_a_low_llm_score_rejects_the_attempt_and_records_it() {
    let fakes = fakes();
    let (ollama, _server) = ollama_fixture(2);
    write(
        &fakes.repo,
        ".inchworm/flows.toml",
        r#"[[flow]]
name = "issue"
max_attempts = 1
trigger = { kind = "label", label = "inchworm" }
[[flow.step]]
name = "implement"
identity = "rust-implementer"
[[flow.judge]]
kind = "llm"
rubric = ".inchworm/rubric.md"
model = "gemma4:e4b"
min_score = 7
[flow.outcome]
mode = "draft_pr"
"#,
    );
    write(&fakes.repo, ".inchworm/rubric.md", "Be good.");
    let text = std::fs::read_to_string(&fakes.config).unwrap();
    std::fs::write(&fakes.config, format!("ollama_url = \"{ollama}\"\n{text}")).unwrap();
    inchworm(&fakes)
        .args(["run", "o/r", "5", "--config"])
        .arg(&fakes.config)
        .assert()
        .code(1)
        .stdout(predicates_contains("failure: judge_rejected"));
    inchworm(&fakes)
        .args(["failures", "--class", "judge_rejected", "--config"])
        .arg(&fakes.config)
        .assert()
        .success()
        .stdout(predicates_contains("llm: score 2 below min_score 7"));
}

#[test]
fn run_never_prints_escape_or_bidi_characters_from_failure_text() {
    let fakes = fakes();
    fakes.set(
        "files",
        "src/lib.rs\n.inchworm/\u{1b}]0;pwned\u{7}\u{1b}[2J\u{202e}.toml",
    );
    let out = inchworm(&fakes)
        .args(["run", "o/r", "5", "--config"])
        .arg(&fakes.config)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("failure:"), "{stdout}");
    assert!(!stdout.contains('\u{1b}'), "{stdout:?}");
    assert!(!stdout.contains('\u{202e}'), "{stdout:?}");
}

#[test]
fn the_shipped_unit_keeps_rootless_podman_working_and_restarts_after_transient_faults() {
    let unit = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/contrib/inchworm.service"
    ))
    .unwrap();
    for broken in [
        "PrivateTmp",
        "ProtectKernelTunables",
        "ProtectKernelModules",
        "ProtectControlGroups",
        "LockPersonality",
        "RestartPreventExitStatus",
    ] {
        assert!(!unit.contains(broken), "{broken} is still in the unit");
    }
    assert!(unit.contains("Restart=on-failure"));
    assert!(unit.contains("StartLimitBurst="));
    assert!(unit.contains("StartLimitIntervalSec="));
    assert!(unit.contains("NoNewPrivileges=yes"));
}

#[test]
fn the_readme_creates_the_state_directory_private() {
    let readme =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md")).unwrap();
    assert!(readme.contains("install -d -m 700"));
    assert!(!readme.contains("mkdir -p ~/.config/inchworm ~/.local/state/inchworm"));
}
