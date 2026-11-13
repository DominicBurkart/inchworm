use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};

use crate::config::{
    GlobalConfig, Judge, LEGACY_FLOWS_FILE, Offramp, OutcomeMode, PROJECT_FILE, ProjectConfig,
    ProjectError, RUBRIC_DIR, RepoEntry,
};
use crate::fsutil::{read_regular_text, read_text_following_symlinks};

pub const PROTECTED_GLOB: &str = ".inchworm/**";
pub const PROTECTED_GLOBS: [&str; 2] = [PROJECT_FILE, PROTECTED_GLOB];
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
/// This mirrors `validate_extra` in nanna-coder `harness/src/protected.rs` at
/// commit `239a1c94`, rule for rule and in the same order, and is exactly as
/// strict: it does not reject other control characters (a tab or DEL passes in
/// nanna too). One trailing `/**` is stripped first. The rules are: not empty;
/// relative; not starting with `!`; none of `* ? [ ] { } \\`, newline, carriage
/// return or NUL; no empty, `.` or `..` path segment; no segment ending in a dot
/// or a space or containing a colon.
///
/// ```
/// use inchworm::validate::protected_entry_problem;
///
/// assert!(protected_entry_problem(".inchworm/**").is_none());
/// assert!(protected_entry_problem("scripts/deploy.sh").is_none());
/// assert!(protected_entry_problem("/etc/passwd").is_some());
/// assert!(protected_entry_problem("a/../b").is_some());
/// assert!(protected_entry_problem("a]b").is_some());
/// assert!(protected_entry_problem("dir./f").is_some());
/// ```
pub fn protected_entry_problem(entry: &str) -> Option<&'static str> {
    let literal = entry.strip_suffix("/**").unwrap_or(entry);
    if literal.is_empty() {
        return Some("is empty");
    }
    if literal.starts_with('/') {
        return Some("must be relative");
    }
    if literal.starts_with('!') {
        return Some("must not be negated (leading `!`)");
    }
    if literal.contains(['*', '?', '[', ']', '{', '}', '\\', '\n', '\r', '\0']) {
        return Some(
            "must be a literal path, optionally ending in `/**` (no `* ? [ ] { } \\`, line break or NUL)",
        );
    }
    if literal
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Some(
            "must be normalised and stay inside the repository (no empty, `.` or `..` segment)",
        );
    }
    if literal
        .split('/')
        .any(|part| part.ends_with(['.', ' ']) || part.contains(':'))
    {
        return Some(
            "has a segment ending in a dot or a space, or containing a colon (Windows reads those as another spelling of the same name)",
        );
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

/// The first of `paths` that a step output must not touch: `inchworm.toml` or
/// anything under `.inchworm/`, however it is spelled.
///
/// This is independent of `.nanna/protected.toml`: it is the check inchworm
/// applies to a patch itself. Paths are normalised lexically, compared without
/// regard to ASCII case (case-insensitive file systems), with trailing dots and
/// spaces and `:stream` suffixes removed from each component (other file
/// systems alias those names). A path that climbs out of the repository, is
/// absolute (a leading separator or a drive letter) or contains NUL counts as
/// touching it. Splitting a diff into paths (rename and copy headers, quoted
/// names) is the caller's job.
///
/// ```
/// use inchworm::validate::protected_path_touched;
///
/// assert_eq!(protected_path_touched(["src/lib.rs", "inchworm.toml"]), Some("inchworm.toml"));
/// assert_eq!(protected_path_touched(["./.inchworm/rubric.md"]), Some("./.inchworm/rubric.md"));
/// assert_eq!(protected_path_touched(["src/lib.rs", "docs/inchworm.toml"]), None);
/// ```
pub fn protected_path_touched<'a>(paths: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    paths.into_iter().find(|path| path_is_protected(path))
}

fn component(part: &str) -> &str {
    let stream = part.split(':').next().unwrap_or_default();
    let spaced = stream.trim_end_matches(' ');
    if spaced == ".." || spaced == "." {
        spaced
    } else {
        spaced.trim_end_matches(['.', ' '])
    }
}

fn path_is_protected(path: &str) -> bool {
    if path.contains('\0') || path.starts_with(['/', '\\']) {
        return true;
    }
    let mut chars = path.chars();
    if let (Some(drive), Some(':')) = (chars.next(), chars.next())
        && drive.is_ascii_alphabetic()
    {
        return true;
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split(['/', '\\']) {
        match component(part) {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return true;
                }
            }
            other => parts.push(other),
        }
    }
    match parts.as_slice() {
        [first, ..] if first.eq_ignore_ascii_case(RUBRIC_DIR) => true,
        [only] => only.eq_ignore_ascii_case(PROJECT_FILE),
        _ => false,
    }
}

fn is_git_repository(root: &Path) -> Result<(), &'static str> {
    let git = root.join(".git");
    let meta = std::fs::metadata(&git).map_err(|_| "path is not a git repository")?;
    if meta.is_dir() {
        return Ok(());
    }
    let text = read_text_following_symlinks(&git, MAX_GITFILE_BYTES)
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
    if let Some(message) = symlink_message(&repo.path, RUBRIC_DIR) {
        report.push(slug, Severity::Error, message);
    }
    check_flows(repo, report);
    check_protected(repo, report);
    if let Some(message) = symlink_message(&repo.path, ".nanna/prompt.md") {
        report.push(slug, Severity::Error, message);
    } else if !repo.path.join(".nanna/prompt.md").is_file() {
        report.push(
            slug,
            Severity::Warning,
            "missing .nanna/prompt.md (no project prompt for agents)",
        );
    }
}

/// The first prefix of `relative` (resolved under `root`) that is a symlink.
///
/// The walk goes from the root down and stops at the first missing component,
/// so a symlinked directory is reported before anything below it is read.
///
/// ```
/// use std::path::Path;
/// use inchworm::validate::symlink_component;
///
/// let dir = tempfile::tempdir().unwrap();
/// assert_eq!(symlink_component(dir.path(), Path::new("a/b")), None);
/// ```
pub fn symlink_component(root: &Path, relative: &Path) -> Option<PathBuf> {
    let mut prefix = PathBuf::new();
    for component in relative.components() {
        prefix.push(component);
        match std::fs::symlink_metadata(root.join(&prefix)) {
            Ok(meta) if meta.file_type().is_symlink() => return Some(prefix),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

fn symlink_message(root: &Path, relative: &str) -> Option<String> {
    symlink_component(root, Path::new(relative))
        .map(|prefix| format!("`{}` is a symlink, which is not followed", prefix.display()))
}

fn rubric_problem(root: &Path, rubric: &Path) -> Option<String> {
    if let Some(prefix) = symlink_component(root, rubric) {
        return Some(format!(
            "rubric component `{}` is a symlink, which is not followed",
            prefix.display()
        ));
    }
    let full = root.join(rubric);
    let meta = match std::fs::symlink_metadata(&full) {
        Ok(meta) => meta,
        Err(_) => return Some(format!("rubric {} does not exist", rubric.display())),
    };
    if !meta.is_file() {
        return Some(format!("rubric {} is not a regular file", rubric.display()));
    }
    let inside = full
        .canonicalize()
        .ok()
        .zip(root.canonicalize().ok().map(|root| root.join(RUBRIC_DIR)))
        .is_some_and(|(file, dir)| file.starts_with(dir));
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
    if let Some(message) = symlink_message(&repo.path, "inchworm.toml") {
        report.push(slug, Severity::Error, message);
        return;
    }
    let path = repo.path.join("inchworm.toml");
    let text = match read_regular_text(&path, MAX_FLOWS_BYTES) {
        Ok(text) => text,
        Err(err) => {
            let moved = err.kind() == std::io::ErrorKind::NotFound
                && symlink_component(&repo.path, Path::new(LEGACY_FLOWS_FILE)).is_none()
                && repo.path.join(LEGACY_FLOWS_FILE).exists();
            let message = if moved {
                format!(
                    "{LEGACY_FLOWS_FILE} has moved to {PROJECT_FILE} at the repository root, together with the new [auditor] table; it is no longer read"
                )
            } else {
                format!("cannot read {PROJECT_FILE}: {err}")
            };
            report.push(slug, Severity::Error, message);
            return;
        }
    };
    let file = match ProjectConfig::parse(&text) {
        Ok(file) => file,
        Err(err) => {
            let message = match err {
                ProjectError::Parse(_) => err.to_string(),
                _ => format!("{PROJECT_FILE}: {err}"),
            };
            report.push(slug, Severity::Error, message);
            return;
        }
    };
    for flow in &file.flow {
        if file.offramp.for_flow(&flow.name) == Offramp::Escalate {
            report.push(
                slug,
                Severity::Warning,
                format!(
                    "flow `{}` uses the escalate offramp, the only path that may leave local models; it is not implemented yet",
                    flow.name
                ),
            );
        }
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
    if let Some(message) = symlink_message(&repo.path, ".nanna/protected.toml") {
        report.push(slug, Severity::Error, message);
        return;
    }
    let path = repo.path.join(".nanna/protected.toml");
    let text = match read_regular_text(&path, MAX_PROTECTED_BYTES) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            report.push(
                slug,
                Severity::Error,
                format!(
                    "missing .nanna/protected.toml; {PROJECT_FILE} and {PROTECTED_GLOB} must be protected from agents"
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
    let mut covered = [false; PROTECTED_GLOBS.len()];
    for entry in &entries {
        match entry.as_str() {
            Some(text) => {
                if let Some(problem) = protected_entry_problem(text) {
                    problems.push(format!("entry `{text}` {problem}"));
                }
                for (done, required) in covered.iter_mut().zip(PROTECTED_GLOBS) {
                    *done |= text == required;
                }
            }
            None => problems.push("an `extra` entry is not a string".to_string()),
        }
    }
    for (done, required) in covered.iter().zip(PROTECTED_GLOBS) {
        if !done {
            problems.push(format!("`extra` must contain \"{required}\""));
        }
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
[auditor]
runs_in = { kind = "nanna", identity = "auditor" }

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
        write(root, "inchworm.toml", FLOWS);
        write(root, ".inchworm/rubric.md", "rubric");
        write(
            root,
            ".nanna/protected.toml",
            "extra = [\"inchworm.toml\", \".inchworm/**\", \"x\"]\n",
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

    #[cfg(unix)]
    #[test]
    fn a_git_entry_that_is_a_symlink_to_a_gitdir_is_accepted() {
        let f = fixture();
        let real = f.root.parent().unwrap().join("real-gitdir");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::remove_dir_all(f.root.join(".git")).unwrap();
        std::os::unix::fs::symlink(&real, f.root.join(".git")).unwrap();
        assert!(run(&f).is_ok(), "{:?}", messages(&run(&f)));

        std::fs::remove_file(f.root.join(".git")).unwrap();
        let gitfile = f.root.parent().unwrap().join("gitfile");
        std::fs::write(&gitfile, format!("gitdir: {}\n", real.display())).unwrap();
        std::os::unix::fs::symlink(&gitfile, f.root.join(".git")).unwrap();
        assert!(run(&f).is_ok(), "{:?}", messages(&run(&f)));

        std::fs::remove_file(f.root.join(".git")).unwrap();
        std::os::unix::fs::symlink(f.root.parent().unwrap().join("gone"), f.root.join(".git"))
            .unwrap();
        assert!(!run(&f).is_ok());
    }

    fn run_with(f: &Fixture, project: &str) -> Report {
        write(&f.root, "inchworm.toml", project);
        run(f)
    }

    #[test]
    fn missing_or_broken_flows_are_errors() {
        let f = fixture();
        std::fs::remove_file(f.root.join("inchworm.toml")).unwrap();
        assert!(has(&run(&f), "cannot read inchworm.toml"));

        write(&f.root, "inchworm.toml", "flow = []");
        assert!(has(&run(&f), "no flows"));
    }

    #[test]
    fn oversized_flows_are_refused_without_being_parsed() {
        let f = fixture();
        write(
            &f.root,
            "inchworm.toml",
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
        for relative in ["inchworm.toml", ".nanna/protected.toml"] {
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
                "inchworm.toml",
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
            "inchworm.toml",
            &FLOWS.replace(".inchworm/rubric.md", ".inchworm/judges/passwd"),
        );
        assert!(has(&run(&f), "is a symlink"));
    }

    #[test]
    fn inchworm_must_be_protected_from_agents_exactly_as_nanna_reads_it() {
        let f = fixture();
        let cases: [(&str, &str); 9] = [
            ("extra = [\"other/**\"]\n", "must contain"),
            ("nothing = 1\n", "unknown key `nothing`"),
            (
                "extra = [\"inchworm.toml\", \".inchworm/**\", \"x\"]\nbogus = 1\n",
                "unknown key `bogus`",
            ),
            (
                "extra = [\"inchworm.toml\", \".inchworm/**\", 5]\n",
                "is not a string",
            ),
            ("extra = \".inchworm/**\"\n", "must be an array"),
            (
                "extra = [\"inchworm.toml\", \".inchworm/**\", \"/abs\"]\n",
                "must be relative",
            ),
            (
                "extra = [\"inchworm.toml\", \".inchworm/**\", \"a/../b\"]\n",
                "`..`",
            ),
            (
                "extra = [\"inchworm.toml\", \".inchworm/**\", \"\"]\n",
                "is empty",
            ),
            ("extra = [", "protected.toml"),
        ];
        for (text, expected) in cases {
            write(&f.root, ".nanna/protected.toml", text);
            let report = run(&f);
            assert!(has(&report, expected), "{text:?}: {:?}", messages(&report));
            assert!(!report.is_ok(), "{text:?}");
        }
        let entries = |count: usize| {
            let mut all = vec![
                "\"inchworm.toml\"".to_string(),
                "\".inchworm/**\"".to_string(),
            ];
            all.extend((2..count).map(|n| format!("\"d{n}\"")));
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
    fn each_of_nannas_entry_rules_rejects_its_own_entries() {
        let rules: [(&str, &[&str], &str); 8] = [
            ("empty", &["", "/**"], "is empty"),
            ("absolute", &["/a", "/a/**"], "must be relative"),
            ("negated", &["!a", "!a/**", "!"], "must not be negated"),
            (
                "forbidden characters",
                &[
                    "a*b", "a?b", "a[b", "a]b", "a{b", "a}b", "a\\b", "a\nb", "a\rb", "a\0b",
                    "a/**/b", "a/**/**",
                ],
                "must be a literal path",
            ),
            (
                "not normalised",
                &["a//b", "a/", "./a", "a/./b", "a/../b", "../a", "a/.."],
                "must be normalised",
            ),
            (
                "trailing dot",
                &["x.", "dir./f", "a/b./**"],
                "dot or a space",
            ),
            (
                "trailing space",
                &["x ", "a /b", "a/b /**"],
                "dot or a space",
            ),
            ("colon", &["c:d", "a/b:stream", "a:/b"], "colon"),
        ];
        for (rule, entries, expected) in rules {
            for entry in entries {
                let got = protected_entry_problem(entry);
                assert!(
                    got.is_some_and(|message| message.contains(expected)),
                    "{rule}: {entry:?}: expected `{expected}`, got {got:?}"
                );
            }
        }
    }

    #[test]
    fn entries_nanna_accepts_are_accepted_too() {
        for good in [
            ".inchworm/**",
            "a",
            "a/b",
            "a/b/**",
            "scripts/deploy.sh",
            ".github/**",
            ".hidden/**",
            "a.b/c",
            "a b/c",
            "a\tb",
            "a\u{7f}b",
            "caf\u{e9}/**",
            "dir/file.txt",
            "a/b.c/**",
        ] {
            assert_eq!(protected_entry_problem(good), None, "{good:?}");
        }
    }

    #[test]
    fn the_reasons_for_overlapping_rules_follow_nannas_order() {
        assert!(
            protected_entry_problem("/*")
                .unwrap()
                .contains("must be relative")
        );
        assert!(protected_entry_problem("!*").unwrap().contains("negated"));
        assert!(
            protected_entry_problem("a*/../")
                .unwrap()
                .contains("literal")
        );
        assert!(
            protected_entry_problem("a./../b")
                .unwrap()
                .contains("normalised")
        );
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
        let refused = run_with(&f, &merging);
        assert!(
            has(&refused, "[merge] allow is false"),
            "{:?}",
            messages(&refused)
        );
        assert!(!refused.is_ok());
        write(
            &f.root,
            "inchworm.toml",
            &format!("[merge]\nallow = true\n{merging}"),
        );
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

    #[cfg(unix)]
    fn outside_copy_of(f: &Fixture, directory: &str) -> PathBuf {
        let outside = f
            .root
            .parent()
            .unwrap()
            .join(format!("outside-{}", directory.trim_start_matches('.')));
        std::fs::create_dir_all(&outside).unwrap();
        for entry in std::fs::read_dir(f.root.join(directory)).unwrap() {
            let entry = entry.unwrap();
            std::fs::copy(entry.path(), outside.join(entry.file_name())).unwrap();
        }
        std::fs::remove_dir_all(f.root.join(directory)).unwrap();
        outside
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_inchworm_directory_is_refused_even_when_everything_inside_is_valid() {
        let f = fixture();
        let outside = outside_copy_of(&f, ".inchworm");
        std::os::unix::fs::symlink(&outside, f.root.join(".inchworm")).unwrap();
        let report = run(&f);
        assert!(!report.is_ok(), "{:?}", messages(&report));
        assert!(
            has(&report, "`.inchworm` is a symlink"),
            "{:?}",
            messages(&report)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_nanna_directory_is_refused_even_when_everything_inside_is_valid() {
        let f = fixture();
        let outside = outside_copy_of(&f, ".nanna");
        std::os::unix::fs::symlink(&outside, f.root.join(".nanna")).unwrap();
        let report = run(&f);
        assert!(!report.is_ok(), "{:?}", messages(&report));
        let hits = messages(&report)
            .iter()
            .filter(|m| m.starts_with("`.nanna` is a symlink"))
            .count();
        assert_eq!(
            hits,
            2,
            "protected.toml and prompt.md each report it: {:?}",
            messages(&report)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_prompt_is_refused_but_a_missing_one_is_only_a_warning() {
        let f = fixture();
        let real = f.root.join("prompt-real.md");
        std::fs::write(&real, "p").unwrap();
        std::fs::remove_file(f.root.join(".nanna/prompt.md")).unwrap();
        std::os::unix::fs::symlink(&real, f.root.join(".nanna/prompt.md")).unwrap();
        let report = run(&f);
        assert!(!report.is_ok(), "{:?}", messages(&report));
        assert!(
            has(&report, "`.nanna/prompt.md` is a symlink"),
            "{:?}",
            messages(&report)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_intermediate_rubric_directory_names_the_component() {
        let f = fixture();
        let outside = f.root.parent().unwrap().join("outside-judges");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("r.md"), "rubric").unwrap();
        std::os::unix::fs::symlink(&outside, f.root.join(".inchworm/judges")).unwrap();
        write(
            &f.root,
            "inchworm.toml",
            &FLOWS.replace(".inchworm/rubric.md", ".inchworm/judges/r.md"),
        );
        let report = run(&f);
        assert!(
            has(&report, "`.inchworm/judges` is a symlink"),
            "{:?}",
            messages(&report)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_repo_reached_through_a_symlinked_path_is_still_valid() {
        let f = fixture();
        let alias = f.root.parent().unwrap().join("alias-to-repo");
        std::os::unix::fs::symlink(&f.root, &alias).unwrap();
        let report = validate(&config_for(&alias, &f.bin), None);
        assert!(report.is_ok(), "{:?}", messages(&report));
    }

    #[test]
    fn components_are_checked_from_the_repo_root_down_and_stop_at_a_missing_one() {
        let f = fixture();
        assert_eq!(symlink_component(&f.root, Path::new("inchworm.toml")), None);
        assert_eq!(symlink_component(&f.root, Path::new(".nope/a/b")), None);
    }

    fn padded_to(text: &str, bytes: usize) -> String {
        assert!(text.len() <= bytes);
        format!("{text}{}", " ".repeat(bytes - text.len()))
    }

    #[test]
    fn the_protected_toml_cap_is_exactly_64_kib() {
        let f = fixture();
        let base = "extra = [\"inchworm.toml\", \".inchworm/**\"]\n";
        write(&f.root, ".nanna/protected.toml", &padded_to(base, 65_536));
        assert!(run(&f).is_ok(), "{:?}", messages(&run(&f)));
        write(&f.root, ".nanna/protected.toml", &padded_to(base, 65_537));
        let over = run(&f);
        assert!(
            has(&over, "larger than 65536 bytes"),
            "{:?}",
            messages(&over)
        );
    }

    #[test]
    fn the_flows_cap_is_exactly_256_kib() {
        let f = fixture();
        write(&f.root, "inchworm.toml", &padded_to(FLOWS, 262_144));
        assert!(run(&f).is_ok(), "{:?}", messages(&run(&f)));
        write(&f.root, "inchworm.toml", &padded_to(FLOWS, 262_145));
        let over = run(&f);
        assert!(
            has(&over, "larger than 262144 bytes"),
            "{:?}",
            messages(&over)
        );
    }

    #[test]
    fn the_gitfile_cap_is_exactly_4096_bytes() {
        let f = fixture();
        let real = f.root.join("real-gitdir");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::remove_dir_all(f.root.join(".git")).unwrap();
        let line = "gitdir: real-gitdir\n";
        write(&f.root, ".git", &padded_to(line, 4096));
        assert!(run(&f).is_ok(), "{:?}", messages(&run(&f)));
        write(&f.root, ".git", &padded_to(line, 4097));
        assert!(has(&run(&f), "readable gitdir file"));
    }

    #[test]
    fn only_the_exact_inchworm_entry_counts_as_protecting_inchworm() {
        let f = fixture();
        for lookalike in [
            ".inchworm/foo",
            ".inchworm",
            ".inchworm/",
            ".inchwormX/**",
            ".inchworm/**/x",
            "x/.inchworm/**",
            "inchworm/**",
        ] {
            write(
                &f.root,
                ".nanna/protected.toml",
                &format!("extra = [\"{lookalike}\"]\n"),
            );
            let report = run(&f);
            assert!(
                has(&report, "must contain \".inchworm/**\""),
                "{lookalike}: {:?}",
                messages(&report)
            );
        }
        write(
            &f.root,
            ".nanna/protected.toml",
            "extra = [\"x\", \"inchworm.toml\", \".inchworm/**\"]\n",
        );
        assert!(run(&f).is_ok());
    }

    #[test]
    fn inchworm_toml_and_the_inchworm_directory_must_each_be_protected() {
        let f = fixture();
        for (extra, missing, present) in [
            ("\".inchworm/**\"", "inchworm.toml", ".inchworm/**"),
            ("\"inchworm.toml\"", ".inchworm/**", "inchworm.toml"),
        ] {
            write(
                &f.root,
                ".nanna/protected.toml",
                &format!("extra = [{extra}]\n"),
            );
            let report = run(&f);
            assert!(!report.is_ok());
            assert!(
                has(&report, &format!("must contain \"{missing}\"")),
                "{:?}",
                messages(&report)
            );
            assert!(
                !has(&report, &format!("must contain \"{present}\"")),
                "{:?}",
                messages(&report)
            );
        }
    }

    #[test]
    fn a_patch_touching_inchworm_toml_or_the_inchworm_directory_is_rejected() {
        for touched in [
            "inchworm.toml",
            "./inchworm.toml",
            "src/../inchworm.toml",
            "INCHWORM.TOML",
            "inchworm.toml/",
            ".inchworm/rubric.md",
            ".inchworm",
            "./.INCHWORM/x/y",
            "a/../../outside",
            "..",
            ".inchworm\\rubric.md",
            "//inchworm.toml",
            "inchworm.toml.",
            "inchworm.toml ",
            "inchworm.toml...",
            "inchworm.toml:stream",
            "INCHWORM.TOML::$DATA",
            ".inchworm./x",
            ".inchworm /x",
            "a/../.. /x",
            ".. /inchworm.toml",
            "a\0b",
            "/repo/inchworm.toml",
            "/etc/passwd",
            "\\server\\share",
            "C:/x",
            "c:\\inchworm.toml",
            "x:y/z",
        ] {
            assert_eq!(
                protected_path_touched(["src/main.rs", touched]),
                Some(touched),
                "{touched}"
            );
        }
        for fine in [
            "src/main.rs",
            "docs/inchworm.toml",
            "inchworm.toml.bak",
            "xinchworm.toml",
            ".inchwormx/a",
            "a/.inchworm/b",
            "inchworm/inchworm.toml",
            "docs/inchworm.toml:x",
            "a/.inchworm./b",
            "",
        ] {
            assert_eq!(protected_path_touched([fine]), None, "{fine:?}");
        }
        assert_eq!(protected_path_touched([]), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_inchworm_directory_is_an_error_even_when_no_rubric_is_used() {
        let f = fixture();
        let no_rubric = FLOWS.replace("llm", "nanna_clean").replace(
            "rubric = \".inchworm/rubric.md\"\nmodel = \"gemma4:e4b\"\nmin_score = 7\n",
            "",
        );
        write(&f.root, "inchworm.toml", &no_rubric);
        let outside = outside_copy_of(&f, ".inchworm");
        std::os::unix::fs::symlink(&outside, f.root.join(".inchworm")).unwrap();
        let report = run(&f);
        assert!(!report.is_ok());
        assert!(
            has(&report, "`.inchworm` is a symlink"),
            "{:?}",
            messages(&report)
        );
        assert_eq!(report.error_count(), 1, "{:?}", messages(&report));
    }

    #[test]
    fn a_step_whose_auditor_runs_on_the_host_is_an_error_until_a_host_check_exists() {
        let f = fixture();
        let per_step = FLOWS.replace(
            "[[flow]]",
            "[[auditor.step]]\nflow = \"issue\"\nstep = \"implement\"\nruns_in = { kind = \"host\" }\n\n[[flow]]",
        );
        let report = run_with(&f, &per_step);
        assert!(!report.is_ok());
        assert!(
            has(
                &report,
                "flow `issue` step `implement`: the auditor is set to run on the host"
            ),
            "{:?}",
            messages(&report)
        );
        let as_default = FLOWS.replace(
            "kind = \"nanna\", identity = \"auditor\"",
            "kind = \"host\"",
        );
        let report = run_with(&f, &as_default);
        assert!(has(&report, "silent skip"), "{:?}", messages(&report));
    }

    #[test]
    fn the_moved_error_needs_a_missing_inchworm_toml_and_a_real_old_file() {
        let f = fixture();
        std::fs::remove_file(f.root.join("inchworm.toml")).unwrap();
        std::fs::create_dir(f.root.join("inchworm.toml")).unwrap();
        write(&f.root, ".inchworm/flows.toml", FLOWS);
        let report = run(&f);
        assert!(!has(&report, "has moved"), "{:?}", messages(&report));
        assert!(
            has(&report, "cannot read inchworm.toml"),
            "{:?}",
            messages(&report)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_old_flows_file_is_not_reported_as_moved() {
        let f = fixture();
        std::fs::remove_file(f.root.join("inchworm.toml")).unwrap();
        let target = f.root.join("elsewhere.toml");
        std::fs::write(&target, FLOWS).unwrap();
        std::os::unix::fs::symlink(&target, f.root.join(".inchworm/flows.toml")).unwrap();
        let report = run(&f);
        assert!(!has(&report, "has moved"), "{:?}", messages(&report));
        assert!(
            has(&report, "cannot read inchworm.toml"),
            "{:?}",
            messages(&report)
        );
    }

    #[test]
    fn the_old_flows_file_gets_a_moved_error_and_is_never_read() {
        let f = fixture();
        std::fs::remove_file(f.root.join("inchworm.toml")).unwrap();
        write(&f.root, ".inchworm/flows.toml", FLOWS);
        let report = run(&f);
        assert!(!report.is_ok());
        assert!(
            has(&report, ".inchworm/flows.toml has moved to inchworm.toml"),
            "{:?}",
            messages(&report)
        );
        assert!(!has(&report, "cannot read"));
        std::fs::remove_file(f.root.join(".inchworm/flows.toml")).unwrap();
        assert!(has(&run(&f), "cannot read inchworm.toml"));
    }

    #[test]
    fn a_project_without_an_auditor_or_with_a_bad_offramp_is_an_error() {
        let f = fixture();
        let no_auditor = FLOWS.replace(
            "[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\n",
            "",
        );
        let report = run_with(&f, &no_auditor);
        assert!(
            has(&report, "no [auditor] table"),
            "{:?}",
            messages(&report)
        );
        let bad = format!("[offramp]\ndefault = \"page\"\n{FLOWS}");
        assert!(!run_with(&f, &bad).is_ok());
    }

    #[test]
    fn the_escalate_offramp_is_a_warning_that_says_it_is_not_implemented() {
        let f = fixture();
        let report = run_with(
            &f,
            &format!("[offramp.flow]\nissue = \"escalate\"\n{FLOWS}"),
        );
        assert!(report.is_ok(), "{:?}", messages(&report));
        assert_eq!(report.warning_count(), 1);
        assert!(has(&report, "escalate offramp"));
        assert!(has(&report, "not implemented"));
        let quiet = run_with(&f, &format!("[offramp]\ndefault = \"issue\"\n{FLOWS}"));
        assert_eq!(quiet.warning_count(), 0, "{:?}", messages(&quiet));
    }

    #[test]
    fn problems_with_the_global_config_are_rendered_under_the_config_scope() {
        let f = fixture();
        let config = config_for(&f.root, &f.root.join("no-such-nanna"));
        let rendered = validate(&config, None).render();
        assert!(
            rendered.starts_with("error: [config] nanna_bin"),
            "{rendered}"
        );
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
