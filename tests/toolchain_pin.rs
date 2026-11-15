use std::path::Path;
use std::process::Command;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn pinned_channel() -> String {
    let text = std::fs::read_to_string(root().join("rust-toolchain.toml")).unwrap();
    let table: toml::Table = text.parse().unwrap();
    table["toolchain"]["channel"].as_str().unwrap().to_string()
}

fn is_exact_version(channel: &str) -> bool {
    let parts: Vec<&str> = channel.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

#[test]
fn the_toolchain_is_an_exact_version_and_not_a_moving_channel() {
    let channel = pinned_channel();
    assert!(
        is_exact_version(&channel),
        "rust-toolchain.toml channel `{channel}` must be an exact x.y.z version"
    );
}

#[test]
fn the_version_check_rejects_moving_channels_and_partial_versions() {
    for bad in [
        "stable", "beta", "nightly", "1.97", "1", "1.97.0.1", "1.x.0", "", "1..0",
    ] {
        assert!(!is_exact_version(bad), "{bad:?}");
    }
    assert!(is_exact_version("1.97.0"));
    assert!(is_exact_version("10.100.1000"));
}

#[test]
fn ci_installs_the_same_version() {
    let ci = std::fs::read_to_string(root().join(".github/workflows/ci.yml")).unwrap();
    let versions: Vec<&str> = ci
        .lines()
        .filter_map(|line| line.trim().strip_prefix("toolchain:"))
        .map(str::trim)
        .collect();
    assert_eq!(
        versions,
        vec![pinned_channel().as_str()],
        "ci.yml must install exactly the version pinned in rust-toolchain.toml"
    );
    assert!(
        !ci.contains("rust-toolchain@stable"),
        "ci.yml must not follow the moving stable channel"
    );
}

#[test]
fn the_pinned_toolchain_is_the_one_running_the_tests() {
    let output = Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string()))
        .arg("--version")
        .current_dir(root())
        .output()
        .unwrap();
    let version = String::from_utf8_lossy(&output.stdout).into_owned();
    let channel = pinned_channel();
    assert!(
        version.starts_with(&format!("rustc {channel} ")),
        "tests run on `{}` but rust-toolchain.toml pins {channel}",
        version.trim()
    );
}
