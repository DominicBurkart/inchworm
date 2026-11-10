use std::collections::BTreeSet;
use std::path::Path;

use crate::config::{FlowFile, GlobalConfig, RepoEntry, Trigger};
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

impl<R: Runner> ImageBuilder for NixBuilder<R> {
    fn build(&self, repo: &Path) -> Result<(), String> {
        let flake = format!("path:{}#devContainerImage", repo.display());
        let spec = CmdSpec::new(&self.program, ["build", flake.as_str(), "--no-link"]);
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
/// assert!(!validate_online(&config, &online).is_ok());
/// ```
pub fn validate_online(config: &GlobalConfig, online: &Online<'_>) -> Report {
    let mut report = validate(config);
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

fn check_branch_protection(
    repo: &RepoEntry,
    flow: &crate::config::Flow,
    online: &Online<'_>,
    report: &mut Report,
) {
    let slug = Some(repo.slug.as_str());
    let listed = &flow.outcome.required_checks;
    if listed.is_empty() {
        return;
    }
    let merging = flow.outcome.mode == crate::config::OutcomeMode::MergeWhen;
    match online.github.required_checks(&repo.slug, &flow.base) {
        Ok(Some(protected)) => {
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
                        "flow `{}`: branch protection of `{}` also requires `{check}`, which the flow does not wait for",
                        flow.name, flow.base
                    ),
                );
            }
        }
        Ok(None) => report.push(
            slug,
            if merging { Severity::Error } else { Severity::Warning },
            format!(
                "flow `{}`: branch `{}` has no required status checks, so required_checks cannot match branch protection",
                flow.name, flow.base
            ),
        ),
        Err(err) => report.push(
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
    let Ok(text) = std::fs::read_to_string(repo.path.join(".inchworm/flows.toml")) else {
        return;
    };
    let Ok(file) = FlowFile::parse(&text) else {
        return;
    };

    match online.catalog.identities(&repo.path) {
        Ok(known) => {
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
[[flow]]
name = "issue"
trigger = { kind = "label", label = "inchworm" }
[[flow.step]]
name = "implement"
identity = "rust-implementer"
[[flow.step]]
name = "publish"
identity = "pr-shepherd"
[flow.outcome]
mode = "draft_pr"
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
        fn mark_ready(&self, _: &str, _: u64) -> Result<(), GitHubError> {
            unreachable!()
        }
        fn merge_pr(&self, _: &str, _: u64, _: &str, _: MergeMethod) -> Result<(), GitHubError> {
            unreachable!()
        }
        fn required_checks(&self, _: &str, _: &str) -> Result<Option<Vec<String>>, GitHubError> {
            unreachable!()
        }
    }

    fn repo(dir: &Path) -> GlobalConfig {
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join(".inchworm")).unwrap();
        std::fs::create_dir_all(dir.join(".nanna")).unwrap();
        std::fs::write(dir.join(".inchworm/flows.toml"), FLOWS).unwrap();
        std::fs::write(
            dir.join(".nanna/protected.toml"),
            "extra = [\".inchworm/**\"]\n",
        )
        .unwrap();
        std::fs::write(dir.join(".nanna/prompt.md"), "p").unwrap();
        GlobalConfig::parse(&format!(
            "nanna_bin = \"nanna\"\n[[repo]]\nslug = \"o/r\"\npath = \"{}\"\n",
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
        report.findings.iter().map(|f| f.message.clone()).collect()
    }

    #[test]
    fn a_fully_resolvable_repo_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        let config = repo(dir.path());
        let catalog = Catalog(Ok(vec!["rust-implementer", "pr-shepherd", "reviewer"]));
        let github = github_with_labels(ALL_LABELS);
        let online = Online {
            catalog: &catalog,
            github: &github,
            branch_protection: true,
            builder: None,
        };
        let report = validate_online(&config, &online);
        assert!(report.findings.is_empty(), "{:?}", report.findings);
    }

    #[test]
    fn unresolvable_identities_are_errors_naming_the_step() {
        let dir = tempfile::tempdir().unwrap();
        let config = repo(dir.path());
        let catalog = Catalog(Ok(vec!["rust-implementer"]));
        let github = github_with_labels(ALL_LABELS);
        let online = Online {
            catalog: &catalog,
            github: &github,
            branch_protection: true,
            builder: None,
        };
        let report = validate_online(&config, &online);
        assert_eq!(report.error_count(), 1);
        let message = &messages(&report)[0];
        assert!(
            message.contains("step `publish`") && message.contains("`pr-shepherd`"),
            "{message}"
        );
        assert!(message.contains("known: rust-implementer"));
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
        let report = validate_online(&config, &online);
        assert_eq!(report.error_count(), 1);
        assert!(
            messages(&report)[0].contains("cannot list identities")
                && messages(&report)[0].contains("boom")
        );
    }

    fn protection_flow(mode: &str, checks: &str) -> String {
        FLOWS.replace(
            "[flow.outcome]\nmode = \"draft_pr\"",
            &format!(
                "[[flow.judge]]\nkind = \"nanna_clean\"\n[flow.outcome]\nmode = \"{mode}\"\nrequired_checks = [{checks}]"
            ),
        )
    }

    fn protection_findings(
        flows: &str,
        github: &FakeGitHub,
        enabled: bool,
    ) -> Vec<(Severity, String)> {
        let dir = tempfile::tempdir().unwrap();
        let config = repo(dir.path());
        std::fs::write(dir.path().join(".inchworm/flows.toml"), flows).unwrap();
        let catalog = Catalog(Ok(vec!["rust-implementer", "pr-shepherd"]));
        let online = Online {
            catalog: &catalog,
            github,
            branch_protection: enabled,
            builder: None,
        };
        validate_online(&config, &online)
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
    fn missing_labels_are_errors_with_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let config = repo(dir.path());
        let catalog = Catalog(Ok(vec!["rust-implementer", "pr-shepherd"]));
        let github = github_with_labels(&["inchworm", "inchworm:done"]);
        let online = Online {
            catalog: &catalog,
            github: &github,
            branch_protection: true,
            builder: None,
        };
        let report = validate_online(&config, &online);
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
        let catalog = Catalog(Ok(vec!["rust-implementer", "pr-shepherd"]));
        let online = Online {
            catalog: &catalog,
            github: &BrokenGitHub,
            branch_protection: true,
            builder: None,
        };
        let report = validate_online(&config, &online);
        assert!(messages(&report)[0].contains("cannot list labels"));
    }

    #[test]
    fn images_are_built_only_when_asked_and_failures_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let config = repo(dir.path());
        let catalog = Catalog(Ok(vec!["rust-implementer", "pr-shepherd"]));
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
        assert!(validate_online(&config, &online).findings.is_empty());
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
        let report = validate_online(&config, &online);
        assert!(messages(&report)[0].contains("devContainerImage does not build: no such output"));

        let off = Online {
            catalog: &catalog,
            github: &github,
            branch_protection: true,
            builder: None,
        };
        validate_online(&config, &off);
        assert_eq!(bad.built.borrow().len(), 1);
    }

    #[test]
    fn repos_with_unusable_flows_are_left_to_the_offline_checks() {
        let dir = tempfile::tempdir().unwrap();
        let config = repo(dir.path());
        std::fs::write(dir.path().join(".inchworm/flows.toml"), "garbage = [").unwrap();
        let catalog = Catalog(Err("never asked"));
        let github = FakeGitHub::new();
        let online = Online {
            catalog: &catalog,
            github: &github,
            branch_protection: true,
            builder: None,
        };
        let report = validate_online(&config, &online);
        assert!(messages(&report).iter().all(|m| !m.contains("never asked")));
        std::fs::remove_file(dir.path().join(".inchworm/flows.toml")).unwrap();
        assert!(!validate_online(&config, &online).is_ok());
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
            ["nix build path:/repo#devContainerImage --no-link"]
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
