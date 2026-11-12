use std::path::Path;

use thiserror::Error;
use tokio::sync::OwnedSemaphorePermit;

use crate::clock::Clock;
use crate::config::{Flow, Trigger};
use crate::flow::{
    Dispatcher, FlowInput, FlowOutcome, PROTECTED_DIR, branch_name, nanna_report,
    protected_paths_in, run_flow,
};
use crate::github::PrState;
use crate::github::{GitHub, GitHubError, PullRequest};
use crate::model::{FailureClass, RunState};
use crate::outcome::{ConcludeContext, Concluded, DONE_LABEL, OutcomeEnv, Published, conclude};
use crate::store::{AttemptState, RunId, Store, StoreError};

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
    #[error("run {run} is already {state} and cannot be executed")]
    NotRunnable { run: RunId, state: RunState },
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
    pub clock: &'a dyn Clock,
}

pub(crate) fn trigger_label(flow: &Flow) -> &str {
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
                "pull request #{} touches protected {}/**: {}",
                pr.number,
                PROTECTED_DIR,
                violations.join(", ")
            ),
        ))
    }
}

pub fn enqueue_issue(
    ctx: &RunContext<'_>,
    flow: &Flow,
    issue_number: u64,
    warnings: &mut Vec<String>,
) -> Result<Option<RunId>, StoreError> {
    let run_id = ctx
        .store
        .enqueue_run(ctx.slug, issue_number, &flow.name, ctx.clock.now())?;
    if run_id.is_some() {
        relabel(
            ctx,
            issue_number,
            &[RUNNING_LABEL],
            &[trigger_label(flow)],
            warnings,
        );
    }
    Ok(run_id)
}

fn give_up(
    ctx: &RunContext<'_>,
    flow: &Flow,
    run_id: RunId,
    issue: u64,
    attempts: u32,
    failure: (FailureClass, String, u32),
    mut warnings: Vec<String>,
) -> Result<RunSummary, RunError> {
    let (class, raw, attempt_n) = failure;
    ctx.store
        .set_run_state(run_id, RunState::Failed, ctx.clock.now())?;
    relabel(
        ctx,
        issue,
        &[FAILED_LABEL],
        &[RUNNING_LABEL, trigger_label(flow)],
        &mut warnings,
    );
    note(
        ctx,
        issue,
        &format!(
            "inchworm gave up after {attempts} attempt(s): {}",
            describe_failure(class, &raw, attempt_n)
        ),
        &mut warnings,
    );
    Ok(RunSummary {
        run_id,
        attempts,
        state: RunState::Failed,
        pr: None,
        failure: Some((class, raw)),
        warnings,
    })
}

fn published_pr(ctx: &RunContext<'_>, attempt_pr: Option<u64>) -> Result<PullRequest, RunError> {
    let number = attempt_pr.ok_or_else(|| {
        StoreError::Corrupt("published attempt has no pull request number".into())
    })?;
    Ok(ctx.github.pr(ctx.slug, number)?)
}

fn settle_resumed(
    ctx: &RunContext<'_>,
    flow: &Flow,
    run_id: RunId,
    issue: u64,
    attempt: &crate::store::Attempt,
    warnings: &mut Vec<String>,
) -> Result<Option<(RunState, PullRequest)>, RunError> {
    if !matches!(
        attempt.state,
        AttemptState::Succeeded | AttemptState::Published
    ) {
        return Ok(None);
    }
    let pr = published_pr(ctx, attempt.pr_number)?;
    let target = match (attempt.state, pr.state) {
        (_, PrState::Merged) => RunState::Merged,
        (AttemptState::Succeeded, _) => RunState::PrOpen,
        _ => return Ok(None),
    };
    for step in [RunState::AwaitingCi, RunState::Judging] {
        if ctx.store.run(run_id)?.state.can_transition_to(step) {
            ctx.store.set_run_state(run_id, step, ctx.clock.now())?;
        }
    }
    if attempt.state == AttemptState::Published {
        ctx.store
            .settle_attempt(attempt.id, run_id, target, ctx.clock.now())?;
    } else {
        ctx.store.set_run_state(run_id, target, ctx.clock.now())?;
    }
    relabel(
        ctx,
        issue,
        &[DONE_LABEL],
        &[RUNNING_LABEL, trigger_label(flow)],
        warnings,
    );
    note(
        ctx,
        issue,
        &format!(
            "inchworm resumed after a restart and found {} already settled as {target}.",
            pr.url
        ),
        warnings,
    );
    Ok(Some((target, pr)))
}

async fn settle_published(
    ctx: &RunContext<'_>,
    env: &OutcomeEnv<'_>,
    flow: &Flow,
    issue: u64,
    attempt: &crate::store::Attempt,
    pr: &PullRequest,
    warnings: &mut Vec<String>,
) -> Result<Concluded, RunError> {
    let sha = attempt
        .head_sha
        .clone()
        .unwrap_or_else(|| pr.head_sha.clone());
    let conclude_ctx = ConcludeContext {
        store: ctx.store,
        github: ctx.github,
        clock: ctx.clock,
        slug: ctx.slug,
        repo_path: ctx.repo_path,
        issue,
    };
    let published = Published {
        run_id: attempt.run_id,
        attempt_id: attempt.id,
        attempt_n: attempt.n,
        pr,
        judged_sha: &sha,
        report: crate::judges::NannaReport {
            denial_count: attempt.nanna_denials.unwrap_or(0),
            qa_ok: attempt.nanna_qa_ok,
        },
    };
    Ok(conclude(&conclude_ctx, env, flow, &published, warnings).await?)
}

/// ```
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() {
/// use inchworm::clock::FakeClock;
/// use inchworm::config::FlowFile;
/// use inchworm::flow::Dispatcher;
/// use inchworm::github::{FakeGitHub, Issue};
/// use inchworm::judges::{LlmError, Scorer};
/// use inchworm::nanna_client::{AssignRequest, ClientError, TaskOutcome};
/// use inchworm::outcome::{CiPolicy, OutcomeEnv};
/// use inchworm::run::{run_issue, RunContext};
/// use inchworm::store::Store;
/// use std::sync::Arc;
/// use std::time::Duration;
///
/// struct Cancels;
/// impl Dispatcher for Cancels {
///     fn dispatch(&mut self, _: &AssignRequest, _: Duration) -> Result<TaskOutcome, ClientError> {
///         Ok(TaskOutcome::Cancelled)
///     }
/// }
/// struct NoModel;
/// impl Scorer for NoModel {
///     async fn complete(&self, _: &str, _: &str) -> Result<String, LlmError> {
///         unreachable!()
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
/// let clock = FakeClock::new(0);
/// let ctx = RunContext {
///     store: &store, github: &github, slug: "o/r",
///     repo_path: std::path::Path::new("/repo"), clock: &clock,
/// };
/// let env = OutcomeEnv {
///     scorer: &NoModel,
///     model_permit: Arc::new(tokio::sync::Semaphore::new(1)),
///     ci: CiPolicy::default(),
///     human_gate: inchworm::policy::HumanGate::NotConsulted,
/// };
/// let summary = run_issue(&ctx, &env, &file.flow[0], 1, &mut Cancels).await.unwrap();
/// assert_eq!(summary.state, inchworm::model::RunState::Failed);
/// # }
/// ```
pub async fn run_issue(
    ctx: &RunContext<'_>,
    env: &OutcomeEnv<'_>,
    flow: &Flow,
    issue_number: u64,
    dispatcher: &mut dyn Dispatcher,
) -> Result<RunSummary, RunError> {
    ctx.github.issue(ctx.slug, issue_number)?;
    let mut warnings = Vec::new();
    let run_id = enqueue_issue(ctx, flow, issue_number, &mut warnings)?.ok_or_else(|| {
        RunError::AlreadyRun {
            repo: ctx.slug.to_string(),
            issue: issue_number,
            flow: flow.name.clone(),
        }
    })?;
    let mut summary = execute_run(ctx, env, flow, run_id, dispatcher, None).await?;
    warnings.append(&mut summary.warnings);
    summary.warnings = warnings;
    Ok(summary)
}

pub async fn execute_run(
    ctx: &RunContext<'_>,
    env: &OutcomeEnv<'_>,
    flow: &Flow,
    run_id: RunId,
    dispatcher: &mut dyn Dispatcher,
    mut permit: Option<OwnedSemaphorePermit>,
) -> Result<RunSummary, RunError> {
    let run = ctx.store.run(run_id)?;
    let issue_number = run.issue;
    let issue = ctx.github.issue(ctx.slug, issue_number)?;
    let mut warnings = Vec::new();
    match run.state {
        RunState::Queued => ctx
            .store
            .set_run_state(run_id, RunState::Running, ctx.clock.now())?,
        RunState::Running | RunState::AwaitingCi | RunState::Judging => {}
        state => return Err(RunError::NotRunnable { run: run_id, state }),
    }

    let existing = ctx.store.attempts(run_id)?;
    let mut attempts = u32::try_from(existing.len()).unwrap_or(u32::MAX);
    let mut last_failure: Option<(FailureClass, String, u32)> = None;
    let mut pending_publish = None;
    if let Some(last) = existing.last() {
        match last.state {
            AttemptState::Published => pending_publish = Some(last.clone()),
            AttemptState::Failed => {
                if let (Some(class), Some(raw)) = (last.failure_class, last.raw_error.clone()) {
                    last_failure = Some((class, raw, last.n));
                }
            }
            AttemptState::Dispatched | AttemptState::Succeeded => {}
        }
    }

    if let Some(last) = existing.last()
        && let Some((state, pr)) =
            settle_resumed(ctx, flow, run_id, issue_number, last, &mut warnings)?
    {
        return Ok(RunSummary {
            run_id,
            attempts,
            state,
            pr: Some(pr),
            failure: None,
            warnings,
        });
    }

    loop {
        let (attempt, pr) = if let Some(attempt) = pending_publish.take() {
            if ctx.store.run(run_id)?.state == RunState::Running {
                ctx.store
                    .set_run_state(run_id, RunState::AwaitingCi, ctx.clock.now())?;
            }
            let pr = published_pr(ctx, attempt.pr_number)?;
            (attempt, pr)
        } else {
            if let Some((class, raw, n)) = &last_failure {
                let retry = flow.retry_on.contains(class) && attempts < flow.max_attempts;
                if !retry {
                    let failure = (*class, raw.clone(), *n);
                    return give_up(ctx, flow, run_id, issue_number, attempts, failure, warnings);
                }
                if ctx.store.run(run_id)?.state == RunState::Judging {
                    ctx.store
                        .set_run_state(run_id, RunState::Running, ctx.clock.now())?;
                }
            }
            let previous_failure = last_failure
                .as_ref()
                .map(|(class, raw, n)| describe_failure(*class, raw, *n));
            attempts += 1;
            let attempt = ctx.store.start_attempt(run_id, ctx.clock.now())?;
            let branch = branch_name(
                issue_number,
                ctx.store.issue_attempt_count(ctx.slug, issue_number)?,
            );
            let held = match permit.take() {
                Some(held) => held,
                None => match std::sync::Arc::clone(&env.model_permit)
                    .acquire_owned()
                    .await
                {
                    Ok(held) => held,
                    Err(_) => {
                        let raw = "the model permit was closed".to_string();
                        ctx.store.finish_attempt_failed(
                            attempt.id,
                            FailureClass::OrchestratorLost,
                            &raw,
                            ctx.clock.now(),
                        )?;
                        return give_up(
                            ctx,
                            flow,
                            run_id,
                            issue_number,
                            attempts,
                            (FailureClass::OrchestratorLost, raw, attempt.n),
                            warnings,
                        );
                    }
                },
            };
            let input = FlowInput {
                repo_path: ctx.repo_path,
                issue_number,
                issue_title: &issue.title,
                issue_body: &issue.body,
                attempt: attempt.n,
                branch: &branch,
                previous_failure: previous_failure.as_deref(),
            };
            let outcome = run_flow(flow, &input, dispatcher);
            drop(held);
            let published: Result<PullRequest, (FailureClass, String)> = match outcome {
                FlowOutcome::Completed { steps, .. } => {
                    match ctx.github.pr_for_branch(ctx.slug, &branch) {
                        Ok(Some(pr)) => match protected_pr_files(ctx, &pr) {
                            Err(failure) => Err(failure),
                            Ok(()) => {
                                let report = nanna_report(&steps);
                                ctx.store.record_pr(
                                    attempt.id,
                                    &branch,
                                    &pr.head_sha,
                                    pr.number,
                                )?;
                                ctx.store.mark_published(
                                    attempt.id,
                                    report.denial_count,
                                    report.qa_ok,
                                )?;
                                ctx.store.set_run_state(
                                    run_id,
                                    RunState::AwaitingCi,
                                    ctx.clock.now(),
                                )?;
                                note(
                                    ctx,
                                    issue_number,
                                    &format!(
                                        "inchworm opened {} for this issue from `{branch}` at `{}`.",
                                        pr.url, pr.head_sha
                                    ),
                                    &mut warnings,
                                );
                                Ok(pr)
                            }
                        },
                        Ok(None) => Err((
                            FailureClass::AgentFailed,
                            format!(
                                "the steps completed but no pull request exists for branch `{branch}`"
                            ),
                        )),
                        Err(err) => Err((FailureClass::AgentFailed, err.to_string())),
                    }
                }
                FlowOutcome::Failed {
                    step,
                    class,
                    raw_error,
                    ..
                } => Err((class, format!("step `{step}`: {raw_error}"))),
            };
            match published {
                Ok(pr) => (ctx.store.attempt(attempt.id)?, pr),
                Err((class, raw)) => {
                    ctx.store
                        .finish_attempt_failed(attempt.id, class, &raw, ctx.clock.now())?;
                    last_failure = Some((class, raw, attempt.n));
                    continue;
                }
            }
        };

        let concluded =
            settle_published(ctx, env, flow, issue_number, &attempt, &pr, &mut warnings).await?;
        match concluded {
            Concluded::Accepted { state } => {
                return Ok(RunSummary {
                    run_id,
                    attempts,
                    state,
                    pr: Some(pr),
                    failure: None,
                    warnings,
                });
            }
            Concluded::Rejected { class, raw } => {
                last_failure = Some((class, raw, attempt.n));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;
    use crate::config::MergeMethod;
    use crate::github::{FakeGitHub, Issue};
    use crate::nanna_client::fake::{Scenario, connect};
    use crate::policy::CheckRun;
    use crate::store::{AttemptState, StoreError};
    use crate::testing::{DRAFT_OUTCOME, FakeScorer, env, flow_with, github_with_issue, pr_at};

    fn flow(max_attempts: u32, retry_on: &str) -> Flow {
        flow_with(max_attempts, retry_on, DRAFT_OUTCOME)
    }

    fn github() -> FakeGitHub {
        github_with_issue()
    }

    fn pr(sha: &str) -> PullRequest {
        pr_at(77, sha)
    }

    fn ctx<'a>(store: &'a Store, github: &'a FakeGitHub, clock: &'a FakeClock) -> RunContext<'a> {
        RunContext {
            store,
            github,
            slug: "o/r",
            repo_path: Path::new("/repo"),
            clock,
        }
    }

    async fn go(
        store: &Store,
        github: &dyn GitHub,
        clock: &FakeClock,
        flow: &Flow,
        client: &mut dyn Dispatcher,
    ) -> Result<RunSummary, RunError> {
        let scorer = FakeScorer::new();
        let context = RunContext {
            store,
            github,
            slug: "o/r",
            repo_path: Path::new("/repo"),
            clock,
        };
        run_issue(&context, &env(&scorer), flow, 5, client).await
    }

    #[tokio::test]
    async fn a_successful_run_records_the_pr_and_leaves_it_a_draft() {
        let store = Store::open_in_memory().unwrap();
        let clock = FakeClock::new(100);
        let github = github();
        github.add_pr("o/r", "inchworm/5-a1", pr("deadbeef"));
        let (mut client, fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let summary = go(&store, &github, &clock, &flow(2, ""), &mut client)
            .await
            .unwrap();
        assert_eq!(summary.state, RunState::PrOpen);
        assert_eq!(summary.attempts, 1);
        assert_eq!(summary.pr.as_ref().unwrap().number, 77);
        assert!(summary.warnings.is_empty());
        let attempt = &store.attempts(summary.run_id).unwrap()[0];
        assert_eq!(attempt.pr_number, Some(77));
        assert_eq!(attempt.head_sha.as_deref(), Some("deadbeef"));
        assert_eq!(attempt.branch.as_deref(), Some("inchworm/5-a1"));
        assert_eq!(attempt.state, AttemptState::Succeeded);
        assert_eq!(store.run(summary.run_id).unwrap().state, RunState::PrOpen);
        assert_eq!(github.issue("o/r", 5).unwrap().labels, ["inchworm:done"]);
        assert!(github.comments()[0].2.contains("pull/77"));
        assert!(github.comments()[1].2.contains("outcome `draft_pr`"));
        assert!(github.pr("o/r", 77).unwrap().is_draft);
        assert_eq!(fake.assigned().len(), 2);
    }

    #[tokio::test]
    async fn a_pr_touching_inchworm_is_rejected_even_when_nanna_reported_clean_output() {
        let store = Store::open_in_memory().unwrap();
        let clock = FakeClock::new(100);
        let github = github();
        github.add_pr("o/r", "inchworm/5-a1", pr("deadbeef"));
        github.set_pr_files("o/r", 77, &["src/lib.rs", ".inchworm/flows.toml"]);
        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let summary = go(&store, &github, &clock, &flow(2, ""), &mut client)
            .await
            .unwrap();
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
        fn mark_draft(&self, r: &str, n: u64) -> Result<(), GitHubError> {
            self.0.mark_draft(r, n)
        }
        fn pr_for_branch(&self, _: &str, _: &str) -> Result<Option<PullRequest>, GitHubError> {
            Err(GitHubError::NotFound("pr lookup".into()))
        }
        fn pr_files(&self, r: &str, n: u64) -> Result<Vec<String>, GitHubError> {
            self.0.pr_files(r, n)
        }
        fn pr(&self, r: &str, n: u64) -> Result<PullRequest, GitHubError> {
            self.0.pr(r, n)
        }
        fn pr_patch(&self, r: &str, n: u64) -> Result<String, GitHubError> {
            self.0.pr_patch(r, n)
        }
        fn check_runs(&self, r: &str, s: &str) -> Result<Vec<CheckRun>, GitHubError> {
            self.0.check_runs(r, s)
        }
        fn mark_ready(&self, r: &str, n: u64) -> Result<(), GitHubError> {
            self.0.mark_ready(r, n)
        }
        fn merge_pr(&self, r: &str, n: u64, s: &str, m: MergeMethod) -> Result<(), GitHubError> {
            self.0.merge_pr(r, n, s, m)
        }
        fn required_checks(&self, r: &str, b: &str) -> Result<Option<Vec<String>>, GitHubError> {
            self.0.required_checks(r, b)
        }
    }

    #[tokio::test]
    async fn a_failing_pr_lookup_fails_the_attempt() {
        let store = Store::open_in_memory().unwrap();
        let clock = FakeClock::new(100);
        let github = LookupFails(github());
        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let summary = go(&store, &github, &clock, &flow(1, ""), &mut client)
            .await
            .unwrap();
        let (class, raw) = summary.failure.unwrap();
        assert_eq!(class, FailureClass::AgentFailed);
        assert!(raw.contains("pr lookup"));
    }

    #[tokio::test]
    async fn branches_stay_unique_across_runs_of_the_same_issue() {
        let store = Store::open_in_memory().unwrap();
        let clock = FakeClock::new(100);
        let github = github();
        github.add_pr("o/r", "inchworm/5-a1", pr("old"));
        let (mut client, fake) = connect(vec![
            Scenario::failed("result_failed_structured"),
            Scenario::success(),
            Scenario::success(),
        ]);
        client.initialize().unwrap();
        let first = go(&store, &github, &clock, &flow(1, ""), &mut client)
            .await
            .unwrap();
        assert_eq!(first.state, RunState::Failed);
        github.add_pr(
            "o/r",
            "inchworm/5-a2",
            PullRequest {
                number: 78,
                ..pr("new")
            },
        );
        let second = go(&store, &github, &clock, &flow(1, ""), &mut client)
            .await
            .unwrap();
        assert_eq!(second.state, RunState::PrOpen);
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

    #[tokio::test]
    async fn a_failed_run_is_recorded_labelled_and_reported() {
        let store = Store::open_in_memory().unwrap();
        let clock = FakeClock::new(100);
        let github = github();
        let (mut client, _fake) = connect(vec![Scenario::failed("result_failed_structured")]);
        client.initialize().unwrap();
        let summary = go(&store, &github, &clock, &flow(3, ""), &mut client)
            .await
            .unwrap();
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

    #[tokio::test]
    async fn retryable_failures_run_again_with_the_previous_failure_in_the_prompt() {
        let store = Store::open_in_memory().unwrap();
        let clock = FakeClock::new(100);
        let github = github();
        github.add_pr("o/r", "inchworm/5-a2", pr("cafe"));
        let (mut client, fake) = connect(vec![
            Scenario::failed("result_failed_structured"),
            Scenario::success(),
            Scenario::success(),
        ]);
        client.initialize().unwrap();
        let flow = flow(3, "\"max_iterations\"");
        let summary = go(&store, &github, &clock, &flow, &mut client)
            .await
            .unwrap();
        assert_eq!(summary.state, RunState::PrOpen);
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

    #[tokio::test]
    async fn retries_stop_at_max_attempts() {
        let store = Store::open_in_memory().unwrap();
        let clock = FakeClock::new(100);
        let github = github();
        let (mut client, fake) = connect(vec![
            Scenario::failed("result_failed_structured"),
            Scenario::failed("result_failed_structured"),
        ]);
        client.initialize().unwrap();
        let flow = flow(2, "\"max_iterations\"");
        let summary = go(&store, &github, &clock, &flow, &mut client)
            .await
            .unwrap();
        assert_eq!(summary.state, RunState::Failed);
        assert_eq!(summary.attempts, 2);
        assert_eq!(fake.assigned().len(), 2);
    }

    #[tokio::test]
    async fn completed_steps_without_a_pr_fail_the_attempt() {
        let store = Store::open_in_memory().unwrap();
        let clock = FakeClock::new(100);
        let github = github();
        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let summary = go(&store, &github, &clock, &flow(1, ""), &mut client)
            .await
            .unwrap();
        let (class, raw) = summary.failure.unwrap();
        assert_eq!(class, FailureClass::AgentFailed);
        assert!(raw.contains("no pull request exists for branch `inchworm/5-a1`"));
    }

    #[tokio::test]
    async fn a_run_cannot_be_repeated_while_the_previous_one_is_live() {
        let store = Store::open_in_memory().unwrap();
        let clock = FakeClock::new(100);
        let github = github();
        store.enqueue_run("o/r", 5, "issue", 1).unwrap().unwrap();
        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let err = go(&store, &github, &clock, &flow(1, ""), &mut client)
            .await
            .unwrap_err();
        assert!(
            matches!(err, RunError::AlreadyRun { issue: 5, .. }),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_missing_issue_is_a_github_error_and_nothing_is_enqueued() {
        let store = Store::open_in_memory().unwrap();
        let clock = FakeClock::new(100);
        let github = FakeGitHub::new();
        let (mut client, _fake) = connect(vec![]);
        let err = go(&store, &github, &clock, &flow(1, ""), &mut client)
            .await
            .unwrap_err();
        assert!(matches!(err, RunError::GitHub(_)));
        assert!(store.next_queued("o/r").unwrap().is_none());
    }

    #[tokio::test]
    async fn a_refused_spawn_in_a_single_step_flow_fails_as_spawn_refused() {
        let store = Store::open_in_memory().unwrap();
        let clock = FakeClock::new(100);
        let github = github();
        let (mut client, _fake) = connect(vec![Scenario::refused("assign_refused_legacy")]);
        client.initialize().unwrap();
        let mut flow = flow(1, "");
        flow.step.truncate(1);
        let summary = go(&store, &github, &clock, &flow, &mut client)
            .await
            .unwrap();
        assert_eq!(summary.failure.unwrap().0, FailureClass::SpawnRefused);
        assert!(summary.warnings.is_empty());
    }

    #[tokio::test]
    async fn label_and_comment_failures_become_warnings() {
        let store = Store::open_in_memory().unwrap();
        let clock = FakeClock::new(100);
        let ghost = FakeGitHub::new();
        let mut warnings = Vec::new();
        let context = ctx(&store, &ghost, &clock);
        relabel(&context, 9, &["a"], &[], &mut warnings);
        note(&context, 9, "x", &mut warnings);
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].starts_with("could not update labels"));
        assert!(warnings[1].starts_with("could not comment"));
    }

    #[tokio::test]
    async fn a_closed_model_permit_fails_the_attempt_instead_of_hanging() {
        let store = Store::open_in_memory().unwrap();
        let clock = FakeClock::new(100);
        let github = github();
        let scorer = FakeScorer::new();
        let env = env(&scorer);
        env.model_permit.close();
        let (mut client, _fake) = connect(vec![]);
        let summary = run_issue(
            &ctx(&store, &github, &clock),
            &env,
            &flow(1, ""),
            5,
            &mut client,
        )
        .await
        .unwrap();
        let (class, raw) = summary.failure.unwrap();
        assert_eq!(class, FailureClass::OrchestratorLost);
        assert!(raw.contains("permit was closed"));
    }

    #[tokio::test]
    async fn a_published_attempt_without_a_pr_number_is_a_corrupt_store() {
        let store = Store::open_in_memory().unwrap();
        let clock = FakeClock::new(100);
        let github = github();
        let run = store.enqueue_run("o/r", 5, "issue", 1).unwrap().unwrap();
        store.set_run_state(run, RunState::Running, 2).unwrap();
        let attempt = store.start_attempt(run, 3).unwrap();
        store.mark_published(attempt.id, 0, None).unwrap();
        let scorer = FakeScorer::new();
        let (mut client, _fake) = connect(vec![]);
        let err = execute_run(
            &ctx(&store, &github, &clock),
            &env(&scorer),
            &flow(1, ""),
            run,
            &mut client,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, RunError::Store(StoreError::Corrupt(_))),
            "{err}"
        );
    }

    #[tokio::test]
    async fn an_unrecovered_dispatched_attempt_does_not_block_a_fresh_attempt() {
        let store = Store::open_in_memory().unwrap();
        let clock = FakeClock::new(100);
        let github = github();
        github.add_pr("o/r", "inchworm/5-a2", pr("cafe"));
        let run = store.enqueue_run("o/r", 5, "issue", 1).unwrap().unwrap();
        store.set_run_state(run, RunState::Running, 2).unwrap();
        store.start_attempt(run, 3).unwrap();
        let scorer = FakeScorer::new();
        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let summary = execute_run(
            &ctx(&store, &github, &clock),
            &env(&scorer),
            &flow(2, ""),
            run,
            &mut client,
            None,
        )
        .await
        .unwrap();
        assert_eq!(summary.state, RunState::PrOpen);
        assert_eq!(summary.attempts, 2);
    }
}
