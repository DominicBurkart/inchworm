use std::path::Path;
use std::time::{Duration, Instant};

use crate::classify::{classify_outcome, raw_error};
use crate::config::Flow;
use crate::model::FailureClass;
use crate::nanna_client::{AssignRequest, ClientError, NannaClient, TaskDone, TaskOutcome};

pub const PROTECTED_DIR: &str = ".inchworm";
pub const MAX_FORWARDED_PATCH_BYTES: usize = 100_000;

pub trait Dispatcher {
    fn dispatch(
        &mut self,
        request: &AssignRequest,
        timeout: Duration,
    ) -> Result<TaskOutcome, ClientError>;
}

impl Dispatcher for NannaClient {
    fn dispatch(
        &mut self,
        request: &AssignRequest,
        timeout: Duration,
    ) -> Result<TaskOutcome, ClientError> {
        self.run_task(request, timeout)
    }
}

#[derive(Debug, Clone)]
pub struct FlowInput<'a> {
    pub repo_path: &'a Path,
    pub issue_number: u64,
    pub issue_title: &'a str,
    pub issue_body: &'a str,
    pub attempt: u32,
    pub branch: &'a str,
    pub previous_failure: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRecord {
    pub name: String,
    pub identity: String,
    pub summary: String,
    pub files_modified: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FlowOutcome {
    Completed {
        steps: Vec<StepRecord>,
        last: Box<TaskDone>,
    },
    Failed {
        steps: Vec<StepRecord>,
        step: String,
        class: FailureClass,
        raw_error: String,
    },
}

/// ```
/// use inchworm::flow::branch_name;
///
/// assert_eq!(branch_name(42, 2), "inchworm/42-a2");
/// ```
pub fn branch_name(issue: u64, attempt: u32) -> String {
    format!("inchworm/{issue}-a{attempt}")
}

/// ```
/// use inchworm::flow::touches_protected;
///
/// assert!(touches_protected(".inchworm/flows.toml"));
/// assert!(touches_protected("./src/../.inchworm/rubric.md"));
/// assert!(!touches_protected("src/inchworm.rs"));
/// ```
pub fn touches_protected(path: &str) -> bool {
    let unquoted = path.trim().trim_matches('"').replace('\\', "/");
    let mut stack: Vec<&str> = Vec::new();
    for part in unquoted.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    stack
        .iter()
        .any(|part| part.eq_ignore_ascii_case(PROTECTED_DIR))
}

fn header_paths(patch: &str) -> impl Iterator<Item = &str> {
    patch
        .lines()
        .filter(|line| {
            [
                "diff --git ",
                "--- ",
                "+++ ",
                "rename ",
                "copy ",
                "Binary files ",
            ]
            .iter()
            .any(|prefix| line.starts_with(prefix))
        })
        .flat_map(str::split_whitespace)
        .map(|token| token.trim_matches('"'))
        .map(|token| {
            token
                .strip_prefix("a/")
                .or_else(|| token.strip_prefix("b/"))
                .unwrap_or(token)
        })
}

/// ```
/// use inchworm::flow::protected_paths_in;
///
/// let patch = "diff --git a/.inchworm/flows.toml b/.inchworm/flows.toml\n--- a/.inchworm/flows.toml\n+++ b/.inchworm/flows.toml\n";
/// assert!(!protected_paths_in(&["src/lib.rs".to_string()], &[Some(patch)]).is_empty());
/// assert!(protected_paths_in(&["src/lib.rs".to_string()], &[None]).is_empty());
/// ```
pub fn protected_paths_in(files: &[String], patches: &[Option<&str>]) -> Vec<String> {
    let mut found: Vec<String> = files
        .iter()
        .filter(|f| touches_protected(f))
        .cloned()
        .collect();
    for patch in patches.iter().flatten() {
        for path in header_paths(patch) {
            if touches_protected(path) && !found.iter().any(|f| f == path) {
                found.push(path.to_string());
            }
        }
    }
    found
}

fn truncate_to_boundary(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

fn describe_step(
    flow: &Flow,
    index: usize,
    input: &FlowInput<'_>,
    previous: Option<(&StepRecord, Option<&str>)>,
) -> String {
    let step = &flow.step[index];
    let mut text = format!(
        "Issue #{}: {}\n\n{}\n\n---\nInchworm step {} of {}: {}\nAttempt: {}\nBranch for any pull request: {}\nBase branch: {}\nNever modify anything under {}/.",
        input.issue_number,
        input.issue_title,
        input.issue_body,
        index + 1,
        flow.step.len(),
        step.name,
        input.attempt,
        input.branch,
        flow.base,
        PROTECTED_DIR,
    );
    if let Some(failure) = input.previous_failure {
        text.push_str(&format!("\n\nThe previous attempt failed: {failure}"));
    }
    if let Some((record, patch)) = previous {
        text.push_str(&format!(
            "\n\nOutput of previous step `{}`:\nSummary: {}",
            record.name, record.summary
        ));
        if !record.files_modified.is_empty() {
            text.push_str(&format!(
                "\nFiles modified: {}",
                record.files_modified.join(", ")
            ));
        }
        if let Some(patch) = patch {
            let (kept, cut) = truncate_to_boundary(patch, MAX_FORWARDED_PATCH_BYTES);
            text.push_str(&format!("\nPatch:\n{kept}"));
            if cut {
                text.push_str("\n[patch truncated]");
            }
        }
    }
    text
}

fn failed(
    steps: Vec<StepRecord>,
    step: &str,
    class: FailureClass,
    raw_error: String,
) -> FlowOutcome {
    FlowOutcome::Failed {
        steps,
        step: step.to_string(),
        class,
        raw_error,
    }
}

/// ```
/// use std::path::Path;
/// use std::time::Duration;
/// use inchworm::config::FlowFile;
/// use inchworm::flow::{run_flow, Dispatcher, FlowInput, FlowOutcome};
/// use inchworm::nanna_client::{AssignRequest, ClientError, TaskOutcome};
///
/// struct Down;
/// impl Dispatcher for Down {
///     fn dispatch(&mut self, _: &AssignRequest, _: Duration) -> Result<TaskOutcome, ClientError> {
///         Ok(TaskOutcome::Cancelled)
///     }
/// }
///
/// let file = FlowFile::parse(r#"
/// [[flow]]
/// name = "issue"
/// trigger = { kind = "label", label = "inchworm" }
/// [[flow.step]]
/// name = "implement"
/// identity = "rust-implementer"
/// [flow.outcome]
/// mode = "draft_pr"
/// "#).unwrap();
/// let input = FlowInput {
///     repo_path: Path::new("/repo"),
///     issue_number: 1,
///     issue_title: "t",
///     issue_body: "b",
///     attempt: 1,
///     branch: "inchworm/1-a1",
///     previous_failure: None,
/// };
/// let outcome = run_flow(&file.flow[0], &input, &mut Down);
/// assert!(matches!(outcome, FlowOutcome::Failed { .. }));
/// ```
pub fn run_flow(
    flow: &Flow,
    input: &FlowInput<'_>,
    dispatcher: &mut dyn Dispatcher,
) -> FlowOutcome {
    let started = Instant::now();
    let budget = Duration::from_secs(flow.attempt_timeout_secs);
    let mut records: Vec<StepRecord> = Vec::new();
    let mut forwarded_patch: Option<String> = None;
    let mut last: Option<TaskDone> = None;

    for (index, step) in flow.step.iter().enumerate() {
        let remaining = budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return failed(
                records,
                &step.name,
                FailureClass::Timeout,
                "attempt timeout reached before the step started".to_string(),
            );
        }
        let previous = records.last().map(|r| (r, forwarded_patch.as_deref()));
        let request = AssignRequest {
            description: describe_step(flow, index, input, previous),
            repo_path: input.repo_path.to_path_buf(),
            identity: step.identity.clone(),
            branch: flow.base.clone(),
        };
        let outcome = match dispatcher.dispatch(&request, remaining) {
            Ok(outcome) => outcome,
            Err(err) => {
                return failed(
                    records,
                    &step.name,
                    FailureClass::OrchestratorLost,
                    err.to_string(),
                );
            }
        };
        if let Some(class) = classify_outcome(&outcome) {
            return failed(records, &step.name, class, raw_error(&outcome));
        }
        let TaskOutcome::Completed(done) = outcome else {
            return failed(
                records,
                &step.name,
                FailureClass::AgentFailed,
                "unexpected non-terminal outcome".to_string(),
            );
        };
        let violations = protected_paths_in(
            &done.files_modified,
            &[done.changes_patch.as_deref(), done.format_patch.as_deref()],
        );
        if !violations.is_empty() {
            return failed(
                records,
                &step.name,
                FailureClass::ProtectedPathViolation,
                format!(
                    "step output touches protected {PROTECTED_DIR}/**: {}",
                    violations.join(", ")
                ),
            );
        }
        let is_final = index + 1 == flow.step.len();
        if (!is_final || flow.step.len() == 1) && !done.has_changes() {
            return failed(
                records,
                &step.name,
                FailureClass::EmptyPatch,
                "step produced no changes".to_string(),
            );
        }
        records.push(StepRecord {
            name: step.name.clone(),
            identity: step.identity.clone(),
            summary: done.result_summary.clone(),
            files_modified: done.files_modified.clone(),
        });
        forwarded_patch = done
            .changes_patch
            .clone()
            .or_else(|| done.format_patch.clone());
        last = Some(done);
    }

    match last {
        Some(done) => FlowOutcome::Completed {
            steps: records,
            last: Box::new(done),
        },
        None => failed(
            records,
            "",
            FailureClass::AgentFailed,
            "flow has no steps".to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FlowFile;
    use crate::nanna_client::NannaFailure;
    use crate::nanna_client::fake::{Scenario, connect};
    use proptest::prelude::*;
    use std::collections::VecDeque;

    fn flow(steps: &[(&str, &str)]) -> Flow {
        let mut text = String::from(
            "[[flow]]\nname = \"issue\"\ntrigger = { kind = \"label\", label = \"inchworm\" }\nbase = \"develop\"\n",
        );
        for (name, identity) in steps {
            text.push_str(&format!(
                "[[flow.step]]\nname = \"{name}\"\nidentity = \"{identity}\"\n"
            ));
        }
        text.push_str("[flow.outcome]\nmode = \"draft_pr\"\n");
        FlowFile::parse(&text).unwrap().flow.remove(0)
    }

    fn two_steps() -> Flow {
        flow(&[
            ("implement", "rust-implementer"),
            ("publish", "pr-shepherd"),
        ])
    }

    fn input<'a>(failure: Option<&'a str>) -> FlowInput<'a> {
        FlowInput {
            repo_path: Path::new("/repo"),
            issue_number: 12,
            issue_title: "Fix the thing",
            issue_body: "It is broken.",
            attempt: 2,
            branch: "inchworm/12-a2",
            previous_failure: failure,
        }
    }

    #[test]
    fn steps_run_in_order_and_outputs_flow_forward() {
        let first = Scenario::success_with_patch(
            "diff --git a/src/x.rs b/src/x.rs\n+hello\n",
            &["src/x.rs"],
        );
        let (mut client, fake) = connect(vec![first, Scenario::success()]);
        client.initialize().unwrap();
        let outcome = run_flow(&two_steps(), &input(None), &mut client);
        let FlowOutcome::Completed { steps, last } = outcome else {
            panic!("expected completion, got {outcome:?}");
        };
        assert_eq!(
            steps.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["implement", "publish"]
        );
        assert_eq!(last.result_summary, "Added a doc comment to f.");
        let assigned = fake.assigned();
        assert_eq!(assigned.len(), 2);
        assert_eq!(assigned[0]["identity"], "rust-implementer");
        assert_eq!(assigned[1]["identity"], "pr-shepherd");
        assert_eq!(assigned[0]["branch"], "develop");
        assert_eq!(assigned[0]["repo_path"], "/repo");
        let first_text = assigned[0]["description"].as_str().unwrap();
        assert!(first_text.contains("Issue #12: Fix the thing"));
        assert!(first_text.contains("It is broken."));
        assert!(first_text.contains("step 1 of 2: implement"));
        assert!(first_text.contains("inchworm/12-a2"));
        assert!(!first_text.contains("previous step"));
        let second_text = assigned[1]["description"].as_str().unwrap();
        assert!(second_text.contains("step 2 of 2: publish"));
        assert!(second_text.contains("Output of previous step `implement`"));
        assert!(second_text.contains("Added a doc comment to f."));
        assert!(second_text.contains("Files modified: src/x.rs"));
        assert!(second_text.contains("+hello"));
    }

    #[test]
    fn a_failing_first_step_stops_the_sequence() {
        let (mut client, fake) = connect(vec![
            Scenario::failed("result_failed_structured"),
            Scenario::success(),
        ]);
        client.initialize().unwrap();
        let outcome = run_flow(&two_steps(), &input(None), &mut client);
        let FlowOutcome::Failed {
            steps,
            step,
            class,
            raw_error,
        } = outcome
        else {
            panic!("expected failure");
        };
        assert!(steps.is_empty());
        assert_eq!(step, "implement");
        assert_eq!(class, FailureClass::MaxIterations);
        assert!(raw_error.contains("Maximum iterations"));
        assert_eq!(fake.assigned().len(), 1);
    }

    #[test]
    fn a_failing_second_step_keeps_the_first_record() {
        let (mut client, _fake) = connect(vec![
            Scenario::success(),
            Scenario::failed("result_failed_offline_deps"),
        ]);
        client.initialize().unwrap();
        let outcome = run_flow(&two_steps(), &input(None), &mut client);
        let FlowOutcome::Failed {
            steps, step, class, ..
        } = outcome
        else {
            panic!("expected failure");
        };
        assert_eq!(steps.len(), 1);
        assert_eq!(step, "publish");
        assert_eq!(class, FailureClass::ContainerBuildFailed);
    }

    #[test]
    fn a_refused_spawn_is_spawn_refused() {
        let (mut client, _fake) = connect(vec![Scenario::refused("assign_refused_structured")]);
        client.initialize().unwrap();
        let outcome = run_flow(&two_steps(), &input(None), &mut client);
        assert!(matches!(
            outcome,
            FlowOutcome::Failed {
                class: FailureClass::SpawnRefused,
                ..
            }
        ));
    }

    #[test]
    fn a_cancelled_task_is_cancelled() {
        let (mut client, _fake) = connect(vec![Scenario::Cancelled {
            result: crate::nanna_client::fake::fixture("result_cancelled_structured"),
        }]);
        client.initialize().unwrap();
        let outcome = run_flow(&two_steps(), &input(None), &mut client);
        assert!(matches!(
            outcome,
            FlowOutcome::Failed {
                class: FailureClass::Cancelled,
                ..
            }
        ));
    }

    #[test]
    fn output_touching_inchworm_is_hard_rejected_and_stops_the_flow() {
        let by_file = Scenario::success_with_patch("", &["src/ok.rs", ".inchworm/flows.toml"]);
        let by_patch = Scenario::success_with_patch(
            "diff --git a/.inchworm/rubric.md b/.inchworm/rubric.md\n--- a/.inchworm/rubric.md\n+++ b/.inchworm/rubric.md\n",
            &[],
        );
        let by_rename = Scenario::success_with_patch(
            "diff --git a/src/a.rs b/src/a.rs\nrename from src/a.rs\nrename to .inchworm/a.rs\n",
            &[],
        );
        for scenario in [by_file, by_patch, by_rename] {
            let (mut client, fake) = connect(vec![scenario, Scenario::success()]);
            client.initialize().unwrap();
            let outcome = run_flow(&two_steps(), &input(None), &mut client);
            let FlowOutcome::Failed {
                class,
                raw_error,
                step,
                ..
            } = outcome
            else {
                panic!("expected rejection");
            };
            assert_eq!(class, FailureClass::ProtectedPathViolation);
            assert_eq!(step, "implement");
            assert!(raw_error.contains(".inchworm"), "{raw_error}");
            assert_eq!(fake.assigned().len(), 1);
        }
    }

    #[test]
    fn protected_output_in_a_later_step_is_rejected_too() {
        let bad = Scenario::success_with_patch("", &[".inchworm/flows.toml"]);
        let (mut client, _fake) = connect(vec![Scenario::success(), bad]);
        client.initialize().unwrap();
        let outcome = run_flow(&two_steps(), &input(None), &mut client);
        assert!(matches!(
            outcome,
            FlowOutcome::Failed { class: FailureClass::ProtectedPathViolation, ref steps, .. } if steps.len() == 1
        ));
    }

    #[test]
    fn only_non_final_or_sole_steps_must_change_something() {
        let (mut client, _fake) = connect(vec![Scenario::success_empty()]);
        client.initialize().unwrap();
        let outcome = run_flow(&two_steps(), &input(None), &mut client);
        assert!(matches!(
            outcome,
            FlowOutcome::Failed { class: FailureClass::EmptyPatch, ref step, .. } if step == "implement"
        ));

        let (mut client, _fake) = connect(vec![Scenario::success(), Scenario::success_empty()]);
        client.initialize().unwrap();
        let outcome = run_flow(&two_steps(), &input(None), &mut client);
        assert!(matches!(outcome, FlowOutcome::Completed { .. }));

        let single = flow(&[("implement", "rust-implementer")]);
        let (mut client, _fake) = connect(vec![Scenario::success_empty()]);
        client.initialize().unwrap();
        let outcome = run_flow(&single, &input(None), &mut client);
        assert!(matches!(
            outcome,
            FlowOutcome::Failed {
                class: FailureClass::EmptyPatch,
                ..
            }
        ));
    }

    #[test]
    fn retries_carry_the_previous_failure_summary() {
        let single = flow(&[("implement", "rust-implementer")]);
        let (mut client, fake) = connect(vec![Scenario::success()]);
        client.initialize().unwrap();
        run_flow(
            &single,
            &input(Some("tests failed in module x")),
            &mut client,
        );
        let text = fake.assigned()[0]["description"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(text.contains("The previous attempt failed: tests failed in module x"));
    }

    #[test]
    fn oversized_patches_are_truncated_at_a_char_boundary() {
        let big = "é".repeat(MAX_FORWARDED_PATCH_BYTES);
        let (mut client, fake) = connect(vec![
            Scenario::success_with_patch(&big, &["a.rs"]),
            Scenario::success(),
        ]);
        client.initialize().unwrap();
        run_flow(&two_steps(), &input(None), &mut client);
        let text = fake.assigned()[1]["description"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(text.ends_with("[patch truncated]"));
        assert!(text.len() < MAX_FORWARDED_PATCH_BYTES + 2_000);
    }

    #[test]
    fn short_patches_are_not_marked_truncated() {
        assert_eq!(truncate_to_boundary("abc", 10), ("abc", false));
        assert_eq!(truncate_to_boundary("abcdef", 3), ("abc", true));
        assert_eq!(truncate_to_boundary("aé", 2), ("a", true));
    }

    struct Scripted(VecDeque<Result<TaskOutcome, ClientError>>);

    impl Dispatcher for Scripted {
        fn dispatch(&mut self, _: &AssignRequest, _: Duration) -> Result<TaskOutcome, ClientError> {
            self.0.pop_front().unwrap()
        }
    }

    #[test]
    fn transport_errors_mean_the_orchestrator_lost_nanna() {
        let mut d = Scripted(VecDeque::from([Err(ClientError::Eof)]));
        let outcome = run_flow(&two_steps(), &input(None), &mut d);
        assert!(matches!(
            outcome,
            FlowOutcome::Failed {
                class: FailureClass::OrchestratorLost,
                ..
            }
        ));
    }

    #[test]
    fn an_exhausted_attempt_budget_is_a_timeout_before_dispatch() {
        let mut f = two_steps();
        f.attempt_timeout_secs = 0;
        let mut d = Scripted(VecDeque::new());
        let outcome = run_flow(&f, &input(None), &mut d);
        assert!(matches!(
            outcome,
            FlowOutcome::Failed { class: FailureClass::Timeout, ref step, .. } if step == "implement"
        ));
    }

    #[test]
    fn timed_out_tasks_are_timeouts() {
        let mut d = Scripted(VecDeque::from([Ok(TaskOutcome::TimedOut)]));
        let outcome = run_flow(&two_steps(), &input(None), &mut d);
        assert!(matches!(
            outcome,
            FlowOutcome::Failed {
                class: FailureClass::Timeout,
                ..
            }
        ));
        let refused = TaskOutcome::Refused(NannaFailure::default());
        let mut d = Scripted(VecDeque::from([Ok(refused)]));
        assert!(matches!(
            run_flow(&two_steps(), &input(None), &mut d),
            FlowOutcome::Failed {
                class: FailureClass::SpawnRefused,
                ..
            }
        ));
    }

    #[test]
    fn a_flow_without_steps_fails_cleanly() {
        let mut f = two_steps();
        f.step.clear();
        let mut d = Scripted(VecDeque::new());
        assert!(matches!(
            run_flow(&f, &input(None), &mut d),
            FlowOutcome::Failed {
                class: FailureClass::AgentFailed,
                ..
            }
        ));
    }

    #[test]
    fn protected_detection_handles_encodings_and_lookalikes() {
        for bad in [
            ".inchworm",
            ".inchworm/x",
            "./.inchworm/x",
            "src/../.inchworm/x",
            ".INCHWORM/x",
            "\".inchworm/x\"",
            ".inchworm\\x",
            "a/b/.inchworm/c",
        ] {
            assert!(touches_protected(bad), "{bad}");
        }
        for good in [
            "",
            "inchworm/x",
            ".inchworm.toml",
            "src/.inchworms/x",
            ".inchworm/../x",
            "x/.inchworm/../y",
        ] {
            assert!(!touches_protected(good), "{good}");
        }
    }

    #[test]
    fn patch_scanning_covers_headers_but_ignores_unrelated_lines() {
        let clean = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n+let p = \".inchworm/x\";\n";
        assert!(protected_paths_in(&[], &[Some(clean)]).is_empty());
        let copy = "copy from src/a.rs\ncopy to .inchworm/b.rs\n";
        assert_eq!(protected_paths_in(&[], &[Some(copy)]), [".inchworm/b.rs"]);
        let binary = "Binary files /dev/null and b/.inchworm/blob differ\n";
        assert_eq!(protected_paths_in(&[], &[Some(binary)]), [".inchworm/blob"]);
        let both = protected_paths_in(
            &[".inchworm/x".to_string()],
            &[Some(".inchworm"), Some("--- a/.inchworm/x\n")],
        );
        assert_eq!(both, [".inchworm/x"]);
    }

    proptest! {
        #[test]
        fn any_path_with_an_inchworm_component_is_rejected(
            before in proptest::collection::vec("[a-z]{1,6}", 0..3),
            after in proptest::collection::vec("[a-z]{1,6}", 0..3),
            dot in proptest::bool::ANY,
        ) {
            let mut parts: Vec<String> = Vec::new();
            if dot { parts.push(".".to_string()); }
            parts.extend(before);
            parts.push(".inchworm".to_string());
            parts.extend(after);
            let path = parts.join("/");
            prop_assert!(touches_protected(&path));
        }

        #[test]
        fn paths_without_one_are_accepted(parts in proptest::collection::vec("[a-z][a-z.]{0,6}", 1..5)) {
            prop_assume!(parts.iter().all(|p| p != ".inchworm"));
            prop_assert!(!touches_protected(&parts.join("/")));
        }

        #[test]
        fn patch_headers_naming_inchworm_are_always_found(
            dir in "[a-z]{1,6}",
            name in "[a-z]{1,6}",
        ) {
            let patch = format!("diff --git a/{dir}/{name} b/.inchworm/{name}\n");
            prop_assert!(!protected_paths_in(&[], &[Some(patch.as_str())]).is_empty());
        }
    }
}
