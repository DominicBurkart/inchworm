use std::process::ExitCode;

use inchworm::cli::{Env, run};

fn main() -> ExitCode {
    let code = run(
        std::env::args_os(),
        &Env::from_process(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );
    ExitCode::from(code)
}
