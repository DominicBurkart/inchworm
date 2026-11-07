use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use inchworm::config::GlobalConfig;
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
    },
}

fn run(cli: Cli) -> Result<bool> {
    match cli.command {
        Command::Validate { config } => {
            let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
            let path = config.unwrap_or_else(|| {
                let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
                GlobalConfig::default_path(xdg.as_deref(), &home)
            });
            let config = GlobalConfig::load(&path, &home)?;
            let report = validate(&config);
            print!("{}", report.render());
            Ok(report.is_ok())
        }
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
