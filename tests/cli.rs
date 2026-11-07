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
