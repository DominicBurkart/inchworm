use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;

fn quoted(path: &Path) -> String {
    toml::Value::String(path.to_string_lossy().into_owned()).to_string()
}

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn good_repo(root: &Path) {
    std::fs::create_dir_all(root.join(".git")).unwrap();
    write(
        root,
        "inchworm.toml",
        "[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\n[[flow]]\nname = \"issue\"\ntrigger = { kind = \"label\", label = \"inchworm\" }\n[[flow.step]]\nname = \"implement\"\nidentity = \"rust-implementer\"\n[[flow.judge]]\nkind = \"nanna_clean\"\n[flow.outcome]\nmode = \"draft_pr\"\n",
    );
    write(
        root,
        ".nanna/protected.toml",
        "extra = [\"inchworm.toml\", \".inchworm/**\"]\n",
    );
    write(root, ".nanna/prompt.md", "prompt");
}

fn config(dir: &Path, repo: &Path) -> std::path::PathBuf {
    let bin = dir.join("nanna");
    std::fs::write(&bin, "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = dir.join("config.toml");
    write(
        dir,
        "config.toml",
        &format!(
            "nanna_bin = {}\n[[repo]]\nslug = \"o/r\"\npath = {}\n",
            quoted(&bin),
            quoted(repo)
        ),
    );
    path
}

fn inchworm() -> Command {
    let mut command = Command::cargo_bin("inchworm").unwrap();
    command.env_clear();
    command
}

#[test]
fn validate_exits_zero_for_a_complete_repo_with_no_environment_at_all() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    good_repo(&repo);
    let config = config(dir.path(), &repo);
    inchworm()
        .args(["validate", "--config"])
        .arg(&config)
        .assert()
        .success()
        .stdout(predicate::str::contains("0 error(s), 0 warning(s)"));
}

#[test]
fn validate_exits_one_when_a_repo_has_errors() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    good_repo(&repo);
    std::fs::remove_file(repo.join(".nanna/protected.toml")).unwrap();
    let config = config(dir.path(), &repo);
    inchworm()
        .args(["validate", "--config"])
        .arg(&config)
        .assert()
        .code(1)
        .stdout(predicate::str::contains("error: [o/r]"));
}

#[test]
fn validate_exits_two_with_one_message_when_the_config_is_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    inchworm()
        .args(["validate", "--config"])
        .arg(dir.path().join("missing.toml"))
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cannot read"))
        .stderr(predicate::function(|text: &str| {
            text.matches("os error").count() == 1
        }));
}

#[test]
fn validate_finds_the_default_config_from_home() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    good_repo(&repo);
    let config = config(dir.path(), &repo);
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".config/inchworm")).unwrap();
    std::fs::copy(&config, home.join(".config/inchworm/config.toml")).unwrap();
    inchworm()
        .env("HOME", &home)
        .arg("validate")
        .assert()
        .success();
}

#[test]
fn unknown_subcommands_exit_two_and_print_usage() {
    inchworm()
        .arg("bogus")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("Usage"));
}

#[cfg(unix)]
#[test]
fn validate_exits_one_when_the_inchworm_directory_is_a_symlink_to_an_outside_directory() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    good_repo(&repo);
    write(&repo, ".inchworm/rubric.md", "rubric");
    let outside = dir.path().join("outside");
    std::fs::rename(repo.join(".inchworm"), &outside).unwrap();
    std::os::unix::fs::symlink(&outside, repo.join(".inchworm")).unwrap();
    let config = config(dir.path(), &repo);
    inchworm()
        .args(["validate", "--config"])
        .arg(&config)
        .assert()
        .code(1)
        .stdout(predicate::str::contains("`.inchworm` is a symlink"));
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
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn fakes() -> Fakes {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    good_repo(&repo);
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for name in ["nanna", "gh"] {
        std::fs::copy(
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

fn inchworm_with(fakes: &Fakes) -> Command {
    let mut command = Command::cargo_bin("inchworm").unwrap();
    let path = format!(
        "{}:{}",
        fakes.bin().display(),
        std::env::var("PATH").unwrap()
    );
    command.env("PATH", path);
    command.env("HOME", fakes.dir.path().join("home"));
    command.env_remove("XDG_CONFIG_HOME");
    command
}

#[test]
fn online_validate_passes_when_identities_and_labels_resolve() {
    let fakes = fakes();
    inchworm_with(&fakes)
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
    inchworm_with(&fakes)
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
    inchworm_with(&fakes)
        .env("NIX_ARGS", &args_file)
        .args(["validate", "--build-images", "--config"])
        .arg(&fakes.config)
        .assert()
        .success();
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert_eq!(
        recorded.trim(),
        format!(
            "build git+file://{}#devContainerImage --no-link --no-write-lock-file",
            fakes.repo.display()
        )
    );

    script(
        &bin,
        "nix",
        "#!/bin/sh\necho 'attribute missing' >&2\nexit 1\n",
    );
    inchworm_with(&fakes)
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
    inchworm_with(&fakes)
        .args(["run", "o/r", "5", "--config"])
        .arg(&fakes.config)
        .assert()
        .success()
        .stdout(predicates_contains("awaiting_ci after 1 attempt(s)"))
        .stdout(predicates_contains("pull/9"));
    let again = inchworm_with(&fakes)
        .args(["run", "o/r", "5", "--config"])
        .arg(&fakes.config)
        .assert()
        .code(2);
    again.stderr(predicates_contains("already has a run"));
}

#[test]
fn run_rejects_a_pr_that_touches_inchworm_files() {
    let fakes = fakes();
    fakes.set("files", "F src/lib.rs\nF .inchworm/flows.toml");
    inchworm_with(&fakes)
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
    inchworm_with(&fakes)
        .args(["run", "o/r", "5", "--config"])
        .arg(&fakes.config)
        .assert()
        .code(1)
        .stdout(predicates_contains("failure: max_iterations"));
}

#[test]
fn run_accepts_a_repo_path_and_a_flow_name_and_rejects_unknown_ones() {
    let fakes = fakes();
    inchworm_with(&fakes)
        .args(["run"])
        .arg(&fakes.repo)
        .args(["5", "--flow", "issue", "--config"])
        .arg(&fakes.config)
        .assert()
        .success();
    inchworm_with(&fakes)
        .args(["run", "x/y", "5", "--config"])
        .arg(&fakes.config)
        .assert()
        .code(2)
        .stderr(predicates_contains("not in the config"));
    inchworm_with(&fakes)
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
    inchworm_with(&fakes)
        .args(["run", "o/r", "5", "--config"])
        .arg(&fakes.config)
        .assert()
        .code(2)
        .stderr(predicates_contains("cannot start"));
    std::fs::remove_file(fakes.repo.join("inchworm.toml")).unwrap();
    inchworm_with(&fakes)
        .args(["run", "o/r", "5", "--config"])
        .arg(&fakes.config)
        .assert()
        .code(2)
        .stderr(predicates_contains("cannot read"));
}

#[cfg(unix)]
fn run_and_signal(signal: &str) -> (Option<i32>, String, Fakes) {
    use std::io::Read;
    use std::time::{Duration, Instant};
    let fakes = fakes();
    fakes.set("state", "task_working");
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("inchworm"))
        .env("HOME", fakes.dir.path().join("home"))
        .env_remove("XDG_CONFIG_HOME")
        .env(
            "PATH",
            format!(
                "{}:{}",
                fakes.bin().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .args(["run", "o/r", "5", "--config"])
        .arg(&fakes.config)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let started = fakes.bin().join("started");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !started.exists() {
        assert!(Instant::now() < deadline, "the task never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(300));
    let status = std::process::Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(child.id().to_string())
        .status()
        .unwrap();
    assert!(status.success());
    let deadline = Instant::now() + Duration::from_secs(30);
    let exit = loop {
        if let Some(exit) = child.try_wait().unwrap() {
            break exit;
        }
        assert!(
            Instant::now() < deadline,
            "inchworm did not stop after {signal}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    (exit.code(), stdout, fakes)
}

#[cfg(unix)]
#[test]
fn sigint_cancels_the_task_and_ends_the_run_as_cancelled() {
    let (code, stdout, fakes) = run_and_signal("INT");
    assert_eq!(code, Some(1), "{stdout}");
    assert!(stdout.contains("failure: cancelled"), "{stdout}");
    assert!(
        fakes.bin().join("cancelled").exists(),
        "nanna was never asked to cancel"
    );
}

#[cfg(unix)]
#[test]
fn sigterm_cancels_the_task_and_ends_the_run_as_cancelled() {
    let (code, stdout, fakes) = run_and_signal("TERM");
    assert_eq!(code, Some(1), "{stdout}");
    assert!(stdout.contains("failure: cancelled"), "{stdout}");
    assert!(
        fakes.bin().join("cancelled").exists(),
        "nanna was never asked to cancel"
    );
}

#[test]
fn validate_help_warns_that_build_images_evaluates_repository_nix() {
    Command::cargo_bin("inchworm")
        .unwrap()
        .args(["validate", "--help"])
        .assert()
        .success()
        .stdout(predicates_contains(
            "evaluates the repository's own Nix code",
        ))
        .stdout(predicates_contains("only on repositories you trust"));
}

#[test]
fn run_refuses_an_issue_that_is_closed_or_written_by_an_untrusted_author() {
    let fakes = fakes();
    fakes.set(
        "issue",
        r#"{"number":5,"title":"t","body":"b","labels":[{"name":"inchworm"}],"url":"u","state":"CLOSED","authorAssociation":"OWNER"}"#,
    );
    inchworm_with(&fakes)
        .args(["run", "o/r", "5", "--config"])
        .arg(&fakes.config)
        .assert()
        .code(2)
        .stderr(predicates_contains("the issue is not open"));

    fakes.set(
        "issue",
        r#"{"number":5,"title":"t","body":"b","labels":[{"name":"inchworm"}],"url":"u","state":"OPEN","authorAssociation":"NONE"}"#,
    );
    inchworm_with(&fakes)
        .args(["run", "o/r", "5", "--config"])
        .arg(&fakes.config)
        .assert()
        .code(2)
        .stderr(predicates_contains("`NONE` user"));

    fakes.set(
        "issue",
        r#"{"number":5,"title":"t","body":"b","labels":[{"name":"bug"}],"url":"u","state":"OPEN","authorAssociation":"OWNER"}"#,
    );
    inchworm_with(&fakes)
        .args(["run", "o/r", "5", "--config"])
        .arg(&fakes.config)
        .assert()
        .code(2)
        .stderr(predicates_contains("does not carry the `inchworm` label"));
    assert!(
        !fakes.dir.path().join("state/nanna").exists(),
        "a refused run must not start nanna or leave its state files"
    );
}

#[test]
fn list_prints_one_tab_separated_line_per_registered_directory() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    good_repo(&repo);
    let config = config(dir.path(), &repo);
    inchworm()
        .args(["list", "--config"])
        .arg(&config)
        .assert()
        .success()
        .stdout(format!("o/r\t{}\n", repo.display()));
}

#[cfg(target_os = "linux")]
#[test]
fn list_to_a_full_device_exits_two() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    good_repo(&repo);
    let config = config(dir.path(), &repo);
    let full = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_inchworm"))
        .env_clear()
        .args(["list", "--config"])
        .arg(&config)
        .stdout(full)
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot write the list"));
}

#[test]
fn list_exits_two_when_the_config_is_unreadable_and_prints_nothing_to_stdout() {
    let dir = tempfile::tempdir().unwrap();
    inchworm()
        .args(["list", "--config"])
        .arg(dir.path().join("missing.toml"))
        .assert()
        .code(2)
        .stdout("")
        .stderr(predicate::str::contains("cannot read"));
}
