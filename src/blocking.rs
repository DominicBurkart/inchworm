use std::collections::BTreeSet;

use crate::config::BlockingSpec;
use crate::policy::{CheckConclusion, CheckRun};

fn undefined(base: &str, why: &str) -> Unresolved {
    Unresolved(format!(
        "the required status checks of branch `{base}` are undefined: the branch protection {why}"
    ))
}

/// What a flow's blocking checks are, once the branch protection has been read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// Exactly these names block. An empty list means that no check blocks.
    Names(Vec<String>),
    /// Every check run and commit status on the judged head blocks. The branch protection's
    /// required names, when they could be read, must additionally be present.
    All { required: Vec<String> },
}

/// Why the blocking checks could not be decided. The decision fails closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unresolved(pub String);

/// ```
/// use inchworm::blocking::{resolve, Resolved};
/// use inchworm::config::BlockingSpec;
///
/// let list = ["build".to_string()];
/// assert_eq!(
///     resolve(&BlockingSpec::Explicit(&list), "main", || unreachable!()).unwrap(),
///     Resolved::Names(vec!["build".into()])
/// );
/// let extra = ["lint".to_string()];
/// let read = || Ok(Some(vec!["build".to_string()]));
/// assert_eq!(
///     resolve(&BlockingSpec::Required { extra: &extra }, "main", read).unwrap(),
///     Resolved::Names(vec!["build".into(), "lint".into()])
/// );
/// assert!(resolve(&BlockingSpec::Required { extra: &[] }, "main", || Ok(None)).is_err());
/// ```
pub fn resolve(
    spec: &BlockingSpec<'_>,
    base: &str,
    protection: impl FnOnce() -> Result<Option<Vec<String>>, String>,
) -> Result<Resolved, Unresolved> {
    match spec {
        BlockingSpec::Explicit(list) => Ok(Resolved::Names(list.to_vec())),
        BlockingSpec::Required { extra } => match protection() {
            Err(detail) => Err(undefined(base, &format!("cannot be read: {detail}"))),
            Ok(None) => Err(undefined(base, "does not exist")),
            Ok(Some(required)) if required.is_empty() => Err(undefined(base, "lists no checks")),
            Ok(Some(mut names)) => {
                for name in *extra {
                    if !names.contains(name) {
                        names.push(name.clone());
                    }
                }
                Ok(Resolved::Names(names))
            }
        },
        BlockingSpec::All => Ok(Resolved::All {
            required: protection().ok().flatten().unwrap_or_default(),
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllStatus {
    Green,
    Red(String),
    Vanished(String),
    Waiting(Vec<String>),
}

/// Decides when "every check on this commit" is settled.
///
/// Checks that have not started are invisible until GitHub reports them, so the set is only
/// trusted after it has stopped changing: green needs at least one successful check, none red
/// or pending, every required name present, no check seen earlier gone again, and no new check
/// name for `quiet` seconds. A `skipped` or `neutral` check is satisfied, as it is for GitHub's
/// own branch protection, but it never counts as the successful check.
///
/// ```
/// use inchworm::blocking::{AllStatus, Settler};
/// use inchworm::policy::{CheckConclusion, CheckRun};
///
/// let run = |name: &str| CheckRun {
///     name: name.into(),
///     head_sha: "abc".into(),
///     conclusion: CheckConclusion::Success,
/// };
/// let mut settler = Settler::new(60);
/// assert_eq!(settler.observe(&[], &[run("build")], "abc", 0), AllStatus::Waiting(vec![]));
/// assert_eq!(settler.observe(&[], &[run("build")], "abc", 60), AllStatus::Green);
/// ```
#[derive(Debug, Clone)]
pub struct Settler {
    seen: BTreeSet<String>,
    changed_at: Option<i64>,
    quiet: i64,
}

impl Settler {
    pub fn new(quiet_secs: i64) -> Settler {
        Settler {
            seen: BTreeSet::new(),
            changed_at: None,
            quiet: quiet_secs.max(0),
        }
    }

    pub fn observe(
        &mut self,
        required: &[String],
        runs: &[CheckRun],
        sha: &str,
        now: i64,
    ) -> AllStatus {
        let here: Vec<&CheckRun> = runs
            .iter()
            .filter(|run| run.head_sha.eq_ignore_ascii_case(sha))
            .collect();
        let names: BTreeSet<String> = here.iter().map(|run| run.name.clone()).collect();
        let any_success = here
            .iter()
            .any(|run| run.conclusion == CheckConclusion::Success);
        if let Some(run) = here
            .iter()
            .find(|run| run.conclusion == CheckConclusion::Failure)
        {
            return AllStatus::Red(run.name.clone());
        }
        if let Some(gone) = self.seen.iter().find(|name| !names.contains(*name)) {
            return AllStatus::Vanished(gone.clone());
        }
        let grew = names.iter().any(|name| !self.seen.contains(name));
        if grew || self.changed_at.is_none() {
            self.changed_at = Some(now);
        }
        self.seen.extend(names.iter().cloned());

        let mut waiting: Vec<String> = required
            .iter()
            .filter(|name| !names.contains(*name))
            .cloned()
            .collect();
        for run in &here {
            if run.conclusion == CheckConclusion::Pending && !waiting.contains(&run.name) {
                waiting.push(run.name.clone());
            }
        }
        if !waiting.is_empty() {
            return AllStatus::Waiting(waiting);
        }
        let settled = self
            .changed_at
            .is_some_and(|at| now.saturating_sub(at) >= self.quiet);
        if !any_success || !settled {
            return AllStatus::Waiting(Vec::new());
        }
        AllStatus::Green
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn run(name: &str, sha: &str, conclusion: CheckConclusion) -> CheckRun {
        CheckRun {
            name: name.into(),
            head_sha: sha.into(),
            conclusion,
        }
    }

    fn ok(name: &str) -> CheckRun {
        run(name, SHA, CheckConclusion::Success)
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn explicit_lists_never_read_the_branch_protection() {
        let list = names(&["a"]);
        let resolved = resolve(&BlockingSpec::Explicit(&list), "main", || {
            panic!("must not read")
        });
        assert_eq!(resolved, Ok(Resolved::Names(names(&["a"]))));
        let empty: Vec<String> = Vec::new();
        assert_eq!(
            resolve(&BlockingSpec::Explicit(&empty), "main", || panic!("no")),
            Ok(Resolved::Names(Vec::new()))
        );
    }

    #[test]
    fn required_is_the_protection_plus_extra_without_duplicates() {
        let extra = names(&["lint", "build"]);
        let resolved = resolve(&BlockingSpec::Required { extra: &extra }, "main", || {
            Ok(Some(names(&["build", "test"])))
        });
        assert_eq!(
            resolved,
            Ok(Resolved::Names(names(&["build", "test", "lint"])))
        );
    }

    #[test]
    fn required_fails_closed_when_protection_is_unreadable_absent_or_empty() {
        for (read, expect) in [
            (Err("boom".to_string()), "cannot be read: boom"),
            (Ok(None), "does not exist"),
            (Ok(Some(Vec::new())), "lists no checks"),
        ] {
            let err = resolve(&BlockingSpec::Required { extra: &[] }, "dev", || read).unwrap_err();
            assert!(err.0.contains(expect), "{err:?}");
        }
        let extra = names(&["lint"]);
        assert!(
            resolve(&BlockingSpec::Required { extra: &extra }, "dev", || Ok(
                None
            ))
            .is_err()
        );
    }

    #[test]
    fn all_does_not_need_readable_protection() {
        assert_eq!(
            resolve(&BlockingSpec::All, "main", || Err("x".into())),
            Ok(Resolved::All {
                required: Vec::new()
            })
        );
        assert_eq!(
            resolve(&BlockingSpec::All, "main", || Ok(Some(names(&["a"])))),
            Ok(Resolved::All {
                required: names(&["a"])
            })
        );
    }

    #[test]
    fn green_needs_a_check_and_a_quiet_period() {
        let mut settler = Settler::new(60);
        assert_eq!(
            settler.observe(&[], &[], SHA, 0),
            AllStatus::Waiting(vec![])
        );
        assert_eq!(
            settler.observe(&[], &[], SHA, 500),
            AllStatus::Waiting(vec![])
        );
        assert_eq!(
            settler.observe(&[], &[ok("a")], SHA, 600),
            AllStatus::Waiting(vec![])
        );
        assert_eq!(
            settler.observe(&[], &[ok("a")], SHA, 659),
            AllStatus::Waiting(vec![])
        );
        assert_eq!(settler.observe(&[], &[ok("a")], SHA, 660), AllStatus::Green);
    }

    #[test]
    fn a_check_that_appears_late_restarts_the_quiet_period() {
        let mut settler = Settler::new(60);
        assert_eq!(
            settler.observe(&[], &[ok("build")], SHA, 0),
            AllStatus::Waiting(vec![])
        );
        assert_eq!(
            settler.observe(&[], &[ok("build")], SHA, 50),
            AllStatus::Waiting(vec![])
        );
        let both = [ok("build"), run("deploy", SHA, CheckConclusion::Pending)];
        assert_eq!(
            settler.observe(&[], &both, SHA, 55),
            AllStatus::Waiting(names(&["deploy"]))
        );
        let done = [ok("build"), ok("deploy")];
        assert_eq!(
            settler.observe(&[], &done, SHA, 100),
            AllStatus::Waiting(vec![])
        );
        assert_eq!(
            settler.observe(&[], &done, SHA, 114),
            AllStatus::Waiting(vec![])
        );
        assert_eq!(settler.observe(&[], &done, SHA, 115), AllStatus::Green);
    }

    #[test]
    fn a_check_that_disappears_after_being_seen_is_not_green() {
        let mut settler = Settler::new(0);
        assert_eq!(
            settler.observe(&[], &[ok("a"), ok("b")], SHA, 0),
            AllStatus::Green
        );
        assert_eq!(
            settler.observe(&[], &[ok("a")], SHA, 10),
            AllStatus::Vanished("b".into())
        );
    }

    #[test]
    fn any_red_check_is_red_even_with_a_later_green_rerun_of_the_same_name() {
        let mut settler = Settler::new(0);
        let runs = [
            run("a", SHA, CheckConclusion::Failure),
            run("a", SHA, CheckConclusion::Success),
            ok("b"),
        ];
        assert_eq!(
            settler.observe(&[], &runs, SHA, 0),
            AllStatus::Red("a".into())
        );
    }

    #[test]
    fn pending_and_missing_required_checks_wait() {
        let mut settler = Settler::new(0);
        let runs = [ok("a"), run("b", SHA, CheckConclusion::Pending)];
        assert_eq!(
            settler.observe(&names(&["c"]), &runs, SHA, 0),
            AllStatus::Waiting(names(&["c", "b"]))
        );
        assert_eq!(
            settler.observe(&names(&["a"]), &[ok("a"), ok("b")], SHA, 1),
            AllStatus::Green
        );
    }

    #[test]
    fn other_commits_are_ignored_and_case_in_the_sha_does_not_matter() {
        let mut settler = Settler::new(0);
        let old = run("a", "bbbb", CheckConclusion::Failure);
        assert_eq!(
            settler.observe(&[], std::slice::from_ref(&old), SHA, 0),
            AllStatus::Waiting(vec![])
        );
        let upper = run("a", &SHA.to_uppercase(), CheckConclusion::Success);
        assert_eq!(
            settler.observe(&[], &[old, upper], SHA, 1),
            AllStatus::Green
        );
    }

    #[test]
    fn skipped_checks_are_satisfied_but_a_set_of_only_skipped_checks_is_never_green() {
        let skipped = |name: &str| run(name, SHA, CheckConclusion::Skipped);
        let mut settler = Settler::new(0);
        assert_eq!(
            settler.observe(&[], &[ok("build"), skipped("release")], SHA, 0),
            AllStatus::Green
        );
        let mut settler = Settler::new(0);
        assert_eq!(
            settler.observe(&[], &[skipped("release"), skipped("scan")], SHA, 0),
            AllStatus::Waiting(vec![])
        );
        let mut settler = Settler::new(0);
        assert_eq!(
            settler.observe(&[], &[ok("build"), skipped("release")], SHA, 0),
            AllStatus::Green
        );
        assert_eq!(
            settler.observe(&[], &[ok("build")], SHA, 1),
            AllStatus::Vanished("release".into())
        );
    }

    #[test]
    fn a_negative_quiet_period_is_treated_as_none() {
        let mut settler = Settler::new(-5);
        assert_eq!(settler.observe(&[], &[ok("a")], SHA, 0), AllStatus::Green);
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        fn conclusion() -> impl Strategy<Value = CheckConclusion> {
            prop_oneof![
                Just(CheckConclusion::Success),
                Just(CheckConclusion::Failure),
                Just(CheckConclusion::Pending),
            ]
        }

        proptest! {
            #[test]
            fn green_only_when_every_check_on_the_sha_succeeded_and_the_set_was_quiet(
                steps in proptest::collection::vec(
                    proptest::collection::vec((0usize..4, conclusion(), any::<bool>()), 0..6),
                    1..8,
                ),
                quiet in 0i64..5,
            ) {
                let mut settler = Settler::new(quiet);
                let mut last_set_change = None::<i64>;
                let mut seen = BTreeSet::new();
                for (tick, step) in steps.iter().enumerate() {
                    let now = tick as i64;
                    let runs: Vec<CheckRun> = step
                        .iter()
                        .map(|(n, c, same)| run(
                            &format!("c{n}"),
                            if *same { SHA } else { "other" },
                            *c,
                        ))
                        .collect();
                    let here: Vec<&CheckRun> = runs.iter().filter(|r| r.head_sha == SHA).collect();
                    let current: BTreeSet<String> = here.iter().map(|r| r.name.clone()).collect();
                    let vanished = seen.iter().any(|n| !current.contains(n));
                    let verdict = settler.observe(&[], &runs, SHA, now);
                    if verdict == AllStatus::Green {
                        prop_assert!(!current.is_empty());
                        prop_assert!(!vanished);
                        prop_assert!(here.iter().all(|r| r.conclusion == CheckConclusion::Success));
                        if current.iter().any(|n| !seen.contains(n)) {
                            last_set_change = Some(now);
                        }
                        prop_assert!(
                            now - last_set_change.unwrap_or(now) >= quiet
                                || last_set_change.is_none()
                        );
                    }
                    if !vanished && !here.iter().any(|r| r.conclusion == CheckConclusion::Failure) {
                        if current.iter().any(|n| !seen.contains(n)) {
                            last_set_change = Some(now);
                        }
                        seen.extend(current);
                    }
                }
            }
        }
    }
}
