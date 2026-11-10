use std::process::ExitCode;

use clap::Parser;
use inchworm::cli::{Cli, Tools, execute};

fn main() -> ExitCode {
    let cli = Cli::parse();
    match Tools::from_env().and_then(|tools| execute(cli, &tools)) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(2)
        }
    }
}
