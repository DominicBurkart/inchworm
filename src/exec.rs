use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 32 * 1024 * 1024;

const POLL: Duration = Duration::from_millis(20);

const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(250);

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
    pub timeout: Option<Duration>,
    pub max_output: Option<usize>,
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
            timeout: None,
            max_output: None,
        }
    }

    /// ```
    /// use std::time::Duration;
    /// use inchworm::exec::CmdSpec;
    ///
    /// let spec = CmdSpec::new("nix", ["build"]).with_timeout(Duration::from_secs(3600)).with_max_output(1024);
    /// assert_eq!(spec.timeout, Some(Duration::from_secs(3600)));
    /// assert_eq!(spec.max_output, Some(1024));
    /// ```
    pub fn with_timeout(mut self, timeout: Duration) -> CmdSpec {
        self.timeout = Some(timeout);
        self
    }

    pub fn with_max_output(mut self, bytes: usize) -> CmdSpec {
        self.max_output = Some(bytes);
        self
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

fn pump(mut source: impl Read + Send + 'static, is_stderr: bool, tx: Sender<(bool, Vec<u8>)>) {
    std::thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            match source.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if tx.send((is_stderr, buffer[..read].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

const BUSY_RETRIES: u32 = 40;
const BUSY_PAUSE: Duration = Duration::from_millis(25);

/// ```
/// use std::process::Command;
/// use inchworm::exec::spawn_retrying;
///
/// let mut child = spawn_retrying(&mut Command::new("true")).unwrap();
/// assert!(child.wait().unwrap().success());
/// assert!(spawn_retrying(&mut Command::new("/no/such/program")).is_err());
/// ```
pub fn spawn_retrying(command: &mut Command) -> io::Result<Child> {
    let mut attempts = 0;
    loop {
        match command.spawn() {
            Err(err) if err.raw_os_error() == Some(ETXTBSY) && attempts < BUSY_RETRIES => {
                attempts += 1;
                thread::sleep(BUSY_PAUSE);
            }
            other => return other,
        }
    }
}

const ETXTBSY: i32 = 26;

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// ```
/// use std::time::Duration;
/// use inchworm::exec::{CmdSpec, Runner, SystemRunner};
///
/// let out = SystemRunner.run(&CmdSpec::new("echo", ["hi"])).unwrap();
/// assert!(out.success());
/// assert_eq!(out.stdout.trim(), "hi");
///
/// let slow = CmdSpec::new("sleep", ["30"]).with_timeout(Duration::from_millis(100));
/// let err = SystemRunner.run(&slow).unwrap_err();
/// assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
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
        let mut child = spawn_retrying(&mut command)?;
        let (tx, rx) = mpsc::channel();
        if let Some(stdout) = child.stdout.take() {
            pump(stdout, false, tx.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            pump(stderr, true, tx.clone());
        }
        drop(tx);

        let timeout = spec.timeout.unwrap_or(DEFAULT_TIMEOUT);
        let max_output = spec.max_output.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);
        let deadline = Instant::now() + timeout;
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut status = None;
        let mut drain_until: Option<Instant> = None;
        loop {
            match rx.recv_timeout(POLL) {
                Ok((is_stderr, chunk)) => {
                    let sink = if is_stderr { &mut stderr } else { &mut stdout };
                    sink.extend_from_slice(&chunk);
                    if stdout.len() + stderr.len() > max_output {
                        kill(&mut child);
                        return Err(io::Error::other(format!(
                            "`{}` printed more than {max_output} bytes",
                            spec.display()
                        )));
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    if status.is_some() {
                        break;
                    }
                    if let Some(done) = child.try_wait()? {
                        status = Some(done);
                        break;
                    }
                    if Instant::now() >= deadline {
                        kill(&mut child);
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("`{}` timed out after {timeout:?}", spec.display()),
                        ));
                    }
                    thread::sleep(POLL);
                    continue;
                }
            }
            if status.is_some() {
                if drain_until.is_some_and(|until| Instant::now() >= until) {
                    break;
                }
            } else if let Some(done) = child.try_wait()? {
                status = Some(done);
                drain_until = Some(Instant::now() + DRAIN_AFTER_EXIT);
            } else if Instant::now() >= deadline {
                kill(&mut child);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("`{}` timed out after {timeout:?}", spec.display()),
                ));
            }
        }
        Ok(CmdOutput {
            code: status.and_then(|done| done.code()),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
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

    #[cfg(unix)]
    fn pid_alive(pid: &str) -> bool {
        Command::new("kill")
            .args(["-0", pid])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(unix)]
    #[test]
    fn a_command_that_outlives_its_deadline_is_killed_and_reported_as_timed_out() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let script = format!("echo $$ > {}; exec sleep 60", pidfile.display());
        let spec =
            CmdSpec::new("sh", ["-c", script.as_str()]).with_timeout(Duration::from_millis(300));
        let started = Instant::now();
        let err = SystemRunner.run(&spec).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
        let pid = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .to_string();
        assert!(!pid_alive(&pid), "the timed-out process is still running");
    }

    #[cfg(unix)]
    #[test]
    fn output_beyond_the_cap_kills_the_command_and_is_an_error() {
        let spec = CmdSpec::new("sh", ["-c", "head -c 5000000 /dev/zero; sleep 60"])
            .with_max_output(10_000)
            .with_timeout(Duration::from_secs(30));
        let started = Instant::now();
        let err = SystemRunner.run(&spec).unwrap_err();
        assert!(err.to_string().contains("more than 10000 bytes"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_cap_counts_stdout_and_stderr_together() {
        let spec = CmdSpec::new(
            "sh",
            ["-c", "head -c 600 /dev/zero; head -c 600 /dev/zero >&2"],
        )
        .with_max_output(1000);
        let err = SystemRunner.run(&spec).unwrap_err();
        assert!(err.to_string().contains("more than 1000 bytes"), "{err}");
        let spec = CmdSpec::new(
            "sh",
            ["-c", "head -c 400 /dev/zero; head -c 400 /dev/zero >&2"],
        )
        .with_max_output(1000);
        let out = SystemRunner.run(&spec).unwrap();
        assert_eq!(out.stdout.len(), 400);
        assert_eq!(out.stderr.len(), 400);
    }

    #[cfg(unix)]
    #[test]
    fn a_background_process_holding_the_pipes_open_does_not_hang_the_call() {
        let spec = CmdSpec::new("sh", ["-c", "sleep 30 & echo done"])
            .with_timeout(Duration::from_secs(20));
        let started = Instant::now();
        let out = SystemRunner.run(&spec).unwrap();
        assert_eq!(out.stdout.trim(), "done");
        assert!(out.success());
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_command_that_closes_its_pipes_and_keeps_running_still_hits_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let script = format!(
            "echo $$ > {}; exec >&- 2>&-; exec sleep 60",
            pidfile.display()
        );
        let spec =
            CmdSpec::new("sh", ["-c", script.as_str()]).with_timeout(Duration::from_millis(300));
        let started = Instant::now();
        let err = SystemRunner.run(&spec).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
        let pid = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .to_string();
        assert!(!pid_alive(&pid), "the timed-out process is still running");
    }

    #[cfg(unix)]
    #[test]
    fn the_cap_kills_the_process_and_stdin_is_not_left_open() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let script = format!(
            "echo $$ > {}; head -c 5000000 /dev/zero; exec sleep 60",
            pidfile.display()
        );
        let spec = CmdSpec::new("sh", ["-c", script.as_str()])
            .with_max_output(10_000)
            .with_timeout(Duration::from_secs(30));
        let err = SystemRunner.run(&spec).unwrap_err();
        assert!(err.to_string().contains("more than 10000 bytes"), "{err}");
        let pid = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .to_string();
        assert!(!pid_alive(&pid), "the capped process is still running");

        let started = Instant::now();
        let out = SystemRunner
            .run(&CmdSpec::new("cat", [] as [&str; 0]).with_timeout(Duration::from_secs(30)))
            .unwrap();
        assert!(out.success() && out.stdout.is_empty());
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "cat waited for stdin"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_script_still_open_for_writing_is_retried_until_it_can_run() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        let mut writer = std::fs::File::create(&path).unwrap();
        writer.write_all(b"#!/bin/sh\necho ran\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let busy = Command::new(&path).spawn().unwrap_err();
        assert_eq!(busy.raw_os_error(), Some(ETXTBSY), "{busy}");
        let closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(writer);
        });
        let out = SystemRunner
            .run(&CmdSpec::new(path.to_str().unwrap(), [] as [&str; 0]))
            .unwrap();
        closer.join().unwrap();
        assert_eq!(out.stdout.trim(), "ran");
    }

    #[cfg(unix)]
    #[test]
    fn a_script_that_stays_busy_gives_up_after_a_bounded_number_of_retries() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        let mut writer = std::fs::File::create(&path).unwrap();
        writer.write_all(b"#!/bin/sh\ntrue\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = Instant::now();
        let err = spawn_retrying(&mut Command::new(&path)).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(ETXTBSY), "{err}");
        assert!(
            started.elapsed() >= Duration::from_millis(800),
            "{:?}",
            started.elapsed()
        );
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
        drop(writer);
    }

    #[cfg(unix)]
    #[test]
    fn output_exactly_at_the_cap_is_allowed_and_one_byte_more_is_not() {
        let at = CmdSpec::new("sh", ["-c", "head -c 1000 /dev/zero"]).with_max_output(1000);
        assert_eq!(SystemRunner.run(&at).unwrap().stdout.len(), 1000);
        let over = CmdSpec::new("sh", ["-c", "head -c 1001 /dev/zero"]).with_max_output(1000);
        assert!(SystemRunner.run(&over).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn output_written_just_after_exit_is_still_captured() {
        let spec = CmdSpec::new("sh", ["-c", "(sleep 0.1; echo late) & exit 0"]);
        let out = SystemRunner.run(&spec).unwrap();
        assert_eq!(out.stdout.trim(), "late");
    }

    #[cfg(unix)]
    #[test]
    fn large_output_under_the_cap_is_captured_whole_from_both_streams() {
        let spec = CmdSpec::new(
            "sh",
            [
                "-c",
                "head -c 300000 /dev/zero | tr '\\0' a; head -c 200000 /dev/zero | tr '\\0' b >&2",
            ],
        );
        let out = SystemRunner.run(&spec).unwrap();
        assert_eq!(out.stdout.len(), 300_000);
        assert!(out.stdout.bytes().all(|b| b == b'a'));
        assert_eq!(out.stderr.len(), 200_000);
        assert!(out.stderr.bytes().all(|b| b == b'b'));
    }

    #[cfg(unix)]
    #[test]
    fn a_signalled_command_has_no_exit_code() {
        let out = SystemRunner
            .run(&CmdSpec::new("sh", ["-c", "kill -9 $$"]))
            .unwrap();
        assert_eq!(out.code, None);
        assert!(!out.success());
    }

    #[test]
    fn system_runner_reports_a_missing_program() {
        let spec = CmdSpec::new("/definitely/not/a/program", [] as [&str; 0]);
        let err = SystemRunner.run(&spec).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
    }
}
