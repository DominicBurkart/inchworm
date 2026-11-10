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

const FAKE_NANNA: &str = r#"#!/bin/sh
if [ "$1" = agents ]; then
  printf 'NAME LOOP MODEL MAX_EFFECT\n'
  for name in $FAKE_IDENTITIES; do printf '%s inner m workspace\n' "$name"; done
  exit 0
fi
[ "$1" = mcp-serve ] || exit 9
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$(tr -d '\n' < "$FIXTURES/$2.json")"; }
while read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  case "$line" in
    *'"method":"initialize"'*) reply "$id" initialize_result ;;
    *'"method":"tools/call"'*) reply "$id" create_task_result ;;
    *'"method":"tasks/get"'*) reply "$id" "$FAKE_STATE" ;;
    *'"method":"tasks/result"'*) reply "$id" "$FAKE_RESULT" ;;
  esac
done
"#;

const FAKE_GH: &str = r#"#!/bin/sh
case "$1 $2" in
  "issue view") echo '{"number":5,"title":"Do it","body":"please","labels":[{"name":"inchworm"}],"url":"https://github.com/o/r/issues/5"}' ;;
  "issue edit"|"issue comment") ;;
  "label list") printf '%s\n' "$FAKE_LABELS" ;;
  "pr list") printf '%s\n' "$FAKE_PRS" ;;
  "api --paginate") printf '%s\n' "$FAKE_FILES" ;;
  *) echo "unexpected gh $*" >&2; exit 1 ;;
esac
"#;

struct Fakes {
    dir: tempfile::TempDir,
    repo: std::path::PathBuf,
    config: std::path::PathBuf,
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
    script(&bin, "nanna", FAKE_NANNA);
    script(&bin, "gh", FAKE_GH);
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
        fakes.dir.path().join("bin").display(),
        std::env::var("PATH").unwrap()
    );
    command
        .env("PATH", path)
        .env(
            "FIXTURES",
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/nanna"),
        )
        .env("FAKE_IDENTITIES", "rust-implementer")
        .env(
            "FAKE_LABELS",
            r#"[{"name":"inchworm"},{"name":"inchworm:running"},{"name":"inchworm:failed"},{"name":"inchworm:done"}]"#,
        )
        .env("FAKE_FILES", "src/lib.rs")
        .env("FAKE_STATE", "task_completed")
        .env("FAKE_RESULT", "result_success")
        .env(
            "FAKE_PRS",
            r#"[{"number":9,"headRefOid":"abc123","isDraft":true,"state":"OPEN","url":"https://github.com/o/r/pull/9"}]"#,
        );
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
    inchworm(&fakes)
        .env("FAKE_IDENTITIES", "reviewer")
        .env("FAKE_LABELS", r#"[{"name":"inchworm"}]"#)
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
        .stdout(predicates_contains("awaiting_ci after 1 attempt(s)"))
        .stdout(predicates_contains("pull/9"));
    let again = inchworm(&fakes)
        .args(["run", "o/r", "5", "--config"])
        .arg(&fakes.config)
        .assert()
        .code(2);
    again.stderr(predicates_contains("already has a run"));
}

#[test]
fn run_rejects_a_pr_that_touches_inchworm_files() {
    let fakes = fakes();
    inchworm(&fakes)
        .env("FAKE_FILES", "src/lib.rs\n.inchworm/flows.toml")
        .args(["run", "o/r", "5", "--config"])
        .arg(&fakes.config)
        .assert()
        .code(1)
        .stdout(predicates_contains("failure: protected_path_violation"));
}

#[test]
fn run_reports_a_failed_task_with_exit_one() {
    let fakes = fakes();
    inchworm(&fakes)
        .env("FAKE_STATE", "task_failed")
        .env("FAKE_RESULT", "result_failed_structured")
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
