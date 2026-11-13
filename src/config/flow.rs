use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::FailureClass;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid flow file: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("flow file defines no flows")]
    NoFlows,
    #[error("flow name must not be empty")]
    EmptyFlowName,
    #[error("duplicate flow name `{0}`")]
    DuplicateFlow(String),
    #[error("flow `{flow}`: {reason}")]
    Invalid { flow: String, reason: String },
}

fn invalid(flow: &Flow, reason: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        flow: flow.name.clone(),
        reason: reason.into(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlowFile {
    pub flow: Vec<Flow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Flow {
    pub name: String,
    pub trigger: Trigger,
    #[serde(default = "default_base")]
    pub base: String,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default)]
    pub retry_on: Vec<FailureClass>,
    #[serde(default = "default_attempt_timeout_secs")]
    pub attempt_timeout_secs: u64,
    #[serde(default)]
    pub step: Vec<Step>,
    #[serde(default)]
    pub judge: Vec<Judge>,
    pub outcome: Outcome,
}

fn default_base() -> String {
    "main".to_string()
}

fn default_max_attempts() -> u32 {
    3
}

fn default_attempt_timeout_secs() -> u64 {
    7200
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Trigger {
    Label { label: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub name: String,
    pub identity: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Judge {
    Diff {
        max_lines: u32,
        #[serde(default)]
        forbid_paths: Vec<String>,
    },
    NannaClean {},
    Llm {
        rubric: PathBuf,
        model: String,
        min_score: u8,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeMode {
    DraftPr,
    MergeWhen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMethod {
    #[default]
    Merge,
    Squash,
    Rebase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outcome {
    pub mode: OutcomeMode,
    #[serde(default)]
    pub required_checks: Vec<String>,
    #[serde(default)]
    pub merge_method: MergeMethod,
}

impl Judge {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Judge::Diff { .. } => "diff",
            Judge::NannaClean {} => "nanna_clean",
            Judge::Llm { .. } => "llm",
        }
    }
}

impl FlowFile {
    /// ```
    /// use inchworm::config::{FlowFile, OutcomeMode};
    ///
    /// let file = FlowFile::parse(r#"
    /// [[flow]]
    /// name = "issue"
    /// trigger = { kind = "label", label = "inchworm" }
    ///
    /// [[flow.step]]
    /// name = "implement"
    /// identity = "rust-implementer"
    ///
    /// [flow.outcome]
    /// mode = "draft_pr"
    /// "#).unwrap();
    ///
    /// assert_eq!(file.flow[0].base, "main");
    /// assert_eq!(file.flow[0].max_attempts, 3);
    /// assert_eq!(file.flow[0].outcome.mode, OutcomeMode::DraftPr);
    ///
    /// assert!(FlowFile::parse("flow = []").is_err());
    /// ```
    pub fn parse(text: &str) -> Result<FlowFile, ConfigError> {
        let file: FlowFile = toml::from_str(text)?;
        file.validate()?;
        Ok(file)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.flow.is_empty() {
            return Err(ConfigError::NoFlows);
        }
        let mut names = HashSet::new();
        for flow in &self.flow {
            if flow.name.trim().is_empty() {
                return Err(ConfigError::EmptyFlowName);
            }
            if !names.insert(flow.name.as_str()) {
                return Err(ConfigError::DuplicateFlow(flow.name.clone()));
            }
            flow.validate()?;
        }
        Ok(())
    }
}

impl Flow {
    pub fn validate(&self) -> Result<(), ConfigError> {
        let Trigger::Label { label } = &self.trigger;
        if label.trim().is_empty() {
            return Err(invalid(self, "trigger label must not be empty"));
        }
        if self.base.trim().is_empty() {
            return Err(invalid(self, "base must not be empty"));
        }
        if self.max_attempts == 0 {
            return Err(invalid(self, "max_attempts must be at least 1"));
        }
        if self.attempt_timeout_secs == 0 {
            return Err(invalid(self, "attempt_timeout_secs must be at least 1"));
        }
        if self.step.is_empty() {
            return Err(invalid(self, "at least one step is required"));
        }
        let mut steps = HashSet::new();
        for step in &self.step {
            if step.name.trim().is_empty() {
                return Err(invalid(self, "step name must not be empty"));
            }
            if step.identity.trim().is_empty() {
                return Err(invalid(
                    self,
                    format!("step `{}` has an empty identity", step.name),
                ));
            }
            if !steps.insert(step.name.as_str()) {
                return Err(invalid(
                    self,
                    format!("duplicate step name `{}`", step.name),
                ));
            }
        }
        for judge in &self.judge {
            match judge {
                Judge::Diff { max_lines, .. } if *max_lines == 0 => {
                    return Err(invalid(self, "diff judge max_lines must be at least 1"));
                }
                Judge::Llm {
                    rubric,
                    model,
                    min_score,
                } => {
                    if rubric.as_os_str().is_empty() || model.trim().is_empty() {
                        return Err(invalid(self, "llm judge needs a rubric and a model"));
                    }
                    if *min_score > 10 {
                        return Err(invalid(self, "llm judge min_score must be at most 10"));
                    }
                }
                Judge::Diff { forbid_paths, .. } => {
                    crate::judges::compile_forbid_paths(forbid_paths)
                        .map_err(|reason| invalid(self, reason))?;
                }
                _ => {}
            }
        }
        if self.outcome.mode == OutcomeMode::MergeWhen {
            if self.judge.is_empty() {
                return Err(invalid(self, "merge_when requires at least one judge"));
            }
            if self.outcome.required_checks.is_empty() {
                return Err(invalid(
                    self,
                    "merge_when requires a non-empty required_checks list",
                ));
            }
        }
        if self
            .outcome
            .required_checks
            .iter()
            .any(|c| c.trim().is_empty())
        {
            return Err(invalid(
                self,
                "required_checks must not contain empty names",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn empty_flow_and_step_names_are_rejected() {
        let blank_flow = FULL.replacen("name = \"issue\"", "name = \" \"", 1);
        assert!(matches!(
            FlowFile::parse(&blank_flow),
            Err(ConfigError::EmptyFlowName)
        ));
        let blank_step = FULL.replacen("name = \"implement\"", "name = \"\"", 1);
        assert!(
            FlowFile::parse(&blank_step)
                .unwrap_err()
                .to_string()
                .contains("step name")
        );
    }

    const FULL: &str = r#"
[[flow]]
name = "issue"
trigger = { kind = "label", label = "inchworm" }
base = "main"
max_attempts = 3
retry_on = ["model_error", "agent_failed", "tests_failed", "orchestrator_lost", "judge_rejected"]
attempt_timeout_secs = 7200

[[flow.step]]
name = "implement"
identity = "rust-implementer"

[[flow.step]]
name = "publish"
identity = "pr-shepherd"

[[flow.judge]]
kind = "diff"
max_lines = 400
forbid_paths = ["publish.sh", ".github/**"]

[[flow.judge]]
kind = "nanna_clean"

[[flow.judge]]
kind = "llm"
rubric = ".inchworm/rubric.md"
model = "gemma4:e4b"
min_score = 7

[flow.outcome]
mode = "merge_when"
required_checks = ["Tests", "style"]
merge_method = "merge"
"#;

    fn minimal() -> String {
        r#"
[[flow]]
name = "issue"
trigger = { kind = "label", label = "inchworm" }

[[flow.step]]
name = "implement"
identity = "rust-implementer"

[flow.outcome]
mode = "draft_pr"
"#
        .to_string()
    }

    fn err_text(text: &str) -> String {
        FlowFile::parse(text).unwrap_err().to_string()
    }

    #[test]
    fn parses_the_full_example() {
        let file = FlowFile::parse(FULL).unwrap();
        let flow = &file.flow[0];
        assert_eq!(flow.name, "issue");
        assert_eq!(
            flow.trigger,
            Trigger::Label {
                label: "inchworm".into()
            }
        );
        assert_eq!(flow.step.len(), 2);
        assert_eq!(flow.step[1].identity, "pr-shepherd");
        assert_eq!(flow.judge.len(), 3);
        assert_eq!(flow.retry_on.len(), 5);
        assert_eq!(flow.outcome.mode, OutcomeMode::MergeWhen);
        assert_eq!(flow.outcome.merge_method, MergeMethod::Merge);
    }

    #[test]
    fn minimal_flow_gets_defaults() {
        let file = FlowFile::parse(&minimal()).unwrap();
        let flow = &file.flow[0];
        assert_eq!(flow.base, "main");
        assert_eq!(flow.max_attempts, 3);
        assert_eq!(flow.attempt_timeout_secs, 7200);
        assert!(flow.retry_on.is_empty());
        assert!(flow.judge.is_empty());
        assert_eq!(flow.outcome.merge_method, MergeMethod::Merge);
    }

    #[test]
    fn unknown_keys_are_rejected_everywhere() {
        let top = format!("surprise = 1\n{}", minimal());
        assert!(FlowFile::parse(&top).is_err());
        let in_flow = minimal().replace("name = \"issue\"", "name = \"issue\"\nsurprise = 1");
        assert!(FlowFile::parse(&in_flow).is_err());
        let in_step = minimal().replace(
            "identity = \"rust-implementer\"",
            "identity = \"x\"\nsurprise = 1",
        );
        assert!(FlowFile::parse(&in_step).is_err());
        let in_outcome =
            minimal().replace("mode = \"draft_pr\"", "mode = \"draft_pr\"\nsurprise = 1");
        assert!(FlowFile::parse(&in_outcome).is_err());
        let in_judge = FULL.replace(
            "kind = \"nanna_clean\"",
            "kind = \"nanna_clean\"\nsurprise = 1",
        );
        assert!(FlowFile::parse(&in_judge).is_err());
    }

    #[test]
    fn unknown_trigger_kind_is_rejected() {
        let text = minimal().replace(
            "kind = \"label\", label = \"inchworm\"",
            "kind = \"schedule\", cron = \"* * * * *\"",
        );
        assert!(FlowFile::parse(&text).is_err());
    }

    #[test]
    fn unknown_failure_class_in_retry_on_is_rejected() {
        let text = minimal().replace(
            "name = \"issue\"",
            "name = \"issue\"\nretry_on = [\"nonsense\"]",
        );
        assert!(FlowFile::parse(&text).is_err());
    }

    #[test]
    fn empty_file_and_empty_flow_list_are_rejected() {
        assert!(FlowFile::parse("").is_err());
        assert!(matches!(
            FlowFile::parse("flow = []"),
            Err(ConfigError::NoFlows)
        ));
    }

    #[test]
    fn duplicate_flow_names_are_rejected() {
        let text = format!("{}{}", minimal(), minimal());
        assert!(matches!(
            FlowFile::parse(&text),
            Err(ConfigError::DuplicateFlow(name)) if name == "issue"
        ));
    }

    #[test]
    fn merge_when_requires_judges_and_checks() {
        let no_judges = FULL
            .replace("[[flow.judge]]\nkind = \"diff\"\nmax_lines = 400\nforbid_paths = [\"publish.sh\", \".github/**\"]\n", "")
            .replace("[[flow.judge]]\nkind = \"nanna_clean\"\n", "")
            .replace("[[flow.judge]]\nkind = \"llm\"\nrubric = \".inchworm/rubric.md\"\nmodel = \"gemma4:e4b\"\nmin_score = 7\n", "");
        assert!(err_text(&no_judges).contains("at least one judge"));
        let no_checks = FULL.replace(
            "required_checks = [\"Tests\", \"style\"]",
            "required_checks = []",
        );
        assert!(err_text(&no_checks).contains("required_checks"));
    }

    #[test]
    fn draft_pr_does_not_require_judges() {
        assert!(FlowFile::parse(&minimal()).is_ok());
    }

    #[test]
    fn structural_rules_are_enforced() {
        let cases = [
            ("max_attempts = 0", "max_attempts"),
            ("attempt_timeout_secs = 0", "attempt_timeout_secs"),
            ("base = \"\"", "base"),
        ];
        for (extra, expect) in cases {
            let text = minimal().replace("name = \"issue\"", &format!("name = \"issue\"\n{extra}"));
            assert!(err_text(&text).contains(expect), "{extra}");
        }
        let empty_label = minimal().replace("label = \"inchworm\"", "label = \"\"");
        assert!(err_text(&empty_label).contains("label"));
        let no_steps = minimal().replace(
            "[[flow.step]]\nname = \"implement\"\nidentity = \"rust-implementer\"\n",
            "",
        );
        assert!(err_text(&no_steps).contains("at least one step"));
        let empty_identity =
            minimal().replace("identity = \"rust-implementer\"", "identity = \"\"");
        assert!(err_text(&empty_identity).contains("empty identity"));
        let duplicate_step = minimal().replace(
            "[flow.outcome]",
            "[[flow.step]]\nname = \"implement\"\nidentity = \"other\"\n\n[flow.outcome]",
        );
        assert!(err_text(&duplicate_step).contains("duplicate step"));
    }

    #[test]
    fn judge_rules_are_enforced() {
        let bad_score = FULL.replace("min_score = 7", "min_score = 11");
        assert!(err_text(&bad_score).contains("min_score"));
        let zero_lines = FULL.replace("max_lines = 400", "max_lines = 0");
        assert!(err_text(&zero_lines).contains("max_lines"));
        let empty_model = FULL.replace("model = \"gemma4:e4b\"", "model = \"\"");
        assert!(err_text(&empty_model).contains("rubric and a model"));
        let empty_check = FULL.replace(
            "required_checks = [\"Tests\", \"style\"]",
            "required_checks = [\"Tests\", \" \"]",
        );
        assert!(err_text(&empty_check).contains("empty names"));
    }

    #[test]
    fn malformed_or_empty_forbid_paths_globs_are_rejected_at_load() {
        for bad in ["[", "a/[b", "{a,b", "[z-a]", ""] {
            let text = FULL.replace(
                "forbid_paths = [\"publish.sh\", \".github/**\"]",
                &format!("forbid_paths = [\"publish.sh\", \"{bad}\"]"),
            );
            let message = err_text(&text);
            assert!(message.contains("forbid_paths"), "{bad}: {message}");
        }
        for good in ["[!a]*", "{a,b}.rs", "a\\\\.rs", "**/x", ".github"] {
            let text = FULL.replace(
                "forbid_paths = [\"publish.sh\", \".github/**\"]",
                &format!("forbid_paths = [\"{good}\"]"),
            );
            assert!(FlowFile::parse(&text).is_ok(), "{good}");
        }
    }

    fn name_strategy() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9_-]{0,10}".prop_map(String::from)
    }

    fn judge_strategy() -> impl Strategy<Value = Judge> {
        prop_oneof![
            (
                1u32..5000,
                prop::collection::vec("([a-z]{1,4}/){0,2}(\\*\\*|\\*|[a-z.]{1,4})", 0..3)
            )
                .prop_map(|(max_lines, forbid_paths)| Judge::Diff {
                    max_lines,
                    forbid_paths
                }),
            Just(Judge::NannaClean {}),
            ("[a-z./]{1,12}", name_strategy(), 0u8..=10).prop_map(|(rubric, model, min_score)| {
                Judge::Llm {
                    rubric: PathBuf::from(rubric),
                    model,
                    min_score,
                }
            }),
        ]
    }

    fn flow_strategy() -> impl Strategy<Value = Flow> {
        (
            name_strategy(),
            name_strategy(),
            name_strategy(),
            1u32..10,
            prop::collection::vec(0..FailureClass::ALL.len(), 0..4),
            1u64..100_000,
            prop::collection::vec((name_strategy(), name_strategy()), 1..4),
            prop::collection::vec(judge_strategy(), 1..4),
            prop::collection::vec(name_strategy(), 1..3),
            any::<bool>(),
            prop_oneof![
                Just(MergeMethod::Merge),
                Just(MergeMethod::Squash),
                Just(MergeMethod::Rebase)
            ],
        )
            .prop_map(
                |(
                    name,
                    label,
                    base,
                    max_attempts,
                    retry,
                    timeout,
                    steps,
                    judge,
                    checks,
                    merge,
                    method,
                )| {
                    let mut seen = HashSet::new();
                    let step = steps
                        .into_iter()
                        .filter(|(n, _)| seen.insert(n.clone()))
                        .map(|(name, identity)| Step { name, identity })
                        .collect();
                    Flow {
                        name,
                        trigger: Trigger::Label { label },
                        base,
                        max_attempts,
                        retry_on: retry.into_iter().map(|i| FailureClass::ALL[i]).collect(),
                        attempt_timeout_secs: timeout,
                        step,
                        judge,
                        outcome: Outcome {
                            mode: if merge {
                                OutcomeMode::MergeWhen
                            } else {
                                OutcomeMode::DraftPr
                            },
                            required_checks: checks,
                            merge_method: method,
                        },
                    }
                },
            )
    }

    proptest! {
        #[test]
        fn valid_flows_round_trip_through_toml(flow in flow_strategy()) {
            let file = FlowFile { flow: vec![flow] };
            let text = toml::to_string(&file).unwrap();
            let parsed = FlowFile::parse(&text).unwrap();
            prop_assert_eq!(parsed, file);
        }

        #[test]
        fn zero_attempts_is_always_rejected(mut flow in flow_strategy()) {
            flow.max_attempts = 0;
            let file = FlowFile { flow: vec![flow] };
            prop_assert!(file.validate().is_err());
        }

        #[test]
        fn merge_when_without_judges_is_always_rejected(mut flow in flow_strategy()) {
            flow.outcome.mode = OutcomeMode::MergeWhen;
            flow.judge.clear();
            let file = FlowFile { flow: vec![flow] };
            prop_assert!(file.validate().is_err());
        }

        #[test]
        fn arbitrary_text_never_panics(text in ".*") {
            let _ = FlowFile::parse(&text);
        }
    }
}
