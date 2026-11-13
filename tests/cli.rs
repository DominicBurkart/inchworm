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
        "[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\n[[flow]]\nname = \"issue\"\ntrigger = { kind = \"label\", label = \"inchworm\" }\n[[flow.step]]\nname = \"implement\"\nidentity = \"rust-implementer\"\n[flow.outcome]\nmode = \"draft_pr\"\n",
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
