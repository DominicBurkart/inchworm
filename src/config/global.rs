use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_CONCURRENCY: usize = 64;
pub const MAX_POLL_SECS: u64 = 86_400;
pub const MAX_CONFIG_BYTES: u64 = 256 * 1024;

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
    #[serde(default)]
    pub state_db: Option<PathBuf>,
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

fn invalid(message: impl Into<String>) -> GlobalConfigError {
    GlobalConfigError::Invalid(message.into())
}

/// Whether `slug` is a GitHub `owner/name`.
///
/// ```
/// use inchworm::config::slug_is_valid;
///
/// assert!(slug_is_valid("DominicBurkart/marigold"));
/// assert!(slug_is_valid("o/.github"));
/// assert!(!slug_is_valid("../x"));
/// assert!(!slug_is_valid("a/b/c"));
/// assert!(!slug_is_valid("a_b/c"));
/// ```
pub fn slug_is_valid(slug: &str) -> bool {
    let Some((owner, name)) = slug.split_once('/') else {
        return false;
    };
    let owner_ok = (1..=39).contains(&owner.len())
        && owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !owner.starts_with('-')
        && !owner.ends_with('-');
    let name_ok = (1..=100).contains(&name.len())
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    owner_ok && name_ok
}

/// Why `url` is not an acceptable `http` or `https` endpoint, or `None`.
///
/// ```
/// use inchworm::config::url_problem;
///
/// assert!(url_problem("http://localhost:11434").is_none());
/// assert!(url_problem("https://ollama.example.com/").is_none());
/// assert!(url_problem("http://").is_some());
/// assert!(url_problem("http://user:pw@host").is_some());
/// assert!(url_problem("ftp://host").is_some());
/// ```
pub fn url_problem(url: &str) -> Option<&'static str> {
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Some("must not contain whitespace");
    }
    let rest = if let Some(rest) = url.strip_prefix("http://") {
        rest
    } else if let Some(rest) = url.strip_prefix("https://") {
        rest
    } else {
        return Some("must start with http:// or https://");
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return Some("must not contain credentials");
    }
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        match bracketed.split_once(']') {
            Some((host, "")) => (host, None),
            Some((host, tail)) => match tail.strip_prefix(':') {
                Some(port) => (host, Some(port)),
                None => return Some("has an invalid host"),
            },
            None => return Some("has an invalid host"),
        }
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if host.is_empty()
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
    {
        return Some("needs a host");
    }
    if let Some(port) = port
        && port.parse::<u16>().ok().filter(|p| *p != 0).is_none()
    {
        return Some("has an invalid port");
    }
    None
}

fn uses_home(path: &Path) -> bool {
    path.starts_with("~")
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

    /// Reads the file at `path`, which must be a regular file of at most
    /// [`MAX_CONFIG_BYTES`]. `home` is needed only when the config uses `~`.
    pub fn load(path: &Path, home: Option<&Path>) -> Result<GlobalConfig, GlobalConfigError> {
        let io = |source| GlobalConfigError::Io {
            path: path.to_path_buf(),
            source,
        };
        let text = crate::fsutil::read_regular_text(path, MAX_CONFIG_BYTES).map_err(io)?;
        let config = GlobalConfig::parse(&text)?;
        let needs_home =
            uses_home(&config.nanna_bin) || config.repo.iter().any(|repo| uses_home(&repo.path));
        let expanded = if needs_home {
            let home = home.filter(|home| home.is_absolute()).ok_or_else(|| {
                invalid("the config uses `~` but HOME is not set to an absolute path")
            })?;
            config.expanded_for_validation(home)
        } else {
            config
        };
        expanded.validate()?;
        Ok(expanded)
    }

    pub fn default_path(xdg_config_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
        match xdg_config_home {
            Some(xdg) if xdg.is_absolute() => Some(xdg.join("inchworm/config.toml")),
            _ => home
                .filter(|home| home.is_absolute())
                .map(|home| home.join(".config/inchworm/config.toml")),
        }
    }

    pub fn validate(&self) -> Result<(), GlobalConfigError> {
        let bin = self.nanna_bin.to_string_lossy();
        if bin.trim().is_empty() || bin.trim() != bin || bin.contains('\0') {
            return Err(invalid(
                "nanna_bin must be a non-empty, trimmed path or command name",
            ));
        }
        if let Some(problem) = url_problem(&self.ollama_url) {
            return Err(invalid(format!("ollama_url {problem}")));
        }
        if !(1..=MAX_CONCURRENCY).contains(&self.concurrency) {
            return Err(invalid(format!(
                "concurrency must be between 1 and {MAX_CONCURRENCY}"
            )));
        }
        if !(1..=MAX_POLL_SECS).contains(&self.poll_secs) {
            return Err(invalid(format!(
                "poll_secs must be between 1 and {MAX_POLL_SECS}"
            )));
        }
        if let Some(state_db) = &self.state_db
            && state_db.to_string_lossy().trim().is_empty()
        {
            return Err(invalid("state_db must not be empty"));
        }
        if self.repo.is_empty() {
            return Err(invalid("at least one [[repo]] is required"));
        }
        let mut slugs = HashSet::new();
        let mut paths = HashSet::new();
        for repo in &self.repo {
            if !slug_is_valid(&repo.slug) {
                return Err(invalid(format!(
                    "repo slug `{}` must look like owner/name",
                    repo.slug
                )));
            }
            if !slugs.insert(repo.slug.to_lowercase()) {
                return Err(invalid(format!("duplicate repo slug `{}`", repo.slug)));
            }
            if repo.path.to_string_lossy().trim().is_empty() {
                return Err(invalid(format!("repo `{}` has an empty path", repo.slug)));
            }
            if !paths.insert(repo.path.clone()) {
                return Err(invalid(format!(
                    "repo `{}` reuses the path {}",
                    repo.slug,
                    repo.path.display()
                )));
            }
        }
        Ok(())
    }

    pub fn expanded(mut self, home: &Path) -> GlobalConfig {
        self.state_db = self.state_db.map(|path| expand(&path, home));
        self.expanded_for_validation(home)
    }

    fn expanded_for_validation(mut self, home: &Path) -> GlobalConfig {
        self.nanna_bin = expand(&self.nanna_bin, home);
        for repo in &mut self.repo {
            repo.path = expand(&repo.path, home);
        }
        self
    }

    /// Where the state database lives: `state_db` when set, otherwise
    /// `~/.local/state/inchworm/state.db`. Needs `home` when it uses `~`.
    ///
    /// ```
    /// use std::path::Path;
    /// use inchworm::config::GlobalConfig;
    ///
    /// let config = GlobalConfig::parse("nanna_bin = \"n\"\n[[repo]]\nslug = \"a/b\"\npath = \"/x\"\n").unwrap();
    /// let home = Path::new("/home/me");
    /// assert_eq!(
    ///     config.state_db_path(Some(home)).unwrap(),
    ///     Path::new("/home/me/.local/state/inchworm/state.db")
    /// );
    /// assert!(config.state_db_path(None).is_err());
    /// ```
    pub fn state_db_path(&self, home: Option<&Path>) -> Result<PathBuf, GlobalConfigError> {
        let configured = self
            .state_db
            .clone()
            .unwrap_or_else(|| PathBuf::from("~/.local/state/inchworm/state.db"));
        if !uses_home(&configured) {
            return Ok(configured);
        }
        let home = home
            .filter(|home| home.is_absolute())
            .ok_or_else(|| invalid("the state database needs HOME set to an absolute path"))?;
        Ok(expand(&configured, home))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

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

    fn base() -> GlobalConfig {
        GlobalConfig::parse(FULL).unwrap()
    }

    fn problem(config: &GlobalConfig) -> String {
        config
            .validate()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default()
    }

    type Mutator = fn(&mut GlobalConfig);

    fn violations() -> Vec<(&'static str, Mutator, &'static str)> {
        vec![
            (
                "empty nanna_bin",
                |c| c.nanna_bin = PathBuf::new(),
                "nanna_bin",
            ),
            ("blank nanna_bin", |c| c.nanna_bin = " ".into(), "nanna_bin"),
            (
                "untrimmed nanna_bin",
                |c| c.nanna_bin = " nanna".into(),
                "nanna_bin",
            ),
            (
                "url without a host",
                |c| c.ollama_url = "http://".into(),
                "ollama_url needs a host",
            ),
            (
                "url without a scheme",
                |c| c.ollama_url = "localhost:11434".into(),
                "http:// or https://",
            ),
            (
                "url with credentials",
                |c| c.ollama_url = "http://u:p@host".into(),
                "credentials",
            ),
            (
                "url with a bad port",
                |c| c.ollama_url = "http://host:99999".into(),
                "invalid port",
            ),
            (
                "url with port zero",
                |c| c.ollama_url = "http://host:0".into(),
                "invalid port",
            ),
            (
                "url with whitespace",
                |c| c.ollama_url = "http://ho st".into(),
                "whitespace",
            ),
            (
                "zero concurrency",
                |c| c.concurrency = 0,
                "concurrency must be between 1 and 64",
            ),
            (
                "huge concurrency",
                |c| c.concurrency = 100_000_000,
                "concurrency must be between 1 and 64",
            ),
            (
                "zero poll_secs",
                |c| c.poll_secs = 0,
                "poll_secs must be between 1 and 86400",
            ),
            (
                "huge poll_secs",
                |c| c.poll_secs = i64::MAX as u64,
                "poll_secs must be between 1 and 86400",
            ),
            (
                "empty state_db",
                |c| c.state_db = Some(PathBuf::new()),
                "state_db must not be empty",
            ),
            ("no repos", |c| c.repo.clear(), "[[repo]]"),
            (
                "slug without owner",
                |c| c.repo[0].slug = "marigold".into(),
                "owner/name",
            ),
            (
                "slug with a parent segment",
                |c| c.repo[0].slug = "../x".into(),
                "owner/name",
            ),
            (
                "slug with a dot name",
                |c| c.repo[0].slug = "o/..".into(),
                "owner/name",
            ),
            (
                "slug with three parts",
                |c| c.repo[0].slug = "a/b/c".into(),
                "owner/name",
            ),
            (
                "slug with an empty owner",
                |c| c.repo[0].slug = "/b".into(),
                "owner/name",
            ),
            (
                "slug with an empty name",
                |c| c.repo[0].slug = "a/".into(),
                "owner/name",
            ),
            (
                "slug with a leading dash owner",
                |c| c.repo[0].slug = "-a/b".into(),
                "owner/name",
            ),
            (
                "slug with a trailing dash owner",
                |c| c.repo[0].slug = "a-/b".into(),
                "owner/name",
            ),
            (
                "slug with a single dot name",
                |c| c.repo[0].slug = "o/.".into(),
                "owner/name",
            ),
            (
                "concurrency just above the cap",
                |c| c.concurrency = MAX_CONCURRENCY + 1,
                "concurrency must be between 1 and 64",
            ),
            (
                "poll_secs just above the cap",
                |c| c.poll_secs = MAX_POLL_SECS + 1,
                "poll_secs must be between 1 and 86400",
            ),
            (
                "slug with an underscore owner",
                |c| c.repo[0].slug = "a_b/c".into(),
                "owner/name",
            ),
            (
                "slugs differing only in case",
                |c| c.repo[1].slug = "dominicburkart/MARIGOLD".into(),
                "duplicate repo slug",
            ),
            (
                "empty repo path",
                |c| c.repo[0].path = PathBuf::new(),
                "empty path",
            ),
            (
                "blank repo path",
                |c| c.repo[0].path = "  ".into(),
                "empty path",
            ),
            (
                "one path for two repos",
                |c| c.repo[1].path = c.repo[0].path.clone(),
                "reuses the path",
            ),
        ]
    }

    #[test]
    fn the_base_config_is_valid() {
        assert_eq!(problem(&base()), "");
    }

    #[test]
    fn every_violation_is_rejected_with_its_own_reason() {
        for (name, mutate, expected) in violations() {
            let mut config = base();
            mutate(&mut config);
            let got = problem(&config);
            assert!(
                got.contains(expected),
                "{name}: expected `{expected}`, got `{got}`"
            );
        }
    }

    #[test]
    fn values_just_inside_the_limits_are_accepted() {
        let cases: Vec<(&str, Mutator)> = vec![
            ("concurrency 1", |c| c.concurrency = 1),
            ("concurrency at the cap", |c| {
                c.concurrency = MAX_CONCURRENCY
            }),
            ("poll_secs 1", |c| c.poll_secs = 1),
            ("poll_secs at the cap", |c| c.poll_secs = MAX_POLL_SECS),
            ("bare command name", |c| c.nanna_bin = "nanna".into()),
            ("https url", |c| {
                c.ollama_url = "https://ollama.example.com/api".into()
            }),
            ("ipv6 url", |c| c.ollama_url = "http://[::1]:11434".into()),
            ("ipv6 url without a port", |c| {
                c.ollama_url = "http://[::1]".into()
            }),
            ("url with a query", |c| {
                c.ollama_url = "http://host?x=1".into()
            }),
            ("dot-github repo", |c| c.repo[0].slug = "o/.github".into()),
            ("max length owner", |c| {
                c.repo[0].slug = format!("{}/r", "a".repeat(39))
            }),
            ("max length name", |c| {
                c.repo[0].slug = format!("o/{}", "a".repeat(100))
            }),
        ];
        for (name, mutate) in cases {
            let mut config = base();
            mutate(&mut config);
            assert_eq!(problem(&config), "", "{name}");
        }
        for slug in [
            format!("{}/r", "a".repeat(40)),
            format!("o/{}", "a".repeat(101)),
        ] {
            let mut config = base();
            config.repo[0].slug = slug.clone();
            assert!(problem(&config).contains("owner/name"), "{slug}");
        }
    }

    #[test]
    fn urls_are_checked_for_every_malformed_form() {
        for bad in [
            "",
            "http",
            "http:/x",
            "HTTP://x",
            "http://[::1",
            "http://[::1]x",
            "http://[]",
            "http://:80",
            "http://a b",
            "http://h:",
            "http://h:65536",
            "http://h:-1",
            "http://ho$t",
            "https://",
            "https://@",
            "file:///x",
        ] {
            assert!(url_problem(bad).is_some(), "{bad:?}");
        }
        for good in [
            "http://a",
            "https://a.b-c:1",
            "http://127.0.0.1:65535",
            "http://[2001:db8::1]:8080/x",
        ] {
            assert!(url_problem(good).is_none(), "{good:?}");
        }
    }

    #[test]
    fn parses_the_full_example() {
        let config = base();
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
        assert_eq!(config.state_db, None);
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
    fn missing_required_fields_are_rejected() {
        assert!(GlobalConfig::parse("").is_err());
        assert!(GlobalConfig::parse("[[repo]]\nslug = \"a/b\"\npath = \"/x\"\n").is_err());
    }

    #[test]
    fn expansion_only_touches_a_leading_tilde() {
        let home = Path::new("/home/me");
        let config = base().expanded(home);
        assert_eq!(
            config.nanna_bin,
            PathBuf::from("/home/me/.nix-profile/bin/nanna")
        );
        assert_eq!(
            config.state_db,
            Some(PathBuf::from("/home/me/.local/state/inchworm/state.db"))
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
    fn load_reads_expands_and_needs_home_only_for_a_tilde() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, FULL).unwrap();
        let config = GlobalConfig::load(&path, Some(Path::new("/home/me"))).unwrap();
        assert_eq!(
            config.repo[1].path,
            PathBuf::from("/home/me/code/nanna-coder")
        );

        for home in [None, Some(Path::new("")), Some(Path::new("relative"))] {
            let err = GlobalConfig::load(&path, home).unwrap_err().to_string();
            assert!(err.contains("HOME"), "{home:?}: {err}");
        }

        let no_tilde = "nanna_bin = \"/bin/nanna\"\n[[repo]]\nslug = \"a/b\"\npath = \"/x\"\n";
        std::fs::write(&path, no_tilde).unwrap();
        assert!(GlobalConfig::load(&path, None).is_ok());
    }

    #[test]
    fn load_does_not_need_home_for_the_state_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let text = "nanna_bin = \"/bin/nanna\"\nstate_db = \"~/s.db\"\n[[repo]]\nslug = \"a/b\"\npath = \"/x\"\n";
        std::fs::write(&path, text).unwrap();
        let config = GlobalConfig::load(&path, None).unwrap();
        assert_eq!(config.state_db, Some(PathBuf::from("~/s.db")));
        assert!(config.state_db_path(None).is_err());
        assert_eq!(
            config.state_db_path(Some(Path::new("/home/me"))).unwrap(),
            PathBuf::from("/home/me/s.db")
        );
        let absolute = GlobalConfig {
            state_db: Some("/var/s.db".into()),
            ..config
        };
        assert_eq!(
            absolute.state_db_path(None).unwrap(),
            PathBuf::from("/var/s.db")
        );
    }

    #[test]
    fn load_revalidates_after_expansion() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let text = "nanna_bin = \"nanna\"\n[[repo]]\nslug = \"a/b\"\npath = \"~/x\"\n[[repo]]\nslug = \"a/c\"\npath = \"/home/me/x\"\n";
        std::fs::write(&path, text).unwrap();
        let err = GlobalConfig::load(&path, Some(Path::new("/home/me"))).unwrap_err();
        assert!(err.to_string().contains("reuses the path"), "{err}");
    }

    #[test]
    fn load_reports_unreadable_oversized_and_malformed_files_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let missing = GlobalConfig::load(&dir.path().join("nope.toml"), None).unwrap_err();
        assert!(matches!(missing, GlobalConfigError::Io { .. }));
        let shown = missing.to_string();
        assert_eq!(shown.matches("os error").count(), 1, "{shown}");

        std::fs::write(&path, "garbage = [").unwrap();
        assert!(matches!(
            GlobalConfig::load(&path, None),
            Err(GlobalConfigError::Parse(_))
        ));

        std::fs::write(&path, vec![b' '; MAX_CONFIG_BYTES as usize + 1]).unwrap();
        let big = GlobalConfig::load(&path, None).unwrap_err().to_string();
        assert!(big.contains("larger than"), "{big}");
    }

    #[test]
    fn default_path_prefers_an_absolute_xdg_dir_and_needs_one_of_them() {
        let home = Some(Path::new("/home/me"));
        assert_eq!(
            GlobalConfig::default_path(Some(Path::new("/xdg")), home),
            Some(PathBuf::from("/xdg/inchworm/config.toml"))
        );
        assert_eq!(
            GlobalConfig::default_path(Some(Path::new("/xdg")), None),
            Some(PathBuf::from("/xdg/inchworm/config.toml"))
        );
        assert_eq!(
            GlobalConfig::default_path(Some(Path::new("relative")), home),
            Some(PathBuf::from("/home/me/.config/inchworm/config.toml"))
        );
        assert_eq!(
            GlobalConfig::default_path(None, home),
            Some(PathBuf::from("/home/me/.config/inchworm/config.toml"))
        );
        assert_eq!(GlobalConfig::default_path(None, None), None);
        assert_eq!(GlobalConfig::default_path(None, Some(Path::new(""))), None);
        assert_eq!(GlobalConfig::default_path(Some(Path::new("")), None), None);
    }

    proptest! {
        #[test]
        fn valid_slugs_are_accepted(owner in "[A-Za-z0-9]([A-Za-z0-9-]{0,10}[A-Za-z0-9])?", name in "[A-Za-z0-9_.-]{1,12}") {
            prop_assume!(name != "." && name != "..");
            let slug = format!("{owner}/{name}");
            prop_assert!(slug_is_valid(&slug), "{}", slug);
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
        fn any_single_known_violation_makes_the_config_invalid(index in 0usize..1000) {
            let table = violations();
            let (name, mutate, _) = table[index % table.len()];
            let mut config = base();
            mutate(&mut config);
            prop_assert!(config.validate().is_err(), "{}", name);
        }

        #[test]
        fn arbitrary_text_never_panics(text in ".*") {
            let _ = GlobalConfig::parse(&text);
        }
    }
}
