use crate::config::{MergeMethod, OutcomeMode};
use crate::judges::Verdict;
use crate::model::FailureClass;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckConclusion {
    Success,
    Failure,
    Pending,
}

impl CheckConclusion {
    /// ```
    /// use inchworm::policy::CheckConclusion;
    ///
    /// assert_eq!(CheckConclusion::from_github("completed", Some("success")), CheckConclusion::Success);
    /// assert_eq!(CheckConclusion::from_github("completed", Some("skipped")), CheckConclusion::Failure);
    /// assert_eq!(CheckConclusion::from_github("completed", Some("neutral")), CheckConclusion::Failure);
    /// assert_eq!(CheckConclusion::from_github("completed", None), CheckConclusion::Failure);
    /// assert_eq!(CheckConclusion::from_github("in_progress", None), CheckConclusion::Pending);
    /// assert_eq!(CheckConclusion::from_github("queued", Some("success")), CheckConclusion::Pending);
    /// ```
    pub fn from_github(status: &str, conclusion: Option<&str>) -> CheckConclusion {
        if !status.eq_ignore_ascii_case("completed") {
            return CheckConclusion::Pending;
        }
        match conclusion {
            Some(value) if value.eq_ignore_ascii_case("success") => CheckConclusion::Success,
            _ => CheckConclusion::Failure,
        }
    }
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
    pub expected_judges: &'a [String],
    pub verdicts: &'a [Verdict],
    pub human_gate: HumanGate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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

fn verdicts_match_judges(expected: &[String], verdicts: &[Verdict]) -> bool {
    verdicts.len() == expected.len()
        && expected.iter().enumerate().all(|(index, name)| {
            verdicts
                .iter()
                .filter(|verdict| verdict.judge == *name && verdict.index == index)
                .count()
                == 1
        })
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
///     expected_judges: &["diff".to_string()],
///     verdicts: &verdicts,
///     human_gate: HumanGate::NotConsulted,
/// };
/// assert_eq!(decide(&input), Decision::DraftPr);
/// ```
pub fn decide(input: &PolicyInput<'_>) -> Decision {
    let matched = verdicts_match_judges(input.expected_judges, input.verdicts);
    let all_passed = input.verdicts.iter().all(|verdict| verdict.passed);
    let ci = evaluate_checks(input);

    if !matched || !all_passed {
        let only_model_errors = matched
            && input
                .verdicts
                .iter()
                .filter(|verdict| !verdict.passed)
                .all(|verdict| verdict.model_error);
        return Decision::Reject(if only_model_errors {
            FailureClass::ModelError
        } else {
            FailureClass::JudgeRejected
        });
    }
    if matches!(ci, Ci::Red) {
        return Decision::Reject(FailureClass::CiRed);
    }

    match input.mode {
        OutcomeMode::DraftPr => Decision::DraftPr,
        OutcomeMode::MergeWhen => {
            if matches!(input.human_gate, HumanGate::Solo | HumanGate::NotConsulted) {
                return Decision::DraftPr;
            }
            if input.required_checks.is_empty()
                || input.expected_judges.is_empty()
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
    const SHA64: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn check(name: &str, sha: &str, conclusion: CheckConclusion) -> CheckRun {
        CheckRun {
            name: name.to_string(),
            head_sha: sha.to_string(),
            conclusion,
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    fn passed(name: &str, index: usize) -> Verdict {
        Verdict::pass(name, "ok").with_index(index)
    }

    fn failed(name: &str, index: usize) -> Verdict {
        Verdict::fail(name, "no").with_index(index)
    }

    struct Case {
        mode: OutcomeMode,
        method: MergeMethod,
        required: Vec<String>,
        checks: Vec<CheckRun>,
        judged: String,
        live: String,
        expected: Vec<String>,
        verdicts: Vec<Verdict>,
        gate: HumanGate,
    }

    impl Case {
        fn merge_ready() -> Case {
            Case {
                mode: OutcomeMode::MergeWhen,
                method: MergeMethod::Merge,
                required: names(&["build", "test"]),
                checks: vec![
                    check("build", SHA, CheckConclusion::Success),
                    check("test", SHA, CheckConclusion::Success),
                ],
                judged: SHA.into(),
                live: SHA.into(),
                expected: names(&["diff", "llm"]),
                verdicts: vec![passed("diff", 0), passed("llm", 1)],
                gate: HumanGate::Absent,
            }
        }

        fn decide(&self) -> Decision {
            decide(&PolicyInput {
                mode: self.mode,
                merge_method: self.method,
                required_checks: &self.required,
                checks: &self.checks,
                judged_head_sha: &self.judged,
                live_head_sha: &self.live,
                expected_judges: &self.expected,
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
    fn the_configured_merge_method_is_carried_into_the_decision() {
        for method in [MergeMethod::Merge, MergeMethod::Squash, MergeMethod::Rebase] {
            let mut case = Case::merge_ready();
            case.method = method;
            assert_eq!(
                case.decide(),
                Decision::Merge {
                    head_sha: SHA.into(),
                    method
                }
            );
        }
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
    fn a_failed_judge_wins_over_a_red_check_in_both_modes() {
        for mode in [OutcomeMode::DraftPr, OutcomeMode::MergeWhen] {
            let mut case = Case::merge_ready();
            case.mode = mode;
            case.checks[0].conclusion = CheckConclusion::Failure;
            case.verdicts[1] = failed("llm", 1);
            assert_eq!(case.decide(), Decision::Reject(FailureClass::JudgeRejected));
        }
    }

    #[test]
    fn a_red_check_is_ci_red_even_with_a_pending_one() {
        let mut case = Case::merge_ready();
        case.checks[0].conclusion = CheckConclusion::Pending;
        case.checks[1].conclusion = CheckConclusion::Failure;
        assert_eq!(case.decide(), Decision::Reject(FailureClass::CiRed));
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
    fn a_red_then_green_rerun_of_the_same_check_and_sha_stays_red() {
        let mut case = Case::merge_ready();
        case.checks
            .push(check("test", SHA, CheckConclusion::Failure));
        case.checks
            .push(check("test", SHA, CheckConclusion::Success));
        assert_eq!(case.decide(), Decision::Reject(FailureClass::CiRed));
    }

    #[test]
    fn never_merges_when_the_live_head_moved() {
        let mut case = Case::merge_ready();
        case.live = OTHER.into();
        assert_eq!(case.decide(), Decision::Reject(FailureClass::MergeBlocked));
    }

    #[test]
    fn a_prefix_or_near_miss_of_the_head_is_not_the_same_head() {
        for live in [
            &SHA[..7],
            &SHA[..39],
            "0123456789abcdef0123456789abcdef01234568",
        ] {
            let mut case = Case::merge_ready();
            case.live = live.into();
            assert_eq!(
                case.decide(),
                Decision::Reject(FailureClass::MergeBlocked),
                "{live}"
            );
        }
    }

    #[test]
    fn head_comparison_ignores_hex_case_and_emits_lowercase() {
        let mut case = Case::merge_ready();
        case.live = SHA.to_uppercase();
        assert_eq!(case.decide(), merged());
        case.judged = SHA.to_uppercase();
        case.live = SHA.into();
        case.checks = vec![
            check("build", SHA, CheckConclusion::Success),
            check("test", SHA, CheckConclusion::Success),
        ];
        assert_eq!(case.decide(), merged());
    }

    #[test]
    fn a_sha256_head_is_accepted_and_other_lengths_are_blocked() {
        let mut case = Case::merge_ready();
        case.judged = SHA64.into();
        case.live = SHA64.into();
        case.checks = vec![
            check("build", SHA64, CheckConclusion::Success),
            check("test", SHA64, CheckConclusion::Success),
        ];
        assert_eq!(
            case.decide(),
            Decision::Merge {
                head_sha: SHA64.into(),
                method: MergeMethod::Merge
            }
        );
        let odd = &SHA64[..50];
        case.judged = odd.into();
        case.live = odd.into();
        case.checks = vec![
            check("build", odd, CheckConclusion::Success),
            check("test", odd, CheckConclusion::Success),
        ];
        assert_eq!(case.decide(), Decision::Reject(FailureClass::MergeBlocked));
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
        case.verdicts[1] = failed("llm", 1);
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
        case.verdicts.push(passed("extra", 2));
        assert_eq!(case.decide(), Decision::Reject(FailureClass::JudgeRejected));
    }

    #[test]
    fn two_diff_passes_and_no_llm_verdict_never_merge() {
        let mut case = Case::merge_ready();
        case.verdicts = vec![passed("diff", 0), passed("diff", 1)];
        assert_eq!(case.decide(), Decision::Reject(FailureClass::JudgeRejected));
        case.verdicts = vec![passed("diff", 0), passed("diff", 0)];
        assert_eq!(case.decide(), Decision::Reject(FailureClass::JudgeRejected));
    }

    #[test]
    fn verdicts_must_match_the_configured_judge_by_kind_and_index() {
        let mut swapped = Case::merge_ready();
        swapped.verdicts = vec![passed("llm", 0), passed("diff", 1)];
        assert_eq!(
            swapped.decide(),
            Decision::Reject(FailureClass::JudgeRejected)
        );

        let mut reordered = Case::merge_ready();
        reordered.verdicts = vec![passed("llm", 1), passed("diff", 0)];
        assert_eq!(reordered.decide(), merged());

        let mut two_llm = Case::merge_ready();
        two_llm.expected = names(&["llm", "llm"]);
        two_llm.verdicts = vec![passed("llm", 0), passed("llm", 1)];
        assert_eq!(two_llm.decide(), merged());
        two_llm.verdicts = vec![passed("llm", 0), passed("llm", 0)];
        assert_eq!(
            two_llm.decide(),
            Decision::Reject(FailureClass::JudgeRejected)
        );
    }

    #[test]
    fn infrastructure_failures_are_model_errors_only_when_nothing_else_failed() {
        let mut case = Case::merge_ready();
        case.verdicts[1] = Verdict::fail("llm", "ollama down")
            .with_index(1)
            .with_model_error();
        assert_eq!(case.decide(), Decision::Reject(FailureClass::ModelError));

        case.verdicts[0] = failed("diff", 0);
        assert_eq!(case.decide(), Decision::Reject(FailureClass::JudgeRejected));

        let mut missing = Case::merge_ready();
        missing.verdicts = vec![
            Verdict::fail("llm", "down")
                .with_index(1)
                .with_model_error(),
        ];
        assert_eq!(
            missing.decide(),
            Decision::Reject(FailureClass::JudgeRejected)
        );
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
        case.expected.clear();
        case.verdicts.clear();
        assert_eq!(case.decide(), Decision::Reject(FailureClass::MergeBlocked));
    }

    #[test]
    fn unconsulted_and_solo_human_gates_downgrade_merge_to_draft() {
        for gate in [HumanGate::Solo, HumanGate::NotConsulted] {
            let mut case = Case::merge_ready();
            case.gate = gate;
            assert_eq!(case.decide(), Decision::DraftPr, "{gate:?}");
        }
        assert_eq!(HumanGate::default(), HumanGate::NotConsulted);
    }

    #[test]
    fn absent_and_github_human_gates_allow_merge() {
        for gate in [HumanGate::Absent, HumanGate::Github] {
            let mut case = Case::merge_ready();
            case.gate = gate;
            assert_eq!(case.decide(), merged());
        }
    }

    #[test]
    fn the_unconsulted_gate_still_rejects_bad_judges_and_red_checks() {
        let mut bad_judge = Case::merge_ready();
        bad_judge.gate = HumanGate::NotConsulted;
        bad_judge.verdicts[0] = failed("diff", 0);
        assert_eq!(
            bad_judge.decide(),
            Decision::Reject(FailureClass::JudgeRejected)
        );
        let mut red = Case::merge_ready();
        red.gate = HumanGate::NotConsulted;
        red.checks[0].conclusion = CheckConclusion::Failure;
        assert_eq!(red.decide(), Decision::Reject(FailureClass::CiRed));
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

    #[test]
    fn github_conclusions_map_fail_closed() {
        let table = [
            ("completed", Some("success"), CheckConclusion::Success),
            ("completed", Some("SUCCESS"), CheckConclusion::Success),
            ("completed", Some("failure"), CheckConclusion::Failure),
            ("completed", Some("cancelled"), CheckConclusion::Failure),
            ("completed", Some("timed_out"), CheckConclusion::Failure),
            (
                "completed",
                Some("action_required"),
                CheckConclusion::Failure,
            ),
            ("completed", Some("stale"), CheckConclusion::Failure),
            ("completed", Some("skipped"), CheckConclusion::Failure),
            ("completed", Some("neutral"), CheckConclusion::Failure),
            ("completed", Some("something_new"), CheckConclusion::Failure),
            ("completed", None, CheckConclusion::Failure),
            ("COMPLETED", Some("success"), CheckConclusion::Success),
            ("in_progress", None, CheckConclusion::Pending),
            ("queued", None, CheckConclusion::Pending),
            ("waiting", Some("success"), CheckConclusion::Pending),
            ("", None, CheckConclusion::Pending),
        ];
        for (status, conclusion, expected) in table {
            assert_eq!(
                CheckConclusion::from_github(status, conclusion),
                expected,
                "{status} {conclusion:?}"
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
            Just(SHA64.to_string()),
            Just(SHA[..39].to_string()),
            Just(SHA.to_uppercase()),
            Just(format!("{}8", &SHA[..39])),
            Just(SHA[..7].to_string()),
            "[0-9a-f]{0,64}"
        ]
    }

    fn judge_names() -> impl Strategy<Value = Vec<String>> {
        prop::collection::vec(
            prop_oneof![Just("diff".to_string()), Just("llm".to_string())],
            0..4,
        )
    }

    fn gate() -> impl Strategy<Value = HumanGate> {
        prop_oneof![
            Just(HumanGate::NotConsulted),
            Just(HumanGate::Absent),
            Just(HumanGate::Github),
            Just(HumanGate::Solo),
        ]
    }

    proptest! {
        #[test]
        fn merge_implies_every_safety_condition(
            mode in prop_oneof![Just(OutcomeMode::DraftPr), Just(OutcomeMode::MergeWhen)],
            runs in prop::collection::vec(("[ab]", sha(), conclusion()), 0..8),
            judged in sha(),
            live in sha(),
            expected in judge_names(),
            reported in judge_names(),
            passes in prop::collection::vec(any::<bool>(), 0..4),
            gate in gate(),
            required in prop::collection::vec("[ab]", 0..3),
        ) {
            let checks: Vec<CheckRun> = runs.iter().map(|(n, s, c)| check(n, s, *c)).collect();
            let verdicts: Vec<Verdict> = reported
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let ok = passes.get(i).copied().unwrap_or(true);
                    if ok { passed(name, i) } else { failed(name, i) }
                })
                .collect();
            let decision = decide(&PolicyInput {
                mode,
                merge_method: MergeMethod::Merge,
                required_checks: &required,
                checks: &checks,
                judged_head_sha: &judged,
                live_head_sha: &live,
                expected_judges: &expected,
                verdicts: &verdicts,
                human_gate: gate,
            });
            if let Decision::Merge { head_sha, .. } = decision {
                prop_assert_eq!(mode, OutcomeMode::MergeWhen);
                prop_assert!(gate == HumanGate::Absent || gate == HumanGate::Github);
                prop_assert!(!required.is_empty());
                prop_assert!(!expected.is_empty());
                prop_assert_eq!(&expected, &reported);
                prop_assert!(verdicts.iter().all(|v| v.passed));
                prop_assert!(judged.eq_ignore_ascii_case(&live));
                prop_assert!(head_sha.len() == 40 || head_sha.len() == 64);
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

        #[test]
        fn every_condition_holding_always_merges(
            expected in judge_names().prop_filter("needs a judge", |names| !names.is_empty()),
            required in prop::collection::vec("[ab]", 1..3),
            use_github_gate in any::<bool>(),
            use_sha64 in any::<bool>(),
            upper in any::<bool>(),
        ) {
            let head = if use_sha64 { SHA64 } else { SHA };
            let live = if upper { head.to_uppercase() } else { head.to_string() };
            let checks: Vec<CheckRun> = required
                .iter()
                .map(|name| check(name, head, CheckConclusion::Success))
                .collect();
            let verdicts: Vec<Verdict> = expected
                .iter()
                .enumerate()
                .map(|(i, name)| passed(name, i))
                .collect();
            let decision = decide(&PolicyInput {
                mode: OutcomeMode::MergeWhen,
                merge_method: MergeMethod::Squash,
                required_checks: &required,
                checks: &checks,
                judged_head_sha: head,
                live_head_sha: &live,
                expected_judges: &expected,
                verdicts: &verdicts,
                human_gate: if use_github_gate { HumanGate::Github } else { HumanGate::Absent },
            });
            prop_assert_eq!(
                decision,
                Decision::Merge { head_sha: head.to_string(), method: MergeMethod::Squash }
            );
        }
    }
}
