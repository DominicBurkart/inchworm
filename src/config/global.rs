use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GlobalConfigError {
    #[error("invalid config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("{0}")]
    Invalid(String),
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalConfig {
    pub nanna_bin: PathBuf,
    #[serde(default = "default_ollama_url")]
    pub ollama_url: String,
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
    #[serde(default = "default_state_db")]
    pub state_db: PathBuf,
    pub repo: Vec<RepoEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoEntry {
    pub slug: String,
    pub path: PathBuf,
}

fn default_ollama_url() -> String {
    "http://localhost:11434".to_string()
}

fn default_concurrency() -> usize {
    1
}

fn default_poll_secs() -> u64 {
    120
}

fn default_state_db() -> PathBuf {
    PathBuf::from("~/.local/state/inchworm/state.db")
}

fn invalid(message: impl Into<String>) -> GlobalConfigError {
    GlobalConfigError::Invalid(message.into())
}

fn slug_is_valid(slug: &str) -> bool {
    let part_ok = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    };
    match slug.split_once('/') {
        Some((owner, name)) => part_ok(owner) && part_ok(name),
        None => false,
    }
}

fn expand(path: &Path, home: &Path) -> PathBuf {
    if path == Path::new("~") {
        return home.to_path_buf();
    }
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

impl GlobalConfig {
    /// ```
    /// use inchworm::config::GlobalConfig;
    ///
    /// let config = GlobalConfig::parse(r#"
    /// nanna_bin = "/usr/local/bin/nanna"
    ///
    /// [[repo]]
    /// slug = "DominicBurkart/marigold"
    /// path = "/home/me/marigold"
    /// "#).unwrap();
    ///
    /// assert_eq!(config.concurrency, 1);
    /// assert_eq!(config.poll_secs, 120);
    /// assert_eq!(config.ollama_url, "http://localhost:11434");
    ///
    /// assert!(GlobalConfig::parse("nanna_bin = \"x\"\nrepo = []").is_err());
    /// ```
    pub fn parse(text: &str) -> Result<GlobalConfig, GlobalConfigError> {
        let config: GlobalConfig = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: &Path, home: &Path) -> Result<GlobalConfig, GlobalConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| GlobalConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(GlobalConfig::parse(&text)?.expanded(home))
    }

    pub fn default_path(xdg_config_home: Option<&Path>, home: &Path) -> PathBuf {
        match xdg_config_home {
            Some(xdg) if xdg.is_absolute() => xdg.join("inchworm/config.toml"),
            _ => home.join(".config/inchworm/config.toml"),
        }
    }

    pub fn validate(&self) -> Result<(), GlobalConfigError> {
        if self.nanna_bin.as_os_str().is_empty() {
            return Err(invalid("nanna_bin must not be empty"));
        }
        if !(self.ollama_url.starts_with("http://") || self.ollama_url.starts_with("https://")) {
            return Err(invalid("ollama_url must start with http:// or https://"));
        }
        if self.concurrency == 0 {
            return Err(invalid("concurrency must be at least 1"));
        }
        if self.poll_secs == 0 {
            return Err(invalid("poll_secs must be at least 1"));
        }
        if self.repo.is_empty() {
            return Err(invalid("at least one [[repo]] is required"));
        }
        let mut slugs = HashSet::new();
        for repo in &self.repo {
            if !slug_is_valid(&repo.slug) {
                return Err(invalid(format!(
                    "repo slug `{}` must look like owner/name",
                    repo.slug
                )));
            }
            if !slugs.insert(repo.slug.as_str()) {
                return Err(invalid(format!("duplicate repo slug `{}`", repo.slug)));
            }
            if repo.path.as_os_str().is_empty() {
                return Err(invalid(format!("repo `{}` has an empty path", repo.slug)));
            }
        }
        Ok(())
    }

    pub fn expanded(mut self, home: &Path) -> GlobalConfig {
        self.nanna_bin = expand(&self.nanna_bin, home);
        self.state_db = expand(&self.state_db, home);
        for repo in &mut self.repo {
            repo.path = expand(&repo.path, home);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn a_repo_with_an_empty_path_is_rejected() {
        let text = "nanna_bin = \"nanna\"\n[[repo]]\nslug = \"o/r\"\npath = \"\"\n";
        assert!(
            GlobalConfig::parse(text)
                .unwrap_err()
                .to_string()
                .contains("empty path")
        );
    }

    const FULL: &str = r#"
nanna_bin = "~/.nix-profile/bin/nanna"
ollama_url = "http://localhost:11434"
concurrency = 1
poll_secs = 120
state_db = "~/.local/state/inchworm/state.db"

[[repo]]
slug = "DominicBurkart/marigold"
path = "/home/dominic/code/marigold"

[[repo]]
slug = "DominicBurkart/nanna-coder"
path = "~/code/nanna-coder"
"#;

    fn err_text(text: &str) -> String {
        GlobalConfig::parse(text).unwrap_err().to_string()
    }

    #[test]
    fn parses_the_full_example() {
        let config = GlobalConfig::parse(FULL).unwrap();
        assert_eq!(config.repo.len(), 2);
        assert_eq!(config.repo[1].slug, "DominicBurkart/nanna-coder");
        assert_eq!(config.concurrency, 1);
    }

    #[test]
    fn defaults_apply() {
        let config =
            GlobalConfig::parse("nanna_bin = \"nanna\"\n[[repo]]\nslug = \"a/b\"\npath = \"/x\"\n")
                .unwrap();
        assert_eq!(config.ollama_url, "http://localhost:11434");
        assert_eq!(config.concurrency, 1);
        assert_eq!(config.poll_secs, 120);
        assert_eq!(
            config.state_db,
            PathBuf::from("~/.local/state/inchworm/state.db")
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(GlobalConfig::parse(&format!("surprise = 1\n{FULL}")).is_err());
        let in_repo = FULL.replacen(
            "path = \"/home/dominic/code/marigold\"",
            "path = \"/x\"\nsurprise = 1",
            1,
        );
        assert!(GlobalConfig::parse(&in_repo).is_err());
    }

    #[test]
    fn rules_are_enforced() {
        assert!(
            err_text(&FULL.replace("concurrency = 1", "concurrency = 0")).contains("concurrency")
        );
        assert!(err_text(&FULL.replace("poll_secs = 120", "poll_secs = 0")).contains("poll_secs"));
        assert!(
            err_text(&FULL.replace("http://localhost:11434", "localhost:11434"))
                .contains("ollama_url")
        );
        assert!(
            err_text(&FULL.replace("DominicBurkart/marigold", "marigold")).contains("owner/name")
        );
        assert!(err_text(&FULL.replace("DominicBurkart/marigold", "a/b/c")).contains("owner/name"));
        assert!(
            err_text(&FULL.replace("DominicBurkart/nanna-coder", "DominicBurkart/marigold"))
                .contains("duplicate")
        );
        assert!(
            err_text(&FULL.replace(
                "nanna_bin = \"~/.nix-profile/bin/nanna\"",
                "nanna_bin = \"\""
            ))
            .contains("nanna_bin")
        );
        assert!(err_text("nanna_bin = \"n\"\nrepo = []").contains("[[repo]]"));
    }

    #[test]
    fn missing_required_fields_are_rejected() {
        assert!(GlobalConfig::parse("").is_err());
        assert!(GlobalConfig::parse("[[repo]]\nslug = \"a/b\"\npath = \"/x\"\n").is_err());
    }

    #[test]
    fn expansion_only_touches_a_leading_tilde() {
        let home = Path::new("/home/me");
        let config = GlobalConfig::parse(FULL).unwrap().expanded(home);
        assert_eq!(
            config.nanna_bin,
            PathBuf::from("/home/me/.nix-profile/bin/nanna")
        );
        assert_eq!(
            config.state_db,
            PathBuf::from("/home/me/.local/state/inchworm/state.db")
        );
        assert_eq!(
            config.repo[0].path,
            PathBuf::from("/home/dominic/code/marigold")
        );
        assert_eq!(
            config.repo[1].path,
            PathBuf::from("/home/me/code/nanna-coder")
        );
        assert_eq!(expand(Path::new("~"), home), home);
        assert_eq!(
            expand(Path::new("~other/x"), home),
            PathBuf::from("~other/x")
        );
        assert_eq!(expand(Path::new("/a/~/b"), home), PathBuf::from("/a/~/b"));
    }

    #[test]
    fn load_reads_and_expands() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, FULL).unwrap();
        let config = GlobalConfig::load(&path, Path::new("/home/me")).unwrap();
        assert_eq!(
            config.repo[1].path,
            PathBuf::from("/home/me/code/nanna-coder")
        );
        let missing = GlobalConfig::load(&dir.path().join("nope.toml"), Path::new("/h"));
        assert!(matches!(missing, Err(GlobalConfigError::Io { .. })));
        std::fs::write(&path, "garbage = [").unwrap();
        assert!(matches!(
            GlobalConfig::load(&path, Path::new("/h")),
            Err(GlobalConfigError::Parse(_))
        ));
    }

    #[test]
    fn default_path_prefers_an_absolute_xdg_dir() {
        let home = Path::new("/home/me");
        assert_eq!(
            GlobalConfig::default_path(Some(Path::new("/xdg")), home),
            PathBuf::from("/xdg/inchworm/config.toml")
        );
        assert_eq!(
            GlobalConfig::default_path(Some(Path::new("relative")), home),
            PathBuf::from("/home/me/.config/inchworm/config.toml")
        );
        assert_eq!(
            GlobalConfig::default_path(None, home),
            PathBuf::from("/home/me/.config/inchworm/config.toml")
        );
    }

    proptest! {
        #[test]
        fn valid_slugs_are_accepted(owner in "[A-Za-z0-9_.-]{1,12}", name in "[A-Za-z0-9_.-]{1,12}") {
            let slug = format!("{owner}/{name}");
            prop_assert!(slug_is_valid(&slug));
        }

        #[test]
        fn slugs_without_exactly_one_slash_are_rejected(text in "[A-Za-z0-9_.-]{0,12}") {
            prop_assert!(!slug_is_valid(&text));
            let three = format!("{}/{}/{}", text, text, text);
            prop_assert!(!slug_is_valid(&three));
        }

        #[test]
        fn expansion_is_idempotent(rest in "[a-z/]{0,12}") {
            let home = Path::new("/home/me");
            let once = expand(&Path::new("~").join(&rest), home);
            prop_assert_eq!(expand(&once, home), once);
        }

        #[test]
        fn arbitrary_text_never_panics(text in ".*") {
            let _ = GlobalConfig::parse(&text);
        }
    }
}
