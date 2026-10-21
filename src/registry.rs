//! The user-level registry of directories the daemon watches.

use std::io;
use std::path::{Path, PathBuf};

use crate::config::CONFIG_FILE;

/// A plain-text list of absolute directory paths, one per line.
pub struct Registry {
    file: PathBuf,
}

impl Registry {
    pub fn new(file: PathBuf) -> Self {
        Self { file }
    }

    /// `$INCHWORM_HOME/registry`, else `$XDG_STATE_HOME/inchworm/registry`,
    /// else `~/.local/state/inchworm/registry`.
    pub fn default_location() -> Option<Self> {
        let dir = if let Some(h) = std::env::var_os("INCHWORM_HOME") {
            PathBuf::from(h)
        } else if let Some(x) = std::env::var_os("XDG_STATE_HOME") {
            PathBuf::from(x).join("inchworm")
        } else {
            PathBuf::from(std::env::var_os("HOME")?).join(".local/state/inchworm")
        };
        Some(Self::new(dir.join("registry")))
    }

    pub fn list(&self) -> io::Result<Vec<PathBuf>> {
        match std::fs::read_to_string(&self.file) {
            Ok(s) => Ok(s
                .lines()
                .filter(|l| !l.is_empty())
                .map(PathBuf::from)
                .collect()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(e),
        }
    }

    fn save(&self, dirs: &[PathBuf]) -> io::Result<()> {
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body: String = dirs.iter().map(|d| format!("{}\n", d.display())).collect();
        std::fs::write(&self.file, body)
    }

    /// Register a directory containing an `inchworm.toml`. Returns false if it
    /// was already registered.
    pub fn register(&self, dir: &Path) -> io::Result<bool> {
        let dir = dir.canonicalize()?;
        if !dir.join(CONFIG_FILE).is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no {CONFIG_FILE} in {}", dir.display()),
            ));
        }
        let mut dirs = self.list()?;
        if dirs.contains(&dir) {
            return Ok(false);
        }
        dirs.push(dir);
        self.save(&dirs)?;
        Ok(true)
    }

    /// Returns false if the directory was not registered.
    pub fn unregister(&self, dir: &Path) -> io::Result<bool> {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let mut dirs = self.list()?;
        let before = dirs.len();
        dirs.retain(|d| d != &dir);
        if dirs.len() == before {
            return Ok(false);
        }
        self.save(&dirs)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_list_unregister() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir(&proj).unwrap();
        let reg = Registry::new(tmp.path().join("state/registry"));

        assert!(reg.register(&proj).is_err(), "needs inchworm.toml");
        std::fs::write(proj.join(CONFIG_FILE), "").unwrap();
        assert!(reg.register(&proj).unwrap());
        assert!(!reg.register(&proj).unwrap(), "idempotent");
        assert_eq!(reg.list().unwrap(), [proj.canonicalize().unwrap()]);
        assert!(reg.unregister(&proj).unwrap());
        assert!(reg.list().unwrap().is_empty());
    }
}
