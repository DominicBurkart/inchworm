#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/coverage-no-regression.sh")
}

fn scrubbed(program: &str, home: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("LC_ALL", "C");
    command
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = scrubbed("git", dir)
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

struct Setup {
    dir: tempfile::TempDir,
    repo: PathBuf,
    bin: PathBuf,
    base: String,
}

fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("a.txt"), "one").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("a.txt"), "two").unwrap();
    git(&repo, &["commit", "-q", "-am", "change"]);

    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let cargo = bin.join("cargo");
    std::fs::write(
        &cargo,
        "#!/bin/bash\ncase \"$PWD\" in\n  */base) printf '%s\\n' \"$FAKE_BASE\"; exit \"${FAKE_BASE_EXIT:-0}\" ;;\n  *) printf '%s\\n' \"$FAKE_NEW\"; exit \"${FAKE_NEW_EXIT:-0}\" ;;\nesac\n",
    )
    .unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    Setup {
        dir,
        repo,
        bin,
        base,
    }
}

fn run(s: &Setup, base: &str, new_out: &str, base_out: &str, extra: &[(&str, &str)]) -> Output {
    let path = format!("{}:{}", s.bin.display(), std::env::var("PATH").unwrap());
    let mut command = scrubbed("bash", s.dir.path());
    command
        .arg(script())
        .arg(base)
        .current_dir(&s.repo)
        .env("PATH", path)
        .env("FAKE_NEW", new_out)
        .env("FAKE_BASE", base_out);
    for (key, value) in extra {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn text(output: &Output) -> (i32, String) {
    (
        output.status.code().unwrap(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

fn worktrees(s: &Setup) -> usize {
    git(&s.repo, &["worktree", "list"]).lines().count()
}

#[test]
fn equal_or_better_coverage_passes_and_says_both_numbers() {
    let s = setup();
    let (code, message) = text(&run(
        &s,
        &s.base,
        "80.00% coverage, 8/10 lines covered",
        "80.00% coverage, 8/10",
        &[],
    ));
    assert_eq!(code, 0, "{message}");
    assert!(message.contains("Coverage: 80.00% >= 80.00%"), "{message}");
    let (code, message) = text(&run(&s, &s.base, "90.50% coverage", "80.00% coverage", &[]));
    assert_eq!(code, 0, "{message}");
    assert!(message.contains("90.50% >= 80.00%"), "{message}");
}

#[test]
fn a_drop_fails_and_says_by_how_much() {
    let s = setup();
    let (code, message) = text(&run(&s, &s.base, "79.99% coverage", "80.00% coverage", &[]));
    assert_eq!(code, 1, "{message}");
    assert!(
        message.contains("Coverage dropped: 79.99% < 80.00%"),
        "{message}"
    );
}

#[test]
fn a_base_run_without_a_coverage_line_fails_and_says_why() {
    let s = setup();
    let (code, message) = text(&run(
        &s,
        &s.base,
        "80.00% coverage",
        "Compiling inchworm",
        &[],
    ));
    assert_eq!(code, 1, "{message}");
    assert!(
        message.contains("No coverage line in the tarpaulin output for"),
        "{message}"
    );
    assert!(message.contains("/base"), "{message}");
}

#[test]
fn a_new_run_without_a_coverage_line_fails_and_says_why() {
    let s = setup();
    let (code, message) = text(&run(&s, &s.base, "nothing useful", "80.00% coverage", &[]));
    assert_eq!(code, 1, "{message}");
    assert!(
        message.contains("No coverage line in the tarpaulin output for"),
        "{message}"
    );
    assert!(message.contains("/repo"), "{message}");
}

#[test]
fn a_failing_tarpaulin_run_fails_and_shows_its_output() {
    let s = setup();
    let (code, message) = text(&run(
        &s,
        &s.base,
        "error: could not compile",
        "80.00% coverage",
        &[("FAKE_NEW_EXIT", "101")],
    ));
    assert_eq!(code, 1, "{message}");
    assert!(message.contains("could not compile"), "{message}");
    assert!(message.contains("cargo tarpaulin failed in"), "{message}");
    assert!(
        !message.contains("Coverage"),
        "a failed run must stop before any comparison: {message}"
    );
}

#[test]
fn an_all_zero_base_skips_and_an_unknown_base_is_a_usage_error() {
    let s = setup();
    let zeros = "0".repeat(40);
    let (code, message) = text(&run(&s, &zeros, "1.00% coverage", "99.00% coverage", &[]));
    assert_eq!(code, 0, "{message}");
    assert!(message.contains("skipping"), "{message}");
    let (code, message) = text(&run(
        &s,
        "no-such-ref",
        "80.00% coverage",
        "80.00% coverage",
        &[],
    ));
    assert_eq!(code, 2, "{message}");
    assert!(message.contains("Cannot resolve"), "{message}");
}

#[test]
fn the_script_needs_exactly_one_argument() {
    let s = setup();
    let result = scrubbed("bash", s.dir.path())
        .arg(script())
        .current_dir(&s.repo)
        .output()
        .unwrap();
    let (code, message) = text(&result);
    assert_eq!(code, 2, "{message}");
    assert!(message.contains("usage"), "{message}");
}

#[test]
fn the_baseline_worktree_is_removed_on_every_outcome() {
    let s = setup();
    assert_eq!(worktrees(&s), 1);
    run(&s, &s.base, "80.00% coverage", "80.00% coverage", &[]);
    assert_eq!(worktrees(&s), 1);
    run(&s, &s.base, "79.00% coverage", "80.00% coverage", &[]);
    assert_eq!(worktrees(&s), 1);
    run(&s, &s.base, "80.00% coverage", "no line", &[]);
    assert_eq!(worktrees(&s), 1);
    assert!(s.dir.path().join("repo").exists());
}

#[test]
fn ambient_git_variables_cannot_reach_the_scratch_repository() {
    let decoy = tempfile::tempdir().unwrap();
    let decoy_dir = decoy.path().join("decoy-git-dir");
    std::fs::create_dir_all(&decoy_dir).unwrap();
    let decoy_index = decoy.path().join("decoy-index");
    std::fs::write(&decoy_index, b"decoy index bytes").unwrap();

    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "equal_or_better_coverage_passes_and_says_both_numbers",
            "--test-threads=1",
        ])
        .env("GIT_DIR", &decoy_dir)
        .env("GIT_INDEX_FILE", &decoy_index)
        .env("GIT_WORK_TREE", decoy.path())
        .env("GIT_COMMON_DIR", &decoy_dir)
        .env("GIT_PREFIX", "decoy/")
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(std::fs::read(&decoy_index).unwrap(), b"decoy index bytes");
    assert_eq!(std::fs::read_dir(&decoy_dir).unwrap().count(), 0);
}
