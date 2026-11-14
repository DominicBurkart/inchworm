use std::fmt::Write as _;
use std::path::Path;

use crate::config::{FlowFile, GlobalConfig, Judge, RepoEntry};

pub const PROTECTED_GLOB: &str = ".inchworm/**";

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

    pub(crate) fn push(
        &mut self,
        repo: Option<&str>,
        severity: Severity,
        message: impl Into<String>,
    ) {
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

/// ```
/// use inchworm::config::GlobalConfig;
/// use inchworm::validate::validate;
///
/// let config = GlobalConfig::parse(r#"
/// nanna_bin = "nanna"
/// [[repo]]
/// slug = "o/r"
/// path = "/definitely/not/here"
/// "#).unwrap();
/// let report = validate(&config);
/// assert!(!report.is_ok());
/// assert!(report.render().contains("[o/r]"));
/// ```
pub fn validate(config: &GlobalConfig) -> Report {
    let mut report = Report::default();
    if config.nanna_bin.components().count() > 1 && !config.nanna_bin.is_file() {
        report.push(
            None,
            Severity::Error,
            format!("nanna_bin {} does not exist", config.nanna_bin.display()),
        );
    }
    for repo in &config.repo {
        validate_repo(repo, &mut report);
    }
    report
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
    if !repo.path.join(".git").exists() {
        report.push(slug, Severity::Error, "path is not a git repository");
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

fn check_flows(repo: &RepoEntry, report: &mut Report) {
    let slug = Some(repo.slug.as_str());
    let path = repo.path.join(".inchworm/flows.toml");
    let text = match std::fs::read_to_string(&path) {
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
    let mut declared: Option<Result<Vec<String>, String>> = None;
    for flow in &file.flow {
        for judge in &flow.judge {
            if let Judge::Verifier { checks, .. } = judge {
                let declared = declared.get_or_insert_with(|| declared_checks(&repo.path));
                check_verifier_names(slug, &flow.name, checks, declared, report);
            }
            if let Judge::Llm { rubric, .. } = judge
                && !repo.path.join(rubric).is_file()
            {
                report.push(
                    slug,
                    Severity::Error,
                    format!(
                        "flow `{}`: rubric {} does not exist",
                        flow.name,
                        rubric.display()
                    ),
                );
            }
        }
    }
}

const MAX_VERIFY_FILE_BYTES: u64 = 64 * 1024;

fn declared_checks(repo: &Path) -> Result<Vec<String>, String> {
    use std::io::Read;
    let file = std::fs::File::open(repo.join(".nanna/verify.toml"))
        .map_err(|err| format!("cannot read .nanna/verify.toml: {err}"))?;
    let mut text = String::new();
    file.take(MAX_VERIFY_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|err| format!("cannot read .nanna/verify.toml: {err}"))?;
    if text.len() as u64 > MAX_VERIFY_FILE_BYTES {
        return Err(format!(
            ".nanna/verify.toml is larger than {MAX_VERIFY_FILE_BYTES} bytes"
        ));
    }
    let table: toml::Table = text
        .parse()
        .map_err(|err| format!(".nanna/verify.toml: {err}"))?;
    Ok(table
        .get("check")
        .and_then(toml::Value::as_array)
        .map(|checks| {
            checks
                .iter()
                .filter_map(|check| check.get("name").and_then(toml::Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

fn check_verifier_names(
    slug: Option<&str>,
    flow: &str,
    checks: &[String],
    declared: &Result<Vec<String>, String>,
    report: &mut Report,
) {
    match declared {
        Err(reason) => report.push(
            slug,
            Severity::Warning,
            format!("flow `{flow}`: the verifier judge cannot be checked: {reason}"),
        ),
        Ok(names) => {
            for check in checks.iter().filter(|check| !names.contains(check)) {
                report.push(
                    slug,
                    Severity::Warning,
                    format!(
                        "flow `{flow}`: verifier check `{check}` is not declared in .nanna/verify.toml"
                    ),
                );
            }
        }
    }
}

fn check_protected(repo: &RepoEntry, report: &mut Report) {
    let slug = Some(repo.slug.as_str());
    let path = repo.path.join(".nanna/protected.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => {
            report.push(
                slug,
                Severity::Error,
                format!(
                    "missing .nanna/protected.toml; {PROTECTED_GLOB} must be protected from agents"
                ),
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
    let covered = table
        .get("extra")
        .and_then(toml::Value::as_array)
        .is_some_and(|extra| extra.iter().any(|v| v.as_str() == Some(PROTECTED_GLOB)));
    if !covered {
        report.push(
            slug,
            Severity::Error,
            format!(".nanna/protected.toml extra must contain \"{PROTECTED_GLOB}\""),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

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

    fn config_for(root: &Path) -> GlobalConfig {
        GlobalConfig {
            nanna_bin: PathBuf::from("nanna"),
            ollama_url: "http://localhost:11434".into(),
            concurrency: 1,
            poll_secs: 120,
            state_db: PathBuf::from("/tmp/state.db"),
            repo: vec![RepoEntry {
                slug: "o/r".into(),
                path: root.to_path_buf(),
            }],
        }
    }

    fn messages(report: &Report) -> Vec<String> {
        report.findings.iter().map(|f| f.message.clone()).collect()
    }

    #[test]
    fn a_complete_repo_has_no_findings() {
        let dir = tempfile::tempdir().unwrap();
        good_repo(dir.path());
        let report = validate(&config_for(dir.path()));
        assert!(report.findings.is_empty(), "{:?}", report.findings);
        assert!(report.is_ok());
        assert!(report.render().contains("0 error(s), 0 warning(s)"));
    }

    #[test]
    fn a_missing_directory_is_one_error_and_stops_further_checks() {
        let dir = tempfile::tempdir().unwrap();
        let report = validate(&config_for(&dir.path().join("gone")));
        assert_eq!(report.error_count(), 1);
        assert!(messages(&report)[0].contains("not a directory"));
    }

    #[test]
    fn a_non_git_directory_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        good_repo(dir.path());
        std::fs::remove_dir_all(dir.path().join(".git")).unwrap();
        let report = validate(&config_for(dir.path()));
        assert!(
            messages(&report)
                .iter()
                .any(|m| m.contains("not a git repository"))
        );
    }

    #[test]
    fn a_worktree_style_git_file_counts_as_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        good_repo(dir.path());
        std::fs::remove_dir_all(dir.path().join(".git")).unwrap();
        write(dir.path(), ".git", "gitdir: /elsewhere\n");
        assert!(validate(&config_for(dir.path())).is_ok());
    }

    #[test]
    fn missing_or_broken_flows_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        good_repo(dir.path());
        std::fs::remove_file(dir.path().join(".inchworm/flows.toml")).unwrap();
        let report = validate(&config_for(dir.path()));
        assert!(
            messages(&report)
                .iter()
                .any(|m| m.contains("cannot read .inchworm/flows.toml"))
        );

        write(dir.path(), ".inchworm/flows.toml", "flow = []");
        let report = validate(&config_for(dir.path()));
        assert!(messages(&report).iter().any(|m| m.contains("no flows")));
    }

    #[test]
    fn a_malformed_forbid_paths_glob_is_an_error_before_any_agent_runs() {
        let dir = tempfile::tempdir().unwrap();
        good_repo(dir.path());
        let broken = FLOWS.replace(
            "[flow.outcome]",
            "[[flow.judge]]\nkind = \"diff\"\nmax_lines = 10\nforbid_paths = [\"[\"]\n\n[flow.outcome]",
        );
        assert_ne!(broken, FLOWS);
        write(dir.path(), ".inchworm/flows.toml", &broken);
        let report = validate(&config_for(dir.path()));
        assert!(!report.is_ok());
        assert!(
            messages(&report)
                .iter()
                .any(|m| m.contains("forbid_paths") && m.contains("[")),
            "{:?}",
            messages(&report)
        );
    }

    #[test]
    fn a_missing_rubric_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        good_repo(dir.path());
        std::fs::remove_file(dir.path().join(".inchworm/rubric.md")).unwrap();
        let report = validate(&config_for(dir.path()));
        assert!(messages(&report).iter().any(|m| m.contains("rubric")));
        assert!(!report.is_ok());
    }

    #[test]
    fn inchworm_must_be_protected_from_agents() {
        let dir = tempfile::tempdir().unwrap();
        good_repo(dir.path());

        write(
            dir.path(),
            ".nanna/protected.toml",
            "extra = [\"other/**\"]\n",
        );
        assert!(
            messages(&validate(&config_for(dir.path())))
                .iter()
                .any(|m| m.contains("must contain"))
        );

        write(dir.path(), ".nanna/protected.toml", "nothing = 1\n");
        assert!(!validate(&config_for(dir.path())).is_ok());

        write(dir.path(), ".nanna/protected.toml", "extra = [");
        assert!(
            messages(&validate(&config_for(dir.path())))
                .iter()
                .any(|m| m.contains("protected.toml"))
        );

        std::fs::remove_file(dir.path().join(".nanna/protected.toml")).unwrap();
        assert!(
            messages(&validate(&config_for(dir.path())))
                .iter()
                .any(|m| m.contains("missing .nanna/protected.toml"))
        );
    }

    fn verifier_flows(checks: &str) -> String {
        FLOWS.replace(
            "[flow.outcome]",
            &format!("[[flow.judge]]\nkind = \"verifier\"\nchecks = [{checks}]\n\n[flow.outcome]"),
        )
    }

    #[test]
    fn verifier_checks_must_be_declared_in_the_repository_but_only_warn() {
        let dir = tempfile::tempdir().unwrap();
        good_repo(dir.path());
        write(
            dir.path(),
            ".inchworm/flows.toml",
            &verifier_flows("\"fmt\", \"coverage\""),
        );
        write(
            dir.path(),
            ".nanna/verify.toml",
            "[[check]]\nname = \"fmt\"\ncommand = \"cargo fmt --check\"\n",
        );
        let report = validate(&config_for(dir.path()));
        assert!(report.is_ok());
        assert_eq!(report.warning_count(), 1);
        assert!(messages(&report)[0].contains("`coverage` is not declared"));

        write(
            dir.path(),
            ".nanna/verify.toml",
            "[[check]]\nname = \"fmt\"\ncommand = \"a\"\n[[check]]\nname = \"coverage\"\ncommand = \"b\"\n",
        );
        assert_eq!(validate(&config_for(dir.path())).warning_count(), 0);
    }

    #[test]
    fn a_missing_oversized_or_malformed_verify_file_is_a_warning_not_an_execution() {
        let dir = tempfile::tempdir().unwrap();
        good_repo(dir.path());
        write(
            dir.path(),
            ".inchworm/flows.toml",
            &verifier_flows("\"fmt\""),
        );
        let report = validate(&config_for(dir.path()));
        assert!(report.is_ok());
        assert!(messages(&report)[0].contains("cannot read .nanna/verify.toml"));

        write(dir.path(), ".nanna/verify.toml", &"#".repeat(70_000));
        assert!(messages(&validate(&config_for(dir.path())))[0].contains("larger than"));

        write(dir.path(), ".nanna/verify.toml", "[[check");
        let report = validate(&config_for(dir.path()));
        assert!(report.is_ok());
        assert!(messages(&report)[0].contains(".nanna/verify.toml:"));

        write(dir.path(), ".nanna/verify.toml", "check = 3\n");
        assert!(messages(&validate(&config_for(dir.path())))[0].contains("`fmt` is not declared"));
    }

    #[test]
    fn invalid_verifier_judge_configuration_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        good_repo(dir.path());
        write(
            dir.path(),
            ".inchworm/flows.toml",
            &verifier_flows("\"a b\""),
        );
        let report = validate(&config_for(dir.path()));
        assert!(!report.is_ok());
        assert!(messages(&report).iter().any(|m| m.contains("must be 1 to")));
    }

    #[test]
    fn a_missing_prompt_is_only_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        good_repo(dir.path());
        std::fs::remove_file(dir.path().join(".nanna/prompt.md")).unwrap();
        let report = validate(&config_for(dir.path()));
        assert!(report.is_ok());
        assert_eq!(report.warning_count(), 1);
        assert!(report.render().starts_with("warning: [o/r]"));
    }

    #[test]
    fn nanna_bin_is_checked_only_when_it_is_a_path() {
        let dir = tempfile::tempdir().unwrap();
        good_repo(dir.path());
        let mut config = config_for(dir.path());
        config.nanna_bin = dir.path().join("no-such-nanna");
        let report = validate(&config);
        assert!(messages(&report).iter().any(|m| m.contains("nanna_bin")));
        assert!(report.findings.iter().any(|f| f.repo.is_none()));

        let binary = dir.path().join("nanna");
        std::fs::write(&binary, "").unwrap();
        config.nanna_bin = binary;
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn every_repo_is_reported_separately() {
        let good = tempfile::tempdir().unwrap();
        good_repo(good.path());
        let mut config = config_for(good.path());
        config.repo.push(RepoEntry {
            slug: "o/broken".into(),
            path: good.path().join("gone"),
        });
        let report = validate(&config);
        assert_eq!(report.error_count(), 1);
        assert_eq!(report.findings[0].repo.as_deref(), Some("o/broken"));
    }
}
