use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
}

impl CmdSpec {
    /// ```
    /// use inchworm::exec::CmdSpec;
    ///
    /// let spec = CmdSpec::new("gh", ["issue", "list"]).in_dir("/tmp");
    /// assert_eq!(spec.display(), "gh issue list");
    /// ```
    pub fn new<I, S>(program: impl AsRef<OsStr>, args: I) -> CmdSpec
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        CmdSpec {
            program: program.as_ref().to_os_string(),
            args: args
                .into_iter()
                .map(|a| a.as_ref().to_os_string())
                .collect(),
            cwd: None,
        }
    }

    pub fn in_dir(mut self, dir: impl AsRef<Path>) -> CmdSpec {
        self.cwd = Some(dir.as_ref().to_path_buf());
        self
    }

    pub fn display(&self) -> String {
        let mut parts = vec![self.program.to_string_lossy().into_owned()];
        parts.extend(self.args.iter().map(|a| a.to_string_lossy().into_owned()));
        parts.join(" ")
    }
}

pub trait Runner: Send + Sync {
    fn run(&self, spec: &CmdSpec) -> io::Result<CmdOutput>;
}

/// ```
/// use inchworm::exec::{CmdSpec, Runner, SystemRunner};
///
/// let out = SystemRunner.run(&CmdSpec::new("echo", ["hi"])).unwrap();
/// assert!(out.success());
/// assert_eq!(out.stdout.trim(), "hi");
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, spec: &CmdSpec) -> io::Result<CmdOutput> {
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        let output = command.output()?;
        Ok(CmdOutput {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::sync::Mutex;

    pub struct ScriptedRunner {
        replies: Mutex<Vec<io::Result<CmdOutput>>>,
        pub calls: Mutex<Vec<CmdSpec>>,
    }

    impl ScriptedRunner {
        pub fn new(replies: Vec<io::Result<CmdOutput>>) -> ScriptedRunner {
            let mut replies = replies;
            replies.reverse();
            ScriptedRunner {
                replies: Mutex::new(replies),
                calls: Mutex::new(Vec::new()),
            }
        }

        pub fn ok(stdout: &str) -> io::Result<CmdOutput> {
            Ok(CmdOutput {
                code: Some(0),
                stdout: stdout.to_string(),
                stderr: String::new(),
            })
        }

        pub fn fail(stderr: &str) -> io::Result<CmdOutput> {
            Ok(CmdOutput {
                code: Some(1),
                stdout: String::new(),
                stderr: stderr.to_string(),
            })
        }

        pub fn calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(CmdSpec::display)
                .collect()
        }
    }

    impl Runner for ScriptedRunner {
        fn run(&self, spec: &CmdSpec) -> io::Result<CmdOutput> {
            self.calls.lock().unwrap().push(spec.clone());
            self.replies
                .lock()
                .unwrap()
                .pop()
                .unwrap_or_else(|| Err(io::Error::other("no scripted reply left")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_runner_captures_stdout_stderr_and_exit_code() {
        let spec = CmdSpec::new("sh", ["-c", "echo out; echo err >&2; exit 3"]);
        let out = SystemRunner.run(&spec).unwrap();
        assert_eq!(out.code, Some(3));
        assert!(!out.success());
        assert_eq!(out.stdout.trim(), "out");
        assert_eq!(out.stderr.trim(), "err");
    }

    #[test]
    fn system_runner_honours_the_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let spec = CmdSpec::new("pwd", [] as [&str; 0]).in_dir(dir.path());
        let out = SystemRunner.run(&spec).unwrap();
        let reported = std::fs::canonicalize(out.stdout.trim()).unwrap();
        assert_eq!(reported, std::fs::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn system_runner_reports_a_missing_program() {
        let spec = CmdSpec::new("/definitely/not/a/program", [] as [&str; 0]);
        assert!(SystemRunner.run(&spec).is_err());
    }
}
