use std::ffi::{OsStr, OsString};
use std::io;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
/// let out = SystemRunner::default().run(&CmdSpec::new("echo", ["hi"])).unwrap();
/// assert!(out.success());
/// assert_eq!(out.stdout.trim(), "hi");
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRunner {
    pub timeout: Option<Duration>,
}

impl SystemRunner {
    pub fn with_timeout(timeout: Duration) -> SystemRunner {
        SystemRunner {
            timeout: Some(timeout),
        }
    }
}

fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = pipe.read_to_end(&mut buffer);
        buffer
    })
}

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
        let Some(limit) = self.timeout else {
            let output = command.output()?;
            return Ok(CmdOutput {
                code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        };
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().map(drain);
        let stderr = child.stderr.take().map(drain);
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if started.elapsed() >= limit {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("timed out after {}s", limit.as_secs_f64()),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let collect = |handle: Option<std::thread::JoinHandle<Vec<u8>>>| {
            handle
                .and_then(|handle| handle.join().ok())
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default()
        };
        Ok(CmdOutput {
            code: status.code(),
            stdout: collect(stdout),
            stderr: collect(stderr),
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
        let out = SystemRunner::default().run(&spec).unwrap();
        assert_eq!(out.code, Some(3));
        assert!(!out.success());
        assert_eq!(out.stdout.trim(), "out");
        assert_eq!(out.stderr.trim(), "err");
    }

    #[test]
    fn system_runner_honours_the_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let spec = CmdSpec::new("pwd", [] as [&str; 0]).in_dir(dir.path());
        let out = SystemRunner::default().run(&spec).unwrap();
        let reported = std::fs::canonicalize(out.stdout.trim()).unwrap();
        assert_eq!(reported, std::fs::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn a_timeout_kills_a_hung_command_and_reports_timed_out() {
        let runner = SystemRunner::with_timeout(Duration::from_millis(200));
        let started = Instant::now();
        let err = runner
            .run(&CmdSpec::new("sh", ["-c", "sleep 30"]))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_timeout_does_not_wait_for_a_grandchild_holding_the_pipes() {
        let runner = SystemRunner::with_timeout(Duration::from_millis(200));
        let started = Instant::now();
        let err = runner
            .run(&CmdSpec::new("sh", ["-c", "sleep 5 & wait"]))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn a_command_within_the_timeout_returns_output_and_exit_code() {
        let runner = SystemRunner::with_timeout(Duration::from_secs(10));
        let out = runner
            .run(&CmdSpec::new(
                "sh",
                ["-c", "echo out; echo err >&2; exit 3"],
            ))
            .unwrap();
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stdout.trim(), "out");
        assert_eq!(out.stderr.trim(), "err");
    }

    #[test]
    fn a_missing_program_is_an_error_under_a_timeout_too() {
        let runner = SystemRunner::with_timeout(Duration::from_secs(1));
        assert!(
            runner
                .run(&CmdSpec::new("/definitely/not/a/program", [] as [&str; 0]))
                .is_err()
        );
    }

    #[test]
    fn system_runner_reports_a_missing_program() {
        let spec = CmdSpec::new("/definitely/not/a/program", [] as [&str; 0]);
        assert!(SystemRunner::default().run(&spec).is_err());
    }
}
