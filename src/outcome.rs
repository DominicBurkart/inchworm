use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::clock::Clock;
use crate::config::OutcomeMode;
use crate::config::{Flow, Judge};
use crate::github::{GitHub, PrState, PullRequest};
use crate::judges::{DynScorer, NannaReport, Verdict, diff_judge, llm_judge, nanna_clean_judge};
use crate::model::{FailureClass, RunState};
use crate::policy::{CheckConclusion, CheckRun, Decision, HumanGate, PolicyInput, decide};
use crate::run::{RUNNING_LABEL, trigger_label};
use crate::store::{AttemptId, RunId, Store, StoreError};

pub const DONE_LABEL: &str = "inchworm:done";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CiPolicy {
    pub poll_interval: Duration,
    pub timeout: Duration,
}

impl Default for CiPolicy {
    fn default() -> Self {
        CiPolicy {
            poll_interval: Duration::from_secs(30),
            timeout: Duration::from_secs(2 * 60 * 60),
        }
    }
}

pub struct OutcomeEnv<'a> {
    pub scorer: &'a dyn DynScorer,
    pub model_permit: Arc<Semaphore>,
    pub ci: CiPolicy,
    pub human_gate: HumanGate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiStatus {
    Green,
    Red(String),
    Waiting(Vec<String>),
}

/// ```
/// use inchworm::outcome::{evaluate_required, CiStatus};
/// use inchworm::policy::{CheckConclusion, CheckRun};
///
/// let run = |name: &str, sha: &str, conclusion| CheckRun {
///     name: name.into(), head_sha: sha.into(), conclusion,
/// };
/// let required = ["build".to_string()];
/// assert_eq!(
///     evaluate_required(&required, &[run("build", "abc", CheckConclusion::Success)], "abc"),
///     CiStatus::Green
/// );
/// assert_eq!(
///     evaluate_required(&required, &[run("build", "old", CheckConclusion::Success)], "abc"),
///     CiStatus::Waiting(vec!["build".into()])
/// );
/// ```
pub fn evaluate_required(required: &[String], runs: &[CheckRun], sha: &str) -> CiStatus {
    let mut waiting = Vec::new();
    for name in required {
        let mut seen = false;
        let mut pending = false;
        for run in runs
            .iter()
            .filter(|run| run.name == *name && run.head_sha.eq_ignore_ascii_case(sha))
        {
            seen = true;
            match run.conclusion {
                CheckConclusion::Failure => return CiStatus::Red(name.clone()),
                CheckConclusion::Pending => pending = true,
                CheckConclusion::Success => {}
            }
        }
        if !seen || pending {
            waiting.push(name.clone());
        }
    }
    if waiting.is_empty() {
        CiStatus::Green
    } else {
        CiStatus::Waiting(waiting)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiResult {
    Green(Vec<CheckRun>),
    Red(String),
    TimedOut {
        pending: Vec<String>,
        last_error: Option<String>,
    },
}

pub async fn wait_for_ci(
    github: &dyn GitHub,
    clock: &dyn Clock,
    repo: &str,
    sha: &str,
    required: &[String],
    policy: &CiPolicy,
) -> CiResult {
    if required.is_empty() {
        return CiResult::Green(Vec::new());
    }
    let deadline = clock
        .now()
        .saturating_add(i64::try_from(policy.timeout.as_secs()).unwrap_or(i64::MAX));
    let mut last_error: Option<String>;
    let mut pending = required.to_vec();
    loop {
        match github.check_runs(repo, sha) {
            Ok(runs) => {
                last_error = None;
                match evaluate_required(required, &runs, sha) {
                    CiStatus::Green => return CiResult::Green(runs),
                    CiStatus::Red(name) => return CiResult::Red(name),
                    CiStatus::Waiting(names) => pending = names,
                }
            }
            Err(err) => last_error = Some(err.to_string()),
        }
        if clock.now() >= deadline {
            return CiResult::TimedOut {
                pending,
                last_error,
            };
        }
        clock.sleep(policy.poll_interval).await;
    }
}

pub struct Published<'a> {
    pub run_id: RunId,
    pub attempt_id: AttemptId,
    pub attempt_n: u32,
    pub pr: &'a PullRequest,
    pub judged_sha: &'a str,
    pub report: NannaReport,
}

pub struct JudgeInputs<'a> {
    pub repo_path: &'a Path,
    pub patch: &'a str,
    pub report: &'a NannaReport,
}

pub async fn run_judges(
    flow: &Flow,
    env: &OutcomeEnv<'_>,
    inputs: &JudgeInputs<'_>,
) -> Vec<Verdict> {
    let mut verdicts = Vec::new();
    for (index, judge) in flow.judge.iter().enumerate() {
        let verdict = match judge {
            Judge::Diff {
                max_lines,
                forbid_paths,
            } => diff_judge(inputs.patch, *max_lines, forbid_paths),
            Judge::NannaClean {} => nanna_clean_judge(inputs.report),
            Judge::Llm {
                rubric,
                model,
                min_score,
            } => match std::fs::read_to_string(inputs.repo_path.join(rubric)) {
                Ok(text) => {
                    llm_judge(
                        &env.scorer,
                        &env.model_permit,
                        &text,
                        model,
                        *min_score,
                        inputs.patch,
                    )
                    .await
                }
                Err(err) => Verdict::fail("llm", &format!("cannot read rubric: {err}")),
            },
        };
        verdicts.push(verdict.with_index(index));
    }
    verdicts
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Concluded {
    Accepted { state: RunState },
    Rejected { class: FailureClass, raw: String },
}

pub struct ConcludeContext<'a> {
    pub store: &'a Store,
    pub github: &'a dyn GitHub,
    pub clock: &'a dyn Clock,
    pub slug: &'a str,
    pub repo_path: &'a Path,
    pub issue: u64,
}

fn failed_verdicts(verdicts: &[Verdict]) -> String {
    verdicts
        .iter()
        .filter(|v| !v.passed)
        .map(|v| format!("{}: {}", v.judge, v.detail))
        .collect::<Vec<_>>()
        .join("; ")
}

fn summarise(verdicts: &[Verdict]) -> String {
    if verdicts.is_empty() {
        return "no judges configured".to_string();
    }
    verdicts
        .iter()
        .map(|v| format!("{} {}", v.judge, if v.passed { "pass" } else { "fail" }))
        .collect::<Vec<_>>()
        .join(", ")
}

fn warn_label_comment(
    ctx: &ConcludeContext<'_>,
    add: &[&str],
    remove: &[&str],
    body: &str,
    warnings: &mut Vec<String>,
) {
    if let Err(err) = ctx.github.set_labels(ctx.slug, ctx.issue, add, remove) {
        warnings.push(format!("could not update labels: {err}"));
    }
    if let Err(err) = ctx.github.comment(ctx.slug, ctx.issue, body) {
        warnings.push(format!("could not comment on the issue: {err}"));
    }
}

enum Live {
    Open(PullRequest),
    AlreadyMerged,
    Rejected(FailureClass, String),
}

fn inspect_live(
    ctx: &ConcludeContext<'_>,
    flow: &Flow,
    published: &Published<'_>,
    sha: &str,
) -> Live {
    let live = match ctx.github.pr(ctx.slug, published.pr.number) {
        Ok(live) => live,
        Err(err) => {
            return Live::Rejected(
                FailureClass::MergeBlocked,
                format!("cannot read the live pull request: {err}"),
            );
        }
    };
    match live.state {
        PrState::Merged => return Live::AlreadyMerged,
        PrState::Closed => {
            return Live::Rejected(
                FailureClass::MergeBlocked,
                format!("pull request {} was closed without merging", live.url),
            );
        }
        PrState::Open => {}
    }
    if live.is_cross_repository {
        return Live::Rejected(
            FailureClass::MergeBlocked,
            format!("pull request {} comes from a fork", live.url),
        );
    }
    if live.base_ref != flow.base {
        return Live::Rejected(
            FailureClass::MergeBlocked,
            format!(
                "pull request {} targets `{}` but the flow base is `{}`",
                live.url, live.base_ref, flow.base
            ),
        );
    }
    if !live.head_sha.eq_ignore_ascii_case(sha) {
        return Live::Rejected(
            FailureClass::MergeBlocked,
            format!(
                "head moved from {sha} to {} after the attempt was published",
                live.head_sha
            ),
        );
    }
    Live::Open(live)
}

fn settle_merged(
    ctx: &ConcludeContext<'_>,
    flow: &Flow,
    published: &Published<'_>,
    sha: &str,
    warnings: &mut Vec<String>,
) -> Result<Concluded, StoreError> {
    if ctx.store.run(published.run_id)?.state == RunState::AwaitingCi {
        ctx.store
            .set_run_state(published.run_id, RunState::Judging, ctx.clock.now())?;
    }
    ctx.store.settle_attempt(
        published.attempt_id,
        published.run_id,
        RunState::Merged,
        ctx.clock.now(),
    )?;
    warn_label_comment(
        ctx,
        &[DONE_LABEL],
        &[RUNNING_LABEL, trigger_label(flow)],
        &format!(
            "inchworm found {} already merged after attempt {} (judged head `{sha}`).",
            published.pr.url, published.attempt_n
        ),
        warnings,
    );
    Ok(Concluded::Accepted {
        state: RunState::Merged,
    })
}

pub async fn conclude(
    ctx: &ConcludeContext<'_>,
    env: &OutcomeEnv<'_>,
    flow: &Flow,
    published: &Published<'_>,
    warnings: &mut Vec<String>,
) -> Result<Concluded, StoreError> {
    let sha = published.judged_sha;
    match inspect_live(ctx, flow, published, sha) {
        Live::AlreadyMerged => return settle_merged(ctx, flow, published, sha, warnings),
        Live::Rejected(class, raw) => return reject(ctx, flow, published, class, raw),
        Live::Open(_) => {}
    }
    let run = ctx.store.run(published.run_id)?;
    let ci = wait_for_ci(
        ctx.github,
        ctx.clock,
        ctx.slug,
        sha,
        &flow.outcome.required_checks,
        &env.ci,
    )
    .await;
    if run.state == RunState::AwaitingCi {
        ctx.store
            .set_run_state(published.run_id, RunState::Judging, ctx.clock.now())?;
    }

    let checks = match ci {
        CiResult::Green(runs) => runs,
        CiResult::Red(name) => {
            return reject(
                ctx,
                flow,
                published,
                FailureClass::CiRed,
                format!("required check `{name}` failed on {sha}"),
            );
        }
        CiResult::TimedOut {
            pending,
            last_error,
        } => {
            let mut raw = format!(
                "required checks still not green on {sha} after {}s: {}",
                env.ci.timeout.as_secs(),
                pending.join(", ")
            );
            if let Some(err) = last_error {
                raw.push_str(&format!(" (last github error: {err})"));
            }
            return reject(ctx, flow, published, FailureClass::Timeout, raw);
        }
    };

    let live = match inspect_live(ctx, flow, published, sha) {
        Live::Open(live) => live,
        Live::AlreadyMerged => return settle_merged(ctx, flow, published, sha, warnings),
        Live::Rejected(class, raw) => return reject(ctx, flow, published, class, raw),
    };

    let patch = match ctx.github.pr_patch(ctx.slug, published.pr.number) {
        Ok(patch) => patch,
        Err(err) => {
            return reject(
                ctx,
                flow,
                published,
                FailureClass::JudgeRejected,
                format!("cannot read the pull request patch: {err}"),
            );
        }
    };
    let verdicts = run_judges(
        flow,
        env,
        &JudgeInputs {
            repo_path: ctx.repo_path,
            patch: &patch,
            report: &published.report,
        },
    )
    .await;
    ctx.store
        .record_verdicts(published.attempt_id, sha, &verdicts, ctx.clock.now())?;

    let expected_judges: Vec<String> = flow
        .judge
        .iter()
        .map(|judge| judge.kind_name().to_string())
        .collect();
    let decision = decide(&PolicyInput {
        mode: flow.outcome.mode,
        merge_method: flow.outcome.merge_method,
        required_checks: &flow.outcome.required_checks,
        checks: &checks,
        judged_head_sha: sha,
        live_head_sha: &live.head_sha,
        expected_judges: &expected_judges,
        verdicts: &verdicts,
        human_gate: env.human_gate,
    });

    match decision {
        Decision::DraftPr => {
            ctx.store.settle_attempt(
                published.attempt_id,
                published.run_id,
                RunState::PrOpen,
                ctx.clock.now(),
            )?;
            warn_label_comment(
                ctx,
                &[DONE_LABEL],
                &[RUNNING_LABEL, trigger_label(flow)],
                &format!(
                    "inchworm outcome `draft_pr` after attempt {}: {} stays a draft at `{sha}`. Required checks are green on that commit. Judges: {}.",
                    published.attempt_n,
                    published.pr.url,
                    summarise(&verdicts)
                ),
                warnings,
            );
            Ok(Concluded::Accepted {
                state: RunState::PrOpen,
            })
        }
        Decision::Merge { head_sha, method } => {
            let merged = ctx
                .github
                .mark_ready(ctx.slug, published.pr.number)
                .and_then(|()| {
                    ctx.github
                        .merge_pr(ctx.slug, published.pr.number, &head_sha, method)
                });
            match merged {
                Ok(()) => {
                    ctx.store.settle_attempt(
                        published.attempt_id,
                        published.run_id,
                        RunState::Merged,
                        ctx.clock.now(),
                    )?;
                    warn_label_comment(
                        ctx,
                        &[DONE_LABEL],
                        &[RUNNING_LABEL, trigger_label(flow)],
                        &format!(
                            "inchworm outcome `merge` after attempt {}: merged {} at `{head_sha}` (--match-head-commit). Judges: {}.",
                            published.attempt_n,
                            published.pr.url,
                            summarise(&verdicts)
                        ),
                        warnings,
                    );
                    Ok(Concluded::Accepted {
                        state: RunState::Merged,
                    })
                }
                Err(err) => reject(
                    ctx,
                    flow,
                    published,
                    FailureClass::MergeBlocked,
                    format!("merge was refused: {err}"),
                ),
            }
        }
        other => {
            let class = match other {
                Decision::Reject(class) => class,
                _ => FailureClass::MergeBlocked,
            };
            let raw = if class == FailureClass::JudgeRejected {
                failed_verdicts(&verdicts)
            } else {
                format!("policy refused the outcome for {sha}")
            };
            reject(ctx, flow, published, class, raw)
        }
    }
}

fn restore_draft(
    ctx: &ConcludeContext<'_>,
    flow: &Flow,
    published: &Published<'_>,
) -> Option<String> {
    if flow.outcome.mode != OutcomeMode::MergeWhen {
        return None;
    }
    match ctx.github.pr(ctx.slug, published.pr.number) {
        Ok(live) if live.state == PrState::Open && !live.is_draft => ctx
            .github
            .mark_draft(ctx.slug, published.pr.number)
            .err()
            .map(|err| err.to_string()),
        _ => None,
    }
}

fn reject(
    ctx: &ConcludeContext<'_>,
    flow: &Flow,
    published: &Published<'_>,
    class: FailureClass,
    mut raw: String,
) -> Result<Concluded, StoreError> {
    if let Some(undo) = restore_draft(ctx, flow, published) {
        raw.push_str(&format!("; restoring draft status also failed: {undo}"));
    }
    ctx.store
        .finish_attempt_failed(published.attempt_id, class, &raw, ctx.clock.now())?;
    Ok(Concluded::Rejected { class, raw })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;
    use crate::config::MergeMethod;
    use crate::flow::Dispatcher;
    use crate::github::{FakeGitHub, PrState};
    use crate::judges::LlmError;
    use crate::nanna_client::fake::{Scenario, connect};
    use crate::nanna_client::{AssignRequest, ClientError, TaskOutcome};
    use crate::run::{RunContext, RunError, RunSummary, execute_run, run_issue};
    use crate::store::AttemptState;
    use crate::testing::{DRAFT_OUTCOME, FakeScorer, env, flow_with, github_with_issue, pr_at};
    use proptest::prelude::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const SHA2: &str = "fedcba9876543210fedcba9876543210fedcba98";

    fn check(name: &str, sha: &str, conclusion: CheckConclusion) -> CheckRun {
        CheckRun {
            name: name.into(),
            head_sha: sha.into(),
            conclusion,
        }
    }

    fn patch(added: usize) -> String {
        let body: String = (0..added).map(|n| format!("+line {n}\n")).collect();
        format!("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -0,0 +1,{added} @@\n{body}")
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn outcome_toml(mode: &str, checks: &[&str]) -> String {
        let list: Vec<String> = checks.iter().map(|c| format!("\"{c}\"")).collect();
        format!(
            "[flow.outcome]\nmode = \"{mode}\"\nrequired_checks = [{}]\n",
            list.join(", ")
        )
    }

    fn diff_judge_toml(max_lines: u32) -> String {
        format!("[[flow.judge]]\nkind = \"diff\"\nmax_lines = {max_lines}\n")
    }

    struct Rig {
        store: Store,
        github: FakeGitHub,
        clock: FakeClock,
        scorer: FakeScorer,
        dir: tempfile::TempDir,
    }

    fn rig() -> Rig {
        Rig {
            store: Store::open_in_memory().unwrap(),
            github: github_with_issue(),
            clock: FakeClock::new(1_000),
            scorer: FakeScorer::new(),
            dir: tempfile::tempdir().unwrap(),
        }
    }

    impl Rig {
        fn ctx(&self) -> RunContext<'_> {
            RunContext {
                store: &self.store,
                github: &self.github,
                slug: "o/r",
                repo_path: self.dir.path(),
                clock: &self.clock,
            }
        }

        async fn run(&self, flow: &Flow, scenarios: Vec<Scenario>) -> RunSummary {
            let (mut client, _fake) = connect(scenarios);
            client.initialize().unwrap();
            run_issue(&self.ctx(), &env(&self.scorer), flow, 5, &mut client)
                .await
                .unwrap()
        }
    }

    fn two_successes(attempts: usize) -> Vec<Scenario> {
        (0..attempts * 2)
            .map(|_| Scenario::success_with_patch("+x\n+y\n", &["src/lib.rs"]))
            .collect()
    }

    #[test]
    fn required_checks_must_all_be_green_on_the_exact_sha() {
        let required = names(&["build", "test"]);
        let runs = [
            check("build", SHA, CheckConclusion::Success),
            check("test", SHA2, CheckConclusion::Success),
        ];
        assert_eq!(
            evaluate_required(&required, &runs, SHA),
            CiStatus::Waiting(names(&["test"]))
        );
        let pending = [
            check("build", SHA, CheckConclusion::Success),
            check("test", SHA, CheckConclusion::Pending),
        ];
        assert_eq!(
            evaluate_required(&required, &pending, SHA),
            CiStatus::Waiting(names(&["test"]))
        );
        let red = [
            check("build", SHA, CheckConclusion::Failure),
            check("test", SHA, CheckConclusion::Pending),
        ];
        assert_eq!(
            evaluate_required(&required, &red, SHA),
            CiStatus::Red("build".into())
        );
        let green = [
            check("build", &SHA.to_uppercase(), CheckConclusion::Success),
            check("test", SHA, CheckConclusion::Success),
            check("extra", SHA, CheckConclusion::Failure),
        ];
        assert_eq!(evaluate_required(&required, &green, SHA), CiStatus::Green);
        assert_eq!(evaluate_required(&[], &[], SHA), CiStatus::Green);
    }

    proptest! {
        #[test]
        fn red_only_ever_names_a_required_check_on_the_exact_sha(
            states in prop::collection::vec((0usize..3, 0usize..2, 0usize..3), 0..12)
        ) {
            let required = names(&["a", "b"]);
            let all = ["a", "b", "c"];
            let shas = [SHA, SHA2];
            let conclusions = [CheckConclusion::Success, CheckConclusion::Failure, CheckConclusion::Pending];
            let runs: Vec<CheckRun> = states
                .iter()
                .map(|(n, s, c)| check(all[*n], shas[*s], conclusions[*c]))
                .collect();
            match evaluate_required(&required, &runs, SHA) {
                CiStatus::Red(name) => prop_assert!(runs.iter().any(|r| r.name == name
                    && r.head_sha == SHA
                    && r.conclusion == CheckConclusion::Failure
                    && required.contains(&name))),
                CiStatus::Green => prop_assert!(required.iter().all(|name| runs.iter().any(
                    |r| r.name == *name && r.head_sha == SHA && r.conclusion == CheckConclusion::Success
                ))),
                CiStatus::Waiting(waiting) => prop_assert!(!waiting.is_empty()),
            }
        }
    }

    #[tokio::test]
    async fn waiting_polls_with_the_clock_until_the_checks_are_green() {
        let github = FakeGitHub::new();
        let clock = FakeClock::new(0);
        let required = names(&["build"]);
        github.push_check_poll("o/r", SHA, vec![]);
        github.push_check_poll(
            "o/r",
            SHA,
            vec![check("build", SHA, CheckConclusion::Pending)],
        );
        github.push_check_poll(
            "o/r",
            SHA,
            vec![check("build", SHA, CheckConclusion::Success)],
        );
        let policy = CiPolicy {
            poll_interval: Duration::from_secs(10),
            timeout: Duration::from_secs(100),
        };
        let result = wait_for_ci(&github, &clock, "o/r", SHA, &required, &policy).await;
        assert!(matches!(result, CiResult::Green(runs) if runs.len() == 1));
        assert_eq!(clock.sleeps(), [Duration::from_secs(10); 2]);
        assert_eq!(
            wait_for_ci(&github, &clock, "o/r", SHA, &[], &policy).await,
            CiResult::Green(vec![])
        );
    }

    #[tokio::test]
    async fn waiting_stops_early_on_red_and_times_out_with_the_pending_names() {
        let github = FakeGitHub::new();
        let clock = FakeClock::new(0);
        let policy = CiPolicy {
            poll_interval: Duration::from_secs(30),
            timeout: Duration::from_secs(90),
        };
        github.push_check_poll(
            "o/r",
            SHA,
            vec![check("build", SHA, CheckConclusion::Failure)],
        );
        assert_eq!(
            wait_for_ci(&github, &clock, "o/r", SHA, &names(&["build"]), &policy).await,
            CiResult::Red("build".into())
        );
        assert!(clock.sleeps().is_empty());

        let result = wait_for_ci(&github, &clock, "o/r", SHA2, &names(&["build"]), &policy).await;
        assert_eq!(
            result,
            CiResult::TimedOut {
                pending: names(&["build"]),
                last_error: None
            }
        );
        assert_eq!(clock.now(), 90);

        github.fail_check_runs_with("HTTP 502");
        let result = wait_for_ci(&github, &clock, "o/r", SHA, &names(&["build"]), &policy).await;
        assert!(matches!(
            result,
            CiResult::TimedOut { last_error: Some(e), .. } if e.contains("HTTP 502")
        ));
    }

    #[tokio::test]
    async fn green_ci_and_passing_judges_record_verdicts_and_leave_a_draft() {
        let rig = rig();
        rig.github.add_pr("o/r", "inchworm/5-a1", pr_at(77, SHA));
        rig.github.set_pr_patch("o/r", 77, &patch(1));
        rig.github.push_check_poll(
            "o/r",
            SHA,
            vec![check("build", SHA, CheckConclusion::Pending)],
        );
        rig.github.push_check_poll(
            "o/r",
            SHA,
            vec![check("build", SHA, CheckConclusion::Success)],
        );
        let extra = format!(
            "{}[[flow.judge]]\nkind = \"nanna_clean\"\n{}",
            diff_judge_toml(50),
            outcome_toml("draft_pr", &["build"])
        );
        let summary = rig.run(&flow_with(1, "", &extra), two_successes(1)).await;

        assert_eq!(summary.state, RunState::PrOpen);
        assert_eq!(summary.failure, None);
        let attempt = &rig.store.attempts(summary.run_id).unwrap()[0];
        assert_eq!(attempt.state, AttemptState::Succeeded);
        let verdicts = rig.store.verdicts(attempt.id).unwrap();
        assert_eq!(
            verdicts
                .iter()
                .map(|v| (v.judge.as_str(), v.passed))
                .collect::<Vec<_>>(),
            [("diff", true), ("nanna_clean", true)]
        );
        assert_eq!(rig.github.issue_labels("o/r", 5), ["inchworm:done"]);
        let comments = rig.github.comments();
        assert!(comments[1].2.contains("outcome `draft_pr`"), "{comments:?}");
        assert!(comments[1].2.contains("diff pass, nanna_clean pass"));
        assert!(rig.github.pr("o/r", 77).unwrap().is_draft);
        assert!(rig.github.merge_calls().is_empty());
        assert!(rig.github.ready_calls().is_empty());
        assert_eq!(rig.clock.sleeps().len(), 1);
    }

    #[tokio::test]
    async fn a_red_required_check_fails_the_attempt_as_ci_red_and_skips_the_judges() {
        let rig = rig();
        rig.github.add_pr("o/r", "inchworm/5-a1", pr_at(77, SHA));
        rig.github.push_check_poll(
            "o/r",
            SHA,
            vec![check("build", SHA, CheckConclusion::Failure)],
        );
        let extra = format!(
            "{}{}",
            diff_judge_toml(50),
            outcome_toml("draft_pr", &["build"])
        );
        let summary = rig.run(&flow_with(1, "", &extra), two_successes(1)).await;

        assert_eq!(summary.state, RunState::Failed);
        let (class, raw) = summary.failure.unwrap();
        assert_eq!(class, FailureClass::CiRed);
        assert!(raw.contains("`build`") && raw.contains(SHA), "{raw}");
        let attempt = &rig.store.attempts(summary.run_id).unwrap()[0];
        assert_eq!(attempt.failure_class, Some(FailureClass::CiRed));
        assert!(rig.store.verdicts(attempt.id).unwrap().is_empty());
        assert_eq!(rig.github.issue_labels("o/r", 5), ["inchworm:failed"]);
        assert!(rig.github.comments().last().unwrap().2.contains("ci_red"));
    }

    #[tokio::test]
    async fn checks_that_never_finish_time_out_with_the_timeout_class() {
        let rig = rig();
        rig.github.add_pr("o/r", "inchworm/5-a1", pr_at(77, SHA));
        let extra = outcome_toml("draft_pr", &["build"]);
        let summary = rig.run(&flow_with(1, "", &extra), two_successes(1)).await;
        let (class, raw) = summary.failure.unwrap();
        assert_eq!(class, FailureClass::Timeout);
        assert!(raw.contains("build") && raw.contains("60s"), "{raw}");
        assert!(rig.clock.now() >= 1_060);
    }

    #[tokio::test]
    async fn a_rejecting_judge_retries_the_whole_attempt_when_retry_on_allows_it() {
        let rig = rig();
        rig.github.add_pr("o/r", "inchworm/5-a1", pr_at(77, SHA));
        rig.github.add_pr("o/r", "inchworm/5-a2", pr_at(78, SHA2));
        rig.github.set_pr_patch("o/r", 77, &patch(3));
        rig.github.set_pr_patch("o/r", 78, &patch(1));
        let extra = format!("{}{}", diff_judge_toml(2), DRAFT_OUTCOME);
        let summary = rig
            .run(
                &flow_with(3, "\"judge_rejected\"", &extra),
                two_successes(2),
            )
            .await;

        assert_eq!(summary.state, RunState::PrOpen);
        assert_eq!(summary.attempts, 2);
        assert_eq!(summary.pr.unwrap().number, 78);
        let attempts = rig.store.attempts(summary.run_id).unwrap();
        assert_eq!(attempts[0].failure_class, Some(FailureClass::JudgeRejected));
        assert!(attempts[0].raw_error.as_ref().unwrap().contains("diff:"));
        assert_eq!(attempts[1].state, AttemptState::Succeeded);
        assert!(!rig.store.verdicts(attempts[0].id).unwrap()[0].passed);
        assert!(rig.store.verdicts(attempts[1].id).unwrap()[0].passed);
        assert_eq!(rig.github.issue_labels("o/r", 5), ["inchworm:done"]);
    }

    #[tokio::test]
    async fn the_last_rejected_attempt_fails_the_run_and_labels_the_issue() {
        let rig = rig();
        rig.github.add_pr("o/r", "inchworm/5-a1", pr_at(77, SHA));
        rig.github.set_pr_patch("o/r", 77, &patch(3));
        let extra = format!("{}{}", diff_judge_toml(1), DRAFT_OUTCOME);
        let summary = rig
            .run(
                &flow_with(1, "\"judge_rejected\"", &extra),
                two_successes(1),
            )
            .await;
        assert_eq!(summary.state, RunState::Failed);
        assert_eq!(summary.failure.unwrap().0, FailureClass::JudgeRejected);
        assert_eq!(rig.github.issue_labels("o/r", 5), ["inchworm:failed"]);
        assert!(rig.github.pr("o/r", 77).unwrap().is_draft);
    }

    #[tokio::test]
    async fn the_llm_judge_scores_the_pr_patch_against_the_rubric_file() {
        let rig = rig();
        std::fs::create_dir_all(rig.dir.path().join(".inchworm")).unwrap();
        std::fs::write(rig.dir.path().join(".inchworm/rubric.md"), "be tidy").unwrap();
        rig.github.add_pr("o/r", "inchworm/5-a1", pr_at(77, SHA));
        rig.github.set_pr_patch(
            "o/r",
            77,
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -0,0 +1 @@\n+tidy_line\n",
        );
        let rig = Rig {
            scorer: FakeScorer::replying(vec![Ok(r#"{"score": 4, "reason": "meh"}"#.into())]),
            ..rig
        };
        let extra = format!(
            "[[flow.judge]]\nkind = \"llm\"\nrubric = \".inchworm/rubric.md\"\nmodel = \"m\"\nmin_score = 7\n{}",
            DRAFT_OUTCOME
        );
        let summary = rig.run(&flow_with(1, "", &extra), two_successes(1)).await;
        let (class, raw) = summary.failure.unwrap();
        assert_eq!(class, FailureClass::JudgeRejected);
        assert!(raw.contains("llm: score 4 below min_score 7"), "{raw}");
        let prompts = rig.scorer.prompts();
        assert!(prompts[0].contains("be tidy") && prompts[0].contains("+tidy_line"));
        let attempt = &rig.store.attempts(summary.run_id).unwrap()[0];
        assert_eq!(rig.store.verdicts(attempt.id).unwrap()[0].score, Some(4));
    }

    #[tokio::test]
    async fn the_llm_judge_does_not_deadlock_on_the_shared_permit_and_fails_closed() {
        let rig = rig();
        rig.github.add_pr("o/r", "inchworm/5-a1", pr_at(77, SHA));
        let rig = Rig {
            scorer: FakeScorer::replying(vec![Err(LlmError::Status(500))]),
            ..rig
        };
        std::fs::write(rig.dir.path().join("rubric.md"), "r").unwrap();
        let extra = format!(
            "[[flow.judge]]\nkind = \"llm\"\nrubric = \"rubric.md\"\nmodel = \"m\"\nmin_score = 1\n{}",
            DRAFT_OUTCOME
        );
        let summary = tokio::time::timeout(
            Duration::from_secs(5),
            rig.run(&flow_with(1, "", &extra), two_successes(1)),
        )
        .await
        .unwrap();
        assert_eq!(summary.failure.unwrap().0, FailureClass::ModelError);

        let missing = [Judge::Llm {
            rubric: "nope.md".into(),
            model: "m".into(),
            min_score: 1,
        }];
        let flow = Flow {
            judge: missing.to_vec(),
            ..flow_with(1, "", DRAFT_OUTCOME)
        };
        let verdicts = run_judges(
            &flow,
            &env(&rig.scorer),
            &JudgeInputs {
                repo_path: rig.dir.path(),
                patch: "",
                report: &NannaReport {
                    denial_count: 0,
                    qa_ok: Some(true),
                },
            },
        )
        .await;
        assert!(!verdicts[0].passed && verdicts[0].detail.contains("cannot read rubric"));
    }

    #[tokio::test]
    async fn nanna_clean_sees_denials_and_failed_qa_from_every_step() {
        let flow = Flow {
            judge: vec![Judge::NannaClean {}],
            ..flow_with(1, "", DRAFT_OUTCOME)
        };
        let scorer = FakeScorer::new();
        let judge = |denials, qa_ok| {
            let flow = flow.clone();
            let scorer = &scorer;
            async move {
                run_judges(
                    &flow,
                    &env(scorer),
                    &JudgeInputs {
                        repo_path: Path::new("/"),
                        patch: "",
                        report: &NannaReport {
                            denial_count: denials,
                            qa_ok,
                        },
                    },
                )
                .await[0]
                    .passed
            }
        };
        assert!(judge(0, Some(true)).await);
        assert!(!judge(1, Some(true)).await);
        assert!(!judge(0, Some(false)).await);
        assert!(!judge(0, None).await);
    }

    fn merge_flow() -> Flow {
        let extra = format!("{}{}", diff_judge_toml(50), {
            let mut o = outcome_toml("merge_when", &["build"]);
            o.push_str("merge_method = \"squash\"\n");
            o
        });
        flow_with(1, "", &extra)
    }

    #[tokio::test]
    async fn merge_when_marks_ready_and_merges_the_exact_head_after_green_checks() {
        let rig = rig();
        rig.github.add_pr("o/r", "inchworm/5-a1", pr_at(77, SHA));
        rig.github.set_pr_patch("o/r", 77, &patch(2));
        rig.github.push_check_poll(
            "o/r",
            SHA,
            vec![check("build", SHA, CheckConclusion::Success)],
        );
        let summary = rig.run(&merge_flow(), two_successes(1)).await;
        assert_eq!(summary.state, RunState::Merged);
        assert_eq!(rig.github.ready_calls(), [("o/r".to_string(), 77)]);
        assert_eq!(
            rig.github.merge_calls(),
            [("o/r".to_string(), 77, SHA.to_string(), MergeMethod::Squash)]
        );
        assert_eq!(rig.github.pr("o/r", 77).unwrap().state, PrState::Merged);
        assert_eq!(rig.github.issue_labels("o/r", 5), ["inchworm:done"]);
        assert!(
            rig.github
                .comments()
                .last()
                .unwrap()
                .2
                .contains("--match-head-commit")
        );
    }

    #[tokio::test]
    async fn merge_when_with_an_unconsulted_human_gate_stays_a_draft_and_never_touches_the_pr() {
        let rig = rig();
        rig.github.add_pr("o/r", "inchworm/5-a1", pr_at(77, SHA));
        rig.github.push_check_poll(
            "o/r",
            SHA,
            vec![check("build", SHA, CheckConclusion::Success)],
        );
        rig.github.set_pr_patch("o/r", 77, &patch(2));
        let mut env = env(&rig.scorer);
        env.human_gate = HumanGate::NotConsulted;
        let (mut client, _fake) = connect(two_successes(1));
        client.initialize().unwrap();
        let summary = run_issue(&rig.ctx(), &env, &merge_flow(), 5, &mut client)
            .await
            .unwrap();
        assert_eq!(summary.state, RunState::PrOpen);
        assert!(rig.github.ready_calls().is_empty());
        assert!(rig.github.merge_calls().is_empty());
        assert!(rig.github.pr("o/r", 77).unwrap().is_draft);
    }

    fn live_pr(edit: impl FnOnce(&mut PullRequest)) -> Rig {
        let rig = rig();
        let mut live = pr_at(77, SHA);
        edit(&mut live);
        rig.github.add_pr("o/r", "inchworm/5-a1", live);
        rig.github.push_check_poll(
            "o/r",
            SHA,
            vec![check("build", SHA, CheckConclusion::Success)],
        );
        rig.github.set_pr_patch("o/r", 77, &patch(2));
        rig
    }

    #[tokio::test]
    async fn a_merged_pr_found_while_judging_ends_the_run_merged_without_merging_again() {
        let rig = live_pr(|pr| {
            pr.state = PrState::Merged;
            pr.is_draft = false;
        });
        let (concluded, _) = conclude_for(&rig, &merge_flow(), &pr_at(77, SHA), SHA, 5).await;
        assert_eq!(
            concluded,
            Concluded::Accepted {
                state: RunState::Merged
            }
        );
        assert!(rig.github.merge_calls().is_empty());
        assert!(rig.github.ready_calls().is_empty());
        assert_eq!(
            rig.github.issue_labels("o/r", 5),
            ["inchworm:done".to_string()]
        );
        let run = rig.store.active_run("o/r").unwrap();
        assert!(run.is_none());
    }

    #[tokio::test]
    async fn a_closed_pr_is_never_accepted_as_pr_open() {
        for flow in [merge_flow(), flow_with(1, "", DRAFT_OUTCOME)] {
            let rig = live_pr(|pr| pr.state = PrState::Closed);
            let concluded = conclude_with(&rig, &flow, &pr_at(77, SHA), SHA).await;
            assert!(
                matches!(
                    &concluded,
                    Concluded::Rejected { class: FailureClass::MergeBlocked, raw } if raw.contains("closed")
                ),
                "{concluded:?}"
            );
            assert!(rig.github.merge_calls().is_empty());
            assert!(rig.github.ready_calls().is_empty());
        }
    }

    #[tokio::test]
    async fn a_fork_pr_is_rejected_in_both_modes() {
        for flow in [merge_flow(), flow_with(1, "", DRAFT_OUTCOME)] {
            let rig = live_pr(|pr| pr.is_cross_repository = true);
            let concluded = conclude_with(&rig, &flow, &pr_at(77, SHA), SHA).await;
            assert!(
                matches!(
                    &concluded,
                    Concluded::Rejected { class: FailureClass::MergeBlocked, raw } if raw.contains("fork")
                ),
                "{concluded:?}"
            );
            assert!(rig.github.merge_calls().is_empty());
        }
    }

    #[tokio::test]
    async fn a_pr_targeting_another_base_is_rejected_in_both_modes() {
        for flow in [merge_flow(), flow_with(1, "", DRAFT_OUTCOME)] {
            let rig = live_pr(|pr| pr.base_ref = "release".into());
            let concluded = conclude_with(&rig, &flow, &pr_at(77, SHA), SHA).await;
            assert!(
                matches!(
                    &concluded,
                    Concluded::Rejected { class: FailureClass::MergeBlocked, raw } if raw.contains("release") && raw.contains("main")
                ),
                "{concluded:?}"
            );
            assert!(rig.github.merge_calls().is_empty());
        }
    }

    #[tokio::test]
    async fn a_same_repo_pr_on_the_flow_base_is_accepted() {
        let rig = live_pr(|_| {});
        let concluded =
            conclude_with(&rig, &flow_with(1, "", DRAFT_OUTCOME), &pr_at(77, SHA), SHA).await;
        assert!(
            matches!(concluded, Concluded::Accepted { .. }),
            "{concluded:?}"
        );
    }

    #[tokio::test]
    async fn a_pr_left_ready_by_a_death_between_ready_and_merge_is_made_a_draft_again() {
        let rig = live_pr(|pr| pr.is_draft = false);
        rig.github.fail_merges_with("not mergeable");
        let mut flow = merge_flow();
        flow.outcome.required_checks = names(&["build"]);
        let mut env = env(&rig.scorer);
        env.human_gate = HumanGate::Absent;
        let concluded = conclude_with(&rig, &flow, &pr_at(77, SHA), "b".repeat(40).as_str()).await;
        assert!(
            matches!(concluded, Concluded::Rejected { .. }),
            "{concluded:?}"
        );
        assert!(rig.github.pr("o/r", 77).unwrap().is_draft);
        assert_eq!(rig.github.draft_calls().len(), 1);
    }

    #[tokio::test]
    async fn a_draft_pr_flow_never_touches_a_pr_a_human_marked_ready() {
        let rig = live_pr(|pr| pr.is_draft = false);
        let mut judged = flow_with(1, "", DRAFT_OUTCOME);
        judged.outcome.required_checks = names(&["other"]);
        let concluded = conclude_with(&rig, &judged, &pr_at(77, SHA), SHA).await;
        assert!(
            matches!(concluded, Concluded::Rejected { .. }),
            "{concluded:?}"
        );
        assert!(rig.github.draft_calls().is_empty());
        assert!(!rig.github.pr("o/r", 77).unwrap().is_draft);
    }

    #[tokio::test]
    async fn a_transient_check_run_error_on_resume_is_waited_out_and_reported_honestly() {
        let rig = rig();
        rig.github.add_pr("o/r", "inchworm/5-a1", pr_at(77, SHA));
        rig.github.fail_check_runs_with("HTTP 502");
        let run = published_run(&rig, RunState::Judging);
        let extra = outcome_toml("draft_pr", &["build"]);
        let before = rig.clock.now();
        let summary = execute_run(
            &rig.ctx(),
            &env(&rig.scorer),
            &flow_with(1, "", &extra),
            run,
            &mut MustNotDispatch,
            None,
        )
        .await
        .unwrap();
        let (class, raw) = summary.failure.unwrap();
        assert_eq!(class, FailureClass::Timeout);
        assert!(raw.contains("HTTP 502"), "{raw}");
        assert!(rig.clock.now() > before);
    }

    #[tokio::test]
    async fn merge_when_with_a_short_sha_is_blocked_and_never_merges() {
        let rig = rig();
        rig.github
            .add_pr("o/r", "inchworm/5-a1", pr_at(77, "abc123"));
        rig.github.set_pr_patch("o/r", 77, &patch(2));
        rig.github.push_check_poll(
            "o/r",
            "abc123",
            vec![check("build", "abc123", CheckConclusion::Success)],
        );
        let summary = rig.run(&merge_flow(), two_successes(1)).await;
        assert_eq!(summary.failure.unwrap().0, FailureClass::MergeBlocked);
        assert!(rig.github.merge_calls().is_empty());
        assert!(rig.github.ready_calls().is_empty());
    }

    #[tokio::test]
    async fn a_refused_merge_becomes_merge_blocked_with_the_gh_error() {
        let rig = rig();
        rig.github.add_pr("o/r", "inchworm/5-a1", pr_at(77, SHA));
        rig.github.push_check_poll(
            "o/r",
            SHA,
            vec![check("build", SHA, CheckConclusion::Success)],
        );
        rig.github.set_pr_patch("o/r", 77, &patch(2));
        rig.github.fail_merges_with("not mergeable");
        let summary = rig.run(&merge_flow(), two_successes(1)).await;
        let (class, raw) = summary.failure.unwrap();
        assert_eq!(class, FailureClass::MergeBlocked);
        assert!(raw.contains("not mergeable"), "{raw}");
        assert_eq!(
            rig.store.run(summary.run_id).unwrap().state,
            RunState::Failed
        );
        assert_eq!(rig.github.ready_calls(), [("o/r".to_string(), 77)]);
        assert_eq!(rig.github.draft_calls(), [("o/r".to_string(), 77)]);
        assert!(rig.github.pr("o/r", 77).unwrap().is_draft);
    }

    #[tokio::test]
    async fn a_refused_merge_reports_when_restoring_draft_also_fails() {
        let rig = rig();
        rig.github.add_pr("o/r", "inchworm/5-a1", pr_at(77, SHA));
        rig.github.push_check_poll(
            "o/r",
            SHA,
            vec![check("build", SHA, CheckConclusion::Success)],
        );
        rig.github.set_pr_patch("o/r", 77, &patch(2));
        rig.github.fail_merges_with("not mergeable");
        rig.github.fail_drafts_with("undo denied");
        let summary = rig.run(&merge_flow(), two_successes(1)).await;
        let (class, raw) = summary.failure.unwrap();
        assert_eq!(class, FailureClass::MergeBlocked);
        assert!(raw.contains("not mergeable"), "{raw}");
        assert!(raw.contains("undo denied"), "{raw}");
        assert!(!rig.github.pr("o/r", 77).unwrap().is_draft);
    }

    #[tokio::test]
    async fn a_failed_ready_call_also_leaves_the_pr_draft_and_never_merges() {
        let rig = rig();
        rig.github.add_pr("o/r", "inchworm/5-a1", pr_at(77, SHA));
        rig.github.push_check_poll(
            "o/r",
            SHA,
            vec![check("build", SHA, CheckConclusion::Success)],
        );
        rig.github.set_pr_patch("o/r", 77, &patch(2));
        rig.github.fail_ready_with("no permission");
        let summary = rig.run(&merge_flow(), two_successes(1)).await;
        let (class, raw) = summary.failure.unwrap();
        assert_eq!(class, FailureClass::MergeBlocked);
        assert!(raw.contains("no permission"), "{raw}");
        assert!(rig.github.merge_calls().is_empty());
        assert!(rig.github.pr("o/r", 77).unwrap().is_draft);
    }

    async fn conclude_with(rig: &Rig, flow: &Flow, pr: &PullRequest, judged: &str) -> Concluded {
        conclude_for(rig, flow, pr, judged, 5).await.0
    }

    async fn conclude_for(
        rig: &Rig,
        flow: &Flow,
        pr: &PullRequest,
        judged: &str,
        issue: u64,
    ) -> (Concluded, Vec<String>) {
        let run = rig
            .store
            .enqueue_run("o/r", 5, "issue", 1)
            .unwrap()
            .unwrap();
        rig.store.set_run_state(run, RunState::Running, 2).unwrap();
        let attempt = rig.store.start_attempt(run, 3).unwrap();
        rig.store.mark_published(attempt.id, 0, Some(true)).unwrap();
        rig.store
            .set_run_state(run, RunState::AwaitingCi, 4)
            .unwrap();
        let ctx = ConcludeContext {
            store: &rig.store,
            github: &rig.github,
            clock: &rig.clock,
            slug: "o/r",
            repo_path: rig.dir.path(),
            issue,
        };
        let published = Published {
            run_id: run,
            attempt_id: attempt.id,
            attempt_n: 1,
            pr,
            judged_sha: judged,
            report: NannaReport {
                denial_count: 0,
                qa_ok: Some(true),
            },
        };
        let mut warnings = Vec::new();
        let concluded = conclude(&ctx, &env(&rig.scorer), flow, &published, &mut warnings)
            .await
            .unwrap();
        (concluded, warnings)
    }

    #[tokio::test]
    async fn label_and_comment_failures_on_the_issue_are_warnings_not_errors() {
        let rig = rig();
        rig.github.add_pr("o/r", "b", pr_at(77, SHA));
        rig.github.set_pr_patch("o/r", 77, &patch(2));
        let (concluded, warnings) = conclude_for(
            &rig,
            &flow_with(1, "", DRAFT_OUTCOME),
            &pr_at(77, SHA),
            SHA,
            99,
        )
        .await;
        assert_eq!(
            concluded,
            Concluded::Accepted {
                state: RunState::PrOpen
            }
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }

    #[tokio::test]
    async fn a_resumed_judging_run_with_a_red_check_fails_as_ci_red() {
        let rig = rig();
        rig.github.add_pr("o/r", "inchworm/5-a1", pr_at(77, SHA));
        rig.github.push_check_poll(
            "o/r",
            SHA,
            vec![check("build", SHA, CheckConclusion::Failure)],
        );
        let run = published_run(&rig, RunState::Judging);
        let summary = execute_run(
            &rig.ctx(),
            &env(&rig.scorer),
            &flow_with(1, "", &outcome_toml("draft_pr", &["build"])),
            run,
            &mut MustNotDispatch,
            None,
        )
        .await
        .unwrap();
        assert_eq!(summary.failure.unwrap().0, FailureClass::CiRed);
    }

    #[tokio::test]
    async fn github_errors_while_waiting_are_reported_when_the_wait_times_out() {
        let rig = rig();
        rig.github.add_pr("o/r", "inchworm/5-a1", pr_at(77, SHA));
        rig.github.fail_check_runs_with("HTTP 503");
        let extra = outcome_toml("draft_pr", &["build"]);
        let summary = rig.run(&flow_with(1, "", &extra), two_successes(1)).await;
        let (class, raw) = summary.failure.unwrap();
        assert_eq!(class, FailureClass::Timeout);
        assert!(
            raw.contains("last github error") && raw.contains("HTTP 503"),
            "{raw}"
        );
    }

    #[tokio::test]
    async fn a_head_that_moved_after_publishing_is_never_judged_or_merged() {
        let rig = rig();
        rig.github.add_pr("o/r", "b", pr_at(77, SHA2));
        rig.github.push_check_poll(
            "o/r",
            SHA,
            vec![check("build", SHA, CheckConclusion::Success)],
        );
        let concluded = conclude_with(&rig, &merge_flow(), &pr_at(77, SHA), SHA).await;
        assert!(matches!(
            concluded,
            Concluded::Rejected { class: FailureClass::MergeBlocked, raw } if raw.contains("head moved")
        ));
        assert!(rig.github.merge_calls().is_empty());
    }

    #[tokio::test]
    async fn an_unreadable_live_pr_or_patch_blocks_the_outcome() {
        let rig = rig();
        let concluded =
            conclude_with(&rig, &flow_with(1, "", DRAFT_OUTCOME), &pr_at(99, SHA), SHA).await;
        assert!(matches!(
            concluded,
            Concluded::Rejected { class: FailureClass::MergeBlocked, raw } if raw.contains("live pull request")
        ));

        let rig = Rig {
            store: Store::open_in_memory().unwrap(),
            ..rig
        };
        rig.github.add_pr("o/r", "b", pr_at(77, SHA));
        rig.github.fail_patches_with("HTTP 406");
        let concluded =
            conclude_with(&rig, &flow_with(1, "", DRAFT_OUTCOME), &pr_at(77, SHA), SHA).await;
        assert!(matches!(
            concluded,
            Concluded::Rejected { class: FailureClass::JudgeRejected, raw } if raw.contains("HTTP 406")
        ));
    }

    #[tokio::test]
    async fn merge_when_with_no_required_checks_cannot_reach_merge() {
        let rig = rig();
        rig.github.add_pr("o/r", "b", pr_at(77, SHA));
        let mut flow = merge_flow();
        flow.outcome.required_checks.clear();
        rig.github.set_pr_patch("o/r", 77, &patch(2));
        let concluded = conclude_with(&rig, &flow, &pr_at(77, SHA), SHA).await;
        assert!(matches!(
            concluded,
            Concluded::Rejected {
                class: FailureClass::MergeBlocked,
                ..
            }
        ));
        assert!(rig.github.merge_calls().is_empty());
    }

    struct MustNotDispatch;

    impl Dispatcher for MustNotDispatch {
        fn dispatch(&mut self, _: &AssignRequest, _: Duration) -> Result<TaskOutcome, ClientError> {
            panic!("a resumed published attempt must not be dispatched again");
        }
    }

    fn published_run(rig: &Rig, state: RunState) -> crate::store::RunId {
        let run = rig
            .store
            .enqueue_run("o/r", 5, "issue", 1)
            .unwrap()
            .unwrap();
        rig.store.set_run_state(run, RunState::Running, 2).unwrap();
        let attempt = rig.store.start_attempt(run, 3).unwrap();
        rig.store
            .record_pr(attempt.id, "inchworm/5-a1", SHA, 77)
            .unwrap();
        rig.store.mark_published(attempt.id, 0, Some(true)).unwrap();
        if state != RunState::Running {
            rig.store
                .set_run_state(run, RunState::AwaitingCi, 4)
                .unwrap();
        }
        if state == RunState::Judging {
            rig.store.set_run_state(run, RunState::Judging, 5).unwrap();
        }
        run
    }

    #[tokio::test]
    async fn resuming_published_attempts_continues_from_the_stored_sha_without_dispatching() {
        for state in [RunState::Running, RunState::AwaitingCi, RunState::Judging] {
            let rig = rig();
            rig.github.add_pr("o/r", "inchworm/5-a1", pr_at(77, SHA));
            rig.github.push_check_poll(
                "o/r",
                SHA,
                vec![check("build", SHA, CheckConclusion::Success)],
            );
            rig.github.set_pr_patch("o/r", 77, &patch(2));
            let run = published_run(&rig, state);
            let extra = format!(
                "{}{}",
                diff_judge_toml(50),
                outcome_toml("draft_pr", &["build"])
            );
            let summary = execute_run(
                &rig.ctx(),
                &env(&rig.scorer),
                &flow_with(2, "", &extra),
                run,
                &mut MustNotDispatch,
                None,
            )
            .await
            .unwrap();
            assert_eq!(summary.state, RunState::PrOpen, "{state}");
            assert_eq!(summary.attempts, 1);
        }
    }

    #[tokio::test]
    async fn resuming_a_succeeded_attempt_closes_the_run_without_dispatching_again() {
        for state in [RunState::AwaitingCi, RunState::Judging] {
            let rig = rig();
            rig.github.add_pr("o/r", "inchworm/5-a1", pr_at(77, SHA));
            let run = published_run(&rig, state);
            let attempt = rig.store.attempts(run).unwrap()[0].id;
            rig.store.finish_attempt_ok(attempt, 6).unwrap();
            let summary = execute_run(
                &rig.ctx(),
                &env(&rig.scorer),
                &flow_with(2, "", DRAFT_OUTCOME),
                run,
                &mut MustNotDispatch,
                None,
            )
            .await
            .unwrap();
            assert_eq!(summary.state, RunState::PrOpen, "{state}");
            assert_eq!(summary.attempts, 1);
            assert_eq!(rig.store.run(run).unwrap().state, RunState::PrOpen);
            assert_eq!(rig.store.attempts(run).unwrap().len(), 1);
            assert!(rig.github.pr("o/r", 77).unwrap().is_draft);
        }
    }

    #[tokio::test]
    async fn resuming_a_published_attempt_whose_pr_already_merged_records_the_merge() {
        let rig = rig();
        let mut merged = pr_at(77, SHA);
        merged.state = PrState::Merged;
        merged.is_draft = false;
        rig.github.add_pr("o/r", "inchworm/5-a1", merged);
        let run = published_run(&rig, RunState::Judging);
        let summary = execute_run(
            &rig.ctx(),
            &env(&rig.scorer),
            &merge_flow(),
            run,
            &mut MustNotDispatch,
            None,
        )
        .await
        .unwrap();
        assert_eq!(summary.state, RunState::Merged);
        assert_eq!(rig.store.run(run).unwrap().state, RunState::Merged);
        assert_eq!(
            rig.store.attempts(run).unwrap()[0].state,
            AttemptState::Succeeded
        );
        assert!(rig.github.merge_calls().is_empty());
        assert!(rig.github.draft_calls().is_empty());
    }

    #[tokio::test]
    async fn a_resumed_judging_run_that_is_still_pending_times_out_instead_of_hanging() {
        let rig = rig();
        rig.github.add_pr("o/r", "inchworm/5-a1", pr_at(77, SHA));
        let run = published_run(&rig, RunState::Judging);
        let extra = outcome_toml("draft_pr", &["build"]);
        let summary = execute_run(
            &rig.ctx(),
            &env(&rig.scorer),
            &flow_with(1, "", &extra),
            run,
            &mut MustNotDispatch,
            None,
        )
        .await
        .unwrap();
        assert_eq!(summary.failure.unwrap().0, FailureClass::Timeout);
    }

    #[tokio::test]
    async fn resuming_after_a_lost_dispatch_retries_or_gives_up_per_retry_on() {
        for (retry_on, expect_dispatch) in [("\"orchestrator_lost\"", true), ("", false)] {
            let rig = rig();
            let run = rig
                .store
                .enqueue_run("o/r", 5, "issue", 1)
                .unwrap()
                .unwrap();
            rig.store.set_run_state(run, RunState::Running, 2).unwrap();
            rig.store.start_attempt(run, 3).unwrap();
            assert_eq!(rig.store.recover_after_crash(4).unwrap(), 1);
            rig.github.add_pr("o/r", "inchworm/5-a2", pr_at(78, SHA));
            let (mut client, fake) = connect(two_successes(1));
            client.initialize().unwrap();
            let summary = execute_run(
                &rig.ctx(),
                &env(&rig.scorer),
                &flow_with(2, retry_on, DRAFT_OUTCOME),
                run,
                &mut client,
                None,
            )
            .await
            .unwrap();
            assert_eq!(!fake.assigned().is_empty(), expect_dispatch);
            if expect_dispatch {
                assert_eq!(summary.state, RunState::PrOpen);
                assert!(
                    fake.assigned()[0]["description"]
                        .as_str()
                        .unwrap()
                        .contains("orchestrator_lost")
                );
            } else {
                assert_eq!(summary.state, RunState::Failed);
                assert_eq!(summary.failure.unwrap().0, FailureClass::OrchestratorLost);
                assert!(
                    rig.github
                        .issue_labels("o/r", 5)
                        .contains(&"inchworm:failed".to_string())
                );
            }
        }
    }

    #[tokio::test]
    async fn terminal_runs_cannot_be_executed_again() {
        let rig = rig();
        let run = rig
            .store
            .enqueue_run("o/r", 5, "issue", 1)
            .unwrap()
            .unwrap();
        rig.store.set_run_state(run, RunState::Failed, 2).unwrap();
        let err = execute_run(
            &rig.ctx(),
            &env(&rig.scorer),
            &flow_with(1, "", DRAFT_OUTCOME),
            run,
            &mut MustNotDispatch,
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RunError::NotRunnable { .. }), "{err}");
    }
}
