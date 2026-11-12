use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};

use crate::clock::{Clock, SystemClock};
use crate::config::{FlowFile, GlobalConfig, RepoEntry};
use crate::daemon::{DaemonDeps, DaemonSettings, NannaFactory, run_daemon};
use crate::github::GhCli;
use crate::judges::OllamaClient;
use crate::model::RunState;
use crate::nanna_client::NannaClient;
use crate::online::{ImageBuilder, NannaCatalog, NixBuilder, Online, validate_online};
use crate::outcome::{CiPolicy, OutcomeEnv};
use crate::policy::HumanGate;
use crate::run::{RunContext, run_issue};
use crate::stats::{parse_class, render_failures, render_stats, render_status};
use crate::store::Store;
use crate::validate::validate;

#[derive(Parser)]
#[command(name = "inchworm", version, about = "local coding agent orchestrator")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    Validate {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        online: bool,
        #[arg(long)]
        build_images: bool,
        #[arg(long)]
        skip_branch_protection: bool,
    },
    Status {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    Stats {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        window: Option<String>,
    },
    Failures {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        class: Option<String>,
    },
    Daemon {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    Run {
        repo: String,
        issue: u64,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        flow: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct Tools {
    pub gh: String,
    pub nix: String,
    pub home: PathBuf,
    pub xdg_config_home: Option<PathBuf>,
}

impl Tools {
    pub fn from_env() -> Result<Tools> {
        Ok(Tools {
            gh: "gh".to_string(),
            nix: "nix".to_string(),
            home: PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        })
    }
}

fn load_config(explicit: Option<PathBuf>, tools: &Tools) -> Result<GlobalConfig> {
    let path = explicit.unwrap_or_else(|| {
        GlobalConfig::default_path(tools.xdg_config_home.as_deref(), &tools.home)
    });
    Ok(GlobalConfig::load(&path, &tools.home)?)
}

fn find_repo<'a>(config: &'a GlobalConfig, selector: &str) -> Result<&'a RepoEntry> {
    config
        .repo
        .iter()
        .find(|repo| repo.slug == selector || repo.path == Path::new(selector))
        .ok_or_else(|| anyhow!("repo `{selector}` is not in the config"))
}

const OLLAMA_TIMEOUT: Duration = Duration::from_secs(600);

fn open_store(config: &GlobalConfig) -> Result<Store> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = config.state_db.parent() {
        let existed = parent.exists();
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
        if !existed {
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("cannot restrict {}", parent.display()))?;
        }
    }
    let store = Store::open(&config.state_db)?;
    std::fs::set_permissions(&config.state_db, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("cannot restrict {}", config.state_db.display()))?;
    Ok(store)
}

fn check_nanna_bin(bin: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |path: &Path| {
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    let found = if bin.components().count() > 1 {
        executable(bin)
    } else {
        std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|dir| executable(&dir.join(bin))))
            .unwrap_or(false)
    };
    anyhow::ensure!(
        found,
        "nanna_bin `{}` is not an executable file; fix nanna_bin in the config",
        bin.display()
    );
    Ok(())
}

fn report_command(
    config: &GlobalConfig,
    report: impl FnOnce(&Store) -> Result<String>,
) -> Result<bool> {
    let store = open_store(config)?;
    print!("{}", report(&store)?);
    Ok(true)
}

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

fn daemon_command<S: std::future::Future<Output = ()>>(
    config: GlobalConfig,
    tools: &Tools,
    shutdown: impl FnOnce() -> std::io::Result<S>,
) -> Result<bool> {
    check_nanna_bin(&config.nanna_bin)?;
    drop(open_store(&config)?);
    let settings = DaemonSettings::from_config(&config);
    let deps = DaemonDeps {
        github: Arc::new(GhCli::with_program(tools.gh.as_str())),
        clock: Arc::new(SystemClock),
        dispatchers: Arc::new(NannaFactory::new(config.nanna_bin.clone())),
        scorer: Arc::new(OllamaClient::new(&config.ollama_url, OLLAMA_TIMEOUT)?),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let _entered = runtime.enter();
    let shutdown = shutdown().context("cannot install the termination signal handlers")?;
    runtime.block_on(run_daemon(settings, deps, shutdown))?;
    Ok(true)
}

struct ValidateMode {
    online: bool,
    build_images: bool,
    branch_protection: bool,
}

fn validate_command(config: GlobalConfig, tools: &Tools, mode: ValidateMode) -> bool {
    let ValidateMode {
        online,
        build_images,
        branch_protection,
    } = mode;
    let report = if online || build_images {
        let catalog = NannaCatalog::new(&config.nanna_bin);
        let github = GhCli::with_program(tools.gh.as_str());
        let builder = NixBuilder::with_program(tools.nix.as_str());
        let online = Online {
            catalog: &catalog,
            github: &github,
            branch_protection,
            builder: build_images.then_some(&builder as &dyn ImageBuilder),
        };
        validate_online(&config, &online)
    } else {
        validate(&config)
    };
    print!("{}", report.render());
    report.is_ok()
}

fn run_command(
    config: GlobalConfig,
    tools: &Tools,
    selector: &str,
    issue: u64,
    flow: Option<&str>,
) -> Result<bool> {
    let repo = find_repo(&config, selector)?;
    let flows_path = repo.path.join(".inchworm/flows.toml");
    let text = std::fs::read_to_string(&flows_path)
        .with_context(|| format!("cannot read {}", flows_path.display()))?;
    let file = FlowFile::parse(&text)?;
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

    let store = open_store(&config)?;
    let github = GhCli::with_program(tools.gh.as_str());
    let mut client = NannaClient::spawn(&config.nanna_bin)
        .with_context(|| format!("cannot start `{} mcp-serve`", config.nanna_bin.display()))?;
    client.initialize()?;

    let clock = SystemClock;
    let ctx = RunContext {
        store: &store,
        github: &github,
        slug: &repo.slug,
        repo_path: &repo.path,
        clock: &clock,
    };
    let scorer = OllamaClient::new(&config.ollama_url, OLLAMA_TIMEOUT)?;
    let env = OutcomeEnv {
        scorer: &scorer,
        model_permit: Arc::new(tokio::sync::Semaphore::new(1)),
        ci: CiPolicy::default(),
        human_gate: HumanGate::NotConsulted,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let summary = runtime.block_on(run_issue(&ctx, &env, selected, issue, &mut client))?;
    println!(
        "run {} for {}#{issue}: {} after {} attempt(s)",
        summary.run_id, repo.slug, summary.state, summary.attempts
    );
    if let Some(pr) = &summary.pr {
        println!("pull request: {} (head {})", pr.url, pr.head_sha);
    }
    if let Some((class, raw)) = &summary.failure {
        println!("failure: {class}: {raw}");
    }
    for warning in &summary.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(summary.state != RunState::Failed)
}

/// ```
/// use clap::Parser;
/// use inchworm::cli::{execute, Cli, Tools};
///
/// let tools = Tools {
///     gh: "gh".into(),
///     nix: "nix".into(),
///     home: "/nonexistent-home".into(),
///     xdg_config_home: None,
/// };
/// let cli = Cli::parse_from(["inchworm", "validate", "--config", "/no/such/config.toml"]);
/// assert!(execute(cli, &tools).is_err());
/// ```
pub fn execute(cli: Cli, tools: &Tools) -> Result<bool> {
    match cli.command {
        Command::Validate {
            config,
            online,
            build_images,
            skip_branch_protection,
        } => Ok(validate_command(
            load_config(config, tools)?,
            tools,
            ValidateMode {
                online,
                build_images,
                branch_protection: !skip_branch_protection,
            },
        )),
        Command::Status { config } => report_command(&load_config(config, tools)?, |store| {
            Ok(render_status(store)?)
        }),
        Command::Stats { config, window } => {
            report_command(&load_config(config, tools)?, |store| {
                Ok(render_stats(store, SystemClock.now(), window.as_deref())?)
            })
        }
        Command::Failures { config, class } => {
            let class = class.as_deref().map(parse_class).transpose()?;
            report_command(&load_config(config, tools)?, |store| {
                Ok(render_failures(store, class)?)
            })
        }
        Command::Daemon { config } => {
            daemon_command(load_config(config, tools)?, tools, termination_signal)
        }
        Command::Run {
            repo,
            issue,
            config,
            flow,
        } => run_command(
            load_config(config, tools)?,
            tools,
            &repo,
            issue,
            flow.as_deref(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const FLOWS: &str = "[[flow]]\nname = \"issue\"\ntrigger = { kind = \"label\", label = \"inchworm\" }\n[[flow.step]]\nname = \"implement\"\nidentity = \"rust-implementer\"\n[flow.outcome]\nmode = \"draft_pr\"\n";

    struct Env {
        dir: tempfile::TempDir,
        tools: Tools,
        config: PathBuf,
        repo: PathBuf,
    }

    impl Env {
        fn bin(&self) -> PathBuf {
            self.dir.path().join("bin")
        }

        fn set(&self, name: &str, value: &str) {
            std::fs::write(self.bin().join(name), value).unwrap();
        }

        fn exec(&self, args: &[&str]) -> Result<bool> {
            let mut argv = vec!["inchworm"];
            argv.extend_from_slice(args);
            argv.push("--config");
            let config = self.config.display().to_string();
            argv.push(&config);
            execute(Cli::parse_from(argv), &self.tools)
        }
    }

    fn write(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn env() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        write(&repo, ".inchworm/flows.toml", FLOWS);
        write(
            &repo,
            ".nanna/protected.toml",
            "extra = [\".inchworm/**\"]\n",
        );
        write(&repo, ".nanna/prompt.md", "p");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for name in ["nanna", "gh"] {
            let from = format!("{}/tests/fixtures/bin/{name}", env!("CARGO_MANIFEST_DIR"));
            std::fs::copy(from, bin.join(name)).unwrap();
        }
        write(
            &bin,
            "fixtures",
            &format!("{}/tests/fixtures/nanna", env!("CARGO_MANIFEST_DIR")),
        );
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                "nanna_bin = \"{}\"\nstate_db = \"{}\"\n[[repo]]\nslug = \"o/r\"\npath = \"{}\"\n",
                bin.join("nanna").display(),
                dir.path().join("state/state.db").display(),
                repo.display()
            ),
        )
        .unwrap();
        let tools = Tools {
            gh: bin.join("gh").display().to_string(),
            nix: bin.join("nix").display().to_string(),
            home: dir.path().to_path_buf(),
            xdg_config_home: None,
        };
        Env {
            dir,
            tools,
            config,
            repo,
        }
    }

    fn fake_nix(env: &Env, body: &str) {
        let path = env.bin().join("nix");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn offline_validate_does_not_touch_gh_or_nix() {
        let env = env();
        assert!(env.exec(&["validate"]).unwrap());
    }

    #[test]
    fn online_validate_resolves_identities_and_labels() {
        let env = env();
        assert!(env.exec(&["validate", "--online"]).unwrap());
        env.set("identities", "reviewer");
        assert!(!env.exec(&["validate", "--online"]).unwrap());
        env.set("identities", "rust-implementer");
        env.set("labels", "[]");
        assert!(!env.exec(&["validate", "--online"]).unwrap());
    }

    #[test]
    fn online_validate_compares_required_checks_with_branch_protection() {
        let env = env();
        write(
            &env.repo,
            ".inchworm/flows.toml",
            &FLOWS.replace(
                "mode = \"draft_pr\"",
                "mode = \"draft_pr\"\nrequired_checks = [\"build\"]",
            ),
        );
        assert!(env.exec(&["validate", "--online"]).unwrap());
        env.set("protection", "test\n");
        assert!(!env.exec(&["validate", "--online"]).unwrap());
        assert!(
            env.exec(&["validate", "--online", "--skip-branch-protection"])
                .unwrap()
        );
        env.set("protection", "build\n");
        assert!(env.exec(&["validate", "--online"]).unwrap());
    }

    #[test]
    fn build_images_builds_and_reports_failures() {
        let env = env();
        let record = env.dir.path().join("nix-args");
        fake_nix(&env, &format!("echo \"$@\" > {}", record.display()));
        assert!(env.exec(&["validate", "--build-images"]).unwrap());
        assert_eq!(
            std::fs::read_to_string(&record).unwrap().trim(),
            format!(
                "build path:{}#devContainerImage --no-link",
                env.repo.display()
            )
        );
        fake_nix(&env, "echo broken >&2; exit 1");
        assert!(!env.exec(&["validate", "--build-images"]).unwrap());
    }

    #[test]
    fn run_records_the_pr_and_reports_success() {
        let env = env();
        assert!(env.exec(&["run", "o/r", "5"]).unwrap());
        let store = Store::open(&env.dir.path().join("state/state.db")).unwrap();
        let run = store.recent_runs(1).unwrap().remove(0);
        assert_eq!(run.state, RunState::PrOpen);
        assert_eq!(store.attempts(run.id).unwrap()[0].pr_number, Some(9));
    }

    #[test]
    fn status_stats_and_failures_read_the_store_after_a_run() {
        let env = env();
        assert!(env.exec(&["status"]).unwrap());
        assert!(env.exec(&["stats"]).unwrap());
        assert!(env.exec(&["failures"]).unwrap());
        assert!(env.exec(&["run", "o/r", "5"]).unwrap());
        env.set("state", "task_failed");
        env.set("result", "result_failed_structured");
        assert!(!env.exec(&["run", "o/r", "5"]).unwrap());
        assert!(env.exec(&["status"]).unwrap());
        assert!(env.exec(&["stats", "--window", "24h"]).unwrap());
        assert!(
            env.exec(&["failures", "--class", "max_iterations"])
                .unwrap()
        );
        let store = Store::open(&env.dir.path().join("state/state.db")).unwrap();
        assert_eq!(
            store.success_rates(SystemClock.now(), None).unwrap()[0].total,
            2
        );
    }

    #[test]
    fn the_daemon_starts_polls_and_stops_on_shutdown() {
        let env = env();
        let config = GlobalConfig::load(&env.config, &env.tools.home).unwrap();
        assert!(daemon_command(config, &env.tools, || Ok(std::future::ready(()))).unwrap());
        assert!(env.dir.path().join("state/state.db").exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_sigterm_sent_before_the_shutdown_future_is_first_polled_is_not_lost() {
        let shutdown = termination_signal().unwrap();
        let status = std::process::Command::new("kill")
            .args(["-TERM", &std::process::id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        tokio::time::timeout(Duration::from_secs(5), shutdown)
            .await
            .expect("the SIGTERM was handled");
    }

    #[test]
    fn the_daemon_refuses_to_start_with_a_missing_nanna_binary() {
        let env = env();
        let mut config = GlobalConfig::load(&env.config, &env.tools.home).unwrap();
        config.nanna_bin = PathBuf::from("/no/such/nanna");
        let err = daemon_command(config, &env.tools, || Ok(std::future::ready(()))).unwrap_err();
        assert!(err.to_string().contains("nanna_bin"), "{err}");
        assert!(!env.dir.path().join("state/state.db").exists());
    }

    #[test]
    fn check_nanna_bin_accepts_executables_by_path_or_on_the_path_and_rejects_the_rest() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("nanna");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        assert!(check_nanna_bin(&bin).is_err());
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(check_nanna_bin(&bin).is_ok());
        assert!(check_nanna_bin(dir.path()).is_err());
        assert!(check_nanna_bin(Path::new("sh")).is_ok());
        assert!(check_nanna_bin(Path::new("definitely-not-a-binary-xyz")).is_err());
    }

    #[test]
    fn the_state_directory_and_database_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let env = env();
        let config = GlobalConfig::load(&env.config, &env.tools.home).unwrap();
        drop(open_store(&config).unwrap());
        let db_mode = std::fs::metadata(&config.state_db)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(db_mode & 0o777, 0o600);
        let parent = config.state_db.parent().unwrap();
        assert_eq!(
            std::fs::metadata(parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn stats_and_failures_reject_unknown_arguments() {
        let env = env();
        let err = env.exec(&["stats", "--window", "1y"]).unwrap_err();
        assert!(err.to_string().contains("unknown window"));
        let err = env.exec(&["failures", "--class", "nope"]).unwrap_err();
        assert!(err.to_string().contains("unknown failure class"));
    }

    #[test]
    fn run_prints_label_failures_as_warnings_and_still_succeeds() {
        let env = env();
        env.set("fail_edit", "1");
        assert!(env.exec(&["run", "o/r", "5"]).unwrap());
    }

    #[test]
    fn run_accepts_a_path_selector_and_a_named_flow() {
        let env = env();
        let repo = env.repo.display().to_string();
        assert!(env.exec(&["run", &repo, "5", "--flow", "issue"]).unwrap());
    }

    #[test]
    fn run_returns_false_for_a_failed_task() {
        let env = env();
        env.set("state", "task_failed");
        env.set("result", "result_failed_structured");
        assert!(!env.exec(&["run", "o/r", "5"]).unwrap());
    }

    #[test]
    fn run_rejects_a_pr_touching_inchworm() {
        let env = env();
        env.set("files", "src/lib.rs\n.inchworm/flows.toml");
        assert!(!env.exec(&["run", "o/r", "5"]).unwrap());
    }

    #[test]
    fn run_reports_setup_errors() {
        let env = env();
        let err = env.exec(&["run", "x/y", "5"]).unwrap_err();
        assert!(err.to_string().contains("not in the config"));
        let err = env
            .exec(&["run", "o/r", "5", "--flow", "nope"])
            .unwrap_err();
        assert!(err.to_string().contains("flow nope not found"));
        assert!(env.exec(&["run", "o/r", "5"]).unwrap());
        let store = Store::open(&env.dir.path().join("state/state.db")).unwrap();
        store.enqueue_run("o/r", 5, "issue", 1).unwrap().unwrap();
        drop(store);
        let err = env.exec(&["run", "o/r", "5"]).unwrap_err();
        assert!(err.to_string().contains("already has a run"));
    }

    #[test]
    fn run_fails_when_flows_are_missing_or_nanna_cannot_start() {
        let env = env();
        std::fs::write(
            &env.config,
            format!(
                "nanna_bin = \"/no/such/nanna\"\nstate_db = \"{}\"\n[[repo]]\nslug = \"o/r\"\npath = \"{}\"\n",
                env.dir.path().join("s.db").display(),
                env.repo.display()
            ),
        )
        .unwrap();
        let err = env.exec(&["run", "o/r", "5"]).unwrap_err();
        assert!(err.to_string().contains("cannot start"));
        std::fs::remove_file(env.repo.join(".inchworm/flows.toml")).unwrap();
        let err = env.exec(&["run", "o/r", "5"]).unwrap_err();
        assert!(err.to_string().contains("cannot read"));
    }

    #[test]
    fn tools_come_from_the_environment_and_the_default_config_path_is_used() {
        let tools = Tools::from_env().unwrap();
        assert_eq!(tools.gh, "gh");
        assert_eq!(tools.nix, "nix");
        let env = env();
        let cli = Cli::parse_from(["inchworm", "validate"]);
        assert!(execute(cli, &env.tools).is_err());
        let xdg = Tools {
            xdg_config_home: Some(env.dir.path().to_path_buf()),
            ..env.tools.clone()
        };
        let path = GlobalConfig::default_path(xdg.xdg_config_home.as_deref(), &xdg.home);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::copy(&env.config, &path).unwrap();
        assert!(execute(Cli::parse_from(["inchworm", "validate"]), &xdg).unwrap());
    }
}
