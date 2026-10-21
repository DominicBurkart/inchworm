//! `inchworm.toml` schema. Every field has a default so an empty file is valid.

use std::path::Path;

use serde::{Deserialize, Serialize};

pub const CONFIG_FILE: &str = "inchworm.toml";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {0}: {1}")]
    Io(String, std::io::Error),
    #[error("parsing {0}: {1}")]
    Parse(String, toml::de::Error),
}

/// What to do when an agent falls off its golden path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Offramp {
    /// Do nothing.
    #[default]
    Noop,
    /// Open a GitHub issue describing the failure.
    Issue,
    /// Hand off to a frontier agent.
    Escalate,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub offramp: OfframpConfig,
    pub issues: IssuesConfig,
    pub ci: CiConfig,
    pub conflicts: ConflictsConfig,
    pub review: ReviewConfig,
}

/// A default offramp plus optional per-journey overrides.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OfframpConfig {
    pub default: Offramp,
    pub issues: Option<Offramp>,
    pub ci: Option<Offramp>,
    pub conflicts: Option<Offramp>,
    pub review: Option<Offramp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IssuesConfig {
    pub enabled: bool,
    /// Only work on issues by these authors. Empty means the authenticated user.
    pub authors: Vec<String>,
    pub skip_with_open_pr: bool,
    /// Hand finished PRs to a code review agent.
    pub code_review_agent: bool,
}

impl Default for IssuesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            authors: vec![],
            skip_with_open_pr: true,
            code_review_agent: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CiConfig {
    pub enabled: bool,
    pub retrigger: Retrigger,
    /// Post the root-cause report as a GitHub comment.
    pub comment: bool,
}

impl Default for CiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            retrigger: Retrigger::default(),
            comment: true,
        }
    }
}

/// Which failure classes justify re-running CI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Retrigger {
    pub flake: bool,
    pub transient_network: bool,
    /// Possibly transient network/compute errors.
    pub ambiguous: bool,
}

impl Default for Retrigger {
    fn default() -> Self {
        Self {
            flake: true,
            transient_network: true,
            ambiguous: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConflictsConfig {
    pub enabled: bool,
    /// Convert unapproved PRs to draft when conflicts appear; re-open when resolved.
    pub draft_on_conflict: bool,
}

impl Default for ConflictsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            draft_on_conflict: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewConfig {
    pub enabled: bool,
    pub test_design: bool,
    pub alignment: bool,
    /// Test standards markdown file or directory reviewers point contributors to.
    pub test_standards: Option<String>,
    /// Run a dedicated addresser per review type.
    pub addressers: bool,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            test_design: true,
            alignment: true,
            test_standards: None,
            addressers: true,
        }
    }
}

impl Config {
    pub fn parse(src: &str, origin: &str) -> Result<Self, ConfigError> {
        toml::from_str(src).map_err(|e| ConfigError::Parse(origin.into(), e))
    }

    /// Load `<dir>/inchworm.toml`.
    pub fn load(dir: &Path) -> Result<Self, ConfigError> {
        let file = dir.join(CONFIG_FILE);
        let name = file.display().to_string();
        let src = std::fs::read_to_string(&file).map_err(|e| ConfigError::Io(name.clone(), e))?;
        Self::parse(&src, &name)
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("config serializes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_is_default() {
        assert_eq!(Config::parse("", "t").unwrap(), Config::default());
    }

    #[test]
    fn overrides_apply() {
        let c = Config::parse(
            "[offramp]\ndefault = \"issue\"\nci = \"escalate\"\n[ci.retrigger]\nambiguous = true\n",
            "t",
        )
        .unwrap();
        assert_eq!(c.offramp.default, Offramp::Issue);
        assert_eq!(c.offramp.ci, Some(Offramp::Escalate));
        assert!(c.ci.retrigger.ambiguous && c.ci.retrigger.flake);
    }

    #[test]
    fn unknown_keys_rejected() {
        assert!(Config::parse("bogus = 1", "t").is_err());
    }

    #[test]
    fn roundtrip() {
        let c = Config::default();
        assert_eq!(Config::parse(&c.to_toml(), "t").unwrap(), c);
    }
}
