use std::path::PathBuf;
use std::process::ExitCode;

use inchworm::config::{CONFIG_FILE, Config};
use inchworm::registry::Registry;

const USAGE: &str = "usage: inchworm <command> [dir]

commands:
  init [dir]        write a default inchworm.toml
  check [dir]       validate dir/inchworm.toml
  register [dir]    register dir with the user-level daemon
  unregister [dir]  remove dir from the registry
  list              list registered directories";

fn run(args: &[String]) -> Result<(), String> {
    let dir = || PathBuf::from(args.get(1).map_or(".", String::as_str));
    let registry =
        || Registry::default_location().ok_or("cannot locate state dir (set INCHWORM_HOME)");
    match args.first().map(String::as_str) {
        Some("init") => {
            let file = dir().join(CONFIG_FILE);
            if file.exists() {
                return Err(format!("{} already exists", file.display()));
            }
            std::fs::write(&file, Config::default().to_toml()).map_err(|e| e.to_string())?;
            println!("wrote {}", file.display());
        }
        Some("check") => {
            Config::load(&dir()).map_err(|e| e.to_string())?;
            println!("ok");
        }
        Some("register") => {
            let added = registry()?.register(&dir()).map_err(|e| e.to_string())?;
            println!(
                "{}",
                if added {
                    "registered"
                } else {
                    "already registered"
                }
            );
        }
        Some("unregister") => {
            let removed = registry()?.unregister(&dir()).map_err(|e| e.to_string())?;
            println!(
                "{}",
                if removed {
                    "unregistered"
                } else {
                    "not registered"
                }
            );
        }
        Some("list") => {
            for d in registry()?.list().map_err(|e| e.to_string())? {
                println!("{}", d.display());
            }
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
