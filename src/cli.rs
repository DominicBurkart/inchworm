use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand};

use crate::config::{GlobalConfig, LEGACY_FLOWS_FILE, PROJECT_FILE, ProjectConfig, RepoEntry};
use crate::github::GhCli;
use crate::model::RunState;
use crate::nanna_client::{NannaClient, SpawnOptions, state_dir_for};
use crate::online::{ImageBuilder, NannaCatalog, NixBuilder, Online, validate_online};
use crate::run::{RunContext, preflight, run_issue};
use crate::store::Store;
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
    pub gh: Option<String>,
    pub nix: Option<String>,
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
            gh: None,
            nix: None,
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

#[derive(Debug, Args)]
pub struct ValidateArgs {
    #[arg(long)]
    pub config: Option<PathBuf>,
    #[arg(long)]
    pub online: bool,
    #[arg(
        long,
        help = "Also build each repository's devContainerImage with nix (implies --online). \
                This evaluates the repository's own Nix code on this machine; use it only on repositories you trust."
    )]
    pub build_images: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    pub repo: String,
    pub issue: u64,
    #[arg(long)]
    pub config: Option<PathBuf>,
    #[arg(long)]
    pub flow: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Check the global config and every registered repository.
    Validate(ValidateArgs),
    /// Run one flow for one issue in the foreground. It dispatches the steps to nanna; it does not run an auditor, an offramp, a judge, CI waiting or the outcome policy.
    Run(RunArgs),
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
        Command::Validate(args) => {
            validate_command(args.config, args.online, args.build_images, env, out, err)
        }
        Command::List { config } => list_command(config, env, out, err),
        Command::Run(args) => run_entry(args, env, out, err),
    }
}

fn run_entry(args: RunArgs, env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let loaded = match load_config(args.config, env) {
        Ok(config) => config,
        Err(message) => {
            let _ = writeln!(err, "error: {message}");
            return EXIT_UNUSABLE;
        }
    };
    match run_command(
        loaded,
        env,
        &args.repo,
        args.issue,
        args.flow.as_deref(),
        out,
    ) {
        Ok(true) => EXIT_VALID,
        Ok(false) => EXIT_REPO_ERRORS,
        Err(error) => {
            let _ = writeln!(err, "error: {error:#}");
            EXIT_UNUSABLE
        }
    }
}

fn config_path(config: Option<PathBuf>, env: &Env) -> Option<PathBuf> {
    config
        .or_else(|| GlobalConfig::default_path(env.xdg_config_home.as_deref(), env.home.as_deref()))
}

fn load_config(config: Option<PathBuf>, env: &Env) -> std::result::Result<GlobalConfig, String> {
    let Some(path) = config_path(config, env) else {
        return Err(
            "cannot find the config: pass --config, or set XDG_CONFIG_HOME or HOME".to_string(),
        );
    };
    GlobalConfig::load(&path, env.home.as_deref()).map_err(|error| error.to_string())
}

fn list_command(
    config: Option<PathBuf>,
    env: &Env,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let config = match load_config(config, env) {
        Ok(config) => config,
        Err(message) => {
            let _ = writeln!(err, "error: {message}");
            return EXIT_UNUSABLE;
        }
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
    online: bool,
    build_images: bool,
    env: &Env,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let config = match load_config(config, env) {
        Ok(config) => config,
        Err(message) => {
            let _ = writeln!(err, "error: {message}");
            return EXIT_UNUSABLE;
        }
    };
    let report = if online || build_images {
        let catalog = NannaCatalog::new(&config.nanna_bin);
        let github = GhCli::with_program(env.gh.as_deref().unwrap_or("gh"));
        let builder = NixBuilder::with_program(env.nix.as_deref().unwrap_or("nix"));
        let online = Online {
            catalog: &catalog,
            github: &github,
            builder: build_images.then_some(&builder as &dyn ImageBuilder),
        };
        validate_online(&config, &online, env.path.as_deref())
    } else {
        validate(&config, env.path.as_deref())
    };
    let _ = write!(out, "{}", report.render());
    if report.is_ok() {
        EXIT_VALID
    } else {
        EXIT_REPO_ERRORS
    }
}

fn find_repo<'a>(config: &'a GlobalConfig, selector: &str) -> Result<&'a RepoEntry> {
    for repo in &config.repo {
        if repo.slug == selector || repo.path == Path::new(selector) {
            return Ok(repo);
        }
    }
    Err(anyhow!("repo `{selector}` is not in the config"))
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn install_interrupt() -> Result<Arc<AtomicBool>> {
    static INSTALLED: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);
    let mut installed = INSTALLED.lock().unwrap_or_else(|err| err.into_inner());
    if let Some(flag) = installed.as_ref() {
        return Ok(Arc::clone(flag));
    }
    let flag = Arc::new(AtomicBool::new(false));
    let handler_flag = Arc::clone(&flag);
    ctrlc::set_handler(move || handler_flag.store(true, Ordering::SeqCst))
        .context("cannot install the SIGINT/SIGTERM handler")?;
    *installed = Some(Arc::clone(&flag));
    Ok(flag)
}

fn run_command(
    config: GlobalConfig,
    env: &Env,
    selector: &str,
    issue: u64,
    flow: Option<&str>,
    out: &mut dyn Write,
) -> Result<bool> {
    let repo = find_repo(&config, selector)?;
    let flows_path = repo.path.join(PROJECT_FILE);
    let text = match std::fs::read_to_string(&flows_path) {
        Ok(text) => text,
        Err(error) => {
            let legacy = repo.path.join(LEGACY_FLOWS_FILE);
            let moved = error.kind() == std::io::ErrorKind::NotFound
                && std::fs::symlink_metadata(&legacy).is_ok_and(|meta| meta.is_file());
            if moved {
                return Err(anyhow!(
                    "{LEGACY_FLOWS_FILE} has moved to {PROJECT_FILE} at the repository root, together with the new [auditor] table; it is no longer read"
                ));
            }
            return Err(error).with_context(|| format!("cannot read {}", flows_path.display()));
        }
    };
    let file = ProjectConfig::parse(&text)?;
    let selected = match flow {
        Some(name) => file.flow.iter().find(|f| f.name == name),
        None => file.flow.first(),
    }
    .ok_or_else(|| {
        anyhow!(
            "flow {} not found in {}",
            flow.unwrap_or("(first)"),
            flows_path.display()
        )
    })?;

    let state_db = config.state_db_path(env.home.as_deref())?;
    if let Some(parent) = state_db.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let store = Store::open(&state_db)?;
    let github = GhCli::with_program(env.gh.as_deref().unwrap_or("gh"));
    let clock = now;
    let ctx = RunContext {
        store: &store,
        github: &github,
        slug: &repo.slug,
        repo_path: &repo.path,
        now: &clock,
    };
    preflight(&ctx, selected, issue)?;
    let mut spawn_options = SpawnOptions::new(config.nanna_bin.clone(), state_dir_for(&state_db));
    if let Some(home) = &env.home {
        spawn_options = spawn_options.with_home(home);
    }
    let mut client = NannaClient::spawn(&spawn_options)
        .with_context(|| format!("cannot start `{} mcp-serve`", config.nanna_bin.display()))?;
    client.set_interrupt(install_interrupt()?);
    client.initialize()?;

    let summary = run_issue(&ctx, selected, issue, &mut client)?;
    let _ = writeln!(
        out,
        "run {} for {}#{issue}: {} after {} attempt(s)",
        summary.run_id, repo.slug, summary.state, summary.attempts
    );
    if let Some(pr) = &summary.pr {
        let _ = writeln!(out, "pull request: {} (head {})", pr.url, pr.head_sha);
    }
    if let Some((class, raw)) = &summary.failure {
        let _ = writeln!(out, "failure: {class}: {raw}");
    }
    for warning in &summary.warnings {
        eprintln!("warning: {warning}");
    }
    if summary.state == RunState::AwaitingCi {
        let _ = writeln!(
            out,
            "auditors, offramps, CI, judges and the outcome policy are not run by this command yet"
        );
    }
    Ok(summary.state != RunState::Failed)
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

    struct Fakes {
        setup: Setup,
        env: Env,
        bin: PathBuf,
    }

    impl Fakes {
        fn set(&self, name: &str, value: &str) {
            std::fs::write(self.bin.join(name), value).unwrap();
        }

        fn args(&self, args: &[&str]) -> (u8, String, String) {
            let mut argv = vec!["inchworm"];
            argv.extend_from_slice(args);
            argv.push("--config");
            let config = self.setup.config.display().to_string();
            argv.push(&config);
            exec(&argv, &self.env)
        }
    }

    fn fakes() -> Fakes {
        let setup = setup();
        let bin = setup.dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for name in ["nanna", "gh"] {
            let from = format!("{}/tests/fixtures/bin/{name}", env!("CARGO_MANIFEST_DIR"));
            std::fs::copy(from, bin.join(name)).unwrap();
        }
        std::fs::write(
            bin.join("fixtures"),
            format!("{}/tests/fixtures/nanna", env!("CARGO_MANIFEST_DIR")),
        )
        .unwrap();
        std::fs::write(
            &setup.config,
            format!(
                "nanna_bin = {}\nstate_db = {}\n[[repo]]\nslug = \"o/r\"\npath = {}\n",
                quoted(&bin.join("nanna")),
                quoted(&setup.dir.path().join("state/state.db")),
                quoted(&setup.repo)
            ),
        )
        .unwrap();
        let env = Env {
            home: Some(setup.dir.path().to_path_buf()),
            gh: Some(bin.join("gh").display().to_string()),
            nix: Some(bin.join("nix").display().to_string()),
            ..Env::default()
        };
        Fakes { setup, env, bin }
    }

    fn fake_nix(f: &Fakes, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = f.bin.join("nix");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn offline_validate_does_not_touch_gh_or_nix() {
        let f = fakes();
        assert_eq!(f.args(&["validate"]).0, EXIT_VALID);
    }

    #[test]
    fn online_validate_resolves_identities_and_labels() {
        let f = fakes();
        assert_eq!(f.args(&["validate", "--online"]).0, EXIT_VALID);
        f.set("identities", "reviewer");
        assert_eq!(f.args(&["validate", "--online"]).0, EXIT_REPO_ERRORS);
        f.set("identities", "rust-implementer");
        f.set("labels", "[]");
        assert_eq!(f.args(&["validate", "--online"]).0, EXIT_REPO_ERRORS);
    }

    #[test]
    fn build_images_builds_and_reports_failures() {
        let f = fakes();
        let record = f.setup.dir.path().join("nix-args");
        fake_nix(&f, &format!("echo \"$@\" > {}", record.display()));
        assert_eq!(f.args(&["validate", "--build-images"]).0, EXIT_VALID);
        assert_eq!(
            std::fs::read_to_string(&record).unwrap().trim(),
            format!(
                "build git+file://{}#devContainerImage --no-link --no-write-lock-file",
                f.setup.repo.display()
            )
        );
        fake_nix(&f, "echo broken >&2; exit 1");
        assert_eq!(f.args(&["validate", "--build-images"]).0, EXIT_REPO_ERRORS);
    }

    #[test]
    fn run_records_the_pr_and_reports_success() {
        let f = fakes();
        let (code, out, _) = f.args(&["run", "o/r", "5"]);
        assert_eq!(code, EXIT_VALID, "{out}");
        assert!(
            out.contains("pull request: https://github.com/o/r/pull/9"),
            "{out}"
        );
        let store = Store::open(&f.setup.dir.path().join("state/state.db")).unwrap();
        let run = store.active_run("o/r").unwrap().unwrap();
        assert_eq!(run.state, RunState::AwaitingCi);
        assert_eq!(store.attempts(run.id).unwrap()[0].pr_number, Some(9));
    }

    #[test]
    fn run_accepts_a_path_selector_and_a_named_flow() {
        let f = fakes();
        let repo = f.setup.repo.display().to_string();
        assert_eq!(
            f.args(&["run", &repo, "5", "--flow", "issue"]).0,
            EXIT_VALID
        );
    }

    #[test]
    fn run_returns_one_for_a_failed_task() {
        let f = fakes();
        f.set("state", "task_failed");
        f.set("result", "result_failed_structured");
        let (code, out, _) = f.args(&["run", "o/r", "5"]);
        assert_eq!(code, EXIT_REPO_ERRORS, "{out}");
    }

    #[test]
    fn run_with_an_unusable_config_exits_two_before_touching_anything() {
        let f = fakes();
        let missing = f.setup.dir.path().join("missing.toml");
        let (code, out, err) = exec(
            &[
                "inchworm",
                "run",
                "o/r",
                "5",
                "--config",
                missing.to_str().unwrap(),
            ],
            &f.env,
        );
        assert_eq!((code, out.as_str()), (EXIT_UNUSABLE, ""));
        assert!(err.contains("cannot read"), "{err}");
    }

    #[test]
    fn run_selects_the_repository_by_path_as_well_as_by_slug() {
        let by_slug = fakes().args(&["run", "o/r", "5"]);
        let f = fakes();
        let path = f.setup.repo.display().to_string();
        let by_path = f.args(&["run", &path, "5"]);
        assert_eq!(by_slug.0, by_path.0, "{by_path:?}");
        assert!(by_path.1.contains("for o/r#5"), "{}", by_path.1);
        let (code, _, err) = fakes().args(&["run", "nobody/none", "5"]);
        assert_eq!(code, EXIT_UNUSABLE);
        assert!(err.contains("nobody/none"), "{err}");
    }

    #[test]
    fn run_treats_failed_label_and_comment_updates_as_warnings_not_failures() {
        let baseline = fakes().args(&["run", "o/r", "5"]);
        let f = fakes();
        f.set("edit_fail", "1");
        let degraded = f.args(&["run", "o/r", "5"]);
        assert_eq!(degraded.0, baseline.0, "{degraded:?}");
        assert!(degraded.1.contains("for o/r#5"), "{}", degraded.1);
    }

    #[test]
    fn a_second_run_for_an_issue_with_an_open_run_is_refused_with_exit_two() {
        let f = fakes();
        assert_eq!(f.args(&["run", "o/r", "5"]).0, EXIT_VALID);
        let (code, out, err) = f.args(&["run", "o/r", "5"]);
        assert_eq!((code, out.as_str()), (EXIT_UNUSABLE, ""));
        assert!(err.contains("already has a run for flow `issue`"), "{err}");
    }

    #[test]
    fn the_note_about_what_run_does_not_do_is_printed_only_when_awaiting_ci() {
        let ok = fakes().args(&["run", "o/r", "5"]);
        assert_eq!(ok.0, EXIT_VALID, "{ok:?}");
        assert!(
            ok.1.contains("auditors, offramps, CI, judges and the outcome policy are not run"),
            "{}",
            ok.1
        );
        let f = fakes();
        f.set("state", "task_failed");
        f.set("result", "result_failed_structured");
        let failed = f.args(&["run", "o/r", "5"]);
        assert_eq!(failed.0, EXIT_REPO_ERRORS, "{failed:?}");
        assert!(
            !failed.1.contains("are not run by this command"),
            "{}",
            failed.1
        );
    }

    #[test]
    fn run_help_says_what_the_command_does_not_run() {
        let (code, out, _) = exec(&["inchworm", "run", "--help"], &Env::default());
        assert_eq!(code, EXIT_VALID);
        assert!(
            out.contains("does not run an auditor, an offramp, a judge"),
            "{out}"
        );
    }

    #[test]
    fn run_says_the_flows_file_moved_when_only_the_old_layout_exists() {
        let f = fakes();
        let repo = &f.setup.repo;
        let current = std::fs::read_to_string(repo.join("inchworm.toml")).unwrap();
        std::fs::remove_file(repo.join("inchworm.toml")).unwrap();
        std::fs::write(repo.join(".inchworm/flows.toml"), &current).unwrap();
        let (code, out, err) = f.args(&["run", "o/r", "5"]);
        assert_eq!((code, out.as_str()), (EXIT_UNUSABLE, ""));
        assert!(
            err.contains(".inchworm/flows.toml has moved to inchworm.toml"),
            "{err}"
        );
        std::fs::remove_file(repo.join(".inchworm/flows.toml")).unwrap();
        let (code, _, err) = f.args(&["run", "o/r", "5"]);
        assert_eq!(code, EXIT_UNUSABLE);
        assert!(
            err.contains("cannot read") && !err.contains("has moved"),
            "{err}"
        );
    }

    #[test]
    fn run_does_not_call_an_unreadable_inchworm_toml_a_move() {
        let f = fakes();
        let repo = &f.setup.repo;
        let current = std::fs::read_to_string(repo.join("inchworm.toml")).unwrap();
        std::fs::remove_file(repo.join("inchworm.toml")).unwrap();
        std::fs::create_dir(repo.join("inchworm.toml")).unwrap();
        std::fs::write(repo.join(".inchworm/flows.toml"), current).unwrap();
        let (code, _, err) = f.args(&["run", "o/r", "5"]);
        assert_eq!(code, EXIT_UNUSABLE);
        assert!(
            err.contains("cannot read") && !err.contains("has moved"),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_does_not_follow_a_symlinked_old_flows_file_into_a_move_message() {
        let f = fakes();
        let repo = &f.setup.repo;
        let current = std::fs::read_to_string(repo.join("inchworm.toml")).unwrap();
        std::fs::remove_file(repo.join("inchworm.toml")).unwrap();
        std::fs::write(repo.join("elsewhere.toml"), current).unwrap();
        std::os::unix::fs::symlink(
            repo.join("elsewhere.toml"),
            repo.join(".inchworm/flows.toml"),
        )
        .unwrap();
        let (code, _, err) = f.args(&["run", "o/r", "5"]);
        assert_eq!(code, EXIT_UNUSABLE);
        assert!(
            err.contains("cannot read") && !err.contains("has moved"),
            "{err}"
        );
    }

    #[test]
    fn run_without_a_flow_name_uses_the_first_flow() {
        let f = fakes();
        let path = f.setup.repo.join("inchworm.toml");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("[[flow]]\nname = \"second\"\ntrigger = { kind = \"label\", label = \"second-label\" }\n[[flow.step]]\nname = \"implement\"\nidentity = \"rust-implementer\"\n[[flow.judge]]\nkind = \"nanna_clean\"\n[flow.outcome]\nmode = \"draft_pr\"\n");
        std::fs::write(&path, text).unwrap();
        let (code, out, err) = f.args(&["run", "o/r", "5"]);
        assert_eq!(code, EXIT_VALID, "{out} {err}");
        let g = fakes();
        std::fs::copy(&path, g.setup.repo.join("inchworm.toml")).unwrap();
        let (code, _, err) = g.args(&["run", "o/r", "5", "--flow", "second"]);
        assert_eq!(code, EXIT_UNUSABLE, "{err}");
        assert!(err.contains("second-label"), "{err}");
    }

    #[test]
    fn run_rejects_a_pr_touching_inchworm() {
        let f = fakes();
        f.set("files", "F src/lib.rs\nF .inchworm/flows.toml");
        assert_eq!(f.args(&["run", "o/r", "5"]).0, EXIT_REPO_ERRORS);
    }

    #[test]
    fn run_reports_setup_errors_as_exit_two() {
        let f = fakes();
        let (code, _, err) = f.args(&["run", "x/y", "5"]);
        assert_eq!(code, EXIT_UNUSABLE);
        assert!(err.contains("not in the config"), "{err}");
        let (code, _, err) = f.args(&["run", "o/r", "5", "--flow", "nope"]);
        assert_eq!(code, EXIT_UNUSABLE);
        assert!(err.contains("flow nope not found"), "{err}");
        assert_eq!(f.args(&["run", "o/r", "5"]).0, EXIT_VALID);
        let (code, _, err) = f.args(&["run", "o/r", "5"]);
        assert_eq!(code, EXIT_UNUSABLE);
        assert!(err.contains("already has a run"), "{err}");
    }

    #[test]
    fn run_fails_when_flows_are_missing_or_nanna_cannot_start() {
        let f = fakes();
        std::fs::write(
            &f.setup.config,
            format!(
                "nanna_bin = \"/no/such/nanna\"\nstate_db = {}\n[[repo]]\nslug = \"o/r\"\npath = {}\n",
                quoted(&f.setup.dir.path().join("s.db")),
                quoted(&f.setup.repo)
            ),
        )
        .unwrap();
        let (code, _, err) = f.args(&["run", "o/r", "5"]);
        assert_eq!(code, EXIT_UNUSABLE);
        assert!(err.contains("cannot start"), "{err}");
        std::fs::remove_file(f.setup.repo.join("inchworm.toml")).unwrap();
        let (code, _, err) = f.args(&["run", "o/r", "5"]);
        assert_eq!(code, EXIT_UNUSABLE);
        assert!(err.contains("cannot read"), "{err}");
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
