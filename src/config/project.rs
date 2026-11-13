use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::flow::{ConfigError, Flow, FlowFile, Judge, OutcomeMode};

pub const PROJECT_FILE: &str = "inchworm.toml";
pub const LEGACY_FLOWS_FILE: &str = ".inchworm/flows.toml";
pub const MAX_AUDITOR_STEP_OVERRIDES: usize = 640;

#[derive(Debug, Error)]
pub enum ProjectError {
    #[error("invalid {PROJECT_FILE}: {0}")]
    Parse(#[from] toml::de::Error),
    #[error(
        "{PROJECT_FILE} has no [auditor] table; every agent step needs an auditor, so a missing one is an error, not a default"
    )]
    MissingAuditor,
    #[error("[offramp.flow] names flow `{0}`, which is not defined")]
    UnknownOfframpFlow(String),
    #[error("[[auditor.step]] names flow `{flow}` step `{step}`, which is not defined")]
    UnknownAuditedStep { flow: String, step: String },
    #[error("[[auditor.step]] names flow `{flow}` step `{step}` more than once")]
    DuplicateAuditedStep { flow: String, step: String },
    #[error("[[auditor.step]] has {0} entries; at most {MAX_AUDITOR_STEP_OVERRIDES} are allowed")]
    TooManyAuditedSteps(usize),
    #[error(
        "[auditor] runs_in is host, but no host check exists yet, so a host default would be a silent skip of every auditor; use a nanna identity"
    )]
    HostAuditorDefault,
    #[error(
        "flow `{flow}` step `{step}`: the auditor is set to run on the host, but no host check exists yet, so this step would run unaudited; use a nanna auditor"
    )]
    HostAuditorStep { flow: String, step: String },
    #[error(
        "flow `{flow}` step `{step}`: the auditor has no checks because the flow has no judges; add a judge or name checks, otherwise nothing is audited"
    )]
    AuditorWithoutChecks { flow: String, step: String },
    #[error("[offramp.flow] names flow `{0}` more than once, ignoring case")]
    DuplicateOfframpFlow(String),
    #[error("auditor identity {0:?} is empty or invalid")]
    InvalidAuditorIdentity(String),
    #[error(
        "auditor identity `{identity}` is also the identity of step `{step}` in flow `{flow}`; an agent must not audit itself"
    )]
    SelfAudit {
        identity: String,
        flow: String,
        step: String,
    },
    #[error(
        "[[auditor.step]] for flow `{flow}` step `{step}` selects check `{check}`, but the flow has no such judge"
    )]
    UnknownCheck {
        flow: String,
        step: String,
        check: &'static str,
    },
    #[error(
        "flow `{0}` uses mode merge_when but [merge] allow is false; merging is off unless this project sets allow = true"
    )]
    MergeNotAllowed(String),
    #[error("{0}")]
    Flow(#[from] ConfigError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Offramp {
    #[default]
    Noop,
    Issue,
    Escalate,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfframpConfig {
    #[serde(default)]
    pub default: Offramp,
    #[serde(default)]
    pub flow: HashMap<String, Offramp>,
}

impl OfframpConfig {
    pub fn for_flow(&self, flow: &str) -> Offramp {
        self.flow
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(flow))
            .map(|(_, offramp)| *offramp)
            .unwrap_or(self.default)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuditorPlace {
    Host {},
    Nanna { identity: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    Diff,
    NannaClean,
    Llm,
}

impl Check {
    pub fn name(self) -> &'static str {
        match self {
            Check::Diff => "diff",
            Check::NannaClean => "nanna_clean",
            Check::Llm => "llm",
        }
    }

    fn matches(self, judge: &Judge) -> bool {
        matches!(
            (self, judge),
            (Check::Diff, Judge::Diff { .. })
                | (Check::NannaClean, Judge::NannaClean {})
                | (Check::Llm, Judge::Llm { .. })
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnAuditFailure {
    #[default]
    Block,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepAudit {
    pub flow: String,
    pub step: String,
    #[serde(default)]
    pub runs_in: Option<AuditorPlace>,
    #[serde(default)]
    pub checks: Vec<Check>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditorConfig {
    pub runs_in: AuditorPlace,
    #[serde(default)]
    pub on_failure: OnAuditFailure,
    #[serde(default)]
    pub step: Vec<StepAudit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditPlan {
    pub runs_in: AuditorPlace,
    pub checks: Vec<Check>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditOutcome {
    Passed,
    Rejected(String),
    Errored(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepDecision {
    Continue,
    Halt(String),
}

impl AuditorConfig {
    pub fn plan_for(&self, flow: &Flow, step: &str) -> Option<AuditPlan> {
        let known = flow
            .step
            .iter()
            .any(|candidate| candidate.name.eq_ignore_ascii_case(step));
        if !known {
            return None;
        }
        let override_ = self.step.iter().find(|entry| {
            entry.flow.eq_ignore_ascii_case(&flow.name) && entry.step.eq_ignore_ascii_case(step)
        });
        let runs_in = override_
            .and_then(|entry| entry.runs_in.clone())
            .unwrap_or_else(|| self.runs_in.clone());
        let checks = match override_ {
            Some(entry) if !entry.checks.is_empty() => entry.checks.clone(),
            _ => [Check::Diff, Check::NannaClean, Check::Llm]
                .into_iter()
                .filter(|check| flow.judge.iter().any(|judge| check.matches(judge)))
                .collect(),
        };
        Some(AuditPlan { runs_in, checks })
    }

    pub fn decide(&self, outcome: &AuditOutcome) -> StepDecision {
        match outcome {
            AuditOutcome::Passed => StepDecision::Continue,
            AuditOutcome::Rejected(reason) => {
                StepDecision::Halt(format!("the auditor rejected the step: {reason}"))
            }
            AuditOutcome::Errored(reason) => match self.on_failure {
                OnAuditFailure::Block => StepDecision::Halt(format!(
                    "the auditor failed, and an unaudited step is never accepted: {reason}"
                )),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeConfig {
    #[serde(default)]
    pub allow: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum MergeGrantSetting {
    #[default]
    Denied,
    Granted,
}

impl MergeConfig {
    pub fn grant(&self) -> MergeGrantSetting {
        if self.allow {
            MergeGrantSetting::Granted
        } else {
            MergeGrantSetting::Denied
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    #[serde(default)]
    pub offramp: OfframpConfig,
    #[serde(default)]
    pub auditor: Option<AuditorConfig>,
    #[serde(default)]
    pub merge: MergeConfig,
    #[serde(default)]
    pub flow: Vec<Flow>,
}

fn identity_is_valid(identity: &str) -> bool {
    !identity.is_empty()
        && identity.len() <= 64
        && identity
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

impl ProjectConfig {
    /// ```
    /// use inchworm::config::{Offramp, ProjectConfig};
    ///
    /// let config = ProjectConfig::parse(r#"
    /// [auditor]
    /// runs_in = { kind = "nanna", identity = "auditor" }
    ///
    /// [[flow]]
    /// name = "issue"
    /// trigger = { kind = "label", label = "inchworm" }
    ///
    /// [[flow.step]]
    /// name = "implement"
    /// identity = "rust-implementer"
    ///
    /// [[flow.judge]]
    /// kind = "nanna_clean"
    ///
    /// [flow.outcome]
    /// mode = "draft_pr"
    /// "#).unwrap();
    ///
    /// assert_eq!(config.offramp.for_flow("issue"), Offramp::Noop);
    /// assert!(!config.merge.allow);
    /// assert!(ProjectConfig::parse("[[flow]]").is_err());
    /// ```
    pub fn parse(text: &str) -> Result<ProjectConfig, ProjectError> {
        let config: ProjectConfig = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    pub fn auditor(&self) -> &AuditorConfig {
        self.auditor
            .as_ref()
            .expect("a validated ProjectConfig always has an auditor")
    }

    pub fn flow_file(&self) -> FlowFile {
        FlowFile {
            flow: self.flow.clone(),
        }
    }

    pub fn validate(&self) -> Result<(), ProjectError> {
        self.flow_file().validate()?;
        let auditor = self.auditor.as_ref().ok_or(ProjectError::MissingAuditor)?;
        if auditor.runs_in == (AuditorPlace::Host {}) {
            return Err(ProjectError::HostAuditorDefault);
        }
        let mut offramp_names = HashSet::new();
        for name in self.offramp.flow.keys() {
            if !offramp_names.insert(name.to_ascii_lowercase()) {
                return Err(ProjectError::DuplicateOfframpFlow(name.clone()));
            }
            if !self
                .flow
                .iter()
                .any(|flow| flow.name.eq_ignore_ascii_case(name))
            {
                return Err(ProjectError::UnknownOfframpFlow(name.clone()));
            }
        }
        if auditor.step.len() > MAX_AUDITOR_STEP_OVERRIDES {
            return Err(ProjectError::TooManyAuditedSteps(auditor.step.len()));
        }
        let mut seen = HashSet::new();
        for entry in &auditor.step {
            let flow = self
                .flow
                .iter()
                .find(|flow| flow.name.eq_ignore_ascii_case(&entry.flow));
            let known = flow.is_some_and(|flow| {
                flow.step
                    .iter()
                    .any(|step| step.name.eq_ignore_ascii_case(&entry.step))
            });
            if !known {
                return Err(ProjectError::UnknownAuditedStep {
                    flow: entry.flow.clone(),
                    step: entry.step.clone(),
                });
            }
            if !seen.insert((entry.flow.to_lowercase(), entry.step.to_lowercase())) {
                return Err(ProjectError::DuplicateAuditedStep {
                    flow: entry.flow.clone(),
                    step: entry.step.clone(),
                });
            }
            if let Some(flow) = flow {
                for check in &entry.checks {
                    if !flow.judge.iter().any(|judge| check.matches(judge)) {
                        return Err(ProjectError::UnknownCheck {
                            flow: entry.flow.clone(),
                            step: entry.step.clone(),
                            check: check.name(),
                        });
                    }
                }
            }
        }
        for flow in &self.flow {
            for step in &flow.step {
                let plan = auditor
                    .plan_for(flow, &step.name)
                    .expect("every step of a defined flow has a plan");
                if plan.runs_in == (AuditorPlace::Host {}) {
                    return Err(ProjectError::HostAuditorStep {
                        flow: flow.name.clone(),
                        step: step.name.clone(),
                    });
                }
                if plan.checks.is_empty() {
                    return Err(ProjectError::AuditorWithoutChecks {
                        flow: flow.name.clone(),
                        step: step.name.clone(),
                    });
                }
                if let AuditorPlace::Nanna { identity } = &plan.runs_in {
                    if !identity_is_valid(identity) {
                        return Err(ProjectError::InvalidAuditorIdentity(identity.clone()));
                    }
                    if *identity == step.identity {
                        return Err(ProjectError::SelfAudit {
                            identity: identity.clone(),
                            flow: flow.name.clone(),
                            step: step.name.clone(),
                        });
                    }
                }
            }
            if flow.outcome.mode == OutcomeMode::MergeWhen && !self.merge.allow {
                return Err(ProjectError::MergeNotAllowed(flow.name.clone()));
            }
        }
        if let AuditorPlace::Nanna { identity } = &auditor.runs_in
            && !identity_is_valid(identity)
        {
            return Err(ProjectError::InvalidAuditorIdentity(identity.clone()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOW: &str = r#"
[[flow]]
name = "issue"
trigger = { kind = "label", label = "inchworm" }

[[flow.step]]
name = "research"
identity = "rust-implementer"

[[flow.step]]
name = "implement"
identity = "rust-implementer"

[[flow.judge]]
kind = "diff"
max_lines = 400

[[flow.judge]]
kind = "nanna_clean"

[flow.outcome]
mode = "draft_pr"
"#;

    fn with(head: &str) -> String {
        format!("{head}\n{FLOW}")
    }

    fn minimal() -> String {
        with("[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\n")
    }

    fn error(text: &str) -> String {
        ProjectConfig::parse(text).unwrap_err().to_string()
    }

    #[test]
    fn a_minimal_project_has_safe_defaults() {
        let config = ProjectConfig::parse(&minimal()).unwrap();
        assert_eq!(config.offramp.default, Offramp::Noop);
        assert_eq!(config.offramp.for_flow("issue"), Offramp::Noop);
        assert!(!config.merge.allow);
        assert_eq!(config.merge.grant(), MergeGrantSetting::Denied);
        assert_eq!(config.auditor().on_failure, OnAuditFailure::Block);
        assert_eq!(config.flow.len(), 1);
    }

    #[test]
    fn escalate_is_never_the_default_and_must_be_named() {
        assert_eq!(Offramp::default(), Offramp::Noop);
        assert_eq!(OfframpConfig::default().default, Offramp::Noop);
        let config = ProjectConfig::parse(&with(
            "[offramp]\ndefault = \"issue\"\n[offramp.flow]\nissue = \"escalate\"\n[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\n",
        ))
        .unwrap();
        assert_eq!(config.offramp.default, Offramp::Issue);
        assert_eq!(config.offramp.for_flow("issue"), Offramp::Escalate);
        assert_eq!(config.offramp.for_flow("other"), Offramp::Issue);
    }

    #[test]
    fn offramp_values_are_a_closed_set_and_flows_must_exist() {
        let bad = with(
            "[offramp]\ndefault = \"page-me\"\n[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\n",
        );
        assert!(ProjectConfig::parse(&bad).is_err());
        let ghost = with(
            "[offramp.flow]\nghost = \"issue\"\n[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\n",
        );
        assert!(error(&ghost).contains("`ghost`"));
    }

    #[test]
    fn a_missing_auditor_fails_closed() {
        let text = FLOW.to_string();
        assert!(matches!(
            ProjectConfig::parse(&text),
            Err(ProjectError::MissingAuditor)
        ));
    }

    #[test]
    fn the_auditor_place_is_explicit_and_validated() {
        for bad in [
            "[auditor]\n",
            "[auditor]\nruns_in = { kind = \"cloud\" }\n",
            "[auditor]\nruns_in = { kind = \"nanna\" }\n",
            "[auditor]\nruns_in = { kind = \"host\", identity = \"x\" }\n",
            "[auditor]\nruns_in = { kind = \"nanna\", identity = \"a b\" }\n",
            "[auditor]\nruns_in = { kind = \"nanna\", identity = \"\" }\n",
            "[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\non_failure = \"allow\"\n",
        ] {
            assert!(ProjectConfig::parse(&with(bad)).is_err(), "{bad}");
        }
        let long = format!(
            "[auditor]\nruns_in = {{ kind = \"nanna\", identity = \"{}\" }}\n",
            "a".repeat(65)
        );
        assert!(error(&with(&long)).contains("invalid"));
        assert!(
            ProjectConfig::parse(&with(
                "[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\n"
            ))
            .is_ok()
        );
    }

    #[test]
    fn a_host_auditor_cannot_be_the_project_default() {
        let text = with("[auditor]\nruns_in = { kind = \"host\" }\n");
        assert!(matches!(
            ProjectConfig::parse(&text),
            Err(ProjectError::HostAuditorDefault)
        ));
        assert!(error(&text).contains("silent skip"));
    }

    #[test]
    fn a_per_step_host_auditor_is_a_parse_error_naming_the_step() {
        let text = with(
            "[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\n[[auditor.step]]\nflow = \"issue\"\nstep = \"implement\"\nruns_in = { kind = \"host\" }\n",
        );
        assert!(matches!(
            ProjectConfig::parse(&text),
            Err(ProjectError::HostAuditorStep { flow, step }) if flow == "issue" && step == "implement"
        ));
        assert!(
            error(&text)
                .contains("flow `issue` step `implement`: the auditor is set to run on the host")
        );
    }

    #[test]
    fn per_flow_offramp_keys_use_one_case_insensitive_rule_in_both_directions() {
        let auditor = "[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\n";
        let upper_key = with(&format!("[offramp.flow]\nIssue = \"escalate\"\n{auditor}"));
        let config = ProjectConfig::parse(&upper_key).unwrap();
        assert_eq!(config.offramp.for_flow("issue"), Offramp::Escalate);
        assert_eq!(config.offramp.for_flow("ISSUE"), Offramp::Escalate);
        let reverse = with(&format!(
            "[offramp]\ndefault = \"escalate\"\n[offramp.flow]\nIssue = \"noop\"\n{auditor}"
        ));
        let config = ProjectConfig::parse(&reverse).unwrap();
        assert_eq!(config.offramp.for_flow("issue"), Offramp::Noop);
        assert_eq!(config.offramp.for_flow("other"), Offramp::Escalate);
        let twice = with(&format!(
            "[offramp.flow]\nIssue = \"noop\"\nissue = \"escalate\"\n{auditor}"
        ));
        assert!(matches!(
            ProjectConfig::parse(&twice),
            Err(ProjectError::DuplicateOfframpFlow(_))
        ));
    }

    #[test]
    fn a_step_with_no_checks_is_an_error_because_nothing_would_be_audited() {
        let no_judges = "[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\n\n[[flow]]\nname = \"issue\"\ntrigger = { kind = \"label\", label = \"inchworm\" }\n[[flow.step]]\nname = \"implement\"\nidentity = \"rust-implementer\"\n[flow.outcome]\nmode = \"draft_pr\"\n";
        assert!(matches!(
            ProjectConfig::parse(no_judges),
            Err(ProjectError::AuditorWithoutChecks { flow, step }) if flow == "issue" && step == "implement"
        ));
        assert!(error(no_judges).contains("nothing is audited"));
    }

    #[test]
    fn an_invalid_auditor_identity_is_shown_escaped() {
        let text = with("[auditor]\nruns_in = { kind = \"nanna\", identity = \"a\\u0000b\" }\n");
        let message = error(&text);
        assert!(message.contains("\"a\\0b\""), "{message:?}");
        assert!(!message.contains('\0'));
    }

    #[test]
    fn the_step_override_limit_is_exactly_640_and_the_identity_limit_exactly_64() {
        let flows: String = (0..32)
            .map(|f| {
                let steps: String = (0..20)
                    .map(|n| {
                        format!(
                            "[[flow.step]]\nname = \"s{n}\"\nidentity = \"impl\"\n"
                        )
                    })
                    .collect();
                format!(
                    "[[flow]]\nname = \"f{f}\"\ntrigger = {{ kind = \"label\", label = \"l{f}\" }}\n{steps}[[flow.judge]]\nkind = \"nanna_clean\"\n[flow.outcome]\nmode = \"draft_pr\"\n"
                )
            })
            .collect();
        let overrides: String = (0..32)
            .flat_map(|f| (0..20).map(move |n| (f, n)))
            .map(|(f, n)| {
                format!(
                    "[[auditor.step]]\nflow = \"f{f}\"\nstep = \"s{n}\"\nruns_in = {{ kind = \"nanna\", identity = \"second\" }}\n"
                )
            })
            .collect();
        let head = "[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\n";
        assert!(ProjectConfig::parse(&format!("{head}{overrides}{flows}")).is_ok());
        let extra = "[[auditor.step]]\nflow = \"f0\"\nstep = \"s0\"\n";
        assert!(matches!(
            ProjectConfig::parse(&format!("{head}{overrides}{extra}{flows}")),
            Err(ProjectError::TooManyAuditedSteps(641))
        ));
        for (identity, ok) in [("a".repeat(64), true), ("a".repeat(65), false)] {
            let auditor = with(&format!(
                "[auditor]\nruns_in = {{ kind = \"nanna\", identity = \"{identity}\" }}\n"
            ));
            assert_eq!(
                ProjectConfig::parse(&auditor).is_ok(),
                ok,
                "{}",
                identity.len()
            );
        }
    }

    #[test]
    fn default_checks_are_exactly_the_judges_the_flow_has() {
        let all = FLOW.replace(
            "[flow.outcome]",
            "[[flow.judge]]\nkind = \"llm\"\nrubric = \".inchworm/rubric.md\"\nmodel = \"gemma4:e4b\"\nmin_score = 7\n\n[flow.outcome]",
        );
        let head = "[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\n";
        let config = ProjectConfig::parse(&format!("{head}{all}")).unwrap();
        let plan = config
            .auditor()
            .plan_for(&config.flow[0], "implement")
            .unwrap();
        assert_eq!(
            plan.checks,
            vec![Check::Diff, Check::NannaClean, Check::Llm]
        );
        let diff_only = FLOW.replace("[[flow.judge]]\nkind = \"nanna_clean\"\n", "");
        let config = ProjectConfig::parse(&format!("{head}{diff_only}")).unwrap();
        let plan = config
            .auditor()
            .plan_for(&config.flow[0], "implement")
            .unwrap();
        assert_eq!(plan.checks, vec![Check::Diff]);
        let named = format!(
            "{head}[[auditor.step]]\nflow = \"issue\"\nstep = \"implement\"\nchecks = [\"nanna_clean\"]\n{diff_only}"
        );
        assert!(matches!(
            ProjectConfig::parse(&named),
            Err(ProjectError::UnknownCheck {
                check: "nanna_clean",
                ..
            })
        ));
    }

    #[test]
    fn the_auditor_table_rejects_unknown_keys_and_plans_ignore_step_case() {
        let unknown =
            with("[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\nbogus = 1\n");
        assert!(ProjectConfig::parse(&unknown).is_err());
        let config = ProjectConfig::parse(&minimal()).unwrap();
        let flow = &config.flow[0];
        assert!(config.auditor().plan_for(flow, "IMPLEMENT").is_some());
        assert!(config.auditor().plan_for(flow, "Research").is_some());
    }

    #[test]
    fn the_auditor_identity_rule_matches_the_step_identity_rule() {
        for identity in ["ok", "a-b_c.d", "a b", "", "é", "a/b"] {
            let step_text = format!(
                "[auditor]\nruns_in = {{ kind = \"nanna\", identity = \"auditor\" }}\n{}",
                FLOW.replace("rust-implementer", identity)
            );
            let auditor_text = with(&format!(
                "[auditor]\nruns_in = {{ kind = \"nanna\", identity = \"{identity}\" }}\n"
            ));
            assert_eq!(
                ProjectConfig::parse(&step_text).is_ok(),
                ProjectConfig::parse(&auditor_text).is_ok(),
                "{identity:?}"
            );
        }
    }

    #[test]
    fn an_agent_may_not_audit_itself() {
        let text =
            with("[auditor]\nruns_in = { kind = \"nanna\", identity = \"rust-implementer\" }\n");
        assert!(matches!(
            ProjectConfig::parse(&text),
            Err(ProjectError::SelfAudit { .. })
        ));
        let per_step = with(
            "[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\n[[auditor.step]]\nflow = \"issue\"\nstep = \"implement\"\nruns_in = { kind = \"nanna\", identity = \"rust-implementer\" }\n",
        );
        assert!(matches!(
            ProjectConfig::parse(&per_step),
            Err(ProjectError::SelfAudit { step, .. }) if step == "implement"
        ));
    }

    #[test]
    fn each_step_gets_a_plan_and_overrides_win_for_that_step_only() {
        let text = with(
            "[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\n[[auditor.step]]\nflow = \"issue\"\nstep = \"IMPLEMENT\"\nruns_in = { kind = \"nanna\", identity = \"second\" }\nchecks = [\"diff\"]\n",
        );
        let config = ProjectConfig::parse(&text).unwrap();
        let flow = &config.flow[0];
        let research = config.auditor().plan_for(flow, "research").unwrap();
        assert_eq!(
            research.runs_in,
            AuditorPlace::Nanna {
                identity: "auditor".to_string()
            }
        );
        assert_eq!(research.checks, vec![Check::Diff, Check::NannaClean]);
        let implement = config.auditor().plan_for(flow, "implement").unwrap();
        assert_eq!(
            implement.runs_in,
            AuditorPlace::Nanna {
                identity: "second".to_string()
            }
        );
        assert_eq!(implement.checks, vec![Check::Diff]);
        assert_eq!(config.auditor().plan_for(flow, "ghost"), None);
    }

    #[test]
    fn an_override_that_names_no_checks_keeps_all_of_the_flows_judges() {
        let text = with(
            "[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\n[[auditor.step]]\nflow = \"issue\"\nstep = \"implement\"\nruns_in = { kind = \"nanna\", identity = \"second\" }\n",
        );
        let config = ProjectConfig::parse(&text).unwrap();
        let plan = config
            .auditor()
            .plan_for(&config.flow[0], "implement")
            .unwrap();
        assert_eq!(
            plan.runs_in,
            AuditorPlace::Nanna {
                identity: "second".to_string()
            }
        );
        assert_eq!(plan.checks, vec![Check::Diff, Check::NannaClean]);
    }

    #[test]
    fn step_overrides_must_name_real_steps_and_real_checks_once() {
        let head = "[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\n";
        let ghost_step = with(&format!(
            "{head}[[auditor.step]]\nflow = \"issue\"\nstep = \"ghost\"\n"
        ));
        assert!(matches!(
            ProjectConfig::parse(&ghost_step),
            Err(ProjectError::UnknownAuditedStep { .. })
        ));
        let ghost_flow = with(&format!(
            "{head}[[auditor.step]]\nflow = \"ghost\"\nstep = \"implement\"\n"
        ));
        assert!(matches!(
            ProjectConfig::parse(&ghost_flow),
            Err(ProjectError::UnknownAuditedStep { .. })
        ));
        let twice = with(&format!(
            "{head}[[auditor.step]]\nflow = \"issue\"\nstep = \"implement\"\n[[auditor.step]]\nflow = \"ISSUE\"\nstep = \"Implement\"\n"
        ));
        assert!(matches!(
            ProjectConfig::parse(&twice),
            Err(ProjectError::DuplicateAuditedStep { .. })
        ));
        let no_judge = with(&format!(
            "{head}[[auditor.step]]\nflow = \"issue\"\nstep = \"implement\"\nchecks = [\"llm\"]\n"
        ));
        assert!(matches!(
            ProjectConfig::parse(&no_judge),
            Err(ProjectError::UnknownCheck { check: "llm", .. })
        ));
        let unknown_key = with(&format!(
            "{head}[[auditor.step]]\nflow = \"issue\"\nstep = \"implement\"\nskip = true\n"
        ));
        assert!(ProjectConfig::parse(&unknown_key).is_err());
    }

    #[test]
    fn there_is_no_way_to_skip_the_auditor_for_a_step() {
        let skip = with(
            "[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\n[[auditor.step]]\nflow = \"issue\"\nstep = \"implement\"\nruns_in = { kind = \"none\" }\n",
        );
        assert!(ProjectConfig::parse(&skip).is_err());
        let off = with("[auditor]\nenabled = false\nruns_in = { kind = \"host\" }\n");
        assert!(ProjectConfig::parse(&off).is_err());
    }

    #[test]
    fn a_failing_auditor_halts_the_step() {
        let config = ProjectConfig::parse(&minimal()).unwrap();
        let auditor = config.auditor();
        assert_eq!(
            auditor.decide(&AuditOutcome::Passed),
            StepDecision::Continue
        );
        match auditor.decide(&AuditOutcome::Errored("container exited 137".to_string())) {
            StepDecision::Halt(message) => {
                assert!(
                    message.contains("unaudited step is never accepted"),
                    "{message}"
                );
                assert!(message.contains("137"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        match auditor.decide(&AuditOutcome::Rejected("touches .inchworm".to_string())) {
            StepDecision::Halt(message) => assert!(message.contains("rejected"), "{message}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_auditor_after_the_first_step_fails_closed_when_it_errors() {
        let config = ProjectConfig::parse(&minimal()).unwrap();
        let flow = &config.flow[0];
        let mut reached = Vec::new();
        for step in &flow.step {
            let plan = config.auditor().plan_for(flow, &step.name).unwrap();
            assert!(!plan.checks.is_empty());
            let outcome = AuditOutcome::Errored("auditor unreachable".to_string());
            match config.auditor().decide(&outcome) {
                StepDecision::Continue => reached.push(step.name.clone()),
                StepDecision::Halt(_) => break,
            }
        }
        assert!(reached.is_empty(), "{reached:?}");
    }

    #[test]
    fn merge_is_off_unless_the_project_allows_it() {
        let merge_flow = FLOW.replace("draft_pr", "merge_when").replace(
            "[flow.outcome]",
            "[flow.outcome]\nrequired_checks = [\"Tests\"]",
        );
        let head = "[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\n";
        assert!(matches!(
            ProjectConfig::parse(&format!("{head}{merge_flow}")),
            Err(ProjectError::MergeNotAllowed(name)) if name == "issue"
        ));
        let allowed = format!("[merge]\nallow = true\n{head}{merge_flow}");
        let config = ProjectConfig::parse(&allowed).unwrap();
        assert_eq!(config.merge.grant(), MergeGrantSetting::Granted);
        let denied = format!("[merge]\nallow = false\n{head}{FLOW}");
        assert_eq!(
            ProjectConfig::parse(&denied).unwrap().merge.grant(),
            MergeGrantSetting::Denied
        );
        assert!(
            ProjectConfig::parse(&format!("[merge]\nallow = true\nbypass = 1\n{head}{FLOW}"))
                .is_err()
        );
    }

    #[test]
    fn flow_errors_pass_through_unchanged_and_unknown_keys_are_rejected() {
        let head = "[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\n";
        assert!(error(head).contains("no flows"));
        assert!(error(&format!("{head}{}", FLOW.replace("400", "0"))).contains("max_lines"));
        assert!(ProjectConfig::parse(&format!("mystery = 1\n{head}{FLOW}")).is_err());
        assert!(ProjectConfig::parse(&format!("{head}[offramp]\nunknown = 1\n{FLOW}")).is_err());
    }

    #[test]
    fn the_flow_file_view_equals_the_flows() {
        let config = ProjectConfig::parse(&minimal()).unwrap();
        assert_eq!(config.flow_file().flow, config.flow);
        assert!(config.flow_file().validate().is_ok());
    }

    #[test]
    fn check_names_match_their_toml_spelling() {
        for (check, name) in [
            (Check::Diff, "diff"),
            (Check::NannaClean, "nanna_clean"),
            (Check::Llm, "llm"),
        ] {
            assert_eq!(check.name(), name);
            let parsed: toml::Table = toml::from_str(&format!("c = \"{name}\"")).unwrap();
            assert_eq!(parsed["c"].clone().try_into::<Check>().unwrap(), check);
        }
    }

    #[test]
    fn too_many_step_overrides_are_refused() {
        let entry = "[[auditor.step]]\nflow = \"issue\"\nstep = \"implement\"\n";
        let head = format!(
            "[auditor]\nruns_in = {{ kind = \"nanna\", identity = \"auditor\" }}\n{}",
            entry.repeat(MAX_AUDITOR_STEP_OVERRIDES + 1)
        );
        assert!(matches!(
            ProjectConfig::parse(&with(&head)),
            Err(ProjectError::TooManyAuditedSteps(n)) if n == MAX_AUDITOR_STEP_OVERRIDES + 1
        ));
    }

    #[test]
    fn an_invalid_default_auditor_identity_is_refused_even_when_every_step_overrides_it() {
        let overrides = ["research", "implement"]
            .map(|step| {
                format!(
                    "[[auditor.step]]\nflow = \"issue\"\nstep = \"{step}\"\nruns_in = {{ kind = \"nanna\", identity = \"second\" }}\n"
                )
            })
            .concat();
        let head =
            format!("[auditor]\nruns_in = {{ kind = \"nanna\", identity = \"a b\" }}\n{overrides}");
        assert!(matches!(
            ProjectConfig::parse(&with(&head)),
            Err(ProjectError::InvalidAuditorIdentity(_))
        ));
    }

    #[test]
    fn the_legacy_and_current_file_names_are_fixed() {
        assert_eq!(PROJECT_FILE, "inchworm.toml");
        assert_eq!(LEGACY_FLOWS_FILE, ".inchworm/flows.toml");
    }
}
