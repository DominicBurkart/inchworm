use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use inchworm::config::{FlowFile, GlobalConfig, RepoEntry};
use inchworm::github::GhCli;
use inchworm::model::RunState;
use inchworm::nanna_client::NannaClient;
use inchworm::online::{NannaCatalog, NixBuilder, Online, validate_online};
use inchworm::run::{RunContext, run_issue};
use inchworm::store::Store;
use inchworm::validate::validate;

#[derive(Parser)]
#[command(name = "inchworm", version, about = "local coding agent orchestrator")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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

fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("HOME").context("HOME is not set")?,
    ))
}

fn load_config(explicit: Option<PathBuf>) -> Result<GlobalConfig> {
    let home = home()?;
    let path = explicit.unwrap_or_else(|| {
        let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
        GlobalConfig::default_path(xdg.as_deref(), &home)
    });
    Ok(GlobalConfig::load(&path, &home)?)
}

fn find_repo<'a>(config: &'a GlobalConfig, selector: &str) -> Result<&'a RepoEntry> {
    config
        .repo
        .iter()
        .find(|repo| repo.slug == selector || repo.path == Path::new(selector))
        .ok_or_else(|| anyhow!("repo `{selector}` is not in the config"))
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn validate_command(config: GlobalConfig, online: bool, build_images: bool) -> Result<bool> {
    let report = if online || build_images {
        let catalog = NannaCatalog::new(&config.nanna_bin);
        let github = GhCli::new();
        let builder = NixBuilder::new();
        let online = Online {
            catalog: &catalog,
            github: &github,
            builder: build_images.then_some(&builder as &dyn inchworm::online::ImageBuilder),
        };
        validate_online(&config, &online)
    } else {
        validate(&config)
    };
    print!("{}", report.render());
    Ok(report.is_ok())
}

fn run_command(
    config: GlobalConfig,
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
    let github = GhCli::new();
    let mut client = NannaClient::spawn(&config.nanna_bin)
        .with_context(|| format!("cannot start `{} mcp-serve`", config.nanna_bin.display()))?;
    client.initialize()?;

    let clock = now;
    let ctx = RunContext {
        store: &store,
        github: &github,
        slug: &repo.slug,
        repo_path: &repo.path,
        now: &clock,
    };
    let summary = run_issue(&ctx, selected, issue, &mut client)?;
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
    if summary.state == RunState::AwaitingCi {
        println!("CI, judges and the outcome policy are not run by this command yet");
    }
    Ok(summary.state != RunState::Failed)
}

fn run(cli: Cli) -> Result<bool> {
    match cli.command {
        Command::Validate {
            config,
            online,
            build_images,
        } => validate_command(load_config(config)?, online, build_images),
        Command::Run {
            repo,
            issue,
            config,
            flow,
        } => run_command(load_config(config)?, &repo, issue, flow.as_deref()),
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(2)
        }
    }
}
