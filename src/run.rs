use std::path::Path;

use thiserror::Error;

use crate::config::{Flow, Trigger};
use crate::flow::{
    Dispatcher, FlowInput, FlowOutcome, PROTECTED_DIR, PROTECTED_FILE, branch_name,
    protected_paths_in, run_flow,
};
use crate::github::{GitHub, GitHubError, PullRequest};
use crate::model::{FailureClass, RunState};
use crate::store::{RunId, Store, StoreError};

pub const RUNNING_LABEL: &str = "inchworm:running";
pub const FAILED_LABEL: &str = "inchworm:failed";

#[derive(Debug, Error)]
pub enum RunError {
    #[error("{repo}#{issue} already has a run for flow `{flow}`")]
    AlreadyRun {
        repo: String,
        issue: u64,
        flow: String,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    GitHub(#[from] GitHubError),
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunSummary {
    pub run_id: RunId,
    pub attempts: u32,
    pub state: RunState,
    pub pr: Option<PullRequest>,
    pub failure: Option<(FailureClass, String)>,
    pub warnings: Vec<String>,
}

pub struct RunContext<'a> {
    pub store: &'a Store,
    pub github: &'a dyn GitHub,
    pub slug: &'a str,
    pub repo_path: &'a Path,
    pub now: &'a dyn Fn() -> i64,
}

fn trigger_label(flow: &Flow) -> &str {
    let Trigger::Label { label } = &flow.trigger;
    label
}

fn describe_failure(class: FailureClass, raw: &str, attempt: u32) -> String {
    format!("attempt {attempt} failed with `{class}`: {raw}")
}

fn relabel(
    ctx: &RunContext<'_>,
    issue: u64,
    add: &[&str],
    remove: &[&str],
    warnings: &mut Vec<String>,
) {
    if let Err(err) = ctx.github.set_labels(ctx.slug, issue, add, remove) {
        warnings.push(format!("could not update labels: {err}"));
    }
}

fn note(ctx: &RunContext<'_>, issue: u64, body: &str, warnings: &mut Vec<String>) {
    if let Err(err) = ctx.github.comment(ctx.slug, issue, body) {
        warnings.push(format!("could not comment on the issue: {err}"));
    }
}

fn protected_pr_files(
    ctx: &RunContext<'_>,
    pr: &PullRequest,
) -> Result<(), (FailureClass, String)> {
    let files = ctx
        .github
        .pr_files(ctx.slug, pr.number)
        .map_err(|err| (FailureClass::AgentFailed, err.to_string()))?;
    let violations = protected_paths_in(&files, &[]);
    if violations.is_empty() {
        Ok(())
    } else {
        Err((
            FailureClass::ProtectedPathViolation,
            format!(
                "pull request #{} touches protected {}/** or {}: {}",
                pr.number,
                PROTECTED_DIR,
                PROTECTED_FILE,
                violations.join(", ")
            ),
        ))
    }
}

/// ```
/// use inchworm::config::FlowFile;
/// use inchworm::flow::{Dispatcher};
/// use inchworm::github::{FakeGitHub, Issue};
/// use inchworm::nanna_client::{AssignRequest, ClientError, TaskOutcome};
/// use inchworm::run::{run_issue, RunContext};
/// use inchworm::store::Store;
/// use std::time::Duration;
///
/// struct Cancels;
/// impl Dispatcher for Cancels {
///     fn dispatch(&mut self, _: &AssignRequest, _: Duration) -> Result<TaskOutcome, ClientError> {
///         Ok(TaskOutcome::Cancelled)
///     }
/// }
///
/// let file = FlowFile::parse(r#"
/// [[flow]]
/// name = "issue"
/// max_attempts = 1
/// trigger = { kind = "label", label = "inchworm" }
/// [[flow.step]]
/// name = "implement"
/// identity = "rust-implementer"
/// [flow.outcome]
/// mode = "draft_pr"
/// "#).unwrap();
/// let github = FakeGitHub::new();
/// github.add_issue("o/r", Issue {
///     number: 1, title: "t".into(), body: "b".into(), labels: vec!["inchworm".into()], url: "u".into(),
/// });
/// let store = Store::open_in_memory().unwrap();
/// let ctx = RunContext {
///     store: &store, github: &github, slug: "o/r",
///     repo_path: std::path::Path::new("/repo"), now: &|| 0,
/// };
/// let summary = run_issue(&ctx, &file.flow[0], 1, &mut Cancels).unwrap();
/// assert_eq!(summary.state, inchworm::model::RunState::Failed);
/// ```
pub fn run_issue(
    ctx: &RunContext<'_>,
    flow: &Flow,
    issue_number: u64,
    dispatcher: &mut dyn Dispatcher,
) -> Result<RunSummary, RunError> {
    let issue = ctx.github.issue(ctx.slug, issue_number)?;
    let run_id = ctx
        .store
        .enqueue_run(ctx.slug, issue_number, &flow.name, (ctx.now)())?
        .ok_or_else(|| RunError::AlreadyRun {
            repo: ctx.slug.to_string(),
            issue: issue_number,
            flow: flow.name.clone(),
        })?;
    ctx.store
        .set_run_state(run_id, RunState::Running, (ctx.now)())?;
    let mut warnings = Vec::new();
    relabel(
        ctx,
        issue_number,
        &[RUNNING_LABEL],
        &[trigger_label(flow)],
        &mut warnings,
    );

    let mut previous_failure: Option<String> = None;
    let mut attempts = 0;
    loop {
        attempts += 1;
        let attempt = ctx.store.start_attempt(run_id, (ctx.now)())?;
        let branch = branch_name(
            issue_number,
            ctx.store.issue_attempt_count(ctx.slug, issue_number)?,
        );
        let input = FlowInput {
            repo_path: ctx.repo_path,
            issue_number,
            issue_title: &issue.title,
            issue_body: &issue.body,
            attempt: attempt.n,
            branch: &branch,
            previous_failure: previous_failure.as_deref(),
        };
        let outcome = match run_flow(flow, &input, dispatcher) {
            FlowOutcome::Completed { .. } => match ctx.github.pr_for_branch(ctx.slug, &branch) {
                Ok(Some(pr)) if !pr.is_draft => (
                    FailureClass::AgentFailed,
                    format!(
                        "pull request #{} is not a draft; inchworm only accepts draft pull requests",
                        pr.number
                    ),
                ),
                Ok(Some(pr)) => match protected_pr_files(ctx, &pr) {
                    Err(failure) => failure,
                    Ok(()) => {
                        ctx.store
                            .record_pr(attempt.id, &branch, &pr.head_sha, pr.number)?;
                        ctx.store.finish_attempt_ok(attempt.id, (ctx.now)())?;
                        ctx.store
                            .set_run_state(run_id, RunState::AwaitingCi, (ctx.now)())?;
                        note(
                            ctx,
                            issue_number,
                            &format!(
                                "inchworm opened {} for this issue from `{branch}` at `{}`.",
                                pr.url, pr.head_sha
                            ),
                            &mut warnings,
                        );
                        return Ok(RunSummary {
                            run_id,
                            attempts,
                            state: RunState::AwaitingCi,
                            pr: Some(pr),
                            failure: None,
                            warnings,
                        });
                    }
                },
                Ok(None) => (
                    FailureClass::AgentFailed,
                    format!("the steps completed but no pull request exists for branch `{branch}`"),
                ),
                Err(err) => (FailureClass::AgentFailed, err.to_string()),
            },
            FlowOutcome::Failed {
                step,
                class,
                raw_error,
                ..
            } => (class, format!("step `{step}`: {raw_error}")),
        };
        let (class, raw) = outcome;
        ctx.store
            .finish_attempt_failed(attempt.id, class, &raw, (ctx.now)())?;
        let retry = flow.retry_on.contains(&class) && attempts < flow.max_attempts;
        if retry {
            previous_failure = Some(describe_failure(class, &raw, attempt.n));
            continue;
        }
        ctx.store
            .set_run_state(run_id, RunState::Failed, (ctx.now)())?;
        relabel(
            ctx,
            issue_number,
            &[FAILED_LABEL],
            &[RUNNING_LABEL],
            &mut warnings,
        );
        note(
            ctx,
            issue_number,
            &format!(
                "inchworm gave up after {attempts} attempt(s): {}",
                describe_failure(class, &raw, attempt.n)
            ),
            &mut warnings,
        );
        return Ok(RunSummary {
            run_id,
            attempts,
            state: RunState::Failed,
            pr: None,
            failure: Some((class, raw)),
            warnings,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FlowFile;
    use crate::github::{FakeGitHub, Issue, PrState};
    use crate::nanna_client::fake::{Scenario, connect};

    fn flow(max_attempts: u32, retry_on: &str) -> Flow {
        FlowFile::parse(&format!(
            r#"
[[flow]]
name = "issue"
max_attempts = {max_attempts}
retry_on = [{retry_on}]
trigger = {{ kind = "label", label = "inchworm" }}
[[flow.step]]
name = "implement"
identity = "rust-implementer"
[[flow.step]]
name = "publish"
identity = "pr-shepherd"
[flow.outcome]
mode = "draft_pr"
"#
        ))
        .unwrap()
        .flow
        .remove(0)
    }

    fn github() -> FakeGitHub {
        let github = FakeGitHub::new();
        github.add_issue(
            "o/r",
            Issue {
                number: 5,
                title: "Do it".into(),
                body: "please".into(),
                labels: vec!["inchworm".into()],
                url: "https://github.com/o/r/issues/5".into(),
            },
        );
        github
    }

    fn pr(branch_sha: &str) -> PullRequest {
        PullRequest {
            number: 77,
            head_sha: branch_sha.into(),
            is_draft: true,
            state: PrState::Open,
            url: "https://github.com/o/r/pull/77".into(),
        }
    }

    fn ctx<'a>(store: &'a Store, github: &'a FakeGitHub) -> RunContext<'a> {
        RunContext {
            store,
            github,
            slug: "o/r",
            repo_path: Path::new("/repo"),
            now: &|| 100,
        }
    }

    #[test]
    fn a_successful_run_records_the_pr_and_waits_for_ci() {
        let store = Store::open_in_memory().unwrap();
        let github = github();
        github.add_pr("o/r", "inchworm/5-a1", pr("deadbeef"));
        let (mut client, fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let summary = run_issue(&ctx(&store, &github), &flow(2, ""), 5, &mut client).unwrap();
        assert_eq!(summary.state, RunState::AwaitingCi);
        assert_eq!(summary.attempts, 1);
        assert_eq!(summary.pr.as_ref().unwrap().number, 77);
        assert!(summary.warnings.is_empty());
        let attempt = &store.attempts(summary.run_id).unwrap()[0];
        assert_eq!(attempt.pr_number, Some(77));
        assert_eq!(attempt.head_sha.as_deref(), Some("deadbeef"));
        assert_eq!(attempt.branch.as_deref(), Some("inchworm/5-a1"));
        assert_eq!(
            store.run(summary.run_id).unwrap().state,
            RunState::AwaitingCi
        );
        assert_eq!(github.issue("o/r", 5).unwrap().labels, ["inchworm:running"]);
        assert!(github.comments()[0].2.contains("pull/77"));
        assert_eq!(fake.assigned().len(), 2);
    }

    #[test]
    fn a_ready_for_review_pr_is_not_accepted_and_nothing_is_recorded_for_it() {
        let store = Store::open_in_memory().unwrap();
        let github = github();
        let mut ready = pr("deadbeef");
        ready.is_draft = false;
        github.add_pr("o/r", "inchworm/5-a1", ready);
        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let summary = run_issue(&ctx(&store, &github), &flow(1, ""), 5, &mut client).unwrap();
        assert_eq!(summary.state, RunState::Failed);
        let (class, raw) = summary.failure.unwrap();
        assert_eq!(class, FailureClass::AgentFailed);
        assert!(
            raw.contains("pull request #77") && raw.contains("not a draft"),
            "{raw}"
        );
        assert!(summary.pr.is_none());
        let attempt = &store.attempts(summary.run_id).unwrap()[0];
        assert_eq!(attempt.pr_number, None);
        assert_eq!(attempt.head_sha, None);
        assert_eq!(attempt.failure_class, Some(FailureClass::AgentFailed));
    }

    #[test]
    fn a_pr_touching_inchworm_is_rejected_even_when_nanna_reported_clean_output() {
        let store = Store::open_in_memory().unwrap();
        let github = github();
        github.add_pr("o/r", "inchworm/5-a1", pr("deadbeef"));
        github.set_pr_files("o/r", 77, &["src/lib.rs", ".inchworm/flows.toml"]);
        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let summary = run_issue(&ctx(&store, &github), &flow(2, ""), 5, &mut client).unwrap();
        assert_eq!(summary.state, RunState::Failed);
        let (class, raw) = summary.failure.unwrap();
        assert_eq!(class, FailureClass::ProtectedPathViolation);
        assert!(
            raw.contains("pull request #77") && raw.contains(".inchworm/flows.toml"),
            "{raw}"
        );
        assert!(summary.pr.is_none());
        assert!(
            store.attempts(summary.run_id).unwrap()[0]
                .pr_number
                .is_none()
        );
        assert!(github.comments()[0].2.contains("protected_path_violation"));
    }

    struct LookupFails(FakeGitHub);

    impl GitHub for LookupFails {
        fn open_issues_with_label(&self, r: &str, l: &str) -> Result<Vec<Issue>, GitHubError> {
            self.0.open_issues_with_label(r, l)
        }
        fn issue(&self, r: &str, n: u64) -> Result<Issue, GitHubError> {
            self.0.issue(r, n)
        }
        fn labels(&self, r: &str) -> Result<Vec<String>, GitHubError> {
            self.0.labels(r)
        }
        fn set_labels(&self, r: &str, n: u64, a: &[&str], d: &[&str]) -> Result<(), GitHubError> {
            self.0.set_labels(r, n, a, d)
        }
        fn comment(&self, r: &str, n: u64, b: &str) -> Result<(), GitHubError> {
            self.0.comment(r, n, b)
        }
        fn pr_for_branch(&self, _: &str, _: &str) -> Result<Option<PullRequest>, GitHubError> {
            Err(GitHubError::NotFound("pr lookup".into()))
        }
        fn pr_files(&self, r: &str, n: u64) -> Result<Vec<String>, GitHubError> {
            self.0.pr_files(r, n)
        }
    }

    #[test]
    fn a_failing_pr_lookup_fails_the_attempt() {
        let store = Store::open_in_memory().unwrap();
        let github = LookupFails(github());
        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let context = RunContext {
            store: &store,
            github: &github,
            slug: "o/r",
            repo_path: Path::new("/repo"),
            now: &|| 1,
        };
        let summary = run_issue(&context, &flow(1, ""), 5, &mut client).unwrap();
        let (class, raw) = summary.failure.unwrap();
        assert_eq!(class, FailureClass::AgentFailed);
        assert!(raw.contains("pr lookup"));
    }

    #[test]
    fn branches_stay_unique_across_runs_of_the_same_issue() {
        let store = Store::open_in_memory().unwrap();
        let github = github();
        github.add_pr("o/r", "inchworm/5-a1", pr("old"));
        let (mut client, fake) = connect(vec![
            Scenario::failed("result_failed_structured"),
            Scenario::success(),
            Scenario::success(),
        ]);
        client.initialize().unwrap();
        let first = run_issue(&ctx(&store, &github), &flow(1, ""), 5, &mut client).unwrap();
        assert_eq!(first.state, RunState::Failed);
        github.add_pr(
            "o/r",
            "inchworm/5-a2",
            PullRequest {
                number: 78,
                ..pr("new")
            },
        );
        let second = run_issue(&ctx(&store, &github), &flow(1, ""), 5, &mut client).unwrap();
        assert_eq!(second.state, RunState::AwaitingCi);
        assert_eq!(second.pr.unwrap().number, 78);
        let attempts = store.attempts(second.run_id).unwrap();
        assert_eq!(attempts[0].n, 1);
        assert_eq!(attempts[0].branch.as_deref(), Some("inchworm/5-a2"));
        let prompt = fake.assigned()[1]["description"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(prompt.contains("Branch for any pull request: inchworm/5-a2"));
    }

    #[test]
    fn a_failed_run_is_recorded_labelled_and_reported() {
        let store = Store::open_in_memory().unwrap();
        let github = github();
        let (mut client, _fake) = connect(vec![Scenario::failed("result_failed_structured")]);
        client.initialize().unwrap();
        let summary = run_issue(&ctx(&store, &github), &flow(3, ""), 5, &mut client).unwrap();
        assert_eq!(summary.state, RunState::Failed);
        assert_eq!(summary.attempts, 1);
        let (class, raw) = summary.failure.clone().unwrap();
        assert_eq!(class, FailureClass::MaxIterations);
        assert!(raw.contains("step `implement`"));
        let attempt = &store.attempts(summary.run_id).unwrap()[0];
        assert_eq!(attempt.failure_class, Some(FailureClass::MaxIterations));
        assert_eq!(github.issue("o/r", 5).unwrap().labels, ["inchworm:failed"]);
        assert!(github.comments()[0].2.contains("max_iterations"));
    }

    #[test]
    fn retryable_failures_run_again_with_the_previous_failure_in_the_prompt() {
        let store = Store::open_in_memory().unwrap();
        let github = github();
        github.add_pr("o/r", "inchworm/5-a2", pr("cafe"));
        let (mut client, fake) = connect(vec![
            Scenario::failed("result_failed_structured"),
            Scenario::success(),
            Scenario::success(),
        ]);
        client.initialize().unwrap();
        let flow = flow(3, "\"max_iterations\"");
        let summary = run_issue(&ctx(&store, &github), &flow, 5, &mut client).unwrap();
        assert_eq!(summary.state, RunState::AwaitingCi);
        assert_eq!(summary.attempts, 2);
        let attempts = store.attempts(summary.run_id).unwrap();
        assert_eq!(attempts[0].failure_class, Some(FailureClass::MaxIterations));
        assert_eq!(attempts[1].pr_number, Some(77));
        let second_prompt = fake.assigned()[1]["description"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            second_prompt
                .contains("The previous attempt failed: attempt 1 failed with `max_iterations`")
        );
        assert!(second_prompt.contains("inchworm/5-a2"));
    }

    #[test]
    fn retries_stop_at_max_attempts() {
        let store = Store::open_in_memory().unwrap();
        let github = github();
        let (mut client, fake) = connect(vec![
            Scenario::failed("result_failed_structured"),
            Scenario::failed("result_failed_structured"),
        ]);
        client.initialize().unwrap();
        let flow = flow(2, "\"max_iterations\"");
        let summary = run_issue(&ctx(&store, &github), &flow, 5, &mut client).unwrap();
        assert_eq!(summary.state, RunState::Failed);
        assert_eq!(summary.attempts, 2);
        assert_eq!(fake.assigned().len(), 2);
    }

    #[test]
    fn completed_steps_without_a_pr_fail_the_attempt() {
        let store = Store::open_in_memory().unwrap();
        let github = github();
        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let summary = run_issue(&ctx(&store, &github), &flow(1, ""), 5, &mut client).unwrap();
        let (class, raw) = summary.failure.unwrap();
        assert_eq!(class, FailureClass::AgentFailed);
        assert!(raw.contains("no pull request exists for branch `inchworm/5-a1`"));
    }

    #[test]
    fn a_run_cannot_be_repeated_while_the_previous_one_is_live() {
        let store = Store::open_in_memory().unwrap();
        let github = github();
        github.add_pr("o/r", "inchworm/5-a1", pr("x"));
        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        run_issue(&ctx(&store, &github), &flow(1, ""), 5, &mut client).unwrap();
        let err = run_issue(&ctx(&store, &github), &flow(1, ""), 5, &mut client).unwrap_err();
        assert!(
            matches!(err, RunError::AlreadyRun { issue: 5, .. }),
            "{err}"
        );
    }

    #[test]
    fn a_missing_issue_is_a_github_error_and_nothing_is_enqueued() {
        let store = Store::open_in_memory().unwrap();
        let github = FakeGitHub::new();
        let (mut client, _fake) = connect(vec![]);
        let err = run_issue(&ctx(&store, &github), &flow(1, ""), 5, &mut client).unwrap_err();
        assert!(matches!(err, RunError::GitHub(_)));
        assert!(store.next_queued("o/r").unwrap().is_none());
    }

    #[test]
    fn a_refused_spawn_in_a_single_step_flow_fails_as_spawn_refused() {
        let store = Store::open_in_memory().unwrap();
        let github = github();
        let (mut client, _fake) = connect(vec![Scenario::refused("assign_refused_legacy")]);
        client.initialize().unwrap();
        let mut flow = flow(1, "");
        flow.step.truncate(1);
        let summary = run_issue(&ctx(&store, &github), &flow, 5, &mut client).unwrap();
        assert_eq!(summary.failure.unwrap().0, FailureClass::SpawnRefused);
        assert!(summary.warnings.is_empty());
    }

    #[test]
    fn label_and_comment_failures_become_warnings() {
        let store = Store::open_in_memory().unwrap();
        let ghost = FakeGitHub::new();
        let mut warnings = Vec::new();
        let context = ctx(&store, &ghost);
        relabel(&context, 9, &["a"], &[], &mut warnings);
        note(&context, 9, "x", &mut warnings);
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].starts_with("could not update labels"));
        assert!(warnings[1].starts_with("could not comment"));
    }
}
