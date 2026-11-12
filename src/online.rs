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
/// let online = Online { catalog: &Empty, github: &github, builder: None };
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
    use crate::exec::testing::ScriptedRunner;
    use crate::github::{FakeGitHub, GitHub, GitHubError, Issue, PullRequest};
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
            builder: None,
        };
        let report = validate_online(&config, &online);
        assert_eq!(report.error_count(), 1);
        assert!(
            messages(&report)[0].contains("cannot list identities")
                && messages(&report)[0].contains("boom")
        );
    }

    #[test]
    fn every_status_label_is_required_and_named_in_its_own_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = repo(dir.path());
        let catalog = Catalog(Ok(vec!["rust-implementer", "pr-shepherd"]));
        let github = github_with_labels(&["inchworm"]);
        let online = Online {
            catalog: &catalog,
            github: &github,
            builder: None,
        };
        let found = messages(&validate_online(&config, &online));
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
        let flows = std::fs::read_to_string(dir.path().join(".inchworm/flows.toml")).unwrap();
        std::fs::write(
            dir.path().join(".inchworm/flows.toml"),
            flows.replace("label = \"inchworm\"", "label = \"queue-me\""),
        )
        .unwrap();
        let catalog = Catalog(Ok(vec!["rust-implementer", "pr-shepherd"]));
        let github = github_with_labels(&[
            "inchworm",
            "inchworm:running",
            "inchworm:failed",
            "inchworm:done",
        ]);
        let online = Online {
            catalog: &catalog,
            github: &github,
            builder: None,
        };
        let found = messages(&validate_online(&config, &online));
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
        let catalog = Catalog(Ok(vec!["rust-implementer", "pr-shepherd"]));
        let github = github_with_labels(&["inchworm", "inchworm:done"]);
        let online = Online {
            catalog: &catalog,
            github: &github,
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
            builder: Some(&bad),
        };
        let report = validate_online(&config, &online);
        assert!(messages(&report)[0].contains("devContainerImage does not build: no such output"));

        let off = Online {
            catalog: &catalog,
            github: &github,
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
