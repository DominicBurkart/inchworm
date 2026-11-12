use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

macro_rules! failure_classes {
    ($(#[$meta:meta])* $($variant:ident => $name:literal),+ $(,)?) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub enum FailureClass {
            $(
                #[serde(rename = $name)]
                $variant,
            )+
        }

        impl FailureClass {
            pub const ALL: &'static [FailureClass] = &[$(FailureClass::$variant),+];

            pub const fn as_str(self) -> &'static str {
                match self {
                    $(FailureClass::$variant => $name,)+
                }
            }
        }
    };
}

failure_classes! {
    ///
    /// ```
    /// use inchworm::model::FailureClass;
    ///
    /// let class: FailureClass = "ci_red".parse().unwrap();
    /// assert_eq!(class, FailureClass::CiRed);
    /// assert_eq!(class.to_string(), "ci_red");
    /// assert!(FailureClass::ALL.contains(&class));
    /// assert!("bogus".parse::<FailureClass>().is_err());
    /// ```
    SpawnRefused => "spawn_refused",
    Escalated => "escalated",
    ContainerBuildFailed => "container_build_failed",
    ModelError => "model_error",
    MaxIterations => "max_iterations",
    ProtectedPathViolation => "protected_path_violation",
    AgentFailed => "agent_failed",
    TestsFailed => "tests_failed",
    Cancelled => "cancelled",
    Timeout => "timeout",
    OrchestratorLost => "orchestrator_lost",
    EmptyPatch => "empty_patch",
    PatchApplyFailed => "patch_apply_failed",
    CiRed => "ci_red",
    JudgeRejected => "judge_rejected",
    MergeBlocked => "merge_blocked",
}

fn is_unsafe_char(ch: char) -> bool {
    (ch.is_control() && ch != '\n' && ch != '\t')
        || matches!(ch, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// ```
/// use inchworm::model::sanitize_text;
///
/// assert_eq!(sanitize_text("ok\u{1b}[31mred", 100), "ok?[31mred");
/// assert_eq!(sanitize_text("abcdef", 3), "abc...(truncated)");
/// assert_eq!(sanitize_text("a\u{202e}b", 100), "a?b");
/// ```
pub fn sanitize_text(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (count, ch) in text.chars().enumerate() {
        if count == max_chars {
            out.push_str("...(truncated)");
            break;
        }
        if is_unsafe_char(ch) {
            out.push('?');
        } else {
            out.push(ch);
        }
    }
    out
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("unknown failure class `{0}`")]
pub struct UnknownFailureClass(pub String);

impl fmt::Display for FailureClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for FailureClass {
    type Err = UnknownFailureClass;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        FailureClass::ALL
            .iter()
            .copied()
            .find(|class| class.as_str() == s)
            .ok_or_else(|| UnknownFailureClass(s.to_string()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Running,
    AwaitingCi,
    Judging,
    Merged,
    PrOpen,
    Failed,
    Escalated,
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("illegal run transition {from} -> {to}")]
pub struct InvalidTransition {
    pub from: RunState,
    pub to: RunState,
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("unknown run state `{0}`")]
pub struct UnknownRunState(pub String);

impl RunState {
    pub const ALL: &'static [RunState] = &[
        RunState::Queued,
        RunState::Running,
        RunState::AwaitingCi,
        RunState::Judging,
        RunState::Merged,
        RunState::PrOpen,
        RunState::Failed,
        RunState::Escalated,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            RunState::Queued => "queued",
            RunState::Running => "running",
            RunState::AwaitingCi => "awaiting_ci",
            RunState::Judging => "judging",
            RunState::Merged => "merged",
            RunState::PrOpen => "pr_open",
            RunState::Failed => "failed",
            RunState::Escalated => "escalated",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            RunState::Merged | RunState::PrOpen | RunState::Failed | RunState::Escalated
        )
    }

    pub const fn can_transition_to(self, to: RunState) -> bool {
        use RunState::*;
        matches!(
            (self, to),
            (Queued, Running | Failed)
                | (Running, AwaitingCi | Failed | Escalated)
                | (AwaitingCi, Judging | Failed | Running)
                | (Judging, Merged | PrOpen | Failed | Running)
        )
    }

    /// ```
    /// use inchworm::model::RunState;
    ///
    /// let state = RunState::Queued.transition(RunState::Running).unwrap();
    /// assert_eq!(state, RunState::Running);
    /// assert!(RunState::Merged.transition(RunState::Running).is_err());
    /// assert!(RunState::PrOpen.is_terminal());
    /// ```
    pub fn transition(self, to: RunState) -> Result<RunState, InvalidTransition> {
        if self.can_transition_to(to) {
            Ok(to)
        } else {
            Err(InvalidTransition { from: self, to })
        }
    }
}

impl fmt::Display for RunState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RunState {
    type Err = UnknownRunState;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        RunState::ALL
            .iter()
            .copied()
            .find(|state| state.as_str() == s)
            .ok_or_else(|| UnknownRunState(s.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashSet;

    #[test]
    fn sanitize_text_strips_control_characters_and_caps_length() {
        assert_eq!(sanitize_text("a\u{1b}[2Jb\u{7}c\rd", 100), "a?[2Jb?c?d");
        assert_eq!(
            sanitize_text("keep\nnewlines\tand tabs", 100),
            "keep\nnewlines\tand tabs"
        );
        assert_eq!(sanitize_text("abc", 3), "abc");
        assert_eq!(sanitize_text("abcd", 3), "abc...(truncated)");
        assert_eq!(sanitize_text("\u{9b}x", 10), "?x");
        assert_eq!(sanitize_text("", 0), "");
    }

    #[test]
    fn names_are_unique_and_snake_case() {
        let names: HashSet<&str> = FailureClass::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(names.len(), FailureClass::ALL.len());
        for name in names {
            assert!(name.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
            assert!(!name.starts_with('_') && !name.ends_with('_'));
        }
    }

    #[test]
    fn serde_name_matches_as_str() {
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            class: FailureClass,
        }
        for class in FailureClass::ALL {
            let text = toml::to_string(&Wrap { class: *class }).unwrap();
            assert_eq!(text.trim(), format!("class = \"{}\"", class.as_str()));
            let back: Wrap = toml::from_str(&text).unwrap();
            assert_eq!(back.class, *class);
        }
    }

    #[test]
    fn unknown_name_is_rejected() {
        assert_eq!(
            "nope".parse::<FailureClass>(),
            Err(UnknownFailureClass("nope".to_string()))
        );
        assert!("".parse::<FailureClass>().is_err());
        assert!("Timeout".parse::<FailureClass>().is_err());
    }

    #[test]
    fn display_is_as_str() {
        assert_eq!(FailureClass::CiRed.to_string(), "ci_red");
    }

    #[test]
    fn terminal_states_have_no_outgoing_transitions() {
        for from in RunState::ALL.iter().filter(|s| s.is_terminal()) {
            for to in RunState::ALL {
                assert!(!from.can_transition_to(*to), "{from} -> {to}");
            }
        }
    }

    #[test]
    fn happy_path_is_legal() {
        let path = [
            RunState::Queued,
            RunState::Running,
            RunState::AwaitingCi,
            RunState::Judging,
            RunState::Merged,
        ];
        for pair in path.windows(2) {
            assert_eq!(pair[0].transition(pair[1]), Ok(pair[1]));
        }
        assert!(RunState::Judging.can_transition_to(RunState::PrOpen));
        assert!(RunState::Running.can_transition_to(RunState::Escalated));
    }

    #[test]
    fn a_rejected_judging_can_return_to_running_for_a_retry() {
        assert!(RunState::Judging.can_transition_to(RunState::Running));
        assert!(RunState::AwaitingCi.can_transition_to(RunState::Running));
        assert!(!RunState::Judging.can_transition_to(RunState::AwaitingCi));
    }

    #[test]
    fn skipping_ci_or_judging_is_illegal() {
        assert!(!RunState::Running.can_transition_to(RunState::Judging));
        assert!(!RunState::Running.can_transition_to(RunState::Merged));
        assert!(!RunState::AwaitingCi.can_transition_to(RunState::Merged));
        assert!(!RunState::Queued.can_transition_to(RunState::AwaitingCi));
        assert_eq!(
            RunState::Queued.transition(RunState::Merged),
            Err(InvalidTransition {
                from: RunState::Queued,
                to: RunState::Merged
            })
        );
    }

    #[test]
    fn nothing_transitions_to_itself() {
        for state in RunState::ALL {
            assert!(!state.can_transition_to(*state));
        }
    }

    #[test]
    fn every_non_terminal_state_can_reach_failed_or_a_terminal() {
        for start in RunState::ALL.iter().filter(|s| !s.is_terminal()) {
            let mut seen = vec![*start];
            let mut frontier = vec![*start];
            while let Some(current) = frontier.pop() {
                for next in RunState::ALL {
                    if current.can_transition_to(*next) && !seen.contains(next) {
                        seen.push(*next);
                        frontier.push(*next);
                    }
                }
            }
            assert!(seen.iter().any(|s| s.is_terminal()), "{start}");
        }
    }

    proptest! {
        #[test]
        fn run_state_text_round_trips(index in 0..RunState::ALL.len()) {
            let state = RunState::ALL[index];
            prop_assert_eq!(state.to_string().parse::<RunState>(), Ok(state));
        }

        #[test]
        fn transition_agrees_with_can_transition_to(a in 0..RunState::ALL.len(), b in 0..RunState::ALL.len()) {
            let (from, to) = (RunState::ALL[a], RunState::ALL[b]);
            prop_assert_eq!(from.transition(to).is_ok(), from.can_transition_to(to));
        }

        #[test]
        fn random_walks_never_leave_a_terminal_state(steps in prop::collection::vec(0..RunState::ALL.len(), 0..20)) {
            let mut state = RunState::Queued;
            let mut terminal_seen = false;
            for index in steps {
                if let Ok(next) = state.transition(RunState::ALL[index]) {
                    prop_assert!(!terminal_seen);
                    state = next;
                    terminal_seen = state.is_terminal();
                }
            }
        }

        #[test]
        fn unknown_run_state_text_is_rejected(text in "[a-z_]{1,20}") {
            let known = RunState::ALL.iter().any(|s| s.as_str() == text);
            prop_assert_eq!(text.parse::<RunState>().is_ok(), known);
        }
    }

    proptest! {
        #[test]
        fn display_and_parse_round_trip(index in 0..FailureClass::ALL.len()) {
            let class = FailureClass::ALL[index];
            prop_assert_eq!(class.to_string().parse::<FailureClass>(), Ok(class));
        }

        #[test]
        fn arbitrary_text_never_panics(text in ".*") {
            let parsed = text.parse::<FailureClass>();
            prop_assert_eq!(parsed.is_ok(), FailureClass::ALL.iter().any(|c| c.as_str() == text));
        }
    }
}
