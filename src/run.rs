use std::path::Path;

use thiserror::Error;
use tokio::sync::OwnedSemaphorePermit;

use crate::clock::Clock;
use crate::config::{Flow, Trigger};
use crate::flow::{
    Dispatcher, FlowInput, FlowOutcome, PROTECTED_DIR, PROTECTED_FILE, branch_name, nanna_report,
    protected_paths_in, run_flow,
};
use crate::github::{GitHub, GitHubError, Issue, PrState, PullRequest, issue_branch_attempt};
use crate::model::{FailureClass, RunState};
use crate::outcome::{ConcludeContext, Concluded, DONE_LABEL, OutcomeEnv, Published, conclude};
use crate::store::{AttemptState, RunId, Store, StoreError, Subject};

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
    #[error("{repo}#{issue} cannot be run: {reason}")]
    NotRunnable {
        repo: String,
        issue: u64,
        reason: String,
    },
    #[error("run {run} is already {state} and cannot be executed")]
    RunNotRunnable { run: RunId, state: RunState },
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

struct RunGuard<'a, 'c> {
    ctx: &'a RunContext<'c>,
    run_id: RunId,
    issue: u64,
    armed: bool,
}

impl<'a, 'c> RunGuard<'a, 'c> {
    fn new(ctx: &'a RunContext<'c>, run_id: RunId, issue: u64) -> Self {
        RunGuard {
            ctx,
            run_id,
            issue,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RunGuard<'_, '_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let now = self.ctx.clock.now();
        if let Ok(attempts) = self.ctx.store.attempts(self.run_id) {
            for attempt in attempts
                .iter()
                .filter(|attempt| attempt.state == AttemptState::Dispatched)
            {
                let _ = self.ctx.store.finish_attempt_failed(
                    attempt.id,
                    FailureClass::OrchestratorLost,
                    "inchworm stopped before this attempt finished",
                    now,
                );
            }
        }
        let _ = self
            .ctx
            .store
            .set_run_state(self.run_id, RunState::Failed, now);
        let _ = self.ctx.github.set_labels(
            self.ctx.slug,
            self.issue,
            &[FAILED_LABEL],
            &[RUNNING_LABEL],
        );
        let _ = self.ctx.github.comment(
            self.ctx.slug,
            self.issue,
            "inchworm stopped before this run finished; the run is marked failed and can be started again.",
        );
    }
}

pub const TRUSTED_AUTHOR_ASSOCIATIONS: [&str; 3] = ["OWNER", "MEMBER", "COLLABORATOR"];

pub(crate) fn check_runnable(
    ctx: &RunContext<'_>,
    flow: &Flow,
    issue: &Issue,
    number: u64,
) -> Result<(), RunError> {
    let refuse = |reason: String| RunError::NotRunnable {
        repo: ctx.slug.to_string(),
        issue: number,
        reason,
    };
    if !issue.open {
        return Err(refuse("the issue is not open".to_string()));
    }
    let label = trigger_label(flow);
    if !issue.labels.iter().any(|l| l == label) {
        return Err(refuse(format!(
            "the issue does not carry the `{label}` label"
        )));
    }
    if !TRUSTED_AUTHOR_ASSOCIATIONS.contains(&issue.author_association.as_str()) {
        let shown = if issue.author_association.is_empty() {
            "unknown"
        } else {
            issue.author_association.as_str()
        };
        return Err(refuse(format!(
            "the issue was written by a `{shown}` user and its text would be given to an agent; inchworm only runs issues written by the repository's owner, members or collaborators ({})",
            TRUSTED_AUTHOR_ASSOCIATIONS.join(", ")
        )));
    }
    let upcoming = ctx.store.issue_attempt_count(ctx.slug, number)? + 1;
    let open = ctx.github.open_prs_for_issue(ctx.slug, number)?;
    if let Some((branch, pr)) = open
        .iter()
        .find(|(branch, _)| issue_branch_attempt(branch, number).is_some_and(|n| n < upcoming))
    {
        return Err(refuse(format!(
            "a pull request from an earlier attempt is still open on `{branch}`: {}",
            pr.url
        )));
    }
    Ok(())
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

pub const MAX_PUBLIC_TEXT_CHARS: usize = 2000;

/// ```
/// use inchworm::run::public_text;
///
/// assert_eq!(public_text("ping @octocat\u{1b}[31m"), "ping \u{ff20}octocat[31m");
/// assert!(public_text(&"x".repeat(100_000)).chars().count() < 2200);
/// ```
pub fn public_text(raw: &str) -> String {
    let mut shown = String::new();
    let mut count = 0usize;
    let mut cut = 0usize;
    for c in raw.chars() {
        let c = match c {
            '@' => '\u{ff20}',
            '\n' | '\t' => c,
            '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}' => continue,
            c if c.is_control() => continue,
            c => c,
        };
        if count < MAX_PUBLIC_TEXT_CHARS {
            shown.push(c);
            count += 1;
        } else {
            cut += 1;
        }
    }
    if cut > 0 {
        shown.push_str(&format!(
            "\n[{cut} more characters left out; the full text is in inchworm's store]"
        ));
    }
    shown
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
                "pull request #{} ({}) touches protected {}/** or {}: {}",
                pr.number,
                pr.url,
                PROTECTED_DIR,
                PROTECTED_FILE,
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
    let run_id = ctx.store.enqueue_run(
        ctx.slug,
        &Subject::issue(issue_number)?,
        &flow.name,
        ctx.clock.now(),
    )?;
    if run_id.is_some() {
        relabel(
            ctx,
            issue_number,
            &[RUNNING_LABEL],
            &[trigger_label(flow), FAILED_LABEL],
            warnings,
        );
    }
    Ok(run_id)
}

#[derive(Debug, Clone)]
enum RetryBlock {
    Leftover(PullRequest),
    Reason(String),
}

fn give_up(
    ctx: &RunContext<'_>,
    flow: &Flow,
    run_id: RunId,
    issue: u64,
    attempts: u32,
    failure: (FailureClass, String, u32, Option<RetryBlock>),
    mut warnings: Vec<String>,
) -> Result<RunSummary, RunError> {
    let (class, raw, attempt_n, block) = failure;
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
            "inchworm gave up after {attempts} attempt(s): {}{}",
            public_text(&describe_failure(class, &raw, attempt_n)),
            match &block {
                Some(RetryBlock::Leftover(pr)) => format!(
                    "\n\nPull request {} was left open and was not retried; inchworm does not close pull requests, so close it before running this issue again.",
                    pr.url
                ),
                Some(RetryBlock::Reason(why)) => format!("\n\n{why}."),
                None => String::new(),
            }
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

pub fn preflight(ctx: &RunContext<'_>, flow: &Flow, issue_number: u64) -> Result<(), RunError> {
    let issue = ctx.github.issue(ctx.slug, issue_number)?;
    if ctx
        .store
        .open_run(ctx.slug, issue_number, &flow.name)?
        .is_some()
    {
        return Err(already_run(ctx, flow, issue_number));
    }
    check_runnable(ctx, flow, &issue, issue_number)
}

fn already_run(ctx: &RunContext<'_>, flow: &Flow, issue_number: u64) -> RunError {
    RunError::AlreadyRun {
        repo: ctx.slug.to_string(),
        issue: issue_number,
        flow: flow.name.clone(),
    }
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
///     open: true, author_association: "OWNER".into(),
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
///     merge_grant: inchworm::policy::MergeGrant::Denied,
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
    preflight(ctx, flow, issue_number)?;
    let mut warnings = Vec::new();
    let run_id = enqueue_issue(ctx, flow, issue_number, &mut warnings)?
        .ok_or_else(|| already_run(ctx, flow, issue_number))?;
    let mut guard = RunGuard::new(ctx, run_id, issue_number);
    let mut summary = execute_run(ctx, env, flow, run_id, dispatcher, None).await?;
    guard.disarm();
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
    let issue_number = run
        .issue_number()
        .ok_or_else(|| StoreError::Corrupt(format!("run {} is not for an issue", run.id)))?;
    let issue = ctx.github.issue(ctx.slug, issue_number)?;
    let mut warnings = Vec::new();
    match run.state {
        RunState::Queued => ctx
            .store
            .set_run_state(run_id, RunState::Running, ctx.clock.now())?,
        RunState::Running | RunState::AwaitingCi | RunState::Judging => {}
        state => return Err(RunError::RunNotRunnable { run: run_id, state }),
    }

    let existing = ctx.store.attempts(run_id)?;
    let mut attempts = u32::try_from(existing.len()).unwrap_or(u32::MAX);
    let mut last_failure: Option<(FailureClass, String, u32)> = None;
    let mut pending_publish = None;
    let mut retry_block: Option<RetryBlock> = None;
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
                let retry = retry_block.is_none()
                    && flow.retry_on.contains(class)
                    && attempts < flow.max_attempts
                    && dispatcher.is_usable();
                if !retry {
                    let failure = (*class, raw.clone(), *n, retry_block.take());
                    return give_up(ctx, flow, run_id, issue_number, attempts, failure, warnings);
                }
                if matches!(
                    ctx.store.run(run_id)?.state,
                    RunState::Judging | RunState::AwaitingCi
                ) {
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
                            (FailureClass::OrchestratorLost, raw, attempt.n, None),
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
            let mut block: Option<RetryBlock> = None;
            let published: Result<PullRequest, (FailureClass, String)> = match outcome {
                FlowOutcome::Completed { steps, .. } => {
                    match ctx.github.pr_for_branch(ctx.slug, &branch) {
                        Ok(Some(pr)) if pr.state != PrState::Open => Err((
                            FailureClass::AgentFailed,
                            format!(
                                "pull request #{} ({}) is {:?}, not open",
                                pr.number, pr.url, pr.state
                            ),
                        )),
                        Ok(Some(pr)) if !pr.is_draft => {
                            let failure = (
                                FailureClass::AgentFailed,
                                format!(
                                    "pull request #{} ({}) is not a draft; inchworm only accepts draft pull requests",
                                    pr.number, pr.url
                                ),
                            );
                            block = Some(RetryBlock::Leftover(pr));
                            Err(failure)
                        }
                        Ok(Some(pr)) => match protected_pr_files(ctx, &pr) {
                            Err(failure) => {
                                block = Some(RetryBlock::Leftover(pr));
                                Err(failure)
                            }
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
                                        "inchworm found {} for this issue on `{branch}` at `{}` and is waiting for CI.",
                                        pr.url, pr.head_sha
                                    ),
                                    &mut warnings,
                                );
                                Ok(pr)
                            }
                        },
                        Ok(None) => {
                            block = Some(RetryBlock::Reason(
                                "replaying the steps cannot create a pull request, so this is not retried"
                                    .to_string(),
                            ));
                            Err((
                                FailureClass::AgentFailed,
                                format!(
                                    "the steps completed but no pull request exists for branch `{branch}`; inchworm does not publish, so nothing it runs creates one"
                                ),
                            ))
                        }
                        Err(err) => {
                            block = Some(RetryBlock::Reason(
                                "the pull request lookup failed, so replaying the steps would not help"
                                    .to_string(),
                            ));
                            Err((FailureClass::AgentFailed, err.to_string()))
                        }
                    }
                }
                FlowOutcome::Failed {
                    step,
                    class,
                    raw_error,
                    ..
                } => {
                    match ctx.github.pr_for_branch(ctx.slug, &branch) {
                        Ok(Some(pr)) => block = Some(RetryBlock::Leftover(pr)),
                        Ok(None) => {}
                        Err(err) => {
                            block = Some(RetryBlock::Reason(format!(
                                "could not check whether a pull request was left on `{branch}` ({}), so the attempt is not retried",
                                public_text(&err.to_string())
                            )));
                        }
                    }
                    Err((class, format!("step `{step}`: {raw_error}")))
                }
            };
            match published {
                Ok(pr) => (ctx.store.attempt(attempt.id)?, pr),
                Err((class, raw)) => {
                    ctx.store
                        .finish_attempt_failed(attempt.id, class, &raw, ctx.clock.now())?;
                    last_failure = Some((class, raw, attempt.n));
                    retry_block = block;
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
                retry_block = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;
    use crate::config::{FlowFile, MergeMethod};
    use crate::github::{FakeGitHub, Issue};
    use crate::nanna_client::fake::{Scenario, connect};
    use crate::policy::CheckRun;
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
    async fn a_permit_handed_to_execute_run_is_used_instead_of_taking_another() {
        let store = Store::open_in_memory().unwrap();
        let clock = FakeClock::new(100);
        let github = github();
        github.add_pr("o/r", "inchworm/5-a1", pr("deadbeef"));
        let run = store
            .enqueue_run("o/r", &Subject::issue(5).unwrap(), "issue", 1)
            .unwrap()
            .unwrap();
        let scorer = FakeScorer::new();
        let environment = env(&scorer);
        let held = std::sync::Arc::clone(&environment.model_permit)
            .acquire_owned()
            .await
            .unwrap();
        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let summary = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            execute_run(
                &ctx(&store, &github, &clock),
                &environment,
                &flow(1, ""),
                run,
                &mut client,
                Some(held),
            ),
        )
        .await
        .expect("a second permit was requested while the only one was held")
        .unwrap();
        assert_eq!(summary.state, RunState::PrOpen);
    }

    #[tokio::test]
    async fn the_give_up_comment_is_sanitised_and_capped() {
        let store = Store::open_in_memory().unwrap();
        let clock = FakeClock::new(100);
        let github = github();
        let run = store
            .enqueue_run("o/r", &Subject::issue(5).unwrap(), "issue", 1)
            .unwrap()
            .unwrap();
        store.set_run_state(run, RunState::Running, 2).unwrap();
        let attempt = store.start_attempt(run, 3).unwrap();
        let nasty = format!("\u{1b}[31mred{}", "y".repeat(5000));
        store
            .finish_attempt_failed(attempt.id, FailureClass::AgentFailed, &nasty, 4)
            .unwrap();
        let scorer = FakeScorer::new();
        let (mut client, _fake) = connect(vec![]);
        client.initialize().unwrap();
        let summary = execute_run(
            &ctx(&store, &github, &clock),
            &env(&scorer),
            &flow(1, ""),
            run,
            &mut client,
            None,
        )
        .await
        .unwrap();
        assert_eq!(summary.state, RunState::Failed);
        let comments = github.comments();
        let gave_up = comments
            .iter()
            .find(|c| c.2.contains("gave up"))
            .expect("a give-up comment");
        assert!(!gave_up.2.contains('\u{1b}'));
        assert!(gave_up.2.contains("more characters left out"));
        assert!(gave_up.2.len() < 2600);
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
        fn commit_statuses(&self, r: &str, s: &str) -> Result<Vec<CheckRun>, GitHubError> {
            self.0.commit_statuses(r, s)
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
        fn open_prs_for_issue(
            &self,
            r: &str,
            n: u64,
        ) -> Result<Vec<(String, PullRequest)>, GitHubError> {
            self.0.open_prs_for_issue(r, n)
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
        github.set_labels("o/r", 5, &["inchworm"], &[]).unwrap();
        let mut closed = github.pr("o/r", 77).unwrap();
        closed.state = PrState::Closed;
        github.add_pr("o/r", "inchworm/5-a1", closed);
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
        assert!(!prompt.contains("inchworm/5-a2"));
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
        assert!(!second_prompt.contains("inchworm/5-a2"));
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
        store
            .enqueue_run("o/r", &Subject::issue(5).unwrap(), "issue", 1)
            .unwrap()
            .unwrap();
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
        let run = store
            .enqueue_run("o/r", &Subject::issue(5).unwrap(), "issue", 1)
            .unwrap()
            .unwrap();
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
        let run = store
            .enqueue_run("o/r", &Subject::issue(5).unwrap(), "issue", 1)
            .unwrap()
            .unwrap();
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

    fn run4(
        ctx: &RunContext<'_>,
        flow: &Flow,
        issue: u64,
        dispatcher: &mut dyn Dispatcher,
    ) -> Result<RunSummary, RunError> {
        let scorer = FakeScorer::new();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run_issue(ctx, &env(&scorer), flow, issue, dispatcher))
    }

    fn ctx4<'a>(store: &'a Store, github: &'a FakeGitHub) -> RunContext<'a> {
        let clock: &'static FakeClock = Box::leak(Box::new(FakeClock::new(100)));
        RunContext {
            store,
            github,
            slug: "o/r",
            repo_path: Path::new("/repo"),
            clock,
        }
    }

    fn flow4(max_attempts: u32, retry_on: &str) -> Flow {
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
required_checks = []
"#
        ))
        .unwrap()
        .flow
        .remove(0)
    }

    fn github4() -> FakeGitHub {
        let github = FakeGitHub::new();
        github.add_issue(
            "o/r",
            Issue {
                number: 5,
                title: "Do it".into(),
                body: "please".into(),
                labels: vec!["inchworm".into()],
                url: "https://github.com/o/r/issues/5".into(),
                open: true,
                author_association: "OWNER".into(),
            },
        );
        github
    }

    fn pr4(branch_sha: &str) -> PullRequest {
        PullRequest {
            number: 77,
            head_sha: branch_sha.into(),
            is_draft: true,
            state: PrState::Open,
            url: "https://github.com/o/r/pull/77".into(),
            base_ref: "main".into(),
            is_cross_repository: false,
        }
    }

    #[test]
    fn a_successful_run_records_the_pr_and_waits_for_ci() {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        github.add_pr("o/r", "inchworm/5-a1", pr4("deadbeef"));
        let (mut client, fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let summary = run4(&ctx4(&store, &github), &flow4(2, ""), 5, &mut client).unwrap();
        assert_eq!(summary.state, RunState::PrOpen);
        assert_eq!(summary.attempts, 1);
        assert_eq!(summary.pr.as_ref().unwrap().number, 77);
        assert!(summary.warnings.is_empty());
        let attempt = &store.attempts(summary.run_id).unwrap()[0];
        assert_eq!(attempt.state, crate::store::AttemptState::Succeeded);
        assert!(attempt.ended_at.is_some());
        assert_eq!(attempt.failure_class, None);
        assert_eq!(attempt.pr_number, Some(77));
        assert_eq!(attempt.head_sha.as_deref(), Some("deadbeef"));
        assert_eq!(attempt.branch.as_deref(), Some("inchworm/5-a1"));
        assert_eq!(store.run(summary.run_id).unwrap().state, RunState::PrOpen);
        assert_eq!(github.issue("o/r", 5).unwrap().labels, ["inchworm:done"]);
        assert_eq!(github.comments().len(), 2, "{:?}", github.comments());
        assert!(github.comments()[0].2.contains("pull/77"));
        assert!(github.comments()[1].2.contains("stays a draft"));
        assert_eq!(fake.assigned().len(), 2);
    }

    fn refused4(github: &FakeGitHub, store: &Store) -> String {
        let err = run4(&ctx4(store, github), &flow4(1, ""), 5, &mut Panics).unwrap_err();
        assert!(
            matches!(err, RunError::NotRunnable { issue: 5, .. }),
            "{err:?}"
        );
        assert!(
            store.run(1).is_err(),
            "a refused issue must not create a run"
        );
        assert!(github.comments().is_empty());
        err.to_string()
    }

    fn change_issue(github: &FakeGitHub, edit: impl FnOnce(&mut Issue)) {
        let mut issue = github.issue("o/r", 5).unwrap();
        edit(&mut issue);
        github.add_issue("o/r", issue);
    }

    #[test]
    fn a_closed_issue_is_refused_before_anything_is_created() {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        change_issue(&github, |i| i.open = false);
        assert!(refused4(&github, &store).contains("not open"));
        assert_eq!(github.issue("o/r", 5).unwrap().labels, ["inchworm"]);
    }

    #[test]
    fn an_issue_without_the_trigger_label_is_refused() {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        change_issue(&github, |i| i.labels = vec!["bug".into()]);
        let reason = refused4(&github, &store);
        assert!(
            reason.contains("does not carry the `inchworm` label"),
            "{reason}"
        );
    }

    #[test]
    fn only_issues_written_by_owners_members_and_collaborators_are_run() {
        for untrusted in [
            "NONE",
            "CONTRIBUTOR",
            "FIRST_TIME_CONTRIBUTOR",
            "FIRST_TIMER",
            "MANNEQUIN",
            "owner",
            "",
        ] {
            let store = Store::open_in_memory().unwrap();
            let github = github4();
            change_issue(&github, |i| i.author_association = untrusted.into());
            let reason = refused4(&github, &store);
            let shown = if untrusted.is_empty() {
                "unknown"
            } else {
                untrusted
            };
            assert!(reason.contains(&format!("`{shown}` user")), "{reason}");
        }
        for trusted in ["OWNER", "MEMBER", "COLLABORATOR"] {
            let store = Store::open_in_memory().unwrap();
            let github = github4();
            change_issue(&github, |i| i.author_association = trusted.into());
            github.add_pr("o/r", "inchworm/5-a1", pr4("deadbeef"));
            let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
            client.initialize().unwrap();
            let summary = run4(&ctx4(&store, &github), &flow4(1, ""), 5, &mut client).unwrap();
            assert_eq!(summary.state, RunState::PrOpen, "{trusted}");
        }
    }

    #[test]
    fn an_issue_whose_earlier_attempt_left_a_pull_request_open_is_refused() {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        github.add_pr("o/r", "inchworm/5-a1", pr4("deadbeef"));
        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let first = run4(&ctx4(&store, &github), &flow4(1, ""), 5, &mut client).unwrap();
        assert_eq!(first.state, RunState::PrOpen);
        change_issue(&github, |i| i.labels.push("inchworm".into()));

        let err = run4(&ctx4(&store, &github), &flow4(1, ""), 5, &mut Panics).unwrap_err();
        let reason = err.to_string();
        assert!(
            matches!(err, RunError::NotRunnable { issue: 5, .. }),
            "{reason}"
        );
        assert!(
            reason.contains("earlier attempt")
                && reason.contains("inchworm/5-a1")
                && reason.contains("pull/77"),
            "{reason}"
        );
        assert!(
            store.run(2).is_err(),
            "a refused rerun must not create a run"
        );
    }

    #[test]
    fn pull_requests_for_other_issues_do_not_block_a_run() {
        let github = github4();
        github.add_pr("o/r", "inchworm/50-a1", pr4("aaaa"));
        github.add_pr("o/r", "inchworm/6-a1", pr4("bbbb"));
        github.add_pr("o/r", "feature/5-a1", pr4("cccc"));
        github.add_pr("o/r", "inchworm/5-a1", pr4("deadbeef"));
        let mut closed = pr4("dddd");
        closed.state = crate::github::PrState::Closed;
        github.add_pr("o/r2", "inchworm/5-a1", closed);
        let open = github.open_prs_for_issue("o/r", 5).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].0, "inchworm/5-a1");
        assert_eq!(open[0].1.head_sha, "deadbeef");
        assert!(github.open_prs_for_issue("o/r2", 5).unwrap().is_empty());
        assert!(github.open_prs_for_issue("o/none", 5).unwrap().is_empty());
    }

    struct Panics;
    impl Dispatcher for Panics {
        fn dispatch(
            &mut self,
            _: &crate::nanna_client::AssignRequest,
            _: std::time::Duration,
        ) -> Result<crate::nanna_client::TaskOutcome, crate::nanna_client::ClientError> {
            panic!("dispatcher blew up");
        }
    }

    #[test]
    fn a_panic_mid_run_leaves_a_failed_run_and_the_issue_can_be_run_again() {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run4(&ctx4(&store, &github), &flow4(2, ""), 5, &mut Panics)
        }));
        let payload = result.unwrap_err();
        assert!(
            payload.downcast_ref::<&str>().is_some() || payload.downcast_ref::<String>().is_some()
        );

        let run = store.run(1).unwrap();
        assert_eq!(run.state, RunState::Failed);
        let attempts = store.attempts(run.id).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(
            attempts[0].failure_class,
            Some(FailureClass::OrchestratorLost)
        );
        assert!(attempts[0].ended_at.is_some());
        let labels = github.issue("o/r", 5).unwrap().labels;
        assert_eq!(labels, ["inchworm:failed"]);
        assert!(
            github.comments()[0].2.contains("stopped"),
            "{:?}",
            github.comments()
        );

        change_issue(&github, |i| i.labels.push("inchworm".into()));
        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        github.add_pr("o/r", "inchworm/5-a2", pr4("deadbeef"));
        let again = run4(&ctx4(&store, &github), &flow4(1, ""), 5, &mut client).unwrap();
        assert_eq!(again.state, RunState::PrOpen);
    }

    #[test]
    fn a_run_that_finishes_normally_is_not_touched_by_the_guard() {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        github.add_pr("o/r", "inchworm/5-a1", pr4("deadbeef"));
        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let summary = run4(&ctx4(&store, &github), &flow4(1, ""), 5, &mut client).unwrap();
        assert_eq!(summary.state, RunState::PrOpen);
        let attempts = store.attempts(summary.run_id).unwrap();
        assert_eq!(attempts[0].failure_class, None);
        assert_eq!(github.issue("o/r", 5).unwrap().labels, ["inchworm:done"]);
    }

    struct DiesWithTheConnection {
        calls: u32,
    }
    impl Dispatcher for DiesWithTheConnection {
        fn dispatch(
            &mut self,
            _: &crate::nanna_client::AssignRequest,
            _: std::time::Duration,
        ) -> Result<crate::nanna_client::TaskOutcome, crate::nanna_client::ClientError> {
            self.calls += 1;
            Err(crate::nanna_client::ClientError::Eof)
        }

        fn is_usable(&self) -> bool {
            false
        }
    }

    #[test]
    fn a_dead_connection_ends_the_run_instead_of_burning_the_remaining_attempts() {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        let mut dispatcher = DiesWithTheConnection { calls: 0 };
        let summary = run4(
            &ctx4(&store, &github),
            &flow4(3, "\"orchestrator_lost\""),
            5,
            &mut dispatcher,
        )
        .unwrap();
        assert_eq!(summary.state, RunState::Failed);
        assert_eq!(summary.attempts, 1);
        assert_eq!(dispatcher.calls, 1);
        let (class, raw) = summary.failure.unwrap();
        assert_eq!(class, FailureClass::OrchestratorLost);
        assert!(raw.contains("closed the connection"), "{raw}");
        assert_eq!(github.issue("o/r", 5).unwrap().labels, ["inchworm:failed"]);
    }

    #[test]
    fn a_ready_for_review_pr_is_not_accepted_and_nothing_is_recorded_for_it() {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        let mut ready = pr4("deadbeef");
        ready.is_draft = false;
        github.add_pr("o/r", "inchworm/5-a1", ready);
        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let summary = run4(&ctx4(&store, &github), &flow4(1, ""), 5, &mut client).unwrap();
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
    fn a_rejected_pr_is_not_retried_and_its_url_is_in_the_one_comment() {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        let mut ready = pr4("deadbeef");
        ready.is_draft = false;
        github.add_pr("o/r", "inchworm/5-a1", ready);
        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let summary = run4(
            &ctx4(&store, &github),
            &flow4(3, "\"agent_failed\""),
            5,
            &mut client,
        )
        .unwrap();
        assert_eq!(summary.attempts, 1);
        assert_eq!(summary.state, RunState::Failed);
        let comments = github.comments();
        assert_eq!(comments.len(), 1, "{comments:?}");
        assert!(comments[0].2.contains("pull/77"), "{}", comments[0].2);
        assert!(comments[0].2.contains("left open"), "{}", comments[0].2);
    }

    #[test]
    fn a_pr_left_behind_by_a_failed_step_is_reported_and_stops_retries() {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        github.add_pr("o/r", "inchworm/5-a1", pr4("deadbeef"));
        let (mut client, _fake) = connect(vec![Scenario::failed_with_message("boom")]);
        client.initialize().unwrap();
        let summary = run4(
            &ctx4(&store, &github),
            &flow4(3, "\"agent_failed\", \"model_error\", \"tests_failed\""),
            5,
            &mut client,
        )
        .unwrap();
        assert_eq!(summary.attempts, 1);
        assert_eq!(github.comments().len(), 1);
        assert!(github.comments()[0].2.contains("pull/77"));
    }

    #[test]
    fn public_text_defuses_mentions_controls_and_length() {
        assert_eq!(public_text("hi @octocat"), "hi \u{ff20}octocat");
        assert_eq!(public_text("a\u{1b}[0m\u{7}b\nc\td"), "a[0mb\nc\td");
        let long = "x".repeat(MAX_PUBLIC_TEXT_CHARS + 5);
        let shown = public_text(&long);
        assert!(shown.starts_with(&"x".repeat(MAX_PUBLIC_TEXT_CHARS)));
        assert!(shown.contains("5 more characters"), "{shown}");
        assert_eq!(
            public_text(&"x".repeat(MAX_PUBLIC_TEXT_CHARS)).len(),
            MAX_PUBLIC_TEXT_CHARS
        );
    }

    #[test]
    fn public_text_strips_every_bidirectional_control_and_keeps_their_neighbours() {
        let controls: Vec<char> = ['\u{061c}', '\u{200e}', '\u{200f}']
            .into_iter()
            .chain('\u{202a}'..='\u{202e}')
            .chain('\u{2066}'..='\u{2069}')
            .collect();
        for c in controls {
            assert_eq!(
                public_text(&format!("a{c}b")),
                "ab",
                "U+{:04X}",
                u32::from(c)
            );
        }
        for c in [
            '\u{061b}', '\u{200d}', '\u{2029}', '\u{202f}', '\u{2065}', '\u{206a}',
        ] {
            assert_eq!(
                public_text(&format!("a{c}b")),
                format!("a{c}b"),
                "U+{:04X}",
                u32::from(c)
            );
        }
    }

    #[test]
    fn a_run_with_no_pull_request_is_not_replayed() {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        let (mut client, fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let summary = run4(
            &ctx4(&store, &github),
            &flow4(3, "\"agent_failed\""),
            5,
            &mut client,
        )
        .unwrap();
        assert_eq!(summary.attempts, 1);
        let (class, raw) = summary.failure.unwrap();
        assert_eq!(class, FailureClass::AgentFailed);
        assert!(raw.contains("inchworm does not publish"), "{raw}");
        assert_eq!(github.comments().len(), 1);
        assert!(github.comments()[0].2.contains("not retried"));
        drop(client);
        assert_eq!(
            fake.assigned().len(),
            2,
            "one attempt of the two steps, not three"
        );
    }

    #[test]
    fn a_protected_pr_is_left_open_and_never_retried() {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        github.add_pr("o/r", "inchworm/5-a1", pr4("deadbeef"));
        github.set_pr_files("o/r", 77, &[".inchworm/flows.toml"]);
        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let summary = run4(
            &ctx4(&store, &github),
            &flow4(3, "\"protected_path_violation\""),
            5,
            &mut client,
        )
        .unwrap();
        assert_eq!(summary.attempts, 1);
        assert_eq!(github.comments().len(), 1);
        assert!(
            github.comments()[0].2.contains("left open"),
            "{:?}",
            github.comments()
        );
    }

    #[test]
    fn a_closed_or_merged_pr_is_not_accepted_as_the_result() {
        for state in [PrState::Closed, PrState::Merged] {
            let store = Store::open_in_memory().unwrap();
            let github = github4();
            let mut gone = pr4("deadbeef");
            gone.state = state;
            github.add_pr("o/r", "inchworm/5-a1", gone);
            let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
            client.initialize().unwrap();
            let summary = run4(&ctx4(&store, &github), &flow4(1, ""), 5, &mut client).unwrap();
            assert_eq!(summary.state, RunState::Failed, "{state:?}");
            assert!(summary.failure.unwrap().1.contains("not open"));
        }
    }

    #[test]
    fn a_rerun_clears_the_failed_label_and_a_success_clears_the_running_label() {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        let (mut client, _fake) = connect(vec![
            Scenario::success(),
            Scenario::success(),
            Scenario::success(),
            Scenario::success(),
        ]);
        client.initialize().unwrap();
        run4(&ctx4(&store, &github), &flow4(1, ""), 5, &mut client).unwrap();
        assert_eq!(github.issue("o/r", 5).unwrap().labels, ["inchworm:failed"]);
        github.add_pr("o/r", "inchworm/5-a2", pr4("deadbeef"));
        change_issue(&github, |issue| issue.labels.push("inchworm".into()));
        let summary = run4(&ctx4(&store, &github), &flow4(1, ""), 5, &mut client).unwrap();
        assert_eq!(summary.state, RunState::PrOpen);
        assert_eq!(github.issue("o/r", 5).unwrap().labels, ["inchworm:done"]);
    }

    struct LeftoverLookupFails4(FakeGitHub);

    impl GitHub for LeftoverLookupFails4 {
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
            Err(GitHubError::NotFound("gh is down".into()))
        }
        fn pr_files(&self, r: &str, n: u64) -> Result<Vec<String>, GitHubError> {
            self.0.pr_files(r, n)
        }
        fn open_prs_for_issue(
            &self,
            r: &str,
            n: u64,
        ) -> Result<Vec<(String, PullRequest)>, GitHubError> {
            self.0.open_prs_for_issue(r, n)
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
        fn commit_statuses(&self, r: &str, s: &str) -> Result<Vec<CheckRun>, GitHubError> {
            self.0.commit_statuses(r, s)
        }
        fn mark_ready(&self, r: &str, n: u64) -> Result<(), GitHubError> {
            self.0.mark_ready(r, n)
        }
        fn mark_draft(&self, r: &str, n: u64) -> Result<(), GitHubError> {
            self.0.mark_draft(r, n)
        }
        fn merge_pr(&self, r: &str, n: u64, s: &str, m: MergeMethod) -> Result<(), GitHubError> {
            self.0.merge_pr(r, n, s, m)
        }
        fn required_checks(&self, r: &str, b: &str) -> Result<Option<Vec<String>>, GitHubError> {
            self.0.required_checks(r, b)
        }
    }

    #[test]
    fn a_failed_step_with_an_unreadable_pr_lookup_is_not_retried() {
        let store = Store::open_in_memory().unwrap();
        let github = LeftoverLookupFails4(github4());
        let (mut client, _fake) = connect(vec![
            Scenario::failed_with_message("boom"),
            Scenario::failed_with_message("boom"),
        ]);
        client.initialize().unwrap();
        let context = RunContext {
            store: &store,
            github: &github,
            slug: "o/r",
            repo_path: Path::new("/repo"),
            clock: &FakeClock::new(1),
        };
        let summary = run4(
            &context,
            &flow4(3, "\"agent_failed\", \"model_error\", \"max_iterations\""),
            5,
            &mut client,
        )
        .unwrap();
        assert_eq!(summary.attempts, 1);
        assert!(
            github.0.comments()[0].2.contains("could not check whether"),
            "{:?}",
            github.0.comments()
        );
    }

    #[test]
    fn a_given_up_run_posts_exactly_one_comment_with_defused_text() {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        let (mut client, _fake) = connect(vec![Scenario::failed_with_message("ping @someone")]);
        client.initialize().unwrap();
        run4(&ctx4(&store, &github), &flow4(1, ""), 5, &mut client).unwrap();
        let comments = github.comments();
        assert_eq!(comments.len(), 1, "{comments:?}");
        assert!(!comments[0].2.contains("@someone"), "{}", comments[0].2);
    }

    #[test]
    fn a_pr_whose_file_list_cannot_be_read_is_never_accepted_unchecked() {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        github.add_pr("o/r", "inchworm/5-a1", pr4("deadbeef"));
        github.set_pr_files("o/r", 77, &["src/lib.rs"]);
        github.fail_pr_files("API rate limit exceeded");
        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let summary = run4(&ctx4(&store, &github), &flow4(1, ""), 5, &mut client).unwrap();
        assert_eq!(summary.state, RunState::Failed);
        let (class, raw) = summary.failure.unwrap();
        assert_eq!(class, FailureClass::AgentFailed);
        assert!(raw.contains("API rate limit exceeded"), "{raw}");
        assert!(summary.pr.is_none());
        let attempt = &store.attempts(summary.run_id).unwrap()[0];
        assert_eq!(attempt.pr_number, None);
        assert_eq!(attempt.head_sha, None);
    }

    #[test]
    fn a_pr_touching_inchworm_is_rejected_even_when_nanna_reported_clean_output_in_the_foreground_run()
     {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        github.add_pr("o/r", "inchworm/5-a1", pr4("deadbeef"));
        github.set_pr_files("o/r", 77, &["src/lib.rs", ".inchworm/flows.toml"]);
        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let summary = run4(&ctx4(&store, &github), &flow4(2, ""), 5, &mut client).unwrap();
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

    struct LookupFails4(FakeGitHub);

    impl GitHub for LookupFails4 {
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
        fn open_prs_for_issue(
            &self,
            r: &str,
            n: u64,
        ) -> Result<Vec<(String, PullRequest)>, GitHubError> {
            self.0.open_prs_for_issue(r, n)
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
        fn commit_statuses(&self, r: &str, s: &str) -> Result<Vec<CheckRun>, GitHubError> {
            self.0.commit_statuses(r, s)
        }
        fn mark_ready(&self, r: &str, n: u64) -> Result<(), GitHubError> {
            self.0.mark_ready(r, n)
        }
        fn mark_draft(&self, r: &str, n: u64) -> Result<(), GitHubError> {
            self.0.mark_draft(r, n)
        }
        fn merge_pr(&self, r: &str, n: u64, s: &str, m: MergeMethod) -> Result<(), GitHubError> {
            self.0.merge_pr(r, n, s, m)
        }
        fn required_checks(&self, r: &str, b: &str) -> Result<Option<Vec<String>>, GitHubError> {
            self.0.required_checks(r, b)
        }
    }

    #[test]
    fn a_failing_pr_lookup_after_the_steps_is_not_retried() {
        let store = Store::open_in_memory().unwrap();
        let github = LookupFails4(github4());
        let (mut client, fake) = connect(vec![
            Scenario::success(),
            Scenario::success(),
            Scenario::success(),
            Scenario::success(),
        ]);
        client.initialize().unwrap();
        let context = RunContext {
            store: &store,
            github: &github,
            slug: "o/r",
            repo_path: Path::new("/repo"),
            clock: &FakeClock::new(1),
        };
        let summary = run4(&context, &flow4(3, "\"agent_failed\""), 5, &mut client).unwrap();
        assert_eq!(summary.attempts, 1);
        assert!(
            github.0.comments()[0].2.contains("not retried")
                || github.0.comments()[0].2.contains("would not help")
        );
        drop(client);
        assert_eq!(fake.assigned().len(), 2, "one attempt of the two steps");
    }

    #[test]
    fn a_failing_pr_lookup_fails_the_attempt_in_the_foreground_run() {
        let store = Store::open_in_memory().unwrap();
        let github = LookupFails4(github4());
        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let context = RunContext {
            store: &store,
            github: &github,
            slug: "o/r",
            repo_path: Path::new("/repo"),
            clock: &FakeClock::new(1),
        };
        let summary = run4(&context, &flow4(1, ""), 5, &mut client).unwrap();
        let (class, raw) = summary.failure.unwrap();
        assert_eq!(class, FailureClass::AgentFailed);
        assert!(raw.contains("pr lookup"));
    }

    #[test]
    fn branches_stay_unique_across_runs_of_the_same_issue_in_the_foreground_run() {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        github.add_pr("o/r", "inchworm/5-a1", pr4("old"));
        let (mut client, fake) = connect(vec![
            Scenario::failed("result_failed_structured"),
            Scenario::success(),
            Scenario::success(),
        ]);
        client.initialize().unwrap();
        let first = run4(&ctx4(&store, &github), &flow4(1, ""), 5, &mut client).unwrap();
        assert_eq!(first.state, RunState::Failed);
        let mut closed = pr4("old");
        closed.state = crate::github::PrState::Closed;
        github.add_pr("o/r", "inchworm/5-a1", closed);
        change_issue(&github, |i| i.labels.push("inchworm".into()));
        github.add_pr(
            "o/r",
            "inchworm/5-a2",
            PullRequest {
                number: 78,
                ..pr("new")
            },
        );
        let second = run4(&ctx4(&store, &github), &flow4(1, ""), 5, &mut client).unwrap();
        assert_eq!(second.state, RunState::PrOpen);
        assert_eq!(second.pr.unwrap().number, 78);
        let attempts = store.attempts(second.run_id).unwrap();
        assert_eq!(attempts[0].n, 1);
        assert_eq!(attempts[0].branch.as_deref(), Some("inchworm/5-a2"));
        let prompt = fake.assigned()[1]["description"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!prompt.contains("Branch for any pull request"));
    }

    #[test]
    fn a_failed_run_is_recorded_labelled_and_reported_in_the_foreground_run() {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        let (mut client, _fake) = connect(vec![Scenario::failed("result_failed_structured")]);
        client.initialize().unwrap();
        let summary = run4(&ctx4(&store, &github), &flow4(3, ""), 5, &mut client).unwrap();
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
    fn retryable_failures_run_again_with_the_previous_failure_in_the_prompt_in_the_foreground_run()
    {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        github.add_pr("o/r", "inchworm/5-a2", pr4("cafe"));
        let (mut client, fake) = connect(vec![
            Scenario::failed("result_failed_structured"),
            Scenario::success(),
            Scenario::success(),
        ]);
        client.initialize().unwrap();
        let flow = flow4(3, "\"max_iterations\"");
        let summary = run4(&ctx4(&store, &github), &flow, 5, &mut client).unwrap();
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
        assert!(second_prompt.contains("Attempt: 2"));
    }

    #[test]
    fn retries_stop_at_max_attempts_in_the_foreground_run() {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        let (mut client, fake) = connect(vec![
            Scenario::failed("result_failed_structured"),
            Scenario::failed("result_failed_structured"),
        ]);
        client.initialize().unwrap();
        let flow = flow4(2, "\"max_iterations\"");
        let summary = run4(&ctx4(&store, &github), &flow, 5, &mut client).unwrap();
        assert_eq!(summary.state, RunState::Failed);
        assert_eq!(summary.attempts, 2);
        assert_eq!(fake.assigned().len(), 2);
    }

    #[test]
    fn completed_steps_without_a_pr_fail_the_attempt_in_the_foreground_run() {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        let summary = run4(&ctx4(&store, &github), &flow4(1, ""), 5, &mut client).unwrap();
        let (class, raw) = summary.failure.unwrap();
        assert_eq!(class, FailureClass::AgentFailed);
        assert!(raw.contains("no pull request exists for branch `inchworm/5-a1`"));
    }

    #[test]
    fn a_run_cannot_be_repeated_while_the_previous_one_is_live_in_the_foreground_run() {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        github.add_pr("o/r", "inchworm/5-a1", pr4("x"));
        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success()]);
        client.initialize().unwrap();
        store
            .enqueue_run("o/r", &Subject::issue(5).unwrap(), "issue", 1)
            .unwrap()
            .unwrap();
        let err = run4(&ctx4(&store, &github), &flow4(1, ""), 5, &mut client).unwrap_err();
        assert!(
            matches!(err, RunError::AlreadyRun { issue: 5, .. }),
            "{err}"
        );
    }

    #[test]
    fn a_missing_issue_is_a_github_error_and_nothing_is_enqueued_in_the_foreground_run() {
        let store = Store::open_in_memory().unwrap();
        let github = FakeGitHub::new();
        let (mut client, _fake) = connect(vec![]);
        let err = run4(&ctx4(&store, &github), &flow4(1, ""), 5, &mut client).unwrap_err();
        assert!(matches!(err, RunError::GitHub(_)));
        assert!(store.next_queued("o/r").unwrap().is_none());
    }

    #[test]
    fn a_refused_spawn_in_a_single_step_flow_fails_as_spawn_refused_in_the_foreground_run() {
        let store = Store::open_in_memory().unwrap();
        let github = github4();
        let (mut client, _fake) = connect(vec![Scenario::refused("assign_refused_legacy")]);
        client.initialize().unwrap();
        let mut flow = flow4(1, "");
        flow.step.truncate(1);
        let summary = run4(&ctx4(&store, &github), &flow, 5, &mut client).unwrap();
        assert_eq!(summary.failure.unwrap().0, FailureClass::SpawnRefused);
        assert!(summary.warnings.is_empty());
    }

    #[test]
    fn label_and_comment_failures_become_warnings_in_the_foreground_run() {
        let store = Store::open_in_memory().unwrap();
        let ghost = FakeGitHub::new();
        let mut warnings = Vec::new();
        let context = ctx4(&store, &ghost);
        relabel(&context, 9, &["a"], &[], &mut warnings);
        note(&context, 9, "x", &mut warnings);
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].starts_with("could not update labels"));
        assert!(warnings[1].starts_with("could not comment"));
    }
}
