use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand};

use crate::clock::{Clock, SystemClock};
use crate::config::{GlobalConfig, LEGACY_FLOWS_FILE, PROJECT_FILE, ProjectConfig, RepoEntry};
use crate::daemon::{DaemonDeps, DaemonSettings, NannaFactory, run_daemon};
use crate::github::GhCli;
use crate::judges::OllamaClient;
use crate::model::{RunState, sanitize_text};
use crate::nanna_client::{NannaClient, SpawnOptions, state_dir_for};
use crate::online::{ImageBuilder, NannaCatalog, NixBuilder, Online, validate_online};
use crate::outcome::{CiPolicy, OutcomeEnv};
use crate::run::{RunContext, preflight, run_issue};
use crate::stats::{parse_class, render_failures, render_stats, render_status};
use crate::store::{DaemonLock, Store};
use crate::validate::{resolve_command, validate};

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
pub struct ConfigArgs {
    #[arg(long)]
    pub config: Option<PathBuf>,
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
    #[arg(
        long,
        help = "With --online, do not compare each flow's required_checks with the branch protection of its base branch."
    )]
    pub skip_branch_protection: bool,
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

#[derive(Debug, Args)]
pub struct StatsArgs {
    #[arg(long)]
    pub config: Option<PathBuf>,
    #[arg(long)]
    pub window: Option<String>,
}

#[derive(Debug, Args)]
pub struct FailuresArgs {
    #[arg(long)]
    pub config: Option<PathBuf>,
    #[arg(long)]
    pub class: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Check the global config and every registered repository.
    Validate(ValidateArgs),
    /// Run one flow for one issue in the foreground: dispatch the steps to nanna, wait for CI, run the judges and apply the outcome. It does not run an auditor or an offramp.
    Run(RunArgs),
    /// List the registered directories (slug, then path, tab separated).
    List(ConfigArgs),
    /// Show the runs that are queued or in flight and the most recent ones.
    Status(ConfigArgs),
    /// Show success rates per repository and flow.
    Stats(StatsArgs),
    /// Show failure counts per class and the most recent failures.
    Failures(FailuresArgs),
    /// Poll the registered repositories and run issues until stopped by SIGTERM or SIGINT.
    Daemon(ConfigArgs),
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
        Command::Validate(args) => validate_command(args, env, out, err),
        Command::List(args) => list_command(args.config, env, out, err),
        Command::Run(args) => run_entry(args, env, out, err),
        Command::Status(args) => report_command(args.config, env, out, err, |store, _| {
            Ok(render_status(store)?)
        }),
        Command::Stats(args) => report_command(args.config, env, out, err, |store, _| {
            Ok(render_stats(
                store,
                SystemClock.now(),
                args.window.as_deref(),
            )?)
        }),
        Command::Failures(args) => match args.class.as_deref().map(parse_class).transpose() {
            Ok(class) => report_command(args.config, env, out, err, |store, _| {
                Ok(render_failures(store, class)?)
            }),
            Err(error) => {
                let _ = writeln!(err, "error: {error}");
                EXIT_UNUSABLE
            }
        },
        Command::Daemon(args) => daemon_entry(args.config, env, err),
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

fn validate_command(args: ValidateArgs, env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let ValidateArgs {
        config,
        online,
        build_images,
        skip_branch_protection,
    } = args;
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
            branch_protection: !skip_branch_protection,
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

fn prepare_state(config: &GlobalConfig, env: &Env) -> Result<PathBuf> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let state_db = config.state_db_path(env.home.as_deref())?;
    if let Some(parent) = state_db.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("cannot restrict {}", parent.display()))?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&state_db)
        .with_context(|| format!("cannot create {}", state_db.display()))?;
    std::fs::set_permissions(&state_db, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("cannot restrict {}", state_db.display()))?;
    Ok(state_db)
}

fn report_command(
    config: Option<PathBuf>,
    env: &Env,
    out: &mut dyn Write,
    err: &mut dyn Write,
    report: impl FnOnce(&Store, &GlobalConfig) -> Result<String>,
) -> u8 {
    let config = match load_config(config, env) {
        Ok(config) => config,
        Err(message) => {
            let _ = writeln!(err, "error: {message}");
            return EXIT_UNUSABLE;
        }
    };
    let text = prepare_state(&config, env)
        .and_then(|state_db| Ok(Store::open(&state_db)?))
        .and_then(|store| report(&store, &config));
    match text {
        Ok(text) => {
            let _ = write!(out, "{text}");
            EXIT_VALID
        }
        Err(error) => {
            let _ = writeln!(err, "error: {error:#}");
            EXIT_UNUSABLE
        }
    }
}

const OLLAMA_TIMEOUT: Duration = Duration::from_secs(600);

fn termination_signal() -> std::io::Result<impl std::future::Future<Output = ()>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
    })
}

fn daemon_entry(config: Option<PathBuf>, env: &Env, err: &mut dyn Write) -> u8 {
    let config = match load_config(config, env) {
        Ok(config) => config,
        Err(message) => {
            let _ = writeln!(err, "error: {message}");
            return EXIT_UNUSABLE;
        }
    };
    match daemon_command(config, env, termination_signal) {
        Ok(()) => EXIT_VALID,
        Err(error) => {
            let _ = writeln!(err, "error: {error:#}");
            EXIT_UNUSABLE
        }
    }
}

fn daemon_command<S: std::future::Future<Output = ()>>(
    config: GlobalConfig,
    env: &Env,
    shutdown: impl FnOnce() -> std::io::Result<S>,
) -> Result<()> {
    resolve_command(&config.nanna_bin, env.path.as_deref()).map_err(|message| anyhow!(message))?;
    let state_db = prepare_state(&config, env)?;
    let mut spawn_options = SpawnOptions::new(config.nanna_bin.clone(), state_dir_for(&state_db));
    if let Some(home) = &env.home {
        spawn_options = spawn_options.with_home(home);
    }
    let settings = DaemonSettings::from_config(&config, state_db);
    let deps = DaemonDeps {
        github: Arc::new(GhCli::with_program(env.gh.as_deref().unwrap_or("gh"))),
        clock: Arc::new(SystemClock),
        dispatchers: Arc::new(NannaFactory::new(spawn_options)),
        scorer: Arc::new(OllamaClient::new(&config.ollama_url, OLLAMA_TIMEOUT)?),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let _entered = runtime.enter();
    let shutdown = shutdown().context("cannot install the termination signal handlers")?;
    runtime.block_on(run_daemon(settings, deps, shutdown))?;
    Ok(())
}

fn find_repo<'a>(config: &'a GlobalConfig, selector: &str) -> Result<&'a RepoEntry> {
    for repo in &config.repo {
        if repo.slug == selector || repo.path == Path::new(selector) {
            return Ok(repo);
        }
    }
    Err(anyhow!("repo `{selector}` is not in the config"))
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

    let state_db = prepare_state(&config, env)?;
    let _lock = DaemonLock::acquire(&state_db)?;
    let store = Store::open(&state_db)?;
    let github = GhCli::with_program(env.gh.as_deref().unwrap_or("gh"));
    let clock = SystemClock;
    let ctx = RunContext {
        store: &store,
        github: &github,
        slug: &repo.slug,
        repo_path: &repo.path,
        clock: &clock,
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

    let scorer = OllamaClient::new(&config.ollama_url, OLLAMA_TIMEOUT)?;
    let outcome_env = OutcomeEnv {
        scorer: &scorer,
        model_permit: Arc::new(tokio::sync::Semaphore::new(1)),
        ci: CiPolicy::default(),
        merge_grant: file.merge.grant(),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let summary = runtime.block_on(run_issue(&ctx, &outcome_env, selected, issue, &mut client))?;
    let _ = writeln!(
        out,
        "run {} for {}#{issue}: {} after {} attempt(s)",
        summary.run_id, repo.slug, summary.state, summary.attempts
    );
    if let Some(pr) = &summary.pr {
        let _ = writeln!(out, "pull request: {} (head {})", pr.url, pr.head_sha);
    }
    if let Some((class, raw)) = &summary.failure {
        let _ = writeln!(out, "failure: {class}: {}", sanitize_text(raw, 2000));
    }
    for warning in &summary.warnings {
        eprintln!("warning: {}", sanitize_text(warning, 2000));
    }
    if summary.state != RunState::Failed {
        let _ = writeln!(out, "auditors and offramps are not run by this command yet");
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

        fn args_when_unlocked(&self, args: &[&str]) -> (u8, String, String) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let result = self.args(args);
                if !result.2.contains("another inchworm daemon holds")
                    || std::time::Instant::now() > deadline
                {
                    return result;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
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
        let run = store.recent_runs(1).unwrap().remove(0);
        assert_eq!(run.state, RunState::PrOpen);
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

    fn live_run(f: &Fakes) {
        let db = f.setup.dir.path().join("state/state.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let store = Store::open(&db).unwrap();
        store
            .enqueue_run("o/r", &crate::store::Subject::issue(5).unwrap(), "issue", 1)
            .unwrap()
            .unwrap();
    }

    #[test]
    fn a_second_run_for_an_issue_with_an_open_run_is_refused_with_exit_two() {
        let f = fakes();
        live_run(&f);
        let (code, out, err) = f.args(&["run", "o/r", "5"]);
        assert_eq!((code, out.as_str()), (EXIT_UNUSABLE, ""));
        assert!(err.contains("already has a run for flow `issue`"), "{err}");
    }

    #[test]
    fn the_note_about_what_run_does_not_do_is_printed_unless_the_run_failed() {
        let ok = fakes().args(&["run", "o/r", "5"]);
        assert_eq!(ok.0, EXIT_VALID, "{ok:?}");
        assert!(
            ok.1.contains("auditors and offramps are not run by this command yet"),
            "{}",
            ok.1
        );
        let f = fakes();
        f.set("state", "task_failed");
        f.set("result", "result_failed_structured");
        let failed = f.args(&["run", "o/r", "5"]);
        assert_eq!(failed.0, EXIT_REPO_ERRORS, "{failed:?}");
        assert!(
            !failed.1.contains("are not run by this command yet"),
            "{}",
            failed.1
        );
    }

    #[test]
    fn run_help_says_what_the_command_does_not_run() {
        let (code, out, _) = exec(&["inchworm", "run", "--help"], &Env::default());
        assert_eq!(code, EXIT_VALID);
        assert!(
            out.contains("does not run an auditor or an offramp"),
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
    fn status_stats_and_failures_read_the_store_after_a_run() {
        let f = fakes();
        assert_eq!(f.args(&["status"]).0, EXIT_VALID);
        assert_eq!(f.args(&["stats"]).0, EXIT_VALID);
        assert_eq!(f.args(&["failures"]).0, EXIT_VALID);
        assert_eq!(f.args_when_unlocked(&["run", "o/r", "5"]).0, EXIT_VALID);
        f.set("state", "task_failed");
        f.set("result", "result_failed_structured");
        assert_eq!(
            f.args_when_unlocked(&["run", "o/r", "5"]).0,
            EXIT_REPO_ERRORS
        );
        let (code, out, _) = f.args(&["status"]);
        assert_eq!(code, EXIT_VALID);
        assert!(out.contains("o/r"), "{out}");
        assert_eq!(f.args(&["stats", "--window", "24h"]).0, EXIT_VALID);
        let (code, out, _) = f.args(&["failures", "--class", "max_iterations"]);
        assert_eq!(code, EXIT_VALID);
        assert!(out.contains("max_iterations"), "{out}");
        let store = Store::open(&f.setup.dir.path().join("state/state.db")).unwrap();
        assert_eq!(
            store.success_rates(SystemClock.now(), None).unwrap()[0].total,
            2
        );
    }

    #[test]
    fn stats_and_failures_reject_unknown_arguments_with_exit_two() {
        let f = fakes();
        let (code, out, err) = f.args(&["stats", "--window", "1y"]);
        assert_eq!((code, out.as_str()), (EXIT_UNUSABLE, ""));
        assert!(err.contains("unknown window"), "{err}");
        let (code, out, err) = f.args(&["failures", "--class", "nope"]);
        assert_eq!((code, out.as_str()), (EXIT_UNUSABLE, ""));
        assert!(err.contains("unknown failure class"), "{err}");
    }

    #[test]
    fn a_report_command_with_an_unusable_config_exits_two() {
        let f = fakes();
        let missing = f.setup.dir.path().join("missing.toml");
        let (code, out, err) = exec(
            &["inchworm", "status", "--config", missing.to_str().unwrap()],
            &f.env,
        );
        assert_eq!((code, out.as_str()), (EXIT_UNUSABLE, ""));
        assert!(err.contains("cannot read"), "{err}");
    }

    #[test]
    fn the_daemon_starts_polls_and_stops_on_shutdown() {
        let f = fakes();
        let config = GlobalConfig::load(&f.setup.config, f.env.home.as_deref()).unwrap();
        daemon_command(config, &f.env, || Ok(std::future::ready(()))).unwrap();
        assert!(f.setup.dir.path().join("state/state.db").exists());
    }

    #[test]
    fn the_daemon_refuses_a_second_instance_and_a_running_foreground_run() {
        let f = fakes();
        let config = GlobalConfig::load(&f.setup.config, f.env.home.as_deref()).unwrap();
        let state_db = prepare_state(&config, &f.env).unwrap();
        let _held = DaemonLock::acquire(&state_db).unwrap();
        let err = daemon_command(config, &f.env, || Ok(std::future::ready(()))).unwrap_err();
        assert!(err.to_string().contains("another"), "{err:#}");
        let (code, _, err) = f.args(&["run", "o/r", "5"]);
        assert_eq!(code, EXIT_UNUSABLE);
        assert!(err.contains("another"), "{err}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_sigterm_sent_before_the_shutdown_future_is_first_polled_is_not_lost() {
        let shutdown = termination_signal().unwrap();
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        tokio::time::timeout(Duration::from_secs(5), shutdown)
            .await
            .expect("the SIGTERM was handled");
    }

    #[test]
    fn the_daemon_refuses_to_start_with_a_missing_nanna_binary_and_creates_nothing() {
        let f = fakes();
        let mut config = GlobalConfig::load(&f.setup.config, f.env.home.as_deref()).unwrap();
        config.nanna_bin = PathBuf::from("/no/such/nanna");
        let err = daemon_command(config, &f.env, || Ok(std::future::ready(()))).unwrap_err();
        assert!(err.to_string().contains("nanna_bin"), "{err}");
        assert!(!f.setup.dir.path().join("state/state.db").exists());
    }

    #[test]
    fn the_daemon_command_with_an_unusable_config_exits_two() {
        let f = fakes();
        let missing = f.setup.dir.path().join("missing.toml");
        let (code, out, err) = exec(
            &["inchworm", "daemon", "--config", missing.to_str().unwrap()],
            &f.env,
        );
        assert_eq!((code, out.as_str()), (EXIT_UNUSABLE, ""));
        assert!(err.contains("cannot read"), "{err}");
    }

    #[test]
    fn the_daemon_command_reports_a_missing_binary_as_exit_two_on_stderr() {
        let f = fakes();
        std::fs::write(
            &f.setup.config,
            format!(
                "nanna_bin = \"/no/such/nanna\"\nstate_db = {}\n[[repo]]\nslug = \"o/r\"\npath = {}\n",
                quoted(&f.setup.dir.path().join("state/state.db")),
                quoted(&f.setup.repo)
            ),
        )
        .unwrap();
        let (code, out, err) = f.args(&["daemon"]);
        assert_eq!((code, out.as_str()), (EXIT_UNUSABLE, ""));
        assert!(err.contains("nanna_bin"), "{err}");
    }

    #[test]
    fn the_state_directory_and_database_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let f = fakes();
        let config = GlobalConfig::load(&f.setup.config, f.env.home.as_deref()).unwrap();
        let state_db = prepare_state(&config, &f.env).unwrap();
        let db_mode = std::fs::metadata(&state_db).unwrap().permissions().mode();
        assert_eq!(db_mode & 0o777, 0o600);
        let parent = state_db.parent().unwrap();
        assert_eq!(
            std::fs::metadata(parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn an_existing_world_readable_state_directory_is_restricted() {
        use std::os::unix::fs::PermissionsExt;
        let f = fakes();
        let config = GlobalConfig::load(&f.setup.config, f.env.home.as_deref()).unwrap();
        let state_db = config.state_db_path(f.env.home.as_deref()).unwrap();
        let parent = state_db.parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        prepare_state(&config, &f.env).unwrap();
        assert_eq!(
            std::fs::metadata(parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
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
        live_run(&f);
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
