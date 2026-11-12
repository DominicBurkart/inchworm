use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};

use crate::config::{
    FlowFile, GlobalConfig, Judge, OutcomeMode, RUBRIC_DIR, RepoEntry, rubric_path_problem,
};
use crate::fsutil::read_regular_text;

pub const PROTECTED_GLOB: &str = ".inchworm/**";
pub const MAX_FLOWS_BYTES: u64 = 256 * 1024;
pub const MAX_PROTECTED_BYTES: u64 = 64 * 1024;
pub const MAX_PROTECTED_ENTRIES: usize = 256;
const MAX_GITFILE_BYTES: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub repo: Option<String>,
    pub severity: Severity,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub findings: Vec<Finding>,
}

impl Report {
    pub fn error_count(&self) -> usize {
        self.count(Severity::Error)
    }

    pub fn warning_count(&self) -> usize {
        self.count(Severity::Warning)
    }

    pub fn is_ok(&self) -> bool {
        self.error_count() == 0
    }

    fn count(&self, severity: Severity) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == severity)
            .count()
    }

    fn push(&mut self, repo: Option<&str>, severity: Severity, message: impl Into<String>) {
        self.findings.push(Finding {
            repo: repo.map(str::to_string),
            severity,
            message: message.into(),
        });
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for finding in &self.findings {
            let level = match finding.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
            };
            let scope = finding.repo.as_deref().unwrap_or("config");
            let _ = writeln!(out, "{level}: [{scope}] {}", finding.message);
        }
        let _ = writeln!(
            out,
            "{} error(s), {} warning(s)",
            self.error_count(),
            self.warning_count()
        );
        out
    }
}

/// Why a `.nanna/protected.toml` entry would be rejected by nanna's loader.
///
/// Entries are normalised relative literal paths, optionally ending in `/**`.
///
/// ```
/// use inchworm::validate::protected_entry_problem;
///
/// assert!(protected_entry_problem(".inchworm/**").is_none());
/// assert!(protected_entry_problem("scripts/deploy.sh").is_none());
/// assert!(protected_entry_problem("/etc/passwd").is_some());
/// assert!(protected_entry_problem("a/../b").is_some());
/// assert!(protected_entry_problem("a/*/b").is_some());
/// ```
pub fn protected_entry_problem(entry: &str) -> Option<&'static str> {
    let literal = entry.strip_suffix("/**").unwrap_or(entry);
    if literal.is_empty() {
        return Some("is empty");
    }
    if literal.chars().any(|c| c.is_control() || c == '\\') {
        return Some("contains a control character or backslash");
    }
    if literal.contains('*') || literal.contains('?') || literal.contains('[') {
        return Some("contains a wildcard other than a trailing /**");
    }
    if literal.starts_with('/') {
        return Some("is not a relative path");
    }
    for part in literal.split('/') {
        match part {
            "" => return Some("is not normalised (empty path segment)"),
            "." => return Some("is not normalised (`.` segment)"),
            ".." => return Some("contains a `..` segment"),
            _ => {}
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_file())
        .unwrap_or(false)
}

/// Finds `bin`, searching `path_var` when it is a bare command name.
///
/// ```
/// use std::ffi::OsStr;
/// use std::path::Path;
/// use inchworm::validate::resolve_command;
///
/// assert!(resolve_command(Path::new("definitely-not-a-command"), Some(OsStr::new("/nonexistent"))).is_err());
/// ```
pub fn resolve_command(bin: &Path, path_var: Option<&OsStr>) -> Result<PathBuf, String> {
    let bare = {
        let mut components = bin.components();
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
    };
    if !bare {
        return if is_executable(bin) {
            Ok(bin.to_path_buf())
        } else {
            Err(format!(
                "nanna_bin {} is not an executable file",
                bin.display()
            ))
        };
    }
    let found = path_var
        .into_iter()
        .flat_map(std::env::split_paths)
        .find_map(|dir| {
            let candidate = dir.join(bin);
            is_executable(&candidate).then_some(candidate)
        });
    found.ok_or_else(|| format!("nanna_bin `{}` was not found on PATH", bin.display()))
}

/// ```
/// use inchworm::config::GlobalConfig;
/// use inchworm::validate::validate;
///
/// let config = GlobalConfig::parse(r#"
/// nanna_bin = "/bin/sh"
/// [[repo]]
/// slug = "o/r"
/// path = "/definitely/not/here"
/// "#).unwrap();
/// let report = validate(&config, None);
/// assert!(!report.is_ok());
/// assert!(report.render().contains("[o/r]"));
/// ```
pub fn validate(config: &GlobalConfig, path_var: Option<&OsStr>) -> Report {
    let mut report = Report::default();
    if let Err(message) = resolve_command(&config.nanna_bin, path_var) {
        report.push(None, Severity::Error, message);
    }
    for repo in &config.repo {
        validate_repo(repo, &mut report);
    }
    report
}

fn is_git_repository(root: &Path) -> Result<(), &'static str> {
    let git = root.join(".git");
    let meta = std::fs::symlink_metadata(&git).map_err(|_| "path is not a git repository")?;
    if meta.is_dir() {
        return Ok(());
    }
    let text = read_regular_text(&git, MAX_GITFILE_BYTES)
        .map_err(|_| ".git is not a directory or a readable gitdir file")?;
    let target = text
        .trim()
        .strip_prefix("gitdir:")
        .map(str::trim)
        .ok_or(".git file does not start with `gitdir:`")?;
    if root.join(target).is_dir() {
        Ok(())
    } else {
        Err(".git file points at a gitdir that does not exist")
    }
}

fn validate_repo(repo: &RepoEntry, report: &mut Report) {
    let slug = Some(repo.slug.as_str());
    if !repo.path.is_dir() {
        report.push(
            slug,
            Severity::Error,
            format!("path {} is not a directory", repo.path.display()),
        );
        return;
    }
    if let Err(message) = is_git_repository(&repo.path) {
        report.push(slug, Severity::Error, message);
    }
    check_flows(repo, report);
    check_protected(repo, report);
    if !repo.path.join(".nanna/prompt.md").is_file() {
        report.push(
            slug,
            Severity::Warning,
            "missing .nanna/prompt.md (no project prompt for agents)",
        );
    }
}

fn rubric_problem(root: &Path, rubric: &Path) -> Option<String> {
    if let Some(problem) = rubric_path_problem(rubric) {
        return Some(problem.to_string());
    }
    let full = root.join(rubric);
    let meta = match std::fs::symlink_metadata(&full) {
        Ok(meta) => meta,
        Err(_) => return Some(format!("rubric {} does not exist", rubric.display())),
    };
    if meta.file_type().is_symlink() {
        return Some(format!("rubric {} is a symlink", rubric.display()));
    }
    if !meta.is_file() {
        return Some(format!("rubric {} is not a regular file", rubric.display()));
    }
    let inside = match (full.canonicalize(), root.join(RUBRIC_DIR).canonicalize()) {
        (Ok(file), Ok(dir)) => file.starts_with(dir),
        _ => false,
    };
    if !inside {
        return Some(format!(
            "rubric {} resolves outside {RUBRIC_DIR}/",
            rubric.display()
        ));
    }
    None
}

fn check_flows(repo: &RepoEntry, report: &mut Report) {
    let slug = Some(repo.slug.as_str());
    let path = repo.path.join(".inchworm/flows.toml");
    let text = match read_regular_text(&path, MAX_FLOWS_BYTES) {
        Ok(text) => text,
        Err(err) => {
            report.push(
                slug,
                Severity::Error,
                format!("cannot read .inchworm/flows.toml: {err}"),
            );
            return;
        }
    };
    let file = match FlowFile::parse(&text) {
        Ok(file) => file,
        Err(err) => {
            report.push(
                slug,
                Severity::Error,
                format!(".inchworm/flows.toml: {err}"),
            );
            return;
        }
    };
    for flow in &file.flow {
        for judge in &flow.judge {
            if let Judge::Llm { rubric, .. } = judge
                && let Some(problem) = rubric_problem(&repo.path, rubric)
            {
                report.push(
                    slug,
                    Severity::Error,
                    format!("flow `{}`: {problem}", flow.name),
                );
            }
        }
        if flow.outcome.mode == OutcomeMode::MergeWhen {
            report.push(
                slug,
                Severity::Warning,
                format!(
                    "flow `{}` enables merge_when; merging is off by default and is intended to become a nanna capability (nanna-coder #1043)",
                    flow.name
                ),
            );
        }
    }
}

fn check_protected(repo: &RepoEntry, report: &mut Report) {
    let slug = Some(repo.slug.as_str());
    let path = repo.path.join(".nanna/protected.toml");
    let text = match read_regular_text(&path, MAX_PROTECTED_BYTES) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            report.push(
                slug,
                Severity::Error,
                format!(
                    "missing .nanna/protected.toml; {PROTECTED_GLOB} must be protected from agents"
                ),
            );
            return;
        }
        Err(err) => {
            report.push(
                slug,
                Severity::Error,
                format!("cannot read .nanna/protected.toml: {err}"),
            );
            return;
        }
    };
    let table: toml::Table = match text.parse() {
        Ok(table) => table,
        Err(err) => {
            report.push(
                slug,
                Severity::Error,
                format!(".nanna/protected.toml: {err}"),
            );
            return;
        }
    };
    let mut problems = Vec::new();
    for key in table.keys().filter(|key| key.as_str() != "extra") {
        problems.push(format!("unknown key `{key}`"));
    }
    let entries = match table.get("extra") {
        None => Vec::new(),
        Some(toml::Value::Array(items)) => items.clone(),
        Some(_) => {
            problems.push("`extra` must be an array of strings".to_string());
            Vec::new()
        }
    };
    if entries.len() > MAX_PROTECTED_ENTRIES {
        problems.push(format!("more than {MAX_PROTECTED_ENTRIES} entries"));
    }
    let mut covered = false;
    for entry in &entries {
        match entry.as_str() {
            Some(text) => {
                if let Some(problem) = protected_entry_problem(text) {
                    problems.push(format!("entry `{text}` {problem}"));
                }
                covered |= text == PROTECTED_GLOB;
            }
            None => problems.push("an `extra` entry is not a string".to_string()),
        }
    }
    if !covered {
        problems.push(format!("`extra` must contain \"{PROTECTED_GLOB}\""));
    }
    for problem in problems {
        report.push(
            slug,
            Severity::Error,
            format!(".nanna/protected.toml: {problem}"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    const FLOWS: &str = r#"
[[flow]]
name = "issue"
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
"#;

    fn write(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[cfg(not(unix))]
    fn make_executable(_path: &Path) {}

    fn good_repo(root: &Path) {
        std::fs::create_dir_all(root.join(".git")).unwrap();
        write(root, ".inchworm/flows.toml", FLOWS);
        write(root, ".inchworm/rubric.md", "rubric");
        write(
            root,
            ".nanna/protected.toml",
            "extra = [\".inchworm/**\", \"x\"]\n",
        );
        write(root, ".nanna/prompt.md", "prompt");
    }

    fn executable_in(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        make_executable(&path);
        path
    }

    fn config_for(root: &Path, bin: &Path) -> GlobalConfig {
        GlobalConfig {
            nanna_bin: bin.to_path_buf(),
            ollama_url: "http://localhost:11434".into(),
            concurrency: 1,
            poll_secs: 120,
            state_db: None,
            repo: vec![RepoEntry {
                slug: "o/r".into(),
                path: root.to_path_buf(),
            }],
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        bin: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        good_repo(&root);
        let bin = executable_in(dir.path(), "nanna");
        Fixture {
            _dir: dir,
            root,
            bin,
        }
    }

    fn run(f: &Fixture) -> Report {
        validate(&config_for(&f.root, &f.bin), None)
    }

    fn messages(report: &Report) -> Vec<String> {
        report.findings.iter().map(|f| f.message.clone()).collect()
    }

    fn has(report: &Report, needle: &str) -> bool {
        messages(report).iter().any(|m| m.contains(needle))
    }

    #[test]
    fn a_complete_repo_has_no_findings() {
        let f = fixture();
        let report = run(&f);
        assert!(report.findings.is_empty(), "{:?}", report.findings);
        assert!(report.is_ok());
        assert!(report.render().contains("0 error(s), 0 warning(s)"));
    }

    #[test]
    fn a_missing_directory_is_one_error_and_stops_further_checks() {
        let f = fixture();
        let report = validate(&config_for(&f.root.join("gone"), &f.bin), None);
        assert_eq!(report.error_count(), 1);
        assert!(messages(&report)[0].contains("not a directory"));
    }

    #[test]
    fn git_repositories_are_recognised_and_dangling_gitdir_files_are_not() {
        let f = fixture();
        std::fs::remove_dir_all(f.root.join(".git")).unwrap();
        assert!(has(&run(&f), "not a git repository"));

        write(&f.root, ".git", "gitdir: /elsewhere/missing\n");
        assert!(has(&run(&f), "does not exist"));

        write(&f.root, ".git", "not a gitdir file\n");
        assert!(has(&run(&f), "does not start with `gitdir:`"));

        let real = f.root.join("real-gitdir");
        std::fs::create_dir_all(&real).unwrap();
        write(&f.root, ".git", &format!("gitdir: {}\n", real.display()));
        assert!(run(&f).is_ok());

        write(&f.root, ".git", "gitdir: real-gitdir\n");
        assert!(
            run(&f).is_ok(),
            "a relative gitdir resolves from the repo root"
        );

        std::fs::remove_file(f.root.join(".git")).unwrap();
        write(&f.root, ".git", &"x".repeat(MAX_GITFILE_BYTES as usize + 1));
        assert!(has(&run(&f), "readable gitdir file"));
    }

    #[test]
    fn missing_or_broken_flows_are_errors() {
        let f = fixture();
        std::fs::remove_file(f.root.join(".inchworm/flows.toml")).unwrap();
        assert!(has(&run(&f), "cannot read .inchworm/flows.toml"));

        write(&f.root, ".inchworm/flows.toml", "flow = []");
        assert!(has(&run(&f), "no flows"));
    }

    #[test]
    fn oversized_flows_are_refused_without_being_parsed() {
        let f = fixture();
        write(
            &f.root,
            ".inchworm/flows.toml",
            &" ".repeat(MAX_FLOWS_BYTES as usize + 1),
        );
        let report = run(&f);
        assert!(has(&report, "larger than"), "{:?}", messages(&report));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_flows_and_protected_files_are_not_followed_and_not_echoed() {
        let f = fixture();
        let secret = f.root.join("secret.txt");
        std::fs::write(&secret, "TOP-SECRET-VALUE = [").unwrap();
        for relative in [".inchworm/flows.toml", ".nanna/protected.toml"] {
            let link = f.root.join(relative);
            std::fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(&secret, &link).unwrap();
        }
        let report = run(&f);
        assert!(has(&report, "symlink"), "{:?}", messages(&report));
        let text = report.render();
        assert!(!text.contains("TOP-SECRET"), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_in_place_of_protected_toml_does_not_hang_validate() {
        let f = fixture();
        let fifo = f.root.join(".nanna/protected.toml");
        std::fs::remove_file(&fifo).unwrap();
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if !matches!(made, Ok(status) if status.success()) {
            return;
        }
        let config = config_for(&f.root, &f.bin);
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(validate(&config, None));
        });
        let report = receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("validate hung on a FIFO");
        assert!(
            has(&report, "not a regular file"),
            "{:?}",
            messages(&report)
        );
    }

    #[test]
    fn rubrics_must_exist_inside_the_inchworm_directory() {
        let f = fixture();
        std::fs::remove_file(f.root.join(".inchworm/rubric.md")).unwrap();
        assert!(has(&run(&f), "does not exist"));

        std::fs::create_dir_all(f.root.join(".inchworm/rubric.md")).unwrap();
        assert!(has(&run(&f), "not a regular file"));
    }

    #[test]
    fn rubric_paths_outside_the_directory_fail_through_the_flow_rules() {
        let f = fixture();
        for (rubric, expected) in [
            ("/etc/passwd", "relative path"),
            ("../outside.md", "under .inchworm/"),
            (".inchworm/../outside.md", "`..`"),
        ] {
            write(
                &f.root,
                ".inchworm/flows.toml",
                &FLOWS.replace(".inchworm/rubric.md", rubric),
            );
            let report = run(&f);
            assert!(has(&report, expected), "{rubric}: {:?}", messages(&report));
            assert!(!report.is_ok(), "{rubric}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_rubrics_and_directories_that_escape_are_refused() {
        let f = fixture();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("passwd"), "root").unwrap();

        std::fs::remove_file(f.root.join(".inchworm/rubric.md")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("passwd"),
            f.root.join(".inchworm/rubric.md"),
        )
        .unwrap();
        assert!(has(&run(&f), "is a symlink"));

        std::fs::remove_file(f.root.join(".inchworm/rubric.md")).unwrap();
        std::os::unix::fs::symlink(outside.path(), f.root.join(".inchworm/judges")).unwrap();
        write(
            &f.root,
            ".inchworm/flows.toml",
            &FLOWS.replace(".inchworm/rubric.md", ".inchworm/judges/passwd"),
        );
        assert!(has(&run(&f), "resolves outside"));
    }

    #[test]
    fn inchworm_must_be_protected_from_agents_exactly_as_nanna_reads_it() {
        let f = fixture();
        let cases: [(&str, &str); 9] = [
            ("extra = [\"other/**\"]\n", "must contain"),
            ("nothing = 1\n", "unknown key `nothing`"),
            (
                "extra = [\".inchworm/**\", \"x\"]\nbogus = 1\n",
                "unknown key `bogus`",
            ),
            ("extra = [\".inchworm/**\", 5]\n", "is not a string"),
            ("extra = \".inchworm/**\"\n", "must be an array"),
            (
                "extra = [\".inchworm/**\", \"/abs\"]\n",
                "not a relative path",
            ),
            ("extra = [\".inchworm/**\", \"a/../b\"]\n", "`..`"),
            ("extra = [\".inchworm/**\", \"\"]\n", "is empty"),
            ("extra = [", "protected.toml"),
        ];
        for (text, expected) in cases {
            write(&f.root, ".nanna/protected.toml", text);
            let report = run(&f);
            assert!(has(&report, expected), "{text:?}: {:?}", messages(&report));
            assert!(!report.is_ok(), "{text:?}");
        }
        let entries = |count: usize| {
            let mut all = vec!["\".inchworm/**\"".to_string()];
            all.extend((1..count).map(|n| format!("\"d{n}\"")));
            format!("extra = [{}]\n", all.join(","))
        };
        write(
            &f.root,
            ".nanna/protected.toml",
            &entries(MAX_PROTECTED_ENTRIES),
        );
        assert!(
            run(&f).is_ok(),
            "exactly {MAX_PROTECTED_ENTRIES} entries are allowed"
        );
        write(
            &f.root,
            ".nanna/protected.toml",
            &entries(MAX_PROTECTED_ENTRIES + 1),
        );
        let over = run(&f);
        assert!(has(&over, "more than 256"), "{:?}", messages(&over));

        write(
            &f.root,
            ".nanna/protected.toml",
            &format!(
                "extra = [\"{}\"]\n",
                "x".repeat(MAX_PROTECTED_BYTES as usize)
            ),
        );
        assert!(has(&run(&f), "larger than"));

        std::fs::remove_file(f.root.join(".nanna/protected.toml")).unwrap();
        assert!(has(&run(&f), "missing .nanna/protected.toml"));
    }

    #[test]
    fn protected_entries_follow_the_loaders_rules() {
        for good in [
            ".inchworm/**",
            "a",
            "a/b",
            "a/b/**",
            "scripts/deploy.sh",
            ".github/**",
        ] {
            assert_eq!(protected_entry_problem(good), None, "{good}");
        }
        let bad = [
            ("", "is empty"),
            ("/**", "is empty"),
            ("/a", "not a relative path"),
            ("a//b", "empty path segment"),
            ("a/", "empty path segment"),
            ("./a", "`.` segment"),
            ("a/./b", "`.` segment"),
            ("a/../b", "`..`"),
            ("a\\b", "backslash"),
            ("a\nb", "control"),
            ("a*b", "wildcard"),
            ("a?b", "wildcard"),
            ("a[b", "wildcard"),
            ("a/**/b", "wildcard"),
        ];
        for (entry, expected) in bad {
            let got = protected_entry_problem(entry);
            assert!(
                got.is_some_and(|m| m.contains(expected)),
                "{entry:?}: {got:?}"
            );
        }
    }

    #[test]
    fn a_missing_prompt_is_only_a_warning() {
        let f = fixture();
        std::fs::remove_file(f.root.join(".nanna/prompt.md")).unwrap();
        let report = run(&f);
        assert!(report.is_ok());
        assert_eq!(report.warning_count(), 1);
        assert!(report.render().starts_with("warning: [o/r]"));
    }

    #[test]
    fn enabling_merge_when_is_a_warning_that_points_at_the_capability_issue() {
        let f = fixture();
        let merging = FLOWS.replace(
            "mode = \"draft_pr\"",
            "mode = \"merge_when\"\nrequired_checks = [\"Tests\"]",
        );
        write(&f.root, ".inchworm/flows.toml", &merging);
        let report = run(&f);
        assert!(report.is_ok(), "{:?}", messages(&report));
        assert_eq!(report.warning_count(), 1);
        assert!(has(&report, "merge_when"));
        assert!(has(&report, "#1043"));
    }

    #[test]
    fn nanna_bin_is_resolved_on_path_when_it_is_a_bare_name() {
        let f = fixture();
        let bin_dir = f.root.parent().unwrap().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        executable_in(&bin_dir, "mynanna");
        let config = config_for(&f.root, Path::new("mynanna"));
        let path = std::env::join_paths([Path::new("/nonexistent"), &bin_dir]).unwrap();

        assert!(validate(&config, Some(&path)).is_ok());

        let missing = validate(&config, Some(OsStr::new("/nonexistent")));
        assert!(has(&missing, "was not found on PATH"));
        let unset = validate(&config, None);
        assert!(has(&unset, "was not found on PATH"));
        assert!(unset.findings.iter().any(|f| f.repo.is_none()));
    }

    #[test]
    fn nanna_bin_paths_must_be_executable_files() {
        let f = fixture();
        let dir = f.root.parent().unwrap().to_path_buf();
        let mut config = config_for(&f.root, &dir);
        assert!(has(&validate(&config, None), "not an executable file"));

        config.nanna_bin = dir.join("no-such-nanna");
        let report = validate(&config, None);
        assert!(has(&report, "nanna_bin"));
        assert!(report.findings.iter().any(|f| f.repo.is_none()));

        let plain = dir.join("plain");
        std::fs::write(&plain, "").unwrap();
        #[cfg(unix)]
        {
            config.nanna_bin = plain;
            assert!(has(&validate(&config, None), "not an executable file"));
        }

        config.nanna_bin = f.bin.clone();
        assert!(validate(&config, None).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_on_path_named_like_the_command_is_not_a_command() {
        let f = fixture();
        let bin_dir = f.root.parent().unwrap().join("bin2");
        std::fs::create_dir_all(bin_dir.join("mynanna")).unwrap();
        let config = config_for(&f.root, Path::new("mynanna"));
        let path = OsString::from(bin_dir);
        assert!(has(
            &validate(&config, Some(&path)),
            "was not found on PATH"
        ));
    }

    #[test]
    fn every_repo_is_reported_separately() {
        let f = fixture();
        let mut config = config_for(&f.root, &f.bin);
        config.repo.push(RepoEntry {
            slug: "o/broken".into(),
            path: f.root.join("gone"),
        });
        let report = validate(&config, None);
        assert_eq!(report.error_count(), 1);
        assert_eq!(report.findings[0].repo.as_deref(), Some("o/broken"));
    }
}
