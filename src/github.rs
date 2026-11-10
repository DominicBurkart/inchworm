use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use serde::Deserialize;
use thiserror::Error;

use crate::exec::{CmdSpec, Runner, SystemRunner};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum GitHubError {
    #[error("`{command}` failed: {detail}")]
    Command { command: String, detail: String },
    #[error("cannot parse `{command}` output: {detail}")]
    Parse { command: String, detail: String },
    #[error("{0} not found")]
    NotFound(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub head_sha: String,
    pub is_draft: bool,
    pub state: PrState,
    pub url: String,
}

pub trait GitHub {
    fn open_issues_with_label(&self, repo: &str, label: &str) -> Result<Vec<Issue>, GitHubError>;
    fn issue(&self, repo: &str, number: u64) -> Result<Issue, GitHubError>;
    fn labels(&self, repo: &str) -> Result<Vec<String>, GitHubError>;
    fn set_labels(
        &self,
        repo: &str,
        number: u64,
        add: &[&str],
        remove: &[&str],
    ) -> Result<(), GitHubError>;
    fn comment(&self, repo: &str, number: u64, body: &str) -> Result<(), GitHubError>;
    fn pr_for_branch(&self, repo: &str, branch: &str) -> Result<Option<PullRequest>, GitHubError>;
    fn pr_files(&self, repo: &str, number: u64) -> Result<Vec<String>, GitHubError>;
}

#[derive(Deserialize)]
struct WireLabel {
    name: String,
}

#[derive(Deserialize)]
struct WireIssue {
    number: u64,
    title: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    labels: Vec<WireLabel>,
    url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WirePr {
    number: u64,
    head_ref_oid: String,
    #[serde(default)]
    is_draft: bool,
    state: String,
    url: String,
}

impl From<WireIssue> for Issue {
    fn from(wire: WireIssue) -> Issue {
        Issue {
            number: wire.number,
            title: wire.title,
            body: wire.body.unwrap_or_default(),
            labels: wire.labels.into_iter().map(|l| l.name).collect(),
            url: wire.url,
        }
    }
}

fn parse_pr_state(state: &str) -> PrState {
    match state {
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        _ => PrState::Open,
    }
}

pub struct GhCli<R: Runner = SystemRunner> {
    runner: R,
    program: String,
}

impl GhCli<SystemRunner> {
    pub fn new() -> Self {
        GhCli::with_runner(SystemRunner)
    }

    pub fn with_program(program: impl Into<String>) -> Self {
        GhCli {
            runner: SystemRunner,
            program: program.into(),
        }
    }
}

impl Default for GhCli<SystemRunner> {
    fn default() -> Self {
        GhCli::new()
    }
}

impl<R: Runner> GhCli<R> {
    pub fn with_runner(runner: R) -> Self {
        GhCli {
            runner,
            program: "gh".to_string(),
        }
    }

    fn run(&self, args: &[&str]) -> Result<String, GitHubError> {
        let spec = CmdSpec::new(&self.program, args);
        let command = spec.display();
        let output = self.runner.run(&spec).map_err(|err| GitHubError::Command {
            command: command.clone(),
            detail: err.to_string(),
        })?;
        if output.success() {
            Ok(output.stdout)
        } else {
            Err(GitHubError::Command {
                command,
                detail: output.stderr.trim().to_string(),
            })
        }
    }

    fn json<T: for<'de> Deserialize<'de>>(&self, args: &[&str]) -> Result<T, GitHubError> {
        let out = self.run(args)?;
        serde_json::from_str(&out).map_err(|err| GitHubError::Parse {
            command: format!("{} {}", self.program, args.join(" ")),
            detail: err.to_string(),
        })
    }
}

impl<R: Runner> GitHub for GhCli<R> {
    fn open_issues_with_label(&self, repo: &str, label: &str) -> Result<Vec<Issue>, GitHubError> {
        #[rustfmt::skip]
        let args = ["issue", "list", "-R", repo, "--label", label, "--state", "open", "--limit", "100", "--json", "number,title,body,labels,url"];
        let issues: Vec<WireIssue> = self.json(&args)?;
        Ok(issues.into_iter().map(Issue::from).collect())
    }

    fn issue(&self, repo: &str, number: u64) -> Result<Issue, GitHubError> {
        let number = number.to_string();
        #[rustfmt::skip]
        let args = ["issue", "view", &number, "-R", repo, "--json", "number,title,body,labels,url"];
        let issue: WireIssue = self.json(&args)?;
        Ok(issue.into())
    }

    fn labels(&self, repo: &str) -> Result<Vec<String>, GitHubError> {
        let labels: Vec<WireLabel> = self.json(&[
            "label", "list", "-R", repo, "--limit", "500", "--json", "name",
        ])?;
        Ok(labels.into_iter().map(|l| l.name).collect())
    }

    fn set_labels(
        &self,
        repo: &str,
        number: u64,
        add: &[&str],
        remove: &[&str],
    ) -> Result<(), GitHubError> {
        let number = number.to_string();
        let mut args = vec!["issue", "edit", number.as_str(), "-R", repo];
        for label in add {
            args.extend(["--add-label", label]);
        }
        for label in remove {
            args.extend(["--remove-label", label]);
        }
        self.run(&args).map(|_| ())
    }

    fn comment(&self, repo: &str, number: u64, body: &str) -> Result<(), GitHubError> {
        let number = number.to_string();
        self.run(&["issue", "comment", &number, "-R", repo, "--body", body])
            .map(|_| ())
    }

    fn pr_for_branch(&self, repo: &str, branch: &str) -> Result<Option<PullRequest>, GitHubError> {
        #[rustfmt::skip]
        let args = ["pr", "list", "-R", repo, "--head", branch, "--state", "open", "--limit", "1", "--json", "number,headRefOid,isDraft,state,url"];
        let prs: Vec<WirePr> = self.json(&args)?;
        Ok(prs.into_iter().next().map(|pr| PullRequest {
            number: pr.number,
            head_sha: pr.head_ref_oid,
            is_draft: pr.is_draft,
            state: parse_pr_state(&pr.state),
            url: pr.url,
        }))
    }

    fn pr_files(&self, repo: &str, number: u64) -> Result<Vec<String>, GitHubError> {
        let endpoint = format!("repos/{repo}/pulls/{number}/files");
        #[rustfmt::skip]
        let args = ["api", "--paginate", &endpoint, "--jq", ".[] | .filename, (.previous_filename // empty)"];
        let out = self.run(&args)?;
        Ok(out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect())
    }
}

#[derive(Default)]
struct FakeState {
    issues: BTreeMap<(String, u64), Issue>,
    repo_labels: BTreeMap<String, BTreeSet<String>>,
    prs: BTreeMap<(String, String), PullRequest>,
    pr_files: BTreeMap<(String, u64), Vec<String>>,
    comments: Vec<(String, u64, String)>,
}

#[derive(Default)]
pub struct FakeGitHub {
    state: Mutex<FakeState>,
}

impl FakeGitHub {
    pub fn new() -> Self {
        FakeGitHub::default()
    }

    pub fn add_issue(&self, repo: &str, issue: Issue) {
        self.state
            .lock()
            .unwrap()
            .issues
            .insert((repo.to_string(), issue.number), issue);
    }

    pub fn add_repo_label(&self, repo: &str, label: &str) {
        self.state
            .lock()
            .unwrap()
            .repo_labels
            .entry(repo.to_string())
            .or_default()
            .insert(label.to_string());
    }

    pub fn add_pr(&self, repo: &str, branch: &str, pr: PullRequest) {
        self.state
            .lock()
            .unwrap()
            .prs
            .insert((repo.to_string(), branch.to_string()), pr);
    }

    pub fn set_pr_files(&self, repo: &str, number: u64, files: &[&str]) {
        self.state.lock().unwrap().pr_files.insert(
            (repo.to_string(), number),
            files.iter().map(|f| f.to_string()).collect(),
        );
    }

    pub fn comments(&self) -> Vec<(String, u64, String)> {
        self.state.lock().unwrap().comments.clone()
    }
}

impl GitHub for FakeGitHub {
    fn open_issues_with_label(&self, repo: &str, label: &str) -> Result<Vec<Issue>, GitHubError> {
        let state = self.state.lock().unwrap();
        Ok(state
            .issues
            .iter()
            .filter(|((r, _), issue)| r == repo && issue.labels.iter().any(|l| l == label))
            .map(|(_, issue)| issue.clone())
            .collect())
    }

    fn issue(&self, repo: &str, number: u64) -> Result<Issue, GitHubError> {
        self.state
            .lock()
            .unwrap()
            .issues
            .get(&(repo.to_string(), number))
            .cloned()
            .ok_or_else(|| GitHubError::NotFound(format!("{repo}#{number}")))
    }

    fn labels(&self, repo: &str) -> Result<Vec<String>, GitHubError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .repo_labels
            .get(repo)
            .map(|labels| labels.iter().cloned().collect())
            .unwrap_or_default())
    }

    fn set_labels(
        &self,
        repo: &str,
        number: u64,
        add: &[&str],
        remove: &[&str],
    ) -> Result<(), GitHubError> {
        let mut state = self.state.lock().unwrap();
        let issue = state
            .issues
            .get_mut(&(repo.to_string(), number))
            .ok_or_else(|| GitHubError::NotFound(format!("{repo}#{number}")))?;
        issue.labels.retain(|l| !remove.contains(&l.as_str()));
        for label in add {
            if !issue.labels.iter().any(|l| l == label) {
                issue.labels.push((*label).to_string());
            }
        }
        Ok(())
    }

    fn comment(&self, repo: &str, number: u64, body: &str) -> Result<(), GitHubError> {
        let mut state = self.state.lock().unwrap();
        if !state.issues.contains_key(&(repo.to_string(), number)) {
            return Err(GitHubError::NotFound(format!("{repo}#{number}")));
        }
        state
            .comments
            .push((repo.to_string(), number, body.to_string()));
        Ok(())
    }

    fn pr_for_branch(&self, repo: &str, branch: &str) -> Result<Option<PullRequest>, GitHubError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .prs
            .get(&(repo.to_string(), branch.to_string()))
            .cloned())
    }

    fn pr_files(&self, repo: &str, number: u64) -> Result<Vec<String>, GitHubError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .pr_files
            .get(&(repo.to_string(), number))
            .cloned()
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::testing::ScriptedRunner;

    const ISSUE_JSON: &str = r#"{"number":7,"title":"T","body":null,"labels":[{"name":"inchworm"},{"name":"bug"}],"url":"https://github.com/o/r/issues/7"}"#;

    fn issue(number: u64, labels: &[&str]) -> Issue {
        Issue {
            number,
            title: format!("issue {number}"),
            body: "body".into(),
            labels: labels.iter().map(|l| l.to_string()).collect(),
            url: format!("https://github.com/o/r/issues/{number}"),
        }
    }

    #[test]
    fn gh_lists_open_labelled_issues_with_the_expected_command() {
        let runner = ScriptedRunner::new(vec![ScriptedRunner::ok(&format!("[{ISSUE_JSON}]"))]);
        let gh = GhCli::with_runner(runner);
        let issues = gh.open_issues_with_label("o/r", "inchworm").unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].number, 7);
        assert_eq!(issues[0].body, "");
        assert_eq!(issues[0].labels, ["inchworm", "bug"]);
        assert_eq!(
            gh.runner.calls(),
            [
                "gh issue list -R o/r --label inchworm --state open --limit 100 --json number,title,body,labels,url"
            ]
        );
    }

    #[test]
    fn gh_views_one_issue() {
        let gh = GhCli::with_runner(ScriptedRunner::new(vec![ScriptedRunner::ok(ISSUE_JSON)]));
        let issue = gh.issue("o/r", 7).unwrap();
        assert_eq!(issue.title, "T");
        assert_eq!(
            gh.runner.calls(),
            ["gh issue view 7 -R o/r --json number,title,body,labels,url"]
        );
    }

    #[test]
    fn gh_lists_repo_labels() {
        let gh = GhCli::with_runner(ScriptedRunner::new(vec![ScriptedRunner::ok(
            r#"[{"name":"a"},{"name":"b"}]"#,
        )]));
        assert_eq!(gh.labels("o/r").unwrap(), ["a", "b"]);
        assert_eq!(
            gh.runner.calls(),
            ["gh label list -R o/r --limit 500 --json name"]
        );
    }

    #[test]
    fn gh_swaps_labels_and_comments() {
        let gh = GhCli::with_runner(ScriptedRunner::new(vec![
            ScriptedRunner::ok(""),
            ScriptedRunner::ok(""),
        ]));
        gh.set_labels("o/r", 7, &["inchworm:running"], &["inchworm"])
            .unwrap();
        gh.comment("o/r", 7, "hello").unwrap();
        assert_eq!(
            gh.runner.calls(),
            [
                "gh issue edit 7 -R o/r --add-label inchworm:running --remove-label inchworm",
                "gh issue comment 7 -R o/r --body hello"
            ]
        );
    }

    #[test]
    fn gh_finds_the_pr_for_a_branch_or_none() {
        let pr = r#"[{"number":9,"headRefOid":"abc123","isDraft":true,"state":"OPEN","url":"https://github.com/o/r/pull/9"}]"#;
        let gh = GhCli::with_runner(ScriptedRunner::new(vec![
            ScriptedRunner::ok(pr),
            ScriptedRunner::ok("[]"),
        ]));
        let found = gh.pr_for_branch("o/r", "inchworm/7-a1").unwrap().unwrap();
        assert_eq!(found.number, 9);
        assert_eq!(found.head_sha, "abc123");
        assert!(found.is_draft);
        assert_eq!(found.state, PrState::Open);
        assert_eq!(gh.pr_for_branch("o/r", "none").unwrap(), None);
        assert_eq!(
            gh.runner.calls()[0],
            "gh pr list -R o/r --head inchworm/7-a1 --state open --limit 1 --json number,headRefOid,isDraft,state,url"
        );
    }

    #[test]
    fn gh_lists_every_file_of_a_pr_including_rename_sources() {
        let gh = GhCli::with_runner(ScriptedRunner::new(vec![ScriptedRunner::ok(
            "src/a.rs\n.inchworm/old.toml\n\nsrc/b.rs\n",
        )]));
        assert_eq!(
            gh.pr_files("o/r", 9).unwrap(),
            ["src/a.rs", ".inchworm/old.toml", "src/b.rs"]
        );
        assert_eq!(
            gh.runner.calls(),
            [
                "gh api --paginate repos/o/r/pulls/9/files --jq .[] | .filename, (.previous_filename // empty)"
            ]
        );
    }

    #[test]
    fn pr_states_parse() {
        assert_eq!(parse_pr_state("MERGED"), PrState::Merged);
        assert_eq!(parse_pr_state("CLOSED"), PrState::Closed);
        assert_eq!(parse_pr_state("OPEN"), PrState::Open);
    }

    #[test]
    fn gh_failures_carry_the_command_and_stderr() {
        let gh = GhCli::with_runner(ScriptedRunner::new(vec![
            ScriptedRunner::fail("HTTP 404\n"),
            Err(std::io::Error::other("no gh")),
            ScriptedRunner::ok("not json"),
        ]));
        let err = gh.issue("o/r", 1).unwrap_err();
        assert!(
            matches!(&err, GitHubError::Command { detail, .. } if detail == "HTTP 404"),
            "{err:?}"
        );
        assert!(err.to_string().contains("gh issue view 1"));
        assert!(matches!(
            gh.labels("o/r").unwrap_err(),
            GitHubError::Command { .. }
        ));
        assert!(matches!(
            gh.issue("o/r", 1).unwrap_err(),
            GitHubError::Parse { .. }
        ));
    }

    #[test]
    fn default_gh_cli_uses_the_system_runner() {
        let gh = GhCli::default();
        assert_eq!(gh.program, "gh");
    }

    #[test]
    fn fake_lists_only_matching_open_issues() {
        let fake = FakeGitHub::new();
        fake.add_issue("o/r", issue(1, &["inchworm"]));
        fake.add_issue("o/r", issue(2, &["other"]));
        fake.add_issue("x/y", issue(3, &["inchworm"]));
        let found = fake.open_issues_with_label("o/r", "inchworm").unwrap();
        assert_eq!(found.iter().map(|i| i.number).collect::<Vec<_>>(), [1]);
    }

    #[test]
    fn fake_issue_lookup_and_missing() {
        let fake = FakeGitHub::new();
        fake.add_issue("o/r", issue(1, &[]));
        assert_eq!(fake.issue("o/r", 1).unwrap().title, "issue 1");
        assert_eq!(
            fake.issue("o/r", 2).unwrap_err(),
            GitHubError::NotFound("o/r#2".into())
        );
    }

    #[test]
    fn fake_label_swap_is_idempotent() {
        let fake = FakeGitHub::new();
        fake.add_issue("o/r", issue(1, &["inchworm"]));
        for _ in 0..2 {
            fake.set_labels("o/r", 1, &["inchworm:running"], &["inchworm"])
                .unwrap();
        }
        assert_eq!(fake.issue("o/r", 1).unwrap().labels, ["inchworm:running"]);
        assert!(fake.set_labels("o/r", 9, &[], &[]).is_err());
    }

    #[test]
    fn fake_records_comments_and_repo_labels_and_prs() {
        let fake = FakeGitHub::new();
        fake.add_issue("o/r", issue(1, &[]));
        fake.comment("o/r", 1, "hi").unwrap();
        assert!(fake.comment("o/r", 2, "hi").is_err());
        assert_eq!(fake.comments(), [("o/r".to_string(), 1, "hi".to_string())]);
        assert!(fake.labels("o/r").unwrap().is_empty());
        fake.add_repo_label("o/r", "inchworm");
        assert_eq!(fake.labels("o/r").unwrap(), ["inchworm"]);
        assert_eq!(fake.pr_for_branch("o/r", "b").unwrap(), None);
        let pr = PullRequest {
            number: 3,
            head_sha: "sha".into(),
            is_draft: true,
            state: PrState::Open,
            url: "u".into(),
        };
        fake.add_pr("o/r", "b", pr.clone());
        assert_eq!(fake.pr_for_branch("o/r", "b").unwrap(), Some(pr));
        assert!(fake.pr_files("o/r", 3).unwrap().is_empty());
        fake.set_pr_files("o/r", 3, &["a", "b"]);
        assert_eq!(fake.pr_files("o/r", 3).unwrap(), ["a", "b"]);
    }
}
