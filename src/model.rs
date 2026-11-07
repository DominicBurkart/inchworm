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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashSet;

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
