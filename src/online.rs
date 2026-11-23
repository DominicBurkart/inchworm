use std::collections::BTreeSet;
use std::path::Path;

use crate::config::{
    BlockingSpec, FlowFile, GlobalConfig, PROJECT_FILE, ProjectConfig, RepoEntry, Trigger,
};
use crate::exec::{CmdSpec, Runner, SystemRunner};
use crate::github::GitHub;
use crate::validate::{Report, Severity, validate};

pub const STATUS_LABELS: &[&str] = &["inchworm:running", "inchworm:failed", "inchworm:done"];

pub trait IdentityCatalog {
    fn identities(&self, repo: &Path) -> Result<BTreeSet<String>, String>;
}

pub trait ImageBuilder {
    fn build(&self, repo: &Path) -> Result<(), String>;
}

pub struct NannaCatalog<R: Runner = SystemRunner> {
    runner: R,
    nanna_bin: std::path::PathBuf,
}

impl NannaCatalog<SystemRunner> {
    pub fn new(nanna_bin: &Path) -> Self {
        NannaCatalog::with_runner(SystemRunner, nanna_bin)
    }
}

impl<R: Runner> NannaCatalog<R> {
    pub fn with_runner(runner: R, nanna_bin: &Path) -> Self {
        NannaCatalog {
            runner,
            nanna_bin: nanna_bin.to_path_buf(),
        }
    }
}

/// ```
/// use inchworm::online::parse_agents_table;
///
/// let table = "NAME  LOOP  MODEL  MAX_EFFECT\nrust-implementer  inner  m  workspace\nreviewer  middle  m  none\n";
/// let names = parse_agents_table(table);
/// assert!(names.contains("rust-implementer") && names.contains("reviewer"));
/// assert_eq!(names.len(), 2);
/// ```
pub fn parse_agents_table(table: &str) -> BTreeSet<String> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect()
}

impl<R: Runner> IdentityCatalog for NannaCatalog<R> {
    fn identities(&self, repo: &Path) -> Result<BTreeSet<String>, String> {
        let mut spec = CmdSpec::new(&self.nanna_bin, ["agents", "list", "--repo"]);
        spec.args.push(repo.as_os_str().to_os_string());
        let out = self.runner.run(&spec).map_err(|e| e.to_string())?;
        if out.success() {
            Ok(parse_agents_table(&out.stdout))
        } else {
            Err(out.stderr.trim().to_string())
        }
    }
}

pub const NIX_BUILD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3600);

pub struct NixBuilder<R: Runner = SystemRunner> {
    runner: R,
    program: String,
}

impl NixBuilder<SystemRunner> {
    pub fn new() -> Self {
        NixBuilder::with_runner(SystemRunner)
    }

    pub fn with_program(program: impl Into<String>) -> Self {
        NixBuilder {
            runner: SystemRunner,
            program: program.into(),
        }
    }
}

impl Default for NixBuilder<SystemRunner> {
    fn default() -> Self {
        NixBuilder::new()
    }
}

impl<R: Runner> NixBuilder<R> {
    pub fn with_runner(runner: R) -> Self {
        NixBuilder {
            runner,
            program: "nix".to_string(),
        }
    }
}

/// ```
/// use std::path::Path;
/// use inchworm::online::flake_url;
///
/// assert_eq!(flake_url(Path::new("/code/my repo")), "git+file:///code/my%20repo#devContainerImage");
/// ```
pub fn flake_url(repo: &Path) -> String {
    let absolute = std::path::absolute(repo).unwrap_or_else(|_| repo.to_path_buf());
    let mut encoded = String::new();
    for byte in absolute.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    format!("git+file://{encoded}#devContainerImage")
}

impl<R: Runner> ImageBuilder for NixBuilder<R> {
    fn build(&self, repo: &Path) -> Result<(), String> {
        let flake = flake_url(repo);
        let spec = CmdSpec::new(
            &self.program,
            ["build", flake.as_str(), "--no-link", "--no-write-lock-file"],
        )
        .with_timeout(NIX_BUILD_TIMEOUT);
        let out = self.runner.run(&spec).map_err(|e| e.to_string())?;
        if out.success() {
            Ok(())
        } else {
            let tail: Vec<&str> = out.stderr.trim().lines().rev().take(5).collect();
            Err(tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
        }
    }
}

pub struct Online<'a> {
    pub catalog: &'a dyn IdentityCatalog,
    pub github: &'a dyn GitHub,
    pub branch_protection: bool,
    pub builder: Option<&'a dyn ImageBuilder>,
}

/// ```
/// use inchworm::config::GlobalConfig;
/// use inchworm::github::FakeGitHub;
/// use inchworm::online::{validate_online, IdentityCatalog, Online};
/// use std::collections::BTreeSet;
/// use std::path::Path;
///
/// struct Empty;
/// impl IdentityCatalog for Empty {
///     fn identities(&self, _: &Path) -> Result<BTreeSet<String>, String> {
///         Ok(BTreeSet::new())
///     }
/// }
///
/// let config = GlobalConfig::parse("nanna_bin = \"nanna\"\n[[repo]]\nslug = \"o/r\"\npath = \"/gone\"\n").unwrap();
/// let github = FakeGitHub::new();
/// let online = Online { catalog: &Empty, github: &github, branch_protection: true, builder: None };
/// assert!(!validate_online(&config, &online, None).is_ok());
/// ```
pub const AUDITOR_IDENTITY: &str = "auditor";

const NANNA_OLLAMA_ADDRESSES: [&str; 3] = [
    "http://localhost:11434",
    "http://127.0.0.1:11434",
    "http://[::1]:11434",
];

pub fn validate_online(
    config: &GlobalConfig,
    online: &Online<'_>,
    path_var: Option<&std::ffi::OsStr>,
) -> Report {
    let mut report = validate(config, path_var);
    let configured = config.ollama_url.trim().trim_end_matches('/');
    if !NANNA_OLLAMA_ADDRESSES.contains(&configured) {
        report.push(
            None,
            Severity::Warning,
            format!(
                "ollama_url is {} but `nanna mcp-serve` always talks to Ollama at http://localhost:11434; the setting applies only to inchworm's own calls",
                config.ollama_url
            ),
        );
    }
    for repo in &config.repo {
        check_repo_online(repo, online, &mut report);
    }
    report
}

fn required_labels(file: &FlowFile) -> BTreeSet<String> {
    let mut labels: BTreeSet<String> = STATUS_LABELS.iter().map(|l| l.to_string()).collect();
    for flow in &file.flow {
        let Trigger::Label { label } = &flow.trigger;
        labels.insert(label.clone());
    }
    labels
}

fn describe_blocking(spec: &BlockingSpec<'_>, base: &str) -> String {
    match spec {
        BlockingSpec::Explicit([]) => "none (required_checks = [] blocks nothing)".to_string(),
        BlockingSpec::Explicit(list) => format!("exactly [{}]", list.join(", ")),
        BlockingSpec::Required { extra: [] } => {
            format!("the required status checks of `{base}`, read when the decision is made")
        }
        BlockingSpec::Required { extra } => format!(
            "the required status checks of `{base}`, read when the decision is made, plus [{}]",
            extra.join(", ")
        ),
        BlockingSpec::All => "all (every check run and commit status on the judged commit)".into(),
    }
}

fn check_branch_protection(
    repo: &RepoEntry,
    flow: &crate::config::Flow,
    online: &Online<'_>,
    report: &mut Report,
) {
    let slug = Some(repo.slug.as_str());
    let spec = flow.outcome.spec();
    report.push(
        slug,
        Severity::Note,
        format!(
            "flow `{}`: blocking checks are {}",
            flow.name,
            describe_blocking(&spec, &flow.base)
        ),
    );
    let merging = flow.outcome.mode == crate::config::OutcomeMode::MergeWhen;
    if matches!(spec, BlockingSpec::Explicit(list) if list.is_empty()) {
        return;
    }
    let protection = online.github.required_checks(&repo.slug, &flow.base);
    match (&spec, protection) {
        (BlockingSpec::All, _) => {}
        (BlockingSpec::Required { .. }, Ok(Some(protected))) if !protected.is_empty() => {}
        (BlockingSpec::Required { .. }, Ok(_)) => report.push(
            slug,
            Severity::Error,
            format!(
                "flow `{}`: branch `{}` has no required status checks, so the blocking checks are undefined and every decision fails closed as `blocking_unresolved`",
                flow.name, flow.base
            ),
        ),
        (BlockingSpec::Required { .. }, Err(err)) => report.push(
            slug,
            Severity::Error,
            format!(
                "cannot read the branch protection of `{}`, so every decision of flow `{}` fails closed as `blocking_unresolved`: {err}",
                flow.base, flow.name
            ),
        ),
        (BlockingSpec::Explicit(listed), Ok(Some(protected))) => {
            for check in listed.iter().filter(|c| !protected.contains(c)) {
                report.push(
                    slug,
                    Severity::Error,
                    format!(
                        "flow `{}`: required check `{check}` is not required by the branch protection of `{}`",
                        flow.name, flow.base
                    ),
                );
            }
            for check in protected.iter().filter(|c| !listed.contains(c)) {
                report.push(
                    slug,
                    if merging { Severity::Error } else { Severity::Warning },
                    format!(
                        "flow `{}`: branch protection of `{}` also requires `{check}`, which the flow does not wait for (weaker than the branch protection; use blocking = \"required\" or \"all\")",
                        flow.name, flow.base
                    ),
                );
            }
        }
        (BlockingSpec::Explicit(_), Ok(None)) => report.push(
            slug,
            if merging { Severity::Error } else { Severity::Warning },
            format!(
                "flow `{}`: branch `{}` has no required status checks, so required_checks cannot match branch protection",
                flow.name, flow.base
            ),
        ),
        (BlockingSpec::Explicit(_), Err(err)) => report.push(
            slug,
            if err.is_plan_limit() && !merging {
                Severity::Warning
            } else {
                Severity::Error
            },
            format!("cannot read the branch protection of `{}`: {err}", flow.base),
        ),
    }
}

fn check_repo_online(repo: &RepoEntry, online: &Online<'_>, report: &mut Report) {
    let slug = Some(repo.slug.as_str());
    let Ok(text) = std::fs::read_to_string(repo.path.join(PROJECT_FILE)) else {
        return;
    };
    let Ok(project) = ProjectConfig::parse(&text) else {
        return;
    };
    let file = project.flow_file();

    match online.catalog.identities(&repo.path) {
        Ok(known) => {
            if !known.contains(AUDITOR_IDENTITY) {
                report.push(
                    slug,
                    Severity::Error,
                    format!(
                        "nanna has no `{AUDITOR_IDENTITY}` identity here and `nanna mcp-serve` refuses to start without one; add an auditor card to the global catalog (known: {})",
                        known.iter().cloned().collect::<Vec<_>>().join(", ")
                    ),
                );
            }
            for flow in &file.flow {
                for step in &flow.step {
                    if !known.contains(&step.identity) {
                        report.push(
                            slug,
                            Severity::Error,
                            format!(
                                "flow `{}` step `{}`: identity `{}` does not resolve (known: {})",
                                flow.name,
                                step.name,
                                step.identity,
                                known.iter().cloned().collect::<Vec<_>>().join(", ")
                            ),
                        );
                    }
                }
            }
        }
        Err(err) => report.push(
            slug,
            Severity::Error,
            format!("cannot list identities with `nanna agents list`: {err}"),
        ),
    }

    match online.github.labels(&repo.slug) {
        Ok(existing) => {
            for label in required_labels(&file) {
                if !existing.contains(&label) {
                    report.push(
                        slug,
                        Severity::Error,
                        format!(
                            "label `{label}` does not exist (gh label create {label} -R {})",
                            repo.slug
                        ),
                    );
                }
            }
        }
        Err(err) => report.push(slug, Severity::Error, format!("cannot list labels: {err}")),
    }

    if online.branch_protection {
        for flow in &file.flow {
            check_branch_protection(repo, flow, online, report);
        }
    }

    if let Some(builder) = online.builder
        && let Err(err) = builder.build(&repo.path)
    {
        report.push(
            slug,
            Severity::Error,
            format!("devContainerImage does not build: {err}"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MergeMethod;
    use crate::exec::testing::ScriptedRunner;
    use crate::github::{FakeGitHub, GitHub, GitHubError, Issue, PullRequest};
    use crate::policy::CheckRun;
    use std::cell::RefCell;

    const FLOWS: &str = r#"
[auditor]
runs_in = { kind = "nanna", identity = "auditor" }
[[flow]]
name = "issue"
trigger = { kind = "label", label = "inchworm" }
[[flow.step]]
name = "implement"
identity = "rust-implementer"
[[flow.step]]
name = "publish"
identity = "pr-shepherd"
[[flow.judge]]
kind = "nanna_clean"
[flow.outcome]
mode = "draft_pr"
required_checks = []
"#;

    struct Catalog(Result<Vec<&'static str>, &'static str>);

    impl IdentityCatalog for Catalog {
        fn identities(&self, _: &Path) -> Result<BTreeSet<String>, String> {
            self.0
                .clone()
                .map(|v| v.into_iter().map(str::to_string).collect())
                .map_err(str::to_string)
        }
    }

    struct Builder {
        result: Result<(), &'static str>,
        built: RefCell<Vec<std::path::PathBuf>>,
    }

    impl ImageBuilder for Builder {
        fn build(&self, repo: &Path) -> Result<(), String> {
            self.built.borrow_mut().push(repo.to_path_buf());
            self.result.map_err(str::to_string)
        }
    }

    struct BrokenGitHub;

    impl GitHub for BrokenGitHub {
        fn open_issues_with_label(&self, _: &str, _: &str) -> Result<Vec<Issue>, GitHubError> {
            unreachable!()
        }
        fn issue(&self, _: &str, _: u64) -> Result<Issue, GitHubError> {
            unreachable!()
        }
        fn labels(&self, _: &str) -> Result<Vec<String>, GitHubError> {
            Err(GitHubError::NotFound("repo".into()))
        }
        fn set_labels(&self, _: &str, _: u64, _: &[&str], _: &[&str]) -> Result<(), GitHubError> {
            unreachable!()
        }
        fn comment(&self, _: &str, _: u64, _: &str) -> Result<(), GitHubError> {
            unreachable!()
        }
        fn pr_for_branch(&self, _: &str, _: &str) -> Result<Option<PullRequest>, GitHubError> {
            unreachable!()
        }
        fn pr_files(&self, _: &str, _: u64) -> Result<Vec<String>, GitHubError> {
            unreachable!()
        }
        fn pr(&self, _: &str, _: u64) -> Result<PullRequest, GitHubError> {
            unreachable!()
        }
        fn pr_patch(&self, _: &str, _: u64) -> Result<String, GitHubError> {
            unreachable!()
        }
        fn check_runs(&self, _: &str, _: &str) -> Result<Vec<CheckRun>, GitHubError> {
            unreachable!()
        }
        fn commit_statuses(&self, _: &str, _: &str) -> Result<Vec<CheckRun>, GitHubError> {
            unreachable!()
        }
        fn mark_ready(&self, _: &str, _: u64) -> Result<(), GitHubError> {
            unreachable!()
        }
        fn mark_draft(&self, _: &str, _: u64) -> Result<(), GitHubError> {
            unreachable!()
        }
        fn merge_pr(&self, _: &str, _: u64, _: &str, _: MergeMethod) -> Result<(), GitHubError> {
            unreachable!()
        }
        fn required_checks(&self, _: &str, _: &str) -> Result<Option<Vec<String>>, GitHubError> {
            unreachable!()
        }
        fn open_prs_for_issue(
            &self,
            _: &str,
            _: u64,
        ) -> Result<Vec<(String, PullRequest)>, GitHubError> {
            unreachable!()
        }
    }

    fn repo(dir: &Path) -> GlobalConfig {
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join(".inchworm")).unwrap();
        std::fs::create_dir_all(dir.join(".nanna")).unwrap();
        std::fs::write(dir.join("inchworm.toml"), FLOWS).unwrap();
        std::fs::write(
            dir.join(".nanna/protected.toml"),
            "extra = [\"inchworm.toml\", \".inchworm/**\"]\n",
        )
        .unwrap();
        std::fs::write(dir.join(".nanna/prompt.md"), "p").unwrap();
        let nanna = dir.join("fake-nanna");
        std::fs::write(&nanna, "#!/bin/sh\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&nanna, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        GlobalConfig::parse(&format!(
            "nanna_bin = \"{}\"\n[[repo]]\nslug = \"o/r\"\npath = \"{}\"\n",
            nanna.display(),
            dir.display()
        ))
        .unwrap()
    }

    fn github_with_labels(labels: &[&str]) -> FakeGitHub {
        let github = FakeGitHub::new();
        for label in labels {
            github.add_repo_label("o/r", label);
        }
        github
    }

    const ALL_LABELS: &[&str] = &[
        "inchworm",
        "inchworm:running",
        "inchworm:failed",
        "inchworm:done",
    ];

    fn messages(report: &Report) -> Vec<String> {
        report
            .findings
            .iter()
            .filter(|f| f.severity != Severity::Note)
            .map(|f| f.message.clone())
            .collect()
    }

    #[test]
    fn a_fully_resolvable_repo_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        let config = repo(dir.path());
        let catalog = Catalog(Ok(vec![
            "auditor",
            "rust-implementer",
            "pr-shepherd",
            "reviewer",
        ]));
        let github = github_with_labels(ALL_LABELS);
        let online = Online {
            catalog: &catalog,
            github: &github,
            branch_protection: true,
            builder: None,
        };
        let report = validate_online(&config, &online, None);
        assert!(messages(&report).is_empty(), "{:?}", report.findings);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.severity == Severity::Note
                    && f.message.contains("blocking checks are none")),
            "{:?}",
            report.findings
        );
    }

    #[test]
    fn unresolvable_identities_are_errors_naming_the_step() {
        let dir = tempfile::tempdir().unwrap();
        let config = repo(dir.path());
        let catalog = Catalog(Ok(vec!["auditor", "rust-implementer"]));
        let github = github_with_labels(ALL_LABELS);
        let online = Online {
            catalog: &catalog,
            github: &github,
            branch_protection: true,
            builder: None,
        };
        let report = validate_online(&config, &online, None);
        assert_eq!(report.error_count(), 1);
        let message = &messages(&report)[0];
        assert!(
            message.contains("step `publish`") && message.contains("`pr-shepherd`"),
            "{message}"
        );
        assert!(
            message.contains("known: auditor, rust-implementer"),
            "{message}"
        );
    }

    #[test]
    fn a_failing_catalog_is_one_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = repo(dir.path());
        let catalog = Catalog(Err("boom"));
        let github = github_with_labels(ALL_LABELS);
        let online = Online {
            catalog: &catalog,
            github: &github,
            branch_protection: true,
            builder: None,
        };
        let report = validate_online(&config, &online, None);
        assert_eq!(report.error_count(), 1);
        assert!(
            messages(&report)[0].contains("cannot list identities")
                && messages(&report)[0].contains("boom")
        );
    }

    fn protection_flow(mode: &str, checks: &str) -> String {
        let flows = FLOWS.replace(
            "[flow.outcome]\nmode = \"draft_pr\"\nrequired_checks = []",
            &format!("[flow.outcome]\nmode = \"{mode}\"\nrequired_checks = [{checks}]"),
        );
        if mode == "merge_when" {
            format!("[merge]\nallow = true\n{flows}")
        } else {
            flows
        }
    }

    fn protection_findings(
        flows: &str,
        github: &FakeGitHub,
        enabled: bool,
    ) -> Vec<(Severity, String)> {
        let dir = tempfile::tempdir().unwrap();
        let config = repo(dir.path());
        std::fs::write(dir.path().join("inchworm.toml"), flows).unwrap();
        let catalog = Catalog(Ok(vec!["rust-implementer", "pr-shepherd"]));
        let online = Online {
            catalog: &catalog,
            github,
            branch_protection: enabled,
            builder: None,
        };
        validate_online(&config, &online, None)
            .findings
            .into_iter()
            .filter(|f| f.message.contains("branch"))
            .map(|f| (f.severity, f.message))
            .collect()
    }

    #[test]
    fn required_checks_must_match_branch_protection() {
        let github = github_with_labels(ALL_LABELS);
        github.set_protection("o/r", "main", &["build", "test"]);
        let matching = protection_findings(
            &protection_flow("draft_pr", "\"build\", \"test\""),
            &github,
            true,
        );
        assert!(matching.is_empty(), "{matching:?}");

        let missing = protection_findings(
            &protection_flow("draft_pr", "\"build\", \"lint\""),
            &github,
            true,
        );
        assert!(
            missing
                .iter()
                .any(|(s, m)| *s == Severity::Error && m.contains("`lint` is not required"))
        );
        assert!(
            missing
                .iter()
                .any(|(s, m)| *s == Severity::Warning && m.contains("also requires `test`"))
        );

        let merging =
            protection_findings(&protection_flow("merge_when", "\"build\""), &github, true);
        assert!(
            merging
                .iter()
                .any(|(s, m)| *s == Severity::Error && m.contains("also requires `test`"))
        );
    }

    #[test]
    fn unprotected_branches_and_api_errors_are_reported_and_skippable() {
        let github = github_with_labels(ALL_LABELS);
        let flows = protection_flow("draft_pr", "\"build\"");
        let warned = protection_findings(&flows, &github, true);
        assert!(
            warned
                .iter()
                .any(|(s, m)| *s == Severity::Warning && m.contains("no required status checks"))
        );
        let blocked =
            protection_findings(&protection_flow("merge_when", "\"build\""), &github, true);
        assert!(blocked.iter().any(|(s, _)| *s == Severity::Error));
        assert!(protection_findings(&flows, &github, false).is_empty());
        assert!(protection_findings(FLOWS, &github, true).is_empty());
        github.fail_protection_with("HTTP 403");
        let denied = protection_findings(&flows, &github, true);
        assert!(
            denied
                .iter()
                .any(|(s, m)| *s == Severity::Error && m.contains("HTTP 403"))
        );
    }

    fn flows_with(lines: &str) -> String {
        FLOWS.replace("required_checks = []", lines)
    }

    fn all_findings(flows: &str, github: &FakeGitHub) -> Vec<(Severity, String)> {
        let dir = tempfile::tempdir().unwrap();
        let config = repo(dir.path());
        std::fs::write(dir.path().join("inchworm.toml"), flows).unwrap();
        let catalog = Catalog(Ok(vec!["rust-implementer", "pr-shepherd", "auditor"]));
        let online = Online {
            catalog: &catalog,
            github,
            branch_protection: true,
            builder: None,
        };
        validate_online(&config, &online, None)
            .findings
            .into_iter()
            .map(|f| (f.severity, f.message))
            .collect()
    }

    fn note(findings: &[(Severity, String)]) -> String {
        let notes: Vec<_> = findings
            .iter()
            .filter(|(s, m)| *s == Severity::Note && m.contains("blocking checks are"))
            .collect();
        assert_eq!(notes.len(), 1, "{findings:?}");
        notes[0].1.clone()
    }

    #[test]
    fn validate_prints_the_effective_blocking_set_of_every_form() {
        let github = github_with_labels(ALL_LABELS);
        github.set_protection("o/r", "main", &["build"]);
        let required = all_findings(&flows_with(""), &github);
        assert!(note(&required).contains("the required status checks of `main`"));
        assert!(!note(&required).contains("plus"));
        let extra = all_findings(&flows_with("extra = [\"lint\"]"), &github);
        assert!(note(&extra).contains("plus [lint]"), "{extra:?}");
        let all = all_findings(&flows_with("blocking = \"all\""), &github);
        assert!(
            note(&all).contains("all (every check run and commit status"),
            "{all:?}"
        );
        let list = all_findings(&flows_with("required_checks = [\"build\"]"), &github);
        assert!(note(&list).contains("exactly [build]"), "{list:?}");
        let none = all_findings(&flows_with("required_checks = []"), &github);
        assert!(
            note(&none).contains("none (required_checks = [] blocks nothing)"),
            "{none:?}"
        );
        for findings in [&required, &extra, &all, &list, &none] {
            assert!(
                findings.iter().all(|(s, _)| *s != Severity::Error),
                "{findings:?}"
            );
        }
    }

    #[test]
    fn the_required_form_is_an_error_when_the_decision_would_fail_closed() {
        let absent = github_with_labels(ALL_LABELS);
        let findings = all_findings(&flows_with(""), &absent);
        assert!(
            findings.iter().any(|(s, m)| *s == Severity::Error
                && m.contains("no required status checks")
                && m.contains("blocking_unresolved")),
            "{findings:?}"
        );
        let empty = github_with_labels(ALL_LABELS);
        empty.set_protection("o/r", "main", &[]);
        assert!(
            all_findings(&flows_with(""), &empty)
                .iter()
                .any(|(s, m)| *s == Severity::Error && m.contains("blocking_unresolved"))
        );
        let denied = github_with_labels(ALL_LABELS);
        denied.fail_protection_with("HTTP 403");
        let findings = all_findings(&flows_with("extra = [\"lint\"]"), &denied);
        assert!(
            findings.iter().any(|(s, m)| *s == Severity::Error
                && m.contains("HTTP 403")
                && m.contains("blocking_unresolved")),
            "{findings:?}"
        );
    }

    #[test]
    fn all_and_the_empty_explicit_list_do_not_depend_on_the_branch_protection() {
        let denied = github_with_labels(ALL_LABELS);
        denied.fail_protection_with("HTTP 403");
        for lines in ["blocking = \"all\"", "required_checks = []"] {
            let findings = all_findings(&flows_with(lines), &denied);
            assert!(
                findings.iter().all(|(s, _)| *s == Severity::Note),
                "{lines}: {findings:?}"
            );
        }
    }

    #[test]
    fn an_explicit_list_weaker_than_the_protection_is_still_warned_about() {
        let github = github_with_labels(ALL_LABELS);
        github.set_protection("o/r", "main", &["build", "test"]);
        let findings = all_findings(&flows_with("required_checks = [\"build\"]"), &github);
        assert!(
            findings.iter().any(|(s, m)| *s == Severity::Warning
                && m.contains("`test`")
                && m.contains("weaker than the branch protection")),
            "{findings:?}"
        );
    }

    #[test]
    fn plan_limited_branch_protection_is_a_warning_only_for_draft_pr_flows() {
        let github = github_with_labels(ALL_LABELS);
        github.fail_protection_with(
            "gh: Upgrade to GitHub Pro or make this repository public to enable this feature. (HTTP 403)",
        );
        let drafted = protection_findings(&protection_flow("draft_pr", "\"build\""), &github, true);
        assert_eq!(drafted.len(), 1, "{drafted:?}");
        assert_eq!(drafted[0].0, Severity::Warning);
        assert!(drafted[0].1.contains("Upgrade to GitHub Pro"));
        let merging =
            protection_findings(&protection_flow("merge_when", "\"build\""), &github, true);
        assert!(merging.iter().any(|(s, _)| *s == Severity::Error));
        github.fail_protection_with("HTTP 500 Upgrade to GitHub Pro");
        let other = protection_findings(&protection_flow("draft_pr", "\"build\""), &github, true);
        assert!(other.iter().any(|(s, _)| *s == Severity::Error));
    }

    #[test]
    fn a_catalog_without_an_auditor_card_is_an_error_because_nanna_will_not_start() {
        let dir = tempfile::tempdir().unwrap();
        let config = repo(dir.path());
        let catalog = Catalog(Ok(vec!["rust-implementer", "pr-shepherd"]));
        let github = github_with_labels(ALL_LABELS);
        let online = Online {
            catalog: &catalog,
            github: &github,
            branch_protection: false,
            builder: None,
        };
        let report = validate_online(&config, &online, None);
        let found = messages(&report);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("`auditor`") && found[0].contains("refuses to start"),
            "{found:?}"
        );
        assert!(!report.is_ok());
    }

    #[test]
    fn an_ollama_url_that_is_not_nannas_address_is_only_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = repo(dir.path());
        let catalog = Catalog(Ok(vec!["auditor", "rust-implementer", "pr-shepherd"]));
        let github = github_with_labels(ALL_LABELS);
        let online = Online {
            catalog: &catalog,
            github: &github,
            branch_protection: false,
            builder: None,
        };
        for same in [
            "http://localhost:11434",
            "http://localhost:11434/",
            "http://127.0.0.1:11434",
            "http://[::1]:11434/",
        ] {
            config.ollama_url = same.to_string();
            let report = validate_online(&config, &online, None);
            assert!(report.findings.is_empty(), "{same}: {:?}", report.findings);
        }
        for different in [
            "http://gpu-box:11434",
            "http://localhost:8080",
            "https://localhost:11434",
        ] {
            config.ollama_url = different.to_string();
            let report = validate_online(&config, &online, None);
            assert!(report.is_ok(), "{different}");
            assert_eq!(
                report.warning_count(),
                1,
                "{different}: {:?}",
                report.findings
            );
            let message = &report.findings[0].message;
            assert!(
                message.contains(different) && message.contains("localhost:11434"),
                "{message}"
            );
            assert_eq!(report.findings[0].repo, None);
        }
    }

    #[test]
    fn every_status_label_is_required_and_named_in_its_own_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = repo(dir.path());
        let catalog = Catalog(Ok(vec!["auditor", "rust-implementer", "pr-shepherd"]));
        let github = github_with_labels(&["inchworm"]);
        let online = Online {
            catalog: &catalog,
            github: &github,
            branch_protection: false,
            builder: None,
        };
        let found = messages(&validate_online(&config, &online, None));
        assert_eq!(found.len(), 3, "{found:?}");
        for label in ["inchworm:running", "inchworm:failed", "inchworm:done"] {
            assert!(
                found.iter().any(|m| m.contains(&format!("`{label}`"))),
                "{label}: {found:?}"
            );
        }
    }

    #[test]
    fn a_custom_trigger_label_must_exist_too() {
        let dir = tempfile::tempdir().unwrap();
        let config = repo(dir.path());
        let flows = std::fs::read_to_string(dir.path().join("inchworm.toml")).unwrap();
        std::fs::write(
            dir.path().join("inchworm.toml"),
            flows.replace("label = \"inchworm\"", "label = \"queue-me\""),
        )
        .unwrap();
        let catalog = Catalog(Ok(vec!["auditor", "rust-implementer", "pr-shepherd"]));
        let github = github_with_labels(&[
            "inchworm",
            "inchworm:running",
            "inchworm:failed",
            "inchworm:done",
        ]);
        let online = Online {
            catalog: &catalog,
            github: &github,
            branch_protection: false,
            builder: None,
        };
        let found = messages(&validate_online(&config, &online, None));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("`queue-me`") && found[0].contains("gh label create queue-me -R o/r"),
            "{found:?}"
        );
    }

    #[test]
    fn missing_labels_are_errors_with_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let config = repo(dir.path());
        let catalog = Catalog(Ok(vec!["auditor", "rust-implementer", "pr-shepherd"]));
        let github = github_with_labels(&["inchworm", "inchworm:done"]);
        let online = Online {
            catalog: &catalog,
            github: &github,
            branch_protection: true,
            builder: None,
        };
        let report = validate_online(&config, &online, None);
        let found = messages(&report);
        assert_eq!(found.len(), 2);
        assert!(found.iter().any(|m| m.contains("`inchworm:running`")
            && m.contains("gh label create inchworm:running -R o/r")));
        assert!(found.iter().any(|m| m.contains("`inchworm:failed`")));
    }

    #[test]
    fn a_failing_label_listing_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = repo(dir.path());
        let catalog = Catalog(Ok(vec!["auditor", "rust-implementer", "pr-shepherd"]));
        let online = Online {
            catalog: &catalog,
            github: &BrokenGitHub,
            branch_protection: true,
            builder: None,
        };
        let report = validate_online(&config, &online, None);
        assert!(messages(&report)[0].contains("cannot list labels"));
    }

    #[test]
    fn images_are_built_only_when_asked_and_failures_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let config = repo(dir.path());
        let catalog = Catalog(Ok(vec!["auditor", "rust-implementer", "pr-shepherd"]));
        let github = github_with_labels(ALL_LABELS);

        let ok = Builder {
            result: Ok(()),
            built: RefCell::new(vec![]),
        };
        let online = Online {
            catalog: &catalog,
            github: &github,
            branch_protection: true,
            builder: Some(&ok),
        };
        assert!(messages(&validate_online(&config, &online, None)).is_empty());
        assert_eq!(ok.built.borrow().as_slice(), [dir.path().to_path_buf()]);

        let bad = Builder {
            result: Err("no such output"),
            built: RefCell::new(vec![]),
        };
        let online = Online {
            catalog: &catalog,
            github: &github,
            branch_protection: true,
            builder: Some(&bad),
        };
        let report = validate_online(&config, &online, None);
        assert!(messages(&report)[0].contains("devContainerImage does not build: no such output"));

        let off = Online {
            catalog: &catalog,
            github: &github,
            branch_protection: true,
            builder: None,
        };
        validate_online(&config, &off, None);
        assert_eq!(bad.built.borrow().len(), 1);
    }

    #[test]
    fn repos_with_unusable_flows_are_left_to_the_offline_checks() {
        let dir = tempfile::tempdir().unwrap();
        let config = repo(dir.path());
        std::fs::write(dir.path().join("inchworm.toml"), "garbage = [").unwrap();
        let catalog = Catalog(Err("never asked"));
        let github = FakeGitHub::new();
        let online = Online {
            catalog: &catalog,
            github: &github,
            branch_protection: true,
            builder: None,
        };
        let report = validate_online(&config, &online, None);
        assert!(messages(&report).iter().all(|m| !m.contains("never asked")));
        std::fs::remove_file(dir.path().join("inchworm.toml")).unwrap();
        assert!(!validate_online(&config, &online, None).is_ok());
    }

    #[test]
    fn nanna_catalog_runs_agents_list_for_the_repo() {
        let table = "NAME LOOP MODEL MAX_EFFECT\nrust-implementer inner m workspace\n";
        let runner = ScriptedRunner::new(vec![
            ScriptedRunner::ok(table),
            ScriptedRunner::fail("bad\n"),
            Err(std::io::Error::other("gone")),
        ]);
        let catalog = NannaCatalog::with_runner(runner, Path::new("/bin/nanna"));
        let names = catalog.identities(Path::new("/repo")).unwrap();
        assert_eq!(names.into_iter().collect::<Vec<_>>(), ["rust-implementer"]);
        assert_eq!(
            catalog.runner.calls(),
            ["/bin/nanna agents list --repo /repo"]
        );
        assert_eq!(catalog.identities(Path::new("/repo")).unwrap_err(), "bad");
        assert!(
            catalog
                .identities(Path::new("/repo"))
                .unwrap_err()
                .contains("gone")
        );
    }

    #[test]
    fn the_flake_url_names_tracked_files_only_and_escapes_the_path() {
        assert_eq!(
            flake_url(Path::new("/home/me/code/repo")),
            "git+file:///home/me/code/repo#devContainerImage"
        );
        assert_eq!(
            flake_url(Path::new("/tmp/my repo/a#b/c?d/100%")),
            "git+file:///tmp/my%20repo/a%23b/c%3Fd/100%25#devContainerImage"
        );
        assert_eq!(
            flake_url(Path::new("/tmp/caf\u{e9}")),
            "git+file:///tmp/caf%C3%A9#devContainerImage"
        );
        assert!(!flake_url(Path::new("/r")).starts_with("path:"));
    }

    #[test]
    fn a_relative_repo_path_becomes_absolute_in_the_flake_url() {
        let url = flake_url(Path::new("relative/repo"));
        assert!(url.starts_with("git+file:///"), "{url}");
        assert!(url.ends_with("/relative/repo#devContainerImage"), "{url}");
    }

    proptest::proptest! {
        #[test]
        fn an_escaped_path_never_contains_url_metacharacters_and_decodes_back(
            segments in proptest::collection::vec("[ -.0-~\u{e9}\u{4e2d}]{1,8}", 1..4)
        ) {
            let path = format!("/x{}", segments.join("/x"));
            let url = flake_url(Path::new(&path));
            let body = url.strip_prefix("git+file://").unwrap().strip_suffix("#devContainerImage").unwrap();
            proptest::prop_assert!(!body.contains(['#', '?', ' ', '\n']));
            let mut decoded = Vec::new();
            let bytes = body.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'%' {
                    decoded.push(u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap(), 16).unwrap());
                    i += 3;
                } else {
                    decoded.push(bytes[i]);
                    i += 1;
                }
            }
            proptest::prop_assert_eq!(String::from_utf8(decoded).unwrap(), path);
        }
    }

    #[test]
    fn nix_builder_builds_the_dev_container_image_flake_output() {
        let runner = ScriptedRunner::new(vec![
            ScriptedRunner::ok(""),
            ScriptedRunner::fail("l1\nl2\nl3\nl4\nl5\nl6\n"),
            Err(std::io::Error::other("no nix")),
        ]);
        let builder = NixBuilder::with_runner(runner);
        builder.build(Path::new("/repo")).unwrap();
        assert_eq!(
            builder.runner.calls(),
            ["nix build git+file:///repo#devContainerImage --no-link --no-write-lock-file"]
        );
        assert_eq!(
            builder.build(Path::new("/repo")).unwrap_err(),
            "l2\nl3\nl4\nl5\nl6"
        );
        assert!(
            builder
                .build(Path::new("/repo"))
                .unwrap_err()
                .contains("no nix")
        );
    }

    #[test]
    fn default_constructors_use_the_system_runner() {
        let _ = NixBuilder::default();
        let _ = NannaCatalog::new(Path::new("nanna"));
    }
}
