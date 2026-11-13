use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::config::GlobalConfig;
use crate::validate::validate;

pub const EXIT_VALID: u8 = 0;
pub const EXIT_REPO_ERRORS: u8 = 1;
pub const EXIT_UNUSABLE: u8 = 2;

/// The parts of the process environment the commands may read.
///
/// ```
/// use inchworm::cli::Env;
///
/// let env = Env::from_vars(Some("".into()), None, Some("/bin".into()));
/// assert!(env.home.is_none());
/// assert!(env.path.is_some());
/// ```
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub home: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    pub path: Option<OsString>,
}

impl Env {
    pub fn from_vars(
        home: Option<OsString>,
        xdg_config_home: Option<OsString>,
        path: Option<OsString>,
    ) -> Env {
        let present = |value: Option<OsString>| value.filter(|v| !v.is_empty());
        Env {
            home: present(home).map(PathBuf::from),
            xdg_config_home: present(xdg_config_home).map(PathBuf::from),
            path: present(path),
        }
    }

    pub fn from_process() -> Env {
        Env::from_vars(
            std::env::var_os("HOME"),
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("PATH"),
        )
    }
}

#[derive(Debug, Parser)]
#[command(name = "inchworm", version, about = "local coding agent orchestrator")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Check the global config and every registered repository (offline).
    Validate {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// List the registered directories (slug, then path, tab separated).
    List {
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

/// Runs the CLI and returns the process exit code: 0 when everything is
/// valid, 1 when a repository has errors, 2 when the command could not run
/// (bad usage, or the config could not be found, read or parsed).
///
/// ```
/// use inchworm::cli::{run, Env};
///
/// let (mut out, mut err) = (Vec::new(), Vec::new());
/// let code = run(["inchworm", "--version"], &Env::default(), &mut out, &mut err);
/// assert_eq!(code, 0);
/// assert!(String::from_utf8(out).unwrap().starts_with("inchworm "));
/// ```
pub fn run<I, T>(args: I, env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let code = u8::try_from(error.exit_code()).unwrap_or(EXIT_UNUSABLE);
            let rendered = error.render().to_string();
            let _ = if error.use_stderr() {
                write!(err, "{rendered}")
            } else {
                write!(out, "{rendered}")
            };
            return code;
        }
    };
    match cli.command {
        Command::Validate { config } => validate_command(config, env, out, err),
        Command::List { config } => list_command(config, env, out, err),
    }
}

fn load_config(
    config: Option<PathBuf>,
    env: &Env,
    err: &mut dyn Write,
) -> Result<GlobalConfig, u8> {
    let path = config.or_else(|| {
        GlobalConfig::default_path(env.xdg_config_home.as_deref(), env.home.as_deref())
    });
    let Some(path) = path else {
        let _ = writeln!(
            err,
            "error: cannot find the config: pass --config, or set XDG_CONFIG_HOME or HOME"
        );
        return Err(EXIT_UNUSABLE);
    };
    GlobalConfig::load(&path, env.home.as_deref()).map_err(|error| {
        let _ = writeln!(err, "error: {error}");
        EXIT_UNUSABLE
    })
}

fn list_command(
    config: Option<PathBuf>,
    env: &Env,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let config = match load_config(config, env, err) {
        Ok(config) => config,
        Err(code) => return code,
    };
    let bad: Vec<&str> = config
        .repo
        .iter()
        .filter(|repo| {
            repo.slug.chars().any(char::is_control)
                || repo.path.to_string_lossy().chars().any(char::is_control)
        })
        .map(|repo| repo.slug.as_str())
        .collect();
    if !bad.is_empty() {
        let _ = writeln!(
            err,
            "error: cannot list {:?}: a slug or path contains a control character",
            bad
        );
        return EXIT_REPO_ERRORS;
    }
    for repo in &config.repo {
        if let Err(error) = writeln!(out, "{}\t{}", repo.slug, repo.path.display()) {
            let _ = writeln!(err, "error: cannot write the list: {error}");
            return EXIT_UNUSABLE;
        }
    }
    EXIT_VALID
}

fn validate_command(
    config: Option<PathBuf>,
    env: &Env,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let config = match load_config(config, env, err) {
        Ok(config) => config,
        Err(code) => return code,
    };
    let report = validate(&config, env.path.as_deref());
    let _ = write!(out, "{}", report.render());
    if report.is_ok() {
        EXIT_VALID
    } else {
        EXIT_REPO_ERRORS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn exec(args: &[&str], env: &Env) -> (u8, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(args.iter().copied(), env, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn quoted(path: &Path) -> String {
        toml::Value::String(path.to_string_lossy().into_owned()).to_string()
    }

    struct Setup {
        dir: tempfile::TempDir,
        config: PathBuf,
        repo: PathBuf,
    }

    fn setup() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join(".inchworm")).unwrap();
        std::fs::create_dir_all(repo.join(".nanna")).unwrap();
        std::fs::write(
            repo.join("inchworm.toml"),
            "[auditor]\nruns_in = { kind = \"nanna\", identity = \"auditor\" }\n[[flow]]\nname = \"issue\"\ntrigger = { kind = \"label\", label = \"inchworm\" }\n[[flow.step]]\nname = \"implement\"\nidentity = \"rust-implementer\"\n[[flow.judge]]\nkind = \"nanna_clean\"\n[flow.outcome]\nmode = \"draft_pr\"\n",
        )
        .unwrap();
        std::fs::write(
            repo.join(".nanna/protected.toml"),
            "extra = [\"inchworm.toml\", \".inchworm/**\"]\n",
        )
        .unwrap();
        std::fs::write(repo.join(".nanna/prompt.md"), "prompt").unwrap();
        let bin = dir.path().join("nanna");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                "nanna_bin = {}\n[[repo]]\nslug = \"o/r\"\npath = {}\n",
                quoted(&bin),
                quoted(&repo)
            ),
        )
        .unwrap();
        Setup { dir, config, repo }
    }

    #[test]
    fn a_valid_setup_exits_zero_without_any_environment() {
        let s = setup();
        let (code, out, err) = exec(
            &[
                "inchworm",
                "validate",
                "--config",
                s.config.to_str().unwrap(),
            ],
            &Env::default(),
        );
        assert_eq!((code, err.as_str()), (EXIT_VALID, ""));
        assert!(out.contains("0 error(s), 0 warning(s)"), "{out}");
    }

    #[test]
    fn repo_errors_exit_one_and_are_listed_on_stdout() {
        let s = setup();
        std::fs::remove_file(s.repo.join(".nanna/protected.toml")).unwrap();
        let (code, out, err) = exec(
            &[
                "inchworm",
                "validate",
                "--config",
                s.config.to_str().unwrap(),
            ],
            &Env::default(),
        );
        assert_eq!((code, err.as_str()), (EXIT_REPO_ERRORS, ""));
        assert!(out.contains("error: [o/r]"), "{out}");
    }

    #[test]
    fn an_unreadable_config_exits_two_with_one_clear_message() {
        let s = setup();
        let missing = s.dir.path().join("missing.toml");
        let (code, out, err) = exec(
            &[
                "inchworm",
                "validate",
                "--config",
                missing.to_str().unwrap(),
            ],
            &Env::default(),
        );
        assert_eq!((code, out.as_str()), (EXIT_UNUSABLE, ""));
        assert!(err.starts_with("error: cannot read "), "{err}");
        assert_eq!(err.matches("os error").count(), 1, "{err}");
    }

    #[test]
    fn an_invalid_config_exits_two() {
        let s = setup();
        std::fs::write(&s.config, "nanna_bin = ").unwrap();
        let (code, _, err) = exec(
            &[
                "inchworm",
                "validate",
                "--config",
                s.config.to_str().unwrap(),
            ],
            &Env::default(),
        );
        assert_eq!(code, EXIT_UNUSABLE);
        assert!(err.contains("invalid config"), "{err}");
    }

    #[test]
    fn list_prints_each_registered_directory_and_nothing_else() {
        let s = setup();
        let config = s.config.to_str().unwrap();
        let (code, out, err) = exec(&["inchworm", "list", "--config", config], &Env::default());
        assert_eq!((code, err.as_str()), (0, ""));
        assert_eq!(out, format!("o/r\t{}\n", s.repo.display()));
    }

    fn two_repo_config(s: &Setup, second: &str) -> String {
        let text = std::fs::read_to_string(&s.config).unwrap();
        format!(
            "{text}[[repo]]\nslug = \"o/second\"\npath = {}\n",
            quoted(Path::new(second))
        )
    }

    #[test]
    fn list_prints_every_registered_directory_in_order() {
        let s = setup();
        let second = s.dir.path().join("second");
        std::fs::write(&s.config, two_repo_config(&s, second.to_str().unwrap())).unwrap();
        let (code, out, _) = exec(
            &["inchworm", "list", "--config", s.config.to_str().unwrap()],
            &Env::default(),
        );
        assert_eq!(code, 0);
        assert_eq!(
            out,
            format!(
                "o/r\t{}\no/second\t{}\n",
                s.repo.display(),
                second.display()
            )
        );
    }

    #[test]
    fn list_refuses_a_path_with_a_control_character_instead_of_breaking_the_line_format() {
        let s = setup();
        let bad = format!("{}/new\nline", s.dir.path().display());
        std::fs::write(&s.config, two_repo_config(&s, &bad)).unwrap();
        let (code, out, err) = exec(
            &["inchworm", "list", "--config", s.config.to_str().unwrap()],
            &Env::default(),
        );
        assert_eq!((code, out.as_str()), (1, ""));
        assert!(
            err.contains("control character") && err.contains("o/second"),
            "{err}"
        );
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn list_reports_a_failed_write_and_does_not_exit_zero() {
        let s = setup();
        let mut err = Vec::new();
        let code = run(
            ["inchworm", "list", "--config", s.config.to_str().unwrap()],
            &Env::default(),
            &mut FailingWriter,
            &mut err,
        );
        assert_eq!(code, 2);
        assert!(String::from_utf8(err).unwrap().contains("disk full"));
    }

    #[test]
    fn list_reports_an_unusable_config_like_validate_does() {
        let s = setup();
        let missing = s.dir.path().join("missing.toml");
        let (code, out, err) = exec(
            &["inchworm", "list", "--config", missing.to_str().unwrap()],
            &Env::default(),
        );
        assert_eq!((code, out.as_str()), (2, ""));
        assert!(err.contains("cannot read"), "{err}");
        let (code, out, err) = exec(&["inchworm", "list"], &Env::default());
        assert_eq!((code, out.as_str()), (2, ""));
        assert!(err.contains("XDG_CONFIG_HOME or HOME"), "{err}");
    }

    #[test]
    fn the_default_config_path_comes_from_xdg_then_home() {
        let s = setup();
        let xdg = s.dir.path().join("xdg");
        std::fs::create_dir_all(xdg.join("inchworm")).unwrap();
        std::fs::copy(&s.config, xdg.join("inchworm/config.toml")).unwrap();
        let by_xdg = Env {
            xdg_config_home: Some(xdg.clone()),
            ..Env::default()
        };
        assert_eq!(exec(&["inchworm", "validate"], &by_xdg).0, EXIT_VALID);

        let home = s.dir.path().join("home");
        std::fs::create_dir_all(home.join(".config/inchworm")).unwrap();
        std::fs::copy(&s.config, home.join(".config/inchworm/config.toml")).unwrap();
        let by_home = Env {
            home: Some(home),
            ..Env::default()
        };
        assert_eq!(exec(&["inchworm", "validate"], &by_home).0, EXIT_VALID);
    }

    #[test]
    fn without_a_config_argument_xdg_or_home_the_command_says_what_to_set() {
        let (code, out, err) = exec(&["inchworm", "validate"], &Env::default());
        assert_eq!((code, out.as_str()), (EXIT_UNUSABLE, ""));
        assert!(err.contains("--config") && err.contains("HOME"), "{err}");
    }

    #[test]
    fn a_tilde_config_with_home_unset_is_a_clear_error() {
        let s = setup();
        let text = std::fs::read_to_string(&s.config).unwrap();
        let tilde = text.replace(&quoted(&s.repo), "\"~/repo\"");
        std::fs::write(&s.config, tilde).unwrap();
        let (code, _, err) = exec(
            &[
                "inchworm",
                "validate",
                "--config",
                s.config.to_str().unwrap(),
            ],
            &Env::default(),
        );
        assert_eq!(code, EXIT_UNUSABLE);
        assert!(err.contains("HOME"), "{err}");
    }

    #[test]
    fn usage_errors_exit_two_and_show_usage_on_stderr() {
        let (code, out, err) = exec(&["inchworm", "bogus"], &Env::default());
        assert_eq!((code, out.as_str()), (EXIT_UNUSABLE, ""));
        assert!(err.contains("Usage"), "{err}");
        let (code, _, err) = exec(&["inchworm"], &Env::default());
        assert_eq!(code, EXIT_UNUSABLE);
        assert!(err.contains("Usage"), "{err}");
    }

    #[test]
    fn help_and_version_go_to_stdout_with_exit_zero() {
        let (code, out, err) = exec(&["inchworm", "--help"], &Env::default());
        assert_eq!((code, err.as_str()), (EXIT_VALID, ""));
        assert!(out.contains("validate"), "{out}");
        let (code, out, _) = exec(&["inchworm", "validate", "--help"], &Env::default());
        assert_eq!(code, EXIT_VALID);
        assert!(out.contains("--config"), "{out}");
        let (code, out, _) = exec(&["inchworm", "--version"], &Env::default());
        assert_eq!(code, EXIT_VALID);
        assert!(out.starts_with("inchworm "), "{out}");
    }

    #[test]
    fn empty_environment_values_count_as_unset() {
        let env = Env::from_vars(Some("".into()), Some("".into()), Some("".into()));
        assert!(env.home.is_none() && env.xdg_config_home.is_none() && env.path.is_none());
        let env = Env::from_vars(Some("/h".into()), Some("/x".into()), Some("/bin".into()));
        assert_eq!(env.home.as_deref(), Some(Path::new("/h")));
        assert_eq!(env.xdg_config_home.as_deref(), Some(Path::new("/x")));
        assert_eq!(env.path.as_deref(), Some(std::ffi::OsStr::new("/bin")));
    }

    #[test]
    fn from_process_reads_the_real_environment() {
        let env = Env::from_process();
        let expected = std::env::var_os("PATH").filter(|v| !v.is_empty());
        assert_eq!(env.path, expected);
    }
}
