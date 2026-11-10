use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};

use crate::clock::SystemClock;
use crate::config::{FlowFile, GlobalConfig, RepoEntry};
use crate::github::GhCli;
use crate::judges::OllamaClient;
use crate::model::RunState;
use crate::nanna_client::NannaClient;
use crate::online::{ImageBuilder, NannaCatalog, NixBuilder, Online, validate_online};
use crate::outcome::{CiPolicy, OutcomeEnv};
use crate::run::{RunContext, run_issue};
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

fn validate_command(config: GlobalConfig, tools: &Tools, online: bool, build_images: bool) -> bool {
    let report = if online || build_images {
        let catalog = NannaCatalog::new(&config.nanna_bin);
        let github = GhCli::with_program(tools.gh.as_str());
        let builder = NixBuilder::with_program(tools.nix.as_str());
        let online = Online {
            catalog: &catalog,
            github: &github,
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

    if let Some(parent) = config.state_db.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let store = Store::open(&config.state_db)?;
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
    let scorer = OllamaClient::new(&config.ollama_url, OLLAMA_TIMEOUT);
    let env = OutcomeEnv {
        scorer: &scorer,
        model_permit: Arc::new(tokio::sync::Semaphore::new(1)),
        ci: CiPolicy::default(),
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
        } => Ok(validate_command(
            load_config(config, tools)?,
            tools,
            online,
            build_images,
        )),
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
