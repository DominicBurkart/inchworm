//! Startup check that every nanna identity a journey's sequence names exists
//! in the directory's resolved identity catalog.
//!
//! Only identity *names* are discovered here (`[identity] name = "..."`);
//! validating the files is nanna's job.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::Config;

/// Default identity names each journey's sequence runs under.
pub const REQUIRED_IDENTITIES: &[(&str, &[&str])] = &[
    (
        "issues",
        &[
            "inchworm-issue-researcher",
            "inchworm-issue-planner",
            "inchworm-issue-executor",
            "inchworm-auditor",
        ],
    ),
    ("ci", &["inchworm-ci-root-cause", "inchworm-auditor"]),
    (
        "conflicts",
        &["inchworm-conflict-resolver", "inchworm-auditor"],
    ),
];

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("reading {0}: {1}")]
    Io(String, std::io::Error),
    #[error("parsing {0}: {1}")]
    Parse(String, toml::de::Error),
    #[error(
        "repo identity {name:?} in {file} has no global identity to narrow; \
         repo files may only override a global identity"
    )]
    NoGlobalBase { name: String, file: String },
}

/// The names available to a directory, plus where they were looked for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IdentityCatalog {
    pub names: BTreeSet<String>,
    /// Directories that were searched, global first.
    pub searched: Vec<PathBuf>,
}

/// A required identity that the catalog does not contain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingIdentity {
    pub journey: String,
    pub identity: String,
    pub searched: Vec<PathBuf>,
}

impl fmt::Display for MissingIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "journey {:?} needs nanna identity {:?}, which was not found",
            self.journey, self.identity
        )?;
        if self.searched.is_empty() {
            write!(f, " (no identity directory could be resolved)")
        } else {
            let dirs: Vec<_> = self
                .searched
                .iter()
                .map(|d| d.display().to_string())
                .collect();
            write!(f, " (looked in {})", dirs.join(", "))
        }
    }
}

#[derive(Deserialize)]
struct IdentityFile {
    identity: IdentityHeader,
}

#[derive(Deserialize)]
struct IdentityHeader {
    name: String,
}

/// `$NANNA_CONFIG_DIR/agents`, else `$XDG_CONFIG_HOME/nanna/agents`, else
/// `$HOME/.config/nanna/agents`. Empty variables count as unset.
pub fn global_dir_from(lookup: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let non_empty = |k: &str| lookup(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = non_empty("NANNA_CONFIG_DIR")
        .or_else(|| non_empty("XDG_CONFIG_HOME").map(|x| x.join("nanna")))
        .or_else(|| non_empty("HOME").map(|h| h.join(".config/nanna")))?;
    Some(base.join("agents"))
}

pub fn global_dir() -> Option<PathBuf> {
    global_dir_from(&|k| std::env::var_os(k))
}

/// `<repo>/.nanna/agents`.
pub fn repo_dir(repo: &Path) -> PathBuf {
    repo.join(".nanna/agents")
}

/// Names declared by the `*.toml` files directly under `dir`, with the file
/// each came from. A missing directory yields no names.
pub fn discover(dir: &Path) -> Result<Vec<(String, PathBuf)>, IdentityError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(IdentityError::Io(dir.display().to_string(), e)),
    };
    let mut found = vec![];
    for entry in entries {
        let path = entry
            .map_err(|e| IdentityError::Io(dir.display().to_string(), e))?
            .path();
        if path.extension().is_none_or(|e| e != "toml") || !path.is_file() {
            continue;
        }
        let name = path.display().to_string();
        let src = std::fs::read_to_string(&path).map_err(|e| IdentityError::Io(name.clone(), e))?;
        let file: IdentityFile = toml::from_str(&src).map_err(|e| IdentityError::Parse(name, e))?;
        found.push((file.identity.name, path));
    }
    found.sort();
    Ok(found)
}

/// Resolve the catalog: global identities, with repo-local files allowed only
/// as overrides of a global name (they add no names of their own).
pub fn resolve(
    global: Option<&Path>,
    repo: Option<&Path>,
) -> Result<IdentityCatalog, IdentityError> {
    let mut catalog = IdentityCatalog::default();
    if let Some(dir) = global {
        catalog.names = discover(dir)?.into_iter().map(|(n, _)| n).collect();
        catalog.searched.push(dir.to_path_buf());
    }
    if let Some(dir) = repo {
        for (name, file) in discover(dir)? {
            if !catalog.names.contains(&name) {
                return Err(IdentityError::NoGlobalBase {
                    name,
                    file: file.display().to_string(),
                });
            }
        }
        catalog.searched.push(dir.to_path_buf());
    }
    Ok(catalog)
}

/// Resolve the catalog for a registered directory from the environment.
pub fn resolve_for(dir: &Path) -> Result<IdentityCatalog, IdentityError> {
    resolve(global_dir().as_deref(), Some(&repo_dir(dir)))
}

/// `(journey, identity)` pairs required by the journeys `config` leaves enabled.
pub fn required(config: &Config) -> Vec<(&'static str, &'static str)> {
    REQUIRED_IDENTITIES
        .iter()
        .filter(|(journey, _)| match *journey {
            "issues" => config.issues.enabled,
            "ci" => config.ci.enabled,
            "conflicts" => config.conflicts.enabled,
            _ => true,
        })
        .flat_map(|(journey, names)| names.iter().map(move |n| (*journey, *n)))
        .collect()
}

/// Every missing `(journey, identity)` pair, or `Ok` if all are available.
pub fn check_sequences(
    available: &IdentityCatalog,
    required: &[(&str, &str)],
) -> Result<(), Vec<MissingIdentity>> {
    let missing: Vec<_> = required
        .iter()
        .filter(|(_, identity)| !available.names.contains(*identity))
        .map(|(journey, identity)| MissingIdentity {
            journey: (*journey).into(),
            identity: (*identity).into(),
            searched: available.searched.clone(),
        })
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, file: &str, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(file),
            format!("[identity]\nname = \"{name}\"\n[other]\nignored = 1\n"),
        )
        .unwrap();
    }

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| (*v).into())
        }
    }

    #[test]
    fn global_dir_precedence() {
        let all = [
            ("NANNA_CONFIG_DIR", "/n"),
            ("XDG_CONFIG_HOME", "/x"),
            ("HOME", "/h"),
        ];
        assert_eq!(global_dir_from(&env(&all)), Some("/n/agents".into()));
        assert_eq!(
            global_dir_from(&env(&all[1..])),
            Some("/x/nanna/agents".into())
        );
        assert_eq!(
            global_dir_from(&env(&all[2..])),
            Some("/h/.config/nanna/agents".into())
        );
        let empty = [("NANNA_CONFIG_DIR", ""), ("HOME", "/h")];
        assert_eq!(
            global_dir_from(&env(&empty)),
            Some("/h/.config/nanna/agents".into())
        );
        assert_eq!(global_dir_from(&env(&[])), None);
    }

    #[test]
    fn discovers_names_and_skips_non_toml() {
        let t = tempfile::tempdir().unwrap();
        write(t.path(), "b.toml", "beta");
        write(t.path(), "a.toml", "alpha");
        write(t.path(), "notes.md", "nope");
        let names: Vec<_> = discover(t.path())
            .unwrap()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, ["alpha", "beta"]);
        assert!(discover(&t.path().join("missing")).unwrap().is_empty());
    }

    #[test]
    fn malformed_file_is_an_error() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("x.toml"), "[nope]\n").unwrap();
        assert!(matches!(discover(t.path()), Err(IdentityError::Parse(..))));
    }

    #[test]
    fn repo_may_override_but_not_add() {
        let g = tempfile::tempdir().unwrap();
        let r = tempfile::tempdir().unwrap();
        write(g.path(), "a.toml", "alpha");
        write(r.path(), "a.toml", "alpha");
        let c = resolve(Some(g.path()), Some(r.path())).unwrap();
        assert_eq!(c.names.len(), 1);
        assert_eq!(c.searched.len(), 2);

        write(r.path(), "z.toml", "zeta");
        assert!(matches!(
            resolve(Some(g.path()), Some(r.path())),
            Err(IdentityError::NoGlobalBase { name, .. }) if name == "zeta"
        ));
    }

    #[test]
    fn required_skips_disabled_journeys() {
        let all = required(&Config::default());
        assert!(all.contains(&("ci", "inchworm-ci-root-cause")));
        let mut config = Config::default();
        config.ci.enabled = false;
        config.conflicts.enabled = false;
        let some = required(&config);
        assert!(some.iter().all(|(j, _)| *j == "issues"));
        assert!(!some.is_empty());
    }

    #[test]
    fn check_reports_each_missing_with_locations() {
        let g = tempfile::tempdir().unwrap();
        write(g.path(), "a.toml", "inchworm-auditor");
        let catalog = resolve(Some(g.path()), None).unwrap();
        let req = [("ci", "inchworm-auditor"), ("ci", "inchworm-ci-root-cause")];
        let err = check_sequences(&catalog, &req).unwrap_err();
        assert_eq!(err.len(), 1);
        let msg = err[0].to_string();
        assert!(msg.contains("\"ci\"") && msg.contains("inchworm-ci-root-cause"));
        assert!(msg.contains(&g.path().display().to_string()));
        assert!(check_sequences(&catalog, &req[..1]).is_ok());
    }

    #[test]
    fn missing_with_no_dirs_says_so() {
        let err = check_sequences(&IdentityCatalog::default(), &[("ci", "x")]).unwrap_err();
        assert!(err[0].to_string().contains("no identity directory"));
    }
}
