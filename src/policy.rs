use crate::config::{MergeMethod, OutcomeMode};
use crate::judges::Verdict;
use crate::model::FailureClass;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckConclusion {
    Success,
    Failure,
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRun {
    pub name: String,
    pub head_sha: String,
    pub conclusion: CheckConclusion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HumanGate {
    #[default]
    NotConsulted,
    Absent,
    Github,
    Solo,
}

#[derive(Debug, Clone)]
pub struct PolicyInput<'a> {
    pub mode: OutcomeMode,
    pub merge_method: MergeMethod,
    pub required_checks: &'a [String],
    pub checks: &'a [CheckRun],
    pub judged_head_sha: &'a str,
    pub live_head_sha: &'a str,
    pub expected_judges: usize,
    pub verdicts: &'a [Verdict],
    pub human_gate: HumanGate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    DraftPr,
    AwaitCi,
    Merge {
        head_sha: String,
        method: MergeMethod,
    },
    Reject(FailureClass),
}

fn is_full_sha(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

fn same_sha(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

enum Ci {
    Green,
    Red,
    Waiting,
}

fn evaluate_checks(input: &PolicyInput<'_>) -> Ci {
    let mut waiting = false;
    for name in input.required_checks {
        let mut seen = false;
        for run in input
            .checks
            .iter()
            .filter(|run| run.name == *name && same_sha(&run.head_sha, input.judged_head_sha))
        {
            seen = true;
            match run.conclusion {
                CheckConclusion::Failure => return Ci::Red,
                CheckConclusion::Pending => waiting = true,
                CheckConclusion::Success => {}
            }
        }
        if !seen {
            waiting = true;
        }
    }
    if waiting { Ci::Waiting } else { Ci::Green }
}

/// ```
/// use inchworm::config::{MergeMethod, OutcomeMode};
/// use inchworm::judges::Verdict;
/// use inchworm::policy::{decide, Decision, HumanGate, PolicyInput};
///
/// let verdicts = [Verdict::pass("diff", "ok")];
/// let sha = "a".repeat(40);
/// let input = PolicyInput {
///     mode: OutcomeMode::DraftPr,
///     merge_method: MergeMethod::Merge,
///     required_checks: &[],
///     checks: &[],
///     judged_head_sha: &sha,
///     live_head_sha: &sha,
///     expected_judges: 1,
///     verdicts: &verdicts,
///     human_gate: HumanGate::NotConsulted,
/// };
/// assert_eq!(decide(&input), Decision::DraftPr);
/// ```
pub fn decide(input: &PolicyInput<'_>) -> Decision {
    let judges_ok = input.verdicts.len() == input.expected_judges
        && input.verdicts.iter().all(|verdict| verdict.passed);
    let ci = evaluate_checks(input);

    if !judges_ok {
        return Decision::Reject(FailureClass::JudgeRejected);
    }
    if matches!(ci, Ci::Red) {
        return Decision::Reject(FailureClass::CiRed);
    }

    match input.mode {
        OutcomeMode::DraftPr => Decision::DraftPr,
        OutcomeMode::MergeWhen => {
            if input.human_gate == HumanGate::Solo {
                return Decision::DraftPr;
            }
            if input.required_checks.is_empty()
                || input.expected_judges == 0
                || !is_full_sha(input.judged_head_sha)
                || !same_sha(input.judged_head_sha, input.live_head_sha)
            {
                return Decision::Reject(FailureClass::MergeBlocked);
            }
            match ci {
                Ci::Waiting => Decision::AwaitCi,
                _ => Decision::Merge {
                    head_sha: input.judged_head_sha.to_ascii_lowercase(),
                    method: input.merge_method,
                },
            }
        }
    }
}

/// ```
/// use inchworm::config::MergeMethod;
/// use inchworm::policy::merge_args;
///
/// let args = merge_args(7, &"b".repeat(40), MergeMethod::Squash);
/// assert_eq!(args[..3], ["pr", "merge", "7"]);
/// assert!(args.contains(&"--squash".to_string()));
/// assert!(!args.iter().any(|a| a == "--admin"));
/// ```
pub fn merge_args(pr: u64, head_sha: &str, method: MergeMethod) -> Vec<String> {
    let flag = match method {
        MergeMethod::Merge => "--merge",
        MergeMethod::Squash => "--squash",
        MergeMethod::Rebase => "--rebase",
    };
    vec![
        "pr".to_string(),
        "merge".to_string(),
        pr.to_string(),
        "--match-head-commit".to_string(),
        head_sha.to_string(),
        flag.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba98";

    fn check(name: &str, sha: &str, conclusion: CheckConclusion) -> CheckRun {
        CheckRun {
            name: name.to_string(),
            head_sha: sha.to_string(),
            conclusion,
        }
    }

    struct Case {
        mode: OutcomeMode,
        required: Vec<String>,
        checks: Vec<CheckRun>,
        judged: String,
        live: String,
        expected: usize,
        verdicts: Vec<Verdict>,
        gate: HumanGate,
    }

    impl Case {
        fn merge_ready() -> Case {
            Case {
                mode: OutcomeMode::MergeWhen,
                required: vec!["build".into(), "test".into()],
                checks: vec![
                    check("build", SHA, CheckConclusion::Success),
                    check("test", SHA, CheckConclusion::Success),
                ],
                judged: SHA.into(),
                live: SHA.into(),
                expected: 2,
                verdicts: vec![Verdict::pass("diff", "ok"), Verdict::pass("llm", "ok")],
                gate: HumanGate::NotConsulted,
            }
        }

        fn decide(&self) -> Decision {
            decide(&PolicyInput {
                mode: self.mode,
                merge_method: MergeMethod::Merge,
                required_checks: &self.required,
                checks: &self.checks,
                judged_head_sha: &self.judged,
                live_head_sha: &self.live,
                expected_judges: self.expected,
                verdicts: &self.verdicts,
                human_gate: self.gate,
            })
        }
    }

    fn merged() -> Decision {
        Decision::Merge {
            head_sha: SHA.into(),
            method: MergeMethod::Merge,
        }
    }

    #[test]
    fn merge_when_with_everything_green_merges_on_the_exact_head() {
        assert_eq!(Case::merge_ready().decide(), merged());
    }

    #[test]
    fn draft_pr_mode_never_merges_even_when_everything_is_green() {
        let mut case = Case::merge_ready();
        case.mode = OutcomeMode::DraftPr;
        assert_eq!(case.decide(), Decision::DraftPr);
    }

    #[test]
    fn draft_pr_mode_does_not_wait_for_ci() {
        let mut case = Case::merge_ready();
        case.mode = OutcomeMode::DraftPr;
        case.checks.clear();
        assert_eq!(case.decide(), Decision::DraftPr);
    }

    #[test]
    fn red_required_check_rejects_in_both_modes() {
        for mode in [OutcomeMode::DraftPr, OutcomeMode::MergeWhen] {
            let mut case = Case::merge_ready();
            case.mode = mode;
            case.checks[0].conclusion = CheckConclusion::Failure;
            assert_eq!(case.decide(), Decision::Reject(FailureClass::CiRed));
        }
    }

    #[test]
    fn pending_or_missing_required_check_awaits_ci() {
        let mut pending = Case::merge_ready();
        pending.checks[1].conclusion = CheckConclusion::Pending;
        assert_eq!(pending.decide(), Decision::AwaitCi);

        let mut missing = Case::merge_ready();
        missing.checks.pop();
        assert_eq!(missing.decide(), Decision::AwaitCi);
    }

    #[test]
    fn green_checks_on_another_sha_do_not_count() {
        let mut case = Case::merge_ready();
        case.checks[1] = check("test", OTHER, CheckConclusion::Success);
        assert_eq!(case.decide(), Decision::AwaitCi);
    }

    #[test]
    fn red_check_on_another_sha_is_ignored() {
        let mut case = Case::merge_ready();
        case.checks
            .push(check("test", OTHER, CheckConclusion::Failure));
        assert_eq!(case.decide(), merged());
    }

    #[test]
    fn a_second_run_of_a_required_check_on_the_same_head_must_also_pass() {
        let mut case = Case::merge_ready();
        case.checks
            .push(check("test", SHA, CheckConclusion::Failure));
        assert_eq!(case.decide(), Decision::Reject(FailureClass::CiRed));
    }

    #[test]
    fn never_merges_when_the_live_head_moved() {
        let mut case = Case::merge_ready();
        case.live = OTHER.into();
        assert_eq!(case.decide(), Decision::Reject(FailureClass::MergeBlocked));
    }

    #[test]
    fn head_comparison_ignores_hex_case_and_emits_lowercase() {
        let mut case = Case::merge_ready();
        case.live = SHA.to_uppercase();
        assert_eq!(case.decide(), merged());
    }

    #[test]
    fn abbreviated_or_malformed_head_is_blocked() {
        for sha in ["", "0123456", "zzzz456789abcdef0123456789abcdef01234567"] {
            let mut case = Case::merge_ready();
            case.judged = sha.into();
            case.live = sha.into();
            case.checks = vec![
                check("build", sha, CheckConclusion::Success),
                check("test", sha, CheckConclusion::Success),
            ];
            assert_eq!(case.decide(), Decision::Reject(FailureClass::MergeBlocked));
        }
    }

    #[test]
    fn failed_judge_rejects() {
        let mut case = Case::merge_ready();
        case.verdicts[1] = Verdict::fail("llm", "too low");
        assert_eq!(case.decide(), Decision::Reject(FailureClass::JudgeRejected));
    }

    #[test]
    fn missing_verdicts_reject() {
        let mut case = Case::merge_ready();
        case.verdicts.pop();
        assert_eq!(case.decide(), Decision::Reject(FailureClass::JudgeRejected));
    }

    #[test]
    fn extra_verdicts_reject() {
        let mut case = Case::merge_ready();
        case.verdicts.push(Verdict::pass("extra", "ok"));
        assert_eq!(case.decide(), Decision::Reject(FailureClass::JudgeRejected));
    }

    #[test]
    fn merge_without_required_checks_is_blocked() {
        let mut case = Case::merge_ready();
        case.required.clear();
        assert_eq!(case.decide(), Decision::Reject(FailureClass::MergeBlocked));
    }

    #[test]
    fn merge_without_judges_is_blocked() {
        let mut case = Case::merge_ready();
        case.expected = 0;
        case.verdicts.clear();
        assert_eq!(case.decide(), Decision::Reject(FailureClass::MergeBlocked));
    }

    #[test]
    fn solo_human_gate_downgrades_merge_to_draft() {
        let mut case = Case::merge_ready();
        case.gate = HumanGate::Solo;
        assert_eq!(case.decide(), Decision::DraftPr);
    }

    #[test]
    fn other_human_gate_values_do_not_change_the_decision() {
        for gate in [
            HumanGate::NotConsulted,
            HumanGate::Absent,
            HumanGate::Github,
        ] {
            let mut case = Case::merge_ready();
            case.gate = gate;
            assert_eq!(case.decide(), merged());
        }
        assert_eq!(HumanGate::default(), HumanGate::NotConsulted);
    }

    #[test]
    fn merge_args_use_match_head_commit_and_never_admin() {
        for (method, flag) in [
            (MergeMethod::Merge, "--merge"),
            (MergeMethod::Squash, "--squash"),
            (MergeMethod::Rebase, "--rebase"),
        ] {
            let args = merge_args(12, SHA, method);
            assert_eq!(
                args,
                vec!["pr", "merge", "12", "--match-head-commit", SHA, flag]
            );
        }
    }

    fn conclusion() -> impl Strategy<Value = CheckConclusion> {
        prop_oneof![
            Just(CheckConclusion::Success),
            Just(CheckConclusion::Failure),
            Just(CheckConclusion::Pending),
        ]
    }

    fn sha() -> impl Strategy<Value = String> {
        prop_oneof![
            Just(SHA.to_string()),
            Just(OTHER.to_string()),
            "[0-9a-f]{0,40}"
        ]
    }

    proptest! {
        #[test]
        fn merge_implies_every_safety_condition(
            mode in prop_oneof![Just(OutcomeMode::DraftPr), Just(OutcomeMode::MergeWhen)],
            runs in prop::collection::vec(("[ab]", sha(), conclusion()), 0..8),
            judged in sha(),
            live in sha(),
            passes in prop::collection::vec(any::<bool>(), 0..4),
            expected in 0usize..4,
            gate in prop_oneof![
                Just(HumanGate::NotConsulted),
                Just(HumanGate::Absent),
                Just(HumanGate::Github),
                Just(HumanGate::Solo),
            ],
            required in prop::collection::vec("[ab]", 0..3),
        ) {
            let checks: Vec<CheckRun> = runs
                .iter()
                .map(|(n, s, c)| check(n, s, *c))
                .collect();
            let verdicts: Vec<Verdict> = passes
                .iter()
                .map(|p| if *p { Verdict::pass("j", "") } else { Verdict::fail("j", "") })
                .collect();
            let decision = decide(&PolicyInput {
                mode,
                merge_method: MergeMethod::Merge,
                required_checks: &required,
                checks: &checks,
                judged_head_sha: &judged,
                live_head_sha: &live,
                expected_judges: expected,
                verdicts: &verdicts,
                human_gate: gate,
            });
            if let Decision::Merge { head_sha, .. } = decision {
                prop_assert_eq!(mode, OutcomeMode::MergeWhen);
                prop_assert!(gate != HumanGate::Solo);
                prop_assert!(!required.is_empty());
                prop_assert!(expected > 0 && verdicts.len() == expected);
                prop_assert!(verdicts.iter().all(|v| v.passed));
                prop_assert!(judged.eq_ignore_ascii_case(&live));
                prop_assert_eq!(head_sha.len(), 40);
                for name in &required {
                    prop_assert!(checks.iter().any(|c| c.name == *name
                        && c.head_sha.eq_ignore_ascii_case(&judged)
                        && c.conclusion == CheckConclusion::Success));
                    prop_assert!(!checks.iter().any(|c| c.name == *name
                        && c.head_sha.eq_ignore_ascii_case(&judged)
                        && c.conclusion != CheckConclusion::Success));
                }
            }
        }
    }
}
