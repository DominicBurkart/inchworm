use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use serde::Deserialize;
use thiserror::Error;

use crate::config::MergeMethod;
use crate::exec::{CmdSpec, Runner, SystemRunner};
use crate::policy::{CheckConclusion, CheckRun, merge_args};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum GitHubError {
    #[error("`{command}` failed: {detail}")]
    Command { command: String, detail: String },
    #[error("cannot parse `{command}` output: {detail}")]
    Parse { command: String, detail: String },
    #[error("{0} not found")]
    NotFound(String),
}

impl GitHubError {
    pub fn is_plan_limit(&self) -> bool {
        matches!(
            self,
            GitHubError::Command { detail, .. }
                if detail.contains("403") && detail.contains("Upgrade to GitHub Pro")
        )
    }
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
    fn pr(&self, repo: &str, number: u64) -> Result<PullRequest, GitHubError>;
    fn pr_patch(&self, repo: &str, number: u64) -> Result<String, GitHubError>;
    fn check_runs(&self, repo: &str, sha: &str) -> Result<Vec<CheckRun>, GitHubError>;
    fn mark_ready(&self, repo: &str, number: u64) -> Result<(), GitHubError>;
    fn mark_draft(&self, repo: &str, number: u64) -> Result<(), GitHubError>;
    fn merge_pr(
        &self,
        repo: &str,
        number: u64,
        head_sha: &str,
        method: MergeMethod,
    ) -> Result<(), GitHubError>;
    fn required_checks(&self, repo: &str, branch: &str)
    -> Result<Option<Vec<String>>, GitHubError>;
}

#[derive(Deserialize)]
struct WireCheckRun {
    name: String,
    status: String,
    #[serde(default)]
    conclusion: Option<String>,
}

/// ```
/// use inchworm::github::check_conclusion;
/// use inchworm::policy::CheckConclusion;
///
/// assert_eq!(check_conclusion("completed", Some("success")), CheckConclusion::Success);
/// assert_eq!(check_conclusion("completed", Some("skipped")), CheckConclusion::Success);
/// assert_eq!(check_conclusion("completed", Some("timed_out")), CheckConclusion::Failure);
/// assert_eq!(check_conclusion("in_progress", None), CheckConclusion::Pending);
/// ```
pub fn check_conclusion(status: &str, conclusion: Option<&str>) -> CheckConclusion {
    if status != "completed" {
        return CheckConclusion::Pending;
    }
    match conclusion {
        Some("success" | "neutral" | "skipped") => CheckConclusion::Success,
        _ => CheckConclusion::Failure,
    }
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

    fn pr(&self, repo: &str, number: u64) -> Result<PullRequest, GitHubError> {
        let number = number.to_string();
        #[rustfmt::skip]
        let args = ["pr", "view", &number, "-R", repo, "--json", "number,headRefOid,isDraft,state,url"];
        let pr: WirePr = self.json(&args)?;
        Ok(PullRequest {
            number: pr.number,
            head_sha: pr.head_ref_oid,
            is_draft: pr.is_draft,
            state: parse_pr_state(&pr.state),
            url: pr.url,
        })
    }

    fn pr_patch(&self, repo: &str, number: u64) -> Result<String, GitHubError> {
        let number = number.to_string();
        self.run(&["pr", "diff", &number, "-R", repo])
    }

    fn check_runs(&self, repo: &str, sha: &str) -> Result<Vec<CheckRun>, GitHubError> {
        let endpoint = format!("repos/{repo}/commits/{sha}/check-runs");
        #[rustfmt::skip]
        let args = ["api", "--paginate", &endpoint, "--jq", ".check_runs[] | {name, status, conclusion}"];
        let out = self.run(&args)?;
        out.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|line| {
                let wire: WireCheckRun =
                    serde_json::from_str(line).map_err(|err| GitHubError::Parse {
                        command: format!("{} {}", self.program, args.join(" ")),
                        detail: err.to_string(),
                    })?;
                Ok(CheckRun {
                    conclusion: check_conclusion(&wire.status, wire.conclusion.as_deref()),
                    name: wire.name,
                    head_sha: sha.to_string(),
                })
            })
            .collect()
    }

    fn mark_ready(&self, repo: &str, number: u64) -> Result<(), GitHubError> {
        let number = number.to_string();
        self.run(&["pr", "ready", &number, "-R", repo]).map(|_| ())
    }

    fn mark_draft(&self, repo: &str, number: u64) -> Result<(), GitHubError> {
        let number = number.to_string();
        self.run(&["pr", "ready", &number, "--undo", "-R", repo])
            .map(|_| ())
    }

    fn merge_pr(
        &self,
        repo: &str,
        number: u64,
        head_sha: &str,
        method: MergeMethod,
    ) -> Result<(), GitHubError> {
        let mut args = merge_args(number, head_sha, method);
        args.extend(["-R".to_string(), repo.to_string()]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&args).map(|_| ())
    }

    fn required_checks(
        &self,
        repo: &str,
        branch: &str,
    ) -> Result<Option<Vec<String>>, GitHubError> {
        let endpoint = format!("repos/{repo}/branches/{branch}/protection/required_status_checks");
        #[rustfmt::skip]
        let args = ["api", &endpoint, "--jq", "[.contexts[]?, .checks[]?.context] | unique | .[]"];
        match self.run(&args) {
            Ok(out) => Ok(Some(
                out.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect(),
            )),
            Err(GitHubError::Command { detail, .. })
                if detail.contains("404") || detail.contains("not protected") =>
            {
                Ok(None)
            }
            Err(other) => Err(other),
        }
    }
}

#[derive(Default)]
struct FakeState {
    issues: BTreeMap<(String, u64), Issue>,
    repo_labels: BTreeMap<String, BTreeSet<String>>,
    prs: BTreeMap<(String, String), PullRequest>,
    pr_files: BTreeMap<(String, u64), Vec<String>>,
    comments: Vec<(String, u64, String)>,
    patches: BTreeMap<(String, u64), String>,
    check_polls: BTreeMap<(String, String), Vec<Vec<CheckRun>>>,
    ready: Vec<(String, u64)>,
    drafted: Vec<(String, u64)>,
    ready_error: Option<String>,
    draft_error: Option<String>,
    merges: Vec<(String, u64, String, MergeMethod)>,
    protection: BTreeMap<(String, String), Vec<String>>,
    merge_error: Option<String>,
    patch_error: Option<String>,
    check_error: Option<String>,
    protection_error: Option<String>,
    label_error: Option<String>,
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

    pub fn set_pr_patch(&self, repo: &str, number: u64, patch: &str) {
        self.state
            .lock()
            .unwrap()
            .patches
            .insert((repo.to_string(), number), patch.to_string());
    }

    pub fn push_check_poll(&self, repo: &str, sha: &str, runs: Vec<CheckRun>) {
        self.state
            .lock()
            .unwrap()
            .check_polls
            .entry((repo.to_string(), sha.to_string()))
            .or_default()
            .push(runs);
    }

    pub fn set_head_sha(&self, repo: &str, number: u64, sha: &str) {
        let mut state = self.state.lock().unwrap();
        for ((r, _), pr) in state.prs.iter_mut() {
            if r == repo && pr.number == number {
                pr.head_sha = sha.to_string();
            }
        }
    }

    pub fn set_protection(&self, repo: &str, branch: &str, contexts: &[&str]) {
        self.state.lock().unwrap().protection.insert(
            (repo.to_string(), branch.to_string()),
            contexts.iter().map(|c| c.to_string()).collect(),
        );
    }

    pub fn fail_merges_with(&self, message: &str) {
        self.state.lock().unwrap().merge_error = Some(message.to_string());
    }

    pub fn fail_ready_with(&self, message: &str) {
        self.state.lock().unwrap().ready_error = Some(message.to_string());
    }

    pub fn fail_drafts_with(&self, message: &str) {
        self.state.lock().unwrap().draft_error = Some(message.to_string());
    }

    pub fn draft_calls(&self) -> Vec<(String, u64)> {
        self.state.lock().unwrap().drafted.clone()
    }

    pub fn fail_patches_with(&self, message: &str) {
        self.state.lock().unwrap().patch_error = Some(message.to_string());
    }

    pub fn fail_labels_with(&self, message: &str) {
        self.state.lock().unwrap().label_error = Some(message.to_string());
    }

    pub fn fail_protection_with(&self, message: &str) {
        self.state.lock().unwrap().protection_error = Some(message.to_string());
    }

    pub fn fail_check_runs_with(&self, message: &str) {
        self.state.lock().unwrap().check_error = Some(message.to_string());
    }

    pub fn ready_calls(&self) -> Vec<(String, u64)> {
        self.state.lock().unwrap().ready.clone()
    }

    pub fn merge_calls(&self) -> Vec<(String, u64, String, MergeMethod)> {
        self.state.lock().unwrap().merges.clone()
    }

    pub fn issue_labels(&self, repo: &str, number: u64) -> Vec<String> {
        self.issue(repo, number)
            .map(|i| i.labels)
            .unwrap_or_default()
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
        if let Some(message) = state.label_error.clone() {
            return Err(GitHubError::Command {
                command: format!("gh issue edit {number}"),
                detail: message,
            });
        }
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

    fn pr(&self, repo: &str, number: u64) -> Result<PullRequest, GitHubError> {
        self.state
            .lock()
            .unwrap()
            .prs
            .iter()
            .find(|((r, _), pr)| r == repo && pr.number == number)
            .map(|(_, pr)| pr.clone())
            .ok_or_else(|| GitHubError::NotFound(format!("{repo}!{number}")))
    }

    fn pr_patch(&self, repo: &str, number: u64) -> Result<String, GitHubError> {
        self.pr(repo, number)?;
        if let Some(message) = self.state.lock().unwrap().patch_error.clone() {
            return Err(GitHubError::Command {
                command: format!("gh pr diff {number}"),
                detail: message,
            });
        }
        Ok(self
            .state
            .lock()
            .unwrap()
            .patches
            .get(&(repo.to_string(), number))
            .cloned()
            .unwrap_or_default())
    }

    fn check_runs(&self, repo: &str, sha: &str) -> Result<Vec<CheckRun>, GitHubError> {
        let mut state = self.state.lock().unwrap();
        if let Some(message) = state.check_error.clone() {
            return Err(GitHubError::Command {
                command: format!("gh api check-runs {sha}"),
                detail: message,
            });
        }
        let polls = state
            .check_polls
            .entry((repo.to_string(), sha.to_string()))
            .or_default();
        Ok(if polls.len() > 1 {
            polls.remove(0)
        } else {
            polls.first().cloned().unwrap_or_default()
        })
    }

    fn mark_ready(&self, repo: &str, number: u64) -> Result<(), GitHubError> {
        let mut state = self.state.lock().unwrap();
        if let Some(message) = state.ready_error.clone() {
            return Err(GitHubError::Command {
                command: format!("gh pr ready {number}"),
                detail: message,
            });
        }
        let pr = state
            .prs
            .iter_mut()
            .find(|((r, _), pr)| r == repo && pr.number == number)
            .map(|(_, pr)| pr)
            .ok_or_else(|| GitHubError::NotFound(format!("{repo}!{number}")))?;
        pr.is_draft = false;
        state.ready.push((repo.to_string(), number));
        Ok(())
    }

    fn mark_draft(&self, repo: &str, number: u64) -> Result<(), GitHubError> {
        let mut state = self.state.lock().unwrap();
        state.drafted.push((repo.to_string(), number));
        if let Some(message) = state.draft_error.clone() {
            return Err(GitHubError::Command {
                command: format!("gh pr ready {number} --undo"),
                detail: message,
            });
        }
        let pr = state
            .prs
            .iter_mut()
            .find(|((r, _), pr)| r == repo && pr.number == number)
            .map(|(_, pr)| pr)
            .ok_or_else(|| GitHubError::NotFound(format!("{repo}!{number}")))?;
        pr.is_draft = true;
        Ok(())
    }

    fn merge_pr(
        &self,
        repo: &str,
        number: u64,
        head_sha: &str,
        method: MergeMethod,
    ) -> Result<(), GitHubError> {
        let mut state = self.state.lock().unwrap();
        if let Some(message) = state.merge_error.clone() {
            return Err(GitHubError::Command {
                command: format!("gh pr merge {number}"),
                detail: message,
            });
        }
        let pr = state
            .prs
            .iter_mut()
            .find(|((r, _), pr)| r == repo && pr.number == number)
            .map(|(_, pr)| pr)
            .ok_or_else(|| GitHubError::NotFound(format!("{repo}!{number}")))?;
        if !pr.head_sha.eq_ignore_ascii_case(head_sha) {
            return Err(GitHubError::Command {
                command: format!("gh pr merge {number} --match-head-commit {head_sha}"),
                detail: "head branch was modified".to_string(),
            });
        }
        pr.state = PrState::Merged;
        state
            .merges
            .push((repo.to_string(), number, head_sha.to_string(), method));
        Ok(())
    }

    fn required_checks(
        &self,
        repo: &str,
        branch: &str,
    ) -> Result<Option<Vec<String>>, GitHubError> {
        if let Some(message) = self.state.lock().unwrap().protection_error.clone() {
            return Err(GitHubError::Command {
                command: format!("gh api branches/{branch}/protection"),
                detail: message,
            });
        }
        Ok(self
            .state
            .lock()
            .unwrap()
            .protection
            .get(&(repo.to_string(), branch.to_string()))
            .cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::testing::ScriptedRunner;

    fn command_error(detail: &str) -> GitHubError {
        GitHubError::Command {
            command: "gh api".to_string(),
            detail: detail.to_string(),
        }
    }

    #[test]
    fn plan_limit_needs_both_the_403_and_the_upgrade_message() {
        assert!(command_error("gh: Upgrade to GitHub Pro or make this repository public to enable this feature. (HTTP 403)").is_plan_limit());
        assert!(!command_error("gh: Forbidden (HTTP 403)").is_plan_limit());
        assert!(!command_error("Upgrade to GitHub Pro (HTTP 500)").is_plan_limit());
        assert!(!command_error("gh: Not Found (HTTP 404)").is_plan_limit());
        assert!(!GitHubError::NotFound("Upgrade to GitHub Pro 403".to_string()).is_plan_limit());
    }

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
    fn gh_views_one_pr_and_reads_its_patch() {
        let pr = r#"{"number":9,"headRefOid":"abc123","isDraft":false,"state":"MERGED","url":"u"}"#;
        let gh = GhCli::with_runner(ScriptedRunner::new(vec![
            ScriptedRunner::ok(pr),
            ScriptedRunner::ok("diff --git a/x b/x\n"),
        ]));
        let found = gh.pr("o/r", 9).unwrap();
        assert_eq!(
            (found.head_sha.as_str(), found.state),
            ("abc123", PrState::Merged)
        );
        assert_eq!(gh.pr_patch("o/r", 9).unwrap(), "diff --git a/x b/x\n");
        assert_eq!(
            gh.runner.calls(),
            [
                "gh pr view 9 -R o/r --json number,headRefOid,isDraft,state,url",
                "gh pr diff 9 -R o/r"
            ]
        );
    }

    #[test]
    fn gh_reads_check_runs_for_the_exact_sha() {
        let lines = concat!(
            r#"{"name":"build","status":"completed","conclusion":"success"}"#,
            "\n",
            r#"{"name":"test","status":"in_progress","conclusion":null}"#,
            "\n",
            r#"{"name":"lint","status":"completed","conclusion":"failure"}"#,
            "\n",
        );
        let gh = GhCli::with_runner(ScriptedRunner::new(vec![
            ScriptedRunner::ok(lines),
            ScriptedRunner::ok("not json\n"),
        ]));
        let runs = gh.check_runs("o/r", "abc123").unwrap();
        let summary: Vec<_> = runs
            .iter()
            .map(|r| (r.name.as_str(), r.conclusion))
            .collect();
        assert_eq!(
            summary,
            [
                ("build", CheckConclusion::Success),
                ("test", CheckConclusion::Pending),
                ("lint", CheckConclusion::Failure)
            ]
        );
        assert!(runs.iter().all(|r| r.head_sha == "abc123"));
        assert_eq!(
            gh.runner.calls()[0],
            "gh api --paginate repos/o/r/commits/abc123/check-runs --jq .check_runs[] | {name, status, conclusion}"
        );
        assert!(matches!(
            gh.check_runs("o/r", "abc123").unwrap_err(),
            GitHubError::Parse { .. }
        ));
    }

    #[test]
    fn gh_marks_ready_and_merges_only_with_match_head_commit_and_never_admin() {
        let gh = GhCli::with_runner(ScriptedRunner::new(vec![
            ScriptedRunner::ok(""),
            ScriptedRunner::ok(""),
        ]));
        gh.mark_ready("o/r", 9).unwrap();
        gh.merge_pr("o/r", 9, "abc123", MergeMethod::Squash)
            .unwrap();
        let calls = gh.runner.calls();
        assert_eq!(calls[0], "gh pr ready 9 -R o/r");
        assert_eq!(
            calls[1],
            "gh pr merge 9 --match-head-commit abc123 --squash -R o/r"
        );
        assert!(!calls[1].contains("--admin"));
    }

    #[test]
    fn gh_restores_draft_with_ready_undo() {
        let gh = GhCli::with_runner(ScriptedRunner::new(vec![
            ScriptedRunner::ok(""),
            ScriptedRunner::fail("boom"),
        ]));
        gh.mark_draft("o/r", 9).unwrap();
        assert!(gh.mark_draft("o/r", 9).is_err());
        let calls = gh.runner.calls();
        assert_eq!(calls[0], "gh pr ready 9 --undo -R o/r");
    }

    #[test]
    fn gh_reads_branch_protection_and_treats_404_as_unprotected() {
        let gh = GhCli::with_runner(ScriptedRunner::new(vec![
            ScriptedRunner::ok("build\ntest\n"),
            ScriptedRunner::fail("gh: Branch not protected (HTTP 404)"),
            ScriptedRunner::fail("HTTP 500"),
        ]));
        assert_eq!(
            gh.required_checks("o/r", "main").unwrap(),
            Some(vec!["build".to_string(), "test".to_string()])
        );
        assert_eq!(gh.required_checks("o/r", "main").unwrap(), None);
        assert!(gh.required_checks("o/r", "main").is_err());
        assert_eq!(
            gh.runner.calls()[0],
            "gh api repos/o/r/branches/main/protection/required_status_checks --jq [.contexts[]?, .checks[]?.context] | unique | .[]"
        );
    }

    #[test]
    fn fake_pr_actions_track_draft_merge_and_exact_head() {
        let fake = FakeGitHub::new();
        let sha = "a".repeat(40);
        let pr = PullRequest {
            number: 3,
            head_sha: sha.clone(),
            is_draft: true,
            state: PrState::Open,
            url: "u".into(),
        };
        fake.add_pr("o/r", "b", pr);
        assert!(fake.pr("o/r", 4).is_err());
        assert!(fake.pr_patch("o/r", 4).is_err());
        assert_eq!(fake.pr_patch("o/r", 3).unwrap(), "");
        fake.set_pr_patch("o/r", 3, "diff");
        assert_eq!(fake.pr_patch("o/r", 3).unwrap(), "diff");
        assert!(fake.mark_ready("o/r", 4).is_err());
        fake.mark_ready("o/r", 3).unwrap();
        assert!(!fake.pr("o/r", 3).unwrap().is_draft);
        assert_eq!(fake.ready_calls(), [("o/r".to_string(), 3)]);

        assert!(fake.merge_pr("o/r", 9, &sha, MergeMethod::Merge).is_err());
        fake.set_head_sha("o/r", 3, &"b".repeat(40));
        let err = fake
            .merge_pr("o/r", 3, &sha, MergeMethod::Merge)
            .unwrap_err();
        assert!(err.to_string().contains("head branch was modified"));
        fake.merge_pr("o/r", 3, &"B".repeat(40), MergeMethod::Rebase)
            .unwrap();
        assert_eq!(fake.pr("o/r", 3).unwrap().state, PrState::Merged);
        assert_eq!(fake.merge_calls().len(), 1);
        fake.fail_merges_with("blocked");
        assert!(fake.merge_pr("o/r", 3, &sha, MergeMethod::Merge).is_err());
    }

    #[test]
    fn fake_check_polls_replay_in_order_and_then_repeat_the_last() {
        let fake = FakeGitHub::new();
        assert!(fake.check_runs("o/r", "s").unwrap().is_empty());
        let run = |c| CheckRun {
            name: "build".into(),
            head_sha: "s".into(),
            conclusion: c,
        };
        fake.push_check_poll("o/r", "s", vec![run(CheckConclusion::Pending)]);
        fake.push_check_poll("o/r", "s", vec![run(CheckConclusion::Success)]);
        assert_eq!(
            fake.check_runs("o/r", "s").unwrap()[0].conclusion,
            CheckConclusion::Pending
        );
        for _ in 0..2 {
            assert_eq!(
                fake.check_runs("o/r", "s").unwrap()[0].conclusion,
                CheckConclusion::Success
            );
        }
    }

    #[test]
    fn fake_protection_and_labels_accessors() {
        let fake = FakeGitHub::new();
        assert_eq!(fake.required_checks("o/r", "main").unwrap(), None);
        fake.set_protection("o/r", "main", &["a"]);
        assert_eq!(
            fake.required_checks("o/r", "main").unwrap(),
            Some(vec!["a".to_string()])
        );
        assert!(fake.issue_labels("o/r", 1).is_empty());
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
