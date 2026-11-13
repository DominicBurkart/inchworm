#[cfg(not(unix))]
compile_error!(
    "inchworm supports Linux and macOS only: it drives nanna (podman and nix) and its tests need a POSIX shell, kill and /proc"
);

pub mod classify;
pub mod cli;
pub mod config;
pub mod exec;
pub mod flow;
pub mod fsutil;
pub mod github;
pub mod model;
pub mod nanna_client;
pub mod online;
pub mod run;
pub mod store;
pub mod validate;
