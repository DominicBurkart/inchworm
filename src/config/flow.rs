use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

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

pub const MAX_ATTEMPTS: u32 = 20;
pub const MAX_TIMEOUT_SECS: u64 = 7 * 24 * 60 * 60;
pub const MAX_STEPS: usize = 20;
pub const MAX_DIFF_LINES: u32 = 100_000;
pub const MAX_LABEL_LEN: usize = 50;
pub const MAX_NAME_LEN: usize = 64;
pub const RUBRIC_DIR: &str = ".inchworm";

/// Why a rubric path is not allowed, or `None` when it is acceptable.
///
/// A rubric is read into a model prompt, so it must be a plain relative file
/// path under `.inchworm/`, the directory agents are not allowed to write.
///
/// ```
/// use std::path::Path;
/// use inchworm::config::rubric_path_problem;
///
/// assert!(rubric_path_problem(Path::new(".inchworm/rubric.md")).is_none());
/// assert!(rubric_path_problem(Path::new("/etc/passwd")).is_some());
/// assert!(rubric_path_problem(Path::new(".inchworm/../secrets")).is_some());
/// assert!(rubric_path_problem(Path::new("docs/rubric.md")).is_some());
/// ```
pub fn rubric_path_problem(path: &Path) -> Option<&'static str> {
    if path.as_os_str().is_empty() {
        return Some("rubric must not be empty");
    }
    if path.to_string_lossy().contains('\\') {
        return Some("rubric must use `/` separators");
    }
    let mut components = path.components();
    match components.next() {
        Some(Component::Normal(first)) if first == RUBRIC_DIR => {}
        Some(Component::RootDir | Component::Prefix(_)) => {
            return Some("rubric must be a relative path");
        }
        _ => return Some("rubric must be a file under .inchworm/"),
    }
    let mut depth = 0;
    for component in components {
        if component == Component::ParentDir {
            return Some("rubric must not contain `..`");
        }
        depth += 1;
    }
    if depth == 0 {
        return Some("rubric must be a file under .inchworm/");
    }
    None
}

/// Whether `name` is acceptable as a git branch name for a flow base.
///
/// ```
/// use inchworm::config::valid_ref_name;
///
/// assert!(valid_ref_name("main"));
/// assert!(valid_ref_name("release/1.0"));
/// assert!(!valid_ref_name("--force"));
/// assert!(!valid_ref_name(" main"));
/// assert!(!valid_ref_name("../x"));
/// ```
pub fn valid_ref_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 200 || name == "@" {
        return false;
    }
    if name.starts_with('-') || name.starts_with('/') || name.ends_with('/') || name.ends_with('.')
    {
        return false;
    }
    if name.contains("..") || name.contains("//") || name.contains("@{") {
        return false;
    }
    if name
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c))
    {
        return false;
    }
    name.split('/')
        .all(|part| !part.starts_with('.') && !part.ends_with(".lock"))
}

fn flow_name_is_valid(name: &str) -> bool {
    (1..=MAX_NAME_LEN).contains(&name.len())
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Why a `forbid_paths` entry cannot be a glob over repository-relative paths.
///
/// ```
/// use inchworm::config::glob_problem;
///
/// assert!(glob_problem("src/{a,b}/**").is_none());
/// assert!(glob_problem("[abc]*").is_none());
/// assert!(glob_problem("[").is_some());
/// assert!(glob_problem("a}").is_some());
/// assert!(glob_problem("/etc").is_some());
/// assert!(glob_problem("../x").is_some());
/// ```
pub fn glob_problem(pattern: &str) -> Option<&'static str> {
    if pattern.starts_with('/') {
        return Some("must be relative to the repository root");
    }
    if pattern.split('/').any(|part| part == "..") {
        return Some("must not contain a `..` segment");
    }
    let mut chars = pattern.chars();
    let mut braces = 0usize;
    let mut in_class = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            '{' if !in_class => braces += 1,
            '}' if !in_class => match braces.checked_sub(1) {
                Some(rest) => braces = rest,
                None => return Some("has a `}` without a matching `{`"),
            },
            _ => {}
        }
    }
    if in_class {
        return Some("has a `[` without a matching `]`");
    }
    if braces != 0 {
        return Some("has a `{` without a matching `}`");
    }
    None
}

fn has_edge_whitespace(text: &str) -> bool {
    text.trim() != text
}

fn identity_is_valid(identity: &str) -> bool {
    !identity.is_empty()
        && identity.len() <= 64
        && identity
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
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
        let mut labels: std::collections::HashMap<String, &str> = std::collections::HashMap::new();
        for flow in &self.flow {
            if flow.name.trim().is_empty() {
                return Err(ConfigError::EmptyFlowName);
            }
            if !names.insert(flow.name.to_lowercase()) {
                return Err(ConfigError::DuplicateFlow(flow.name.clone()));
            }
            flow.validate()?;
            let Trigger::Label { label } = &flow.trigger;
            if let Some(other) = labels.insert(label.to_lowercase(), flow.name.as_str()) {
                return Err(ConfigError::Invalid {
                    flow: flow.name.clone(),
                    reason: format!(
                        "trigger label `{label}` is also used by flow `{other}`, so one issue would match both"
                    ),
                });
            }
        }
        Ok(())
    }
}

impl Flow {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !flow_name_is_valid(&self.name) {
            return Err(invalid(
                self,
                format!(
                    "flow name must be 1 to {MAX_NAME_LEN} characters of letters, digits, `.`, `_` or `-` and must not start with `.`"
                ),
            ));
        }
        let Trigger::Label { label } = &self.trigger;
        if label.trim().is_empty() {
            return Err(invalid(self, "trigger label must not be empty"));
        }
        if label.chars().any(char::is_control) {
            return Err(invalid(
                self,
                "trigger label must not contain control characters",
            ));
        }
        if has_edge_whitespace(label) || label.len() > MAX_LABEL_LEN {
            return Err(invalid(
                self,
                format!("trigger label must be trimmed and at most {MAX_LABEL_LEN} characters"),
            ));
        }
        if !valid_ref_name(&self.base) {
            return Err(invalid(
                self,
                format!("base `{}` is not a valid git branch name", self.base),
            ));
        }
        if !(1..=MAX_ATTEMPTS).contains(&self.max_attempts) {
            return Err(invalid(
                self,
                format!("max_attempts must be between 1 and {MAX_ATTEMPTS}"),
            ));
        }
        if !(1..=MAX_TIMEOUT_SECS).contains(&self.attempt_timeout_secs) {
            return Err(invalid(
                self,
                format!("attempt_timeout_secs must be between 1 and {MAX_TIMEOUT_SECS}"),
            ));
        }
        self.validate_retry_on()?;
        self.validate_steps()?;
        self.validate_judges()?;
        self.validate_outcome()
    }

    fn validate_retry_on(&self) -> Result<(), ConfigError> {
        let mut seen = HashSet::new();
        for class in &self.retry_on {
            if !seen.insert(*class) {
                return Err(invalid(self, format!("duplicate retry_on entry `{class}`")));
            }
            if matches!(class, FailureClass::Cancelled | FailureClass::Escalated) {
                return Err(invalid(
                    self,
                    format!("retry_on must not contain `{class}`; it is never retried"),
                ));
            }
        }
        if !self.retry_on.is_empty() && self.max_attempts == 1 {
            return Err(invalid(
                self,
                "retry_on is set but max_attempts is 1, so nothing would be retried",
            ));
        }
        Ok(())
    }

    fn validate_steps(&self) -> Result<(), ConfigError> {
        if self.step.is_empty() {
            return Err(invalid(self, "at least one step is required"));
        }
        if self.step.len() > MAX_STEPS {
            return Err(invalid(
                self,
                format!("at most {MAX_STEPS} steps are allowed"),
            ));
        }
        let mut steps = HashSet::new();
        for step in &self.step {
            if step.name.trim().is_empty() {
                return Err(invalid(self, "step name must not be empty"));
            }
            if !identity_is_valid(&step.identity) {
                return Err(invalid(
                    self,
                    format!(
                        "step `{}` has an empty or invalid identity `{}`",
                        step.name, step.identity
                    ),
                ));
            }
            if !steps.insert(step.name.to_lowercase()) {
                return Err(invalid(
                    self,
                    format!("duplicate step name `{}`", step.name),
                ));
            }
        }
        Ok(())
    }

    fn validate_judges(&self) -> Result<(), ConfigError> {
        for (index, judge) in self.judge.iter().enumerate() {
            if self.judge[..index].contains(judge) {
                return Err(invalid(
                    self,
                    format!("duplicate judge at position {}", index + 1),
                ));
            }
            match judge {
                Judge::Diff {
                    max_lines,
                    forbid_paths,
                } => {
                    if !(1..=MAX_DIFF_LINES).contains(max_lines) {
                        return Err(invalid(
                            self,
                            format!("diff judge max_lines must be between 1 and {MAX_DIFF_LINES}"),
                        ));
                    }
                    let mut seen = HashSet::new();
                    for pattern in forbid_paths {
                        if pattern.trim().is_empty() || has_edge_whitespace(pattern) {
                            return Err(invalid(
                                self,
                                "diff judge forbid_paths entries must be non-empty and trimmed",
                            ));
                        }
                        if let Some(problem) = glob_problem(pattern) {
                            return Err(invalid(
                                self,
                                format!(
                                    "diff judge forbid_paths entry `{pattern}` is not a valid glob: it {problem}"
                                ),
                            ));
                        }
                        if !seen.insert(pattern.as_str()) {
                            return Err(invalid(
                                self,
                                format!("duplicate forbid_paths entry `{pattern}`"),
                            ));
                        }
                    }
                }
                Judge::NannaClean {} => {}
                Judge::Llm {
                    rubric,
                    model,
                    min_score,
                } => {
                    if let Some(problem) = rubric_path_problem(rubric) {
                        return Err(invalid(self, format!("llm judge: {problem}")));
                    }
                    if model.trim().is_empty() || has_edge_whitespace(model) {
                        return Err(invalid(self, "llm judge needs a trimmed, non-empty model"));
                    }
                    if model.starts_with('-')
                        || model.chars().any(|c| c.is_control() || c.is_whitespace())
                    {
                        return Err(invalid(
                            self,
                            "llm judge model must not start with `-` or contain whitespace or control characters",
                        ));
                    }
                    if !(1..=10).contains(min_score) {
                        return Err(invalid(
                            self,
                            "llm judge min_score must be between 1 and 10",
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_outcome(&self) -> Result<(), ConfigError> {
        let mut seen = HashSet::new();
        for check in &self.outcome.required_checks {
            if check.trim().is_empty() || has_edge_whitespace(check) {
                return Err(invalid(
                    self,
                    "required_checks must not contain empty or untrimmed names",
                ));
            }
            if !seen.insert(check.as_str()) {
                return Err(invalid(
                    self,
                    format!("duplicate required_checks entry `{check}`"),
                ));
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

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

    fn base_flow() -> Flow {
        FlowFile::parse(FULL).unwrap().flow.remove(0)
    }

    fn reason(flow: &Flow) -> String {
        match flow.validate() {
            Err(ConfigError::Invalid { reason, .. }) => reason,
            Err(other) => other.to_string(),
            Ok(()) => String::new(),
        }
    }

    fn llm(flow: &mut Flow) -> &mut Judge {
        flow.judge
            .iter_mut()
            .find(|judge| matches!(judge, Judge::Llm { .. }))
            .unwrap()
    }

    fn diff(flow: &mut Flow) -> &mut Judge {
        flow.judge
            .iter_mut()
            .find(|judge| matches!(judge, Judge::Diff { .. }))
            .unwrap()
    }

    type Mutator = fn(&mut Flow);
    type Violation = (&'static str, Mutator, &'static str);

    fn violations() -> Vec<Violation> {
        vec![
            (
                "empty trigger label",
                |f| {
                    f.trigger = Trigger::Label {
                        label: String::new(),
                    }
                },
                "trigger label must not be empty",
            ),
            (
                "blank trigger label",
                |f| f.trigger = Trigger::Label { label: "  ".into() },
                "trigger label must not be empty",
            ),
            (
                "untrimmed trigger label",
                |f| f.trigger = Trigger::Label { label: " x".into() },
                "trimmed",
            ),
            (
                "flow name with a slash",
                |f| f.name = "../x".into(),
                "flow name",
            ),
            (
                "flow name with a newline",
                |f| f.name = "a\nb".into(),
                "flow name",
            ),
            (
                "flow name with a space",
                |f| f.name = "a b".into(),
                "flow name",
            ),
            (
                "flow name starting with a dot",
                |f| f.name = ".x".into(),
                "flow name",
            ),
            (
                "overlong flow name",
                |f| f.name = "n".repeat(MAX_NAME_LEN + 1),
                "flow name",
            ),
            (
                "trigger label with NUL",
                |f| {
                    f.trigger = Trigger::Label {
                        label: "a\0b".into(),
                    }
                },
                "control character",
            ),
            (
                "trigger label with a newline",
                |f| {
                    f.trigger = Trigger::Label {
                        label: "a\nb".into(),
                    }
                },
                "control character",
            ),
            (
                "model that looks like an option",
                |f| {
                    if let Judge::Llm { model, .. } = llm(f) {
                        *model = "--flag".into()
                    }
                },
                "model",
            ),
            (
                "model with inner whitespace",
                |f| {
                    if let Judge::Llm { model, .. } = llm(f) {
                        *model = "a b".into()
                    }
                },
                "model",
            ),
            (
                "model with a control character",
                |f| {
                    if let Judge::Llm { model, .. } = llm(f) {
                        *model = "a\u{1}b".into()
                    }
                },
                "model",
            ),
            (
                "forbid_paths with an unclosed bracket",
                |f| {
                    if let Judge::Diff { forbid_paths, .. } = diff(f) {
                        forbid_paths.push("[".into())
                    }
                },
                "valid glob",
            ),
            (
                "forbid_paths with an unclosed brace",
                |f| {
                    if let Judge::Diff { forbid_paths, .. } = diff(f) {
                        forbid_paths.push("{a,b".into())
                    }
                },
                "valid glob",
            ),
            (
                "forbid_paths with a stray closing brace",
                |f| {
                    if let Judge::Diff { forbid_paths, .. } = diff(f) {
                        forbid_paths.push("a}".into())
                    }
                },
                "valid glob",
            ),
            (
                "absolute forbid_paths entry",
                |f| {
                    if let Judge::Diff { forbid_paths, .. } = diff(f) {
                        forbid_paths.push("/etc".into())
                    }
                },
                "valid glob",
            ),
            (
                "forbid_paths entry with a parent segment",
                |f| {
                    if let Judge::Diff { forbid_paths, .. } = diff(f) {
                        forbid_paths.push("../x".into())
                    }
                },
                "valid glob",
            ),
            (
                "overlong trigger label",
                |f| {
                    f.trigger = Trigger::Label {
                        label: "x".repeat(MAX_LABEL_LEN + 1),
                    }
                },
                "at most 50",
            ),
            (
                "empty base",
                |f| f.base = String::new(),
                "not a valid git branch name",
            ),
            (
                "base looks like an option",
                |f| f.base = "--force".into(),
                "not a valid git branch name",
            ),
            (
                "base with leading space",
                |f| f.base = " main".into(),
                "not a valid git branch name",
            ),
            (
                "base with trailing space",
                |f| f.base = "main ".into(),
                "not a valid git branch name",
            ),
            (
                "base with parent segment",
                |f| f.base = "../x".into(),
                "not a valid git branch name",
            ),
            (
                "zero max_attempts",
                |f| {
                    f.max_attempts = 0;
                    f.retry_on.clear()
                },
                "max_attempts must be between 1 and 20",
            ),
            (
                "max_attempts above the cap",
                |f| f.max_attempts = MAX_ATTEMPTS + 1,
                "max_attempts must be between 1 and 20",
            ),
            (
                "max_attempts u32::MAX",
                |f| f.max_attempts = u32::MAX,
                "max_attempts must be between 1 and 20",
            ),
            (
                "zero timeout",
                |f| f.attempt_timeout_secs = 0,
                "attempt_timeout_secs",
            ),
            (
                "timeout above the cap",
                |f| f.attempt_timeout_secs = MAX_TIMEOUT_SECS + 1,
                "attempt_timeout_secs",
            ),
            (
                "duplicate retry_on",
                |f| f.retry_on = vec![FailureClass::ModelError, FailureClass::ModelError],
                "duplicate retry_on",
            ),
            (
                "retry_on cancelled",
                |f| f.retry_on = vec![FailureClass::Cancelled],
                "never retried",
            ),
            (
                "retry_on escalated",
                |f| f.retry_on = vec![FailureClass::Escalated],
                "never retried",
            ),
            (
                "retry_on with a single attempt",
                |f| f.max_attempts = 1,
                "nothing would be retried",
            ),
            ("no steps", |f| f.step.clear(), "at least one step"),
            (
                "too many steps",
                |f| {
                    f.step = (0..=MAX_STEPS)
                        .map(|n| Step {
                            name: format!("s{n}"),
                            identity: "x".into(),
                        })
                        .collect()
                },
                "at most 20 steps",
            ),
            (
                "empty step name",
                |f| f.step[0].name = String::new(),
                "step name must not be empty",
            ),
            (
                "blank step name",
                |f| f.step[0].name = "  ".into(),
                "step name must not be empty",
            ),
            (
                "empty identity",
                |f| f.step[0].identity = String::new(),
                "invalid identity",
            ),
            (
                "identity with a space",
                |f| f.step[0].identity = "a b".into(),
                "invalid identity",
            ),
            (
                "identity with a slash",
                |f| f.step[0].identity = "a/b".into(),
                "invalid identity",
            ),
            (
                "overlong identity",
                |f| f.step[0].identity = "a".repeat(65),
                "invalid identity",
            ),
            (
                "duplicate step name ignoring case",
                |f| f.step[1].name = f.step[0].name.to_uppercase(),
                "duplicate step name",
            ),
            (
                "duplicate judge",
                |f| {
                    let first = f.judge[0].clone();
                    f.judge.push(first)
                },
                "duplicate judge",
            ),
            (
                "diff max_lines zero",
                |f| {
                    if let Judge::Diff { max_lines, .. } = diff(f) {
                        *max_lines = 0
                    }
                },
                "max_lines must be between 1 and 100000",
            ),
            (
                "diff max_lines above the cap",
                |f| {
                    if let Judge::Diff { max_lines, .. } = diff(f) {
                        *max_lines = MAX_DIFF_LINES + 1
                    }
                },
                "max_lines must be between 1 and 100000",
            ),
            (
                "empty forbid_paths entry",
                |f| {
                    if let Judge::Diff { forbid_paths, .. } = diff(f) {
                        forbid_paths.push(String::new())
                    }
                },
                "non-empty and trimmed",
            ),
            (
                "untrimmed forbid_paths entry",
                |f| {
                    if let Judge::Diff { forbid_paths, .. } = diff(f) {
                        forbid_paths.push(" x".into())
                    }
                },
                "non-empty and trimmed",
            ),
            (
                "duplicate forbid_paths entry",
                |f| {
                    if let Judge::Diff { forbid_paths, .. } = diff(f) {
                        forbid_paths.push("publish.sh".into())
                    }
                },
                "duplicate forbid_paths",
            ),
            (
                "absolute rubric",
                |f| {
                    if let Judge::Llm { rubric, .. } = llm(f) {
                        *rubric = "/etc/passwd".into()
                    }
                },
                "relative path",
            ),
            (
                "rubric with a parent segment",
                |f| {
                    if let Judge::Llm { rubric, .. } = llm(f) {
                        *rubric = ".inchworm/../secrets".into()
                    }
                },
                "must not contain `..`",
            ),
            (
                "rubric outside .inchworm",
                |f| {
                    if let Judge::Llm { rubric, .. } = llm(f) {
                        *rubric = "docs/rubric.md".into()
                    }
                },
                "under .inchworm/",
            ),
            (
                "empty rubric",
                |f| {
                    if let Judge::Llm { rubric, .. } = llm(f) {
                        *rubric = PathBuf::new()
                    }
                },
                "rubric must not be empty",
            ),
            (
                "empty model",
                |f| {
                    if let Judge::Llm { model, .. } = llm(f) {
                        model.clear()
                    }
                },
                "trimmed, non-empty model",
            ),
            (
                "untrimmed model",
                |f| {
                    if let Judge::Llm { model, .. } = llm(f) {
                        *model = " m".into()
                    }
                },
                "trimmed, non-empty model",
            ),
            (
                "min_score zero",
                |f| {
                    if let Judge::Llm { min_score, .. } = llm(f) {
                        *min_score = 0
                    }
                },
                "between 1 and 10",
            ),
            (
                "min_score eleven",
                |f| {
                    if let Judge::Llm { min_score, .. } = llm(f) {
                        *min_score = 11
                    }
                },
                "between 1 and 10",
            ),
            (
                "empty required check",
                |f| f.outcome.required_checks.push(String::new()),
                "empty or untrimmed",
            ),
            (
                "untrimmed required check",
                |f| f.outcome.required_checks.push(" x".into()),
                "empty or untrimmed",
            ),
            (
                "duplicate required check",
                |f| f.outcome.required_checks.push("Tests".into()),
                "duplicate required_checks",
            ),
            (
                "merge_when without judges",
                |f| f.judge.clear(),
                "at least one judge",
            ),
            (
                "merge_when without checks",
                |f| f.outcome.required_checks.clear(),
                "non-empty required_checks",
            ),
        ]
    }

    #[test]
    fn the_base_flow_is_valid() {
        assert_eq!(reason(&base_flow()), "");
    }

    #[test]
    fn every_violation_is_rejected_with_its_own_reason() {
        for (name, mutate, expected) in violations() {
            let mut flow = base_flow();
            mutate(&mut flow);
            let got = reason(&flow);
            assert!(
                got.contains(expected),
                "{name}: expected a reason containing `{expected}`, got `{got}`"
            );
        }
    }

    #[test]
    fn boundary_values_just_inside_the_limits_are_accepted() {
        let cases: Vec<(&str, Mutator)> = vec![
            ("max_attempts 1 without retry_on", |f| {
                f.max_attempts = 1;
                f.retry_on.clear()
            }),
            ("flow name of 64 characters", |f| {
                f.name = "n".repeat(MAX_NAME_LEN)
            }),
            ("flow name with dot, dash and underscore", |f| {
                f.name = "A.b_c-9".into()
            }),
            ("label with spaces and a colon", |f| {
                f.trigger = Trigger::Label {
                    label: "needs: review".into(),
                }
            }),
            ("model with a colon and a dot", |f| {
                if let Judge::Llm { model, .. } = llm(f) {
                    *model = "gemma4:e4b-q4.K_M".into()
                }
            }),
            ("globs with classes, braces and stars", |f| {
                if let Judge::Diff { forbid_paths, .. } = diff(f) {
                    forbid_paths.extend(
                        ["**/*.rs", "src/{a,b}/**", "[abc]*", "a\\[b", "docs/[!x]*"]
                            .map(String::from),
                    )
                }
            }),
            ("max_attempts at the cap", |f| f.max_attempts = MAX_ATTEMPTS),
            ("timeout 1", |f| f.attempt_timeout_secs = 1),
            ("timeout at the cap", |f| {
                f.attempt_timeout_secs = MAX_TIMEOUT_SECS
            }),
            ("label of 50 characters", |f| {
                f.trigger = Trigger::Label {
                    label: "x".repeat(MAX_LABEL_LEN),
                }
            }),
            ("20 steps", |f| {
                f.step = (0..MAX_STEPS)
                    .map(|n| Step {
                        name: format!("s{n}"),
                        identity: "x".into(),
                    })
                    .collect()
            }),
            ("identity of 64 characters", |f| {
                f.step[0].identity = format!("{}_-.", "a".repeat(61))
            }),
            ("min_score 1", |f| {
                if let Judge::Llm { min_score, .. } = llm(f) {
                    *min_score = 1
                }
            }),
            ("min_score 10", |f| {
                if let Judge::Llm { min_score, .. } = llm(f) {
                    *min_score = 10
                }
            }),
            ("max_lines 1", |f| {
                if let Judge::Diff { max_lines, .. } = diff(f) {
                    *max_lines = 1
                }
            }),
            ("max_lines at the cap", |f| {
                if let Judge::Diff { max_lines, .. } = diff(f) {
                    *max_lines = MAX_DIFF_LINES
                }
            }),
            ("nested rubric", |f| {
                if let Judge::Llm { rubric, .. } = llm(f) {
                    *rubric = ".inchworm/judges/a.md".into()
                }
            }),
            ("slash in base", |f| f.base = "release/1.0".into()),
            ("version tag as base", |f| f.base = "v1.2.3".into()),
        ];
        for (name, mutate) in cases {
            let mut flow = base_flow();
            mutate(&mut flow);
            assert_eq!(reason(&flow), "", "{name}");
        }
    }

    #[test]
    fn flow_names_must_be_present_and_unique_ignoring_case() {
        let mut file = FlowFile::parse(FULL).unwrap();
        file.flow[0].name = "  ".into();
        assert!(matches!(file.validate(), Err(ConfigError::EmptyFlowName)));
        file.flow[0].name = String::new();
        assert!(matches!(file.validate(), Err(ConfigError::EmptyFlowName)));

        let mut twin = base_flow();
        twin.name = "ISSUE".into();
        let file = FlowFile {
            flow: vec![base_flow(), twin],
        };
        assert!(matches!(
            file.validate(),
            Err(ConfigError::DuplicateFlow(name)) if name == "ISSUE"
        ));
    }

    #[test]
    fn glob_validity_covers_classes_braces_escapes_and_nesting() {
        for good in [
            "**/*.rs",
            "src/{a,b}/**",
            "{a,{b,c}}",
            "[abc]*",
            "[]]",
            "a\\[b",
            "a\\{b",
            "[{]",
            "docs/[!x]*",
            "",
        ] {
            assert_eq!(glob_problem(good), None, "{good:?}");
        }
        for (bad, expected) in [
            ("[", "`[`"),
            ("a[b", "`[`"),
            ("{", "`{`"),
            ("{a,b", "`{`"),
            ("a}", "`}`"),
            ("}{", "`}`"),
            ("{a}}", "`}`"),
            ("/etc", "relative"),
            ("../x", "`..`"),
            ("a/../b", "`..`"),
        ] {
            let got = glob_problem(bad);
            assert!(
                got.is_some_and(|message| message.contains(expected)),
                "{bad:?}: expected `{expected}`, got {got:?}"
            );
        }
    }

    #[test]
    fn two_flows_may_not_share_a_trigger_label_ignoring_case() {
        let mut other = base_flow();
        other.name = "second".into();
        let file = FlowFile {
            flow: vec![base_flow(), other.clone()],
        };
        let err = file.validate().unwrap_err().to_string();
        assert!(
            err.contains("second") && err.contains("trigger label"),
            "{err}"
        );

        other.trigger = Trigger::Label {
            label: "INCHWORM".into(),
        };
        let file = FlowFile {
            flow: vec![base_flow(), other.clone()],
        };
        assert!(file.validate().is_err());

        other.trigger = Trigger::Label {
            label: "another".into(),
        };
        let file = FlowFile {
            flow: vec![base_flow(), other],
        };
        assert!(file.validate().is_ok());
    }

    #[test]
    fn unknown_keys_inside_a_trigger_are_rejected() {
        let text = minimal().replace(
            "trigger = { kind = \"label\", label = \"inchworm\" }",
            "trigger = { kind = \"label\", label = \"inchworm\", extra = 1 }",
        );
        assert!(FlowFile::parse(&text).is_err());
        assert!(FlowFile::parse(&minimal()).is_ok());
    }

    #[test]
    fn rubric_paths_are_confined_to_the_inchworm_directory() {
        let accepted = [
            ".inchworm/rubric.md",
            ".inchworm/judges/a.md",
            ".inchworm/a",
        ];
        for path in accepted {
            assert_eq!(rubric_path_problem(Path::new(path)), None, "{path}");
        }
        let rejected = [
            ("", "must not be empty"),
            ("/etc/passwd", "relative path"),
            ("../x", "under .inchworm/"),
            (".inchworm", "under .inchworm/"),
            (".inchworm/", "under .inchworm/"),
            (".inchworm/../x", "`..`"),
            (".inchworm/a/../../x", "`..`"),
            ("./.inchworm/x", "under .inchworm/"),
            ("docs/rubric.md", "under .inchworm/"),
            (".inchworm.md", "under .inchworm/"),
            (".inchworm\\x", "separators"),
            ("a\\b", "separators"),
        ];
        for (path, expected) in rejected {
            let got = rubric_path_problem(Path::new(path));
            assert!(
                got.is_some_and(|message| message.contains(expected)),
                "{path:?}: expected `{expected}`, got {got:?}"
            );
        }
    }

    #[test]
    fn git_ref_names_are_checked_against_git_rules() {
        for good in ["main", "release/1.0", "feature/x_y", "v1.2.3", "a", "a-b"] {
            assert!(valid_ref_name(good), "{good}");
        }
        let bad = [
            "", "--force", "-x", " main", "main ", "../x", "a..b", "a//b", "/x", "x/", "x.",
            "x.lock", "a/x.lock", "a b", "a~b", "a^b", "a:b", "a?b", "a*b", "a[b", "a\\b", "@",
            "a@{b", ".hidden", "a/.b", "a\u{1}b", "a\tb",
        ];
        for name in bad {
            assert!(!valid_ref_name(name), "{name:?}");
        }
        assert!(!valid_ref_name(&"a".repeat(201)));
        assert!(valid_ref_name(&"a".repeat(200)));
    }

    fn name_strategy() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9_-]{0,10}".prop_map(String::from)
    }

    fn judges_strategy() -> impl Strategy<Value = Vec<Judge>> {
        let diff = (
            1u32..5000,
            prop::collection::btree_set("[a-z*]{1,6}(/[a-z*]{1,6}){0,2}", 0..3),
        )
            .prop_map(|(max_lines, forbid_paths)| Judge::Diff {
                max_lines,
                forbid_paths: forbid_paths.into_iter().collect(),
            });
        let llm = ("[a-z]{1,8}", name_strategy(), 1u8..=10).prop_map(|(file, model, min_score)| {
            Judge::Llm {
                rubric: PathBuf::from(format!(".inchworm/{file}.md")),
                model,
                min_score,
            }
        });
        let judge = prop_oneof![diff, Just(Judge::NannaClean {}), llm];
        prop::collection::vec(judge, 1..4).prop_map(|judges| {
            let mut unique: Vec<Judge> = Vec::new();
            for judge in judges {
                if !unique.contains(&judge) {
                    unique.push(judge);
                }
            }
            unique
        })
    }

    fn flow_strategy() -> impl Strategy<Value = Flow> {
        (
            name_strategy(),
            name_strategy(),
            prop_oneof![
                name_strategy(),
                name_strategy().prop_map(|n| format!("release/{n}"))
            ],
            2u32..=MAX_ATTEMPTS,
            prop::collection::btree_set(0..FailureClass::ALL.len(), 0..4),
            1u64..100_000,
            prop::collection::btree_map(name_strategy(), name_strategy(), 1..4),
            judges_strategy(),
            prop::collection::btree_set(name_strategy(), 1..3),
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
                    Flow {
                        name,
                        trigger: Trigger::Label { label },
                        base,
                        max_attempts,
                        retry_on: retry
                            .into_iter()
                            .map(|i| FailureClass::ALL[i])
                            .filter(|c| {
                                !matches!(c, FailureClass::Cancelled | FailureClass::Escalated)
                            })
                            .collect(),
                        attempt_timeout_secs: timeout,
                        step: steps
                            .into_iter()
                            .map(|(name, identity)| Step { name, identity })
                            .collect(),
                        judge,
                        outcome: Outcome {
                            mode: if merge {
                                OutcomeMode::MergeWhen
                            } else {
                                OutcomeMode::DraftPr
                            },
                            required_checks: checks.into_iter().collect(),
                            merge_method: method,
                        },
                    }
                },
            )
    }

    proptest! {
        #[test]
        fn generated_valid_flows_validate_and_round_trip_through_toml(flow in flow_strategy()) {
            prop_assert_eq!(flow.validate().is_ok(), true, "{:?}", flow.validate());
            let file = FlowFile { flow: vec![flow] };
            let text = toml::to_string(&file).unwrap();
            let parsed = FlowFile::parse(&text).unwrap();
            prop_assert_eq!(parsed, file);
        }

        #[test]
        fn any_single_known_violation_makes_a_generated_flow_invalid(index in 0usize..1000) {
            let table = violations();
            let (name, mutate, _) = table[index % table.len()];
            let mut flow = base_flow();
            mutate(&mut flow);
            prop_assert!(flow.validate().is_err(), "{}", name);
        }

        #[test]
        fn merge_when_without_judges_is_always_rejected(mut flow in flow_strategy()) {
            flow.outcome.mode = OutcomeMode::MergeWhen;
            flow.judge.clear();
            prop_assert!(flow.validate().is_err());
        }

        #[test]
        fn rubrics_with_a_parent_segment_or_outside_the_directory_are_rejected(
            before in prop::collection::vec("[a-z]{1,5}", 0..3),
            after in prop::collection::vec("[a-z]{1,5}", 0..3),
        ) {
            let mut inside = vec![".inchworm".to_string()];
            inside.extend(before.clone());
            inside.push("..".to_string());
            inside.extend(after.clone());
            prop_assert!(rubric_path_problem(Path::new(&inside.join("/"))).is_some());

            let mut outside = before;
            outside.push("rubric.md".into());
            prop_assume!(outside[0] != ".inchworm");
            prop_assert!(rubric_path_problem(Path::new(&outside.join("/"))).is_some());
        }

        #[test]
        fn rubrics_inside_the_directory_are_accepted(parts in prop::collection::vec("[a-z]{1,5}", 1..4)) {
            let path = format!(".inchworm/{}", parts.join("/"));
            prop_assert_eq!(rubric_path_problem(Path::new(&path)), None);
        }

        #[test]
        fn arbitrary_text_never_panics(text in ".*") {
            let _ = FlowFile::parse(&text);
        }
    }
}
