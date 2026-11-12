#![cfg(unix)]

use std::path::Path;
use std::process::Command;

fn script() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/review-verdict.sh")
}

fn verdict(output: &[u8]) -> (i32, String) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("review.txt");
    std::fs::write(&file, output).unwrap();
    let result = Command::new("bash")
        .arg(script())
        .arg(&file)
        .output()
        .unwrap();
    (
        result.status.code().unwrap(),
        String::from_utf8_lossy(&result.stderr).into_owned(),
    )
}

#[test]
fn a_clean_verdict_on_the_first_line_passes() {
    assert_eq!(verdict(b"VERDICT: SAFE\n").0, 0);
    assert_eq!(verdict(b"VERDICT: SAFE\n\nThe diff adds a README.\n").0, 0);
    assert_eq!(verdict(b"VERDICT: SAFE").0, 0);
}

#[test]
fn anything_else_on_the_first_line_blocks() {
    let blocked: &[&[u8]] = &[
        b"",
        b"\n",
        b"VERDICT: UNSAFE\nleaked a token\n",
        b"found AKIA secret\nVERDICT: SAFE\n",
        b"**VERDICT: SAFE**\n",
        b"VERDICT: SAFE \n",
        b" VERDICT: SAFE\n",
        b"VERDICT: SAFE\r\n",
        b"verdict: safe\n",
        b"VERDICT: SAFELY\n",
        b"\nVERDICT: SAFE\n",
        b"The change looks fine.\n",
    ];
    for output in blocked {
        let (code, message) = verdict(output);
        assert_eq!(code, 1, "{:?}", String::from_utf8_lossy(output));
        assert!(message.contains("VERDICT: SAFE"), "{message}");
    }
}

#[test]
fn a_later_unsafe_line_blocks_even_after_a_safe_first_line() {
    let (code, message) = verdict(b"VERDICT: SAFE\nVERDICT: UNSAFE\nactually a secret\n");
    assert_eq!(code, 1);
    assert!(message.contains("UNSAFE"), "{message}");
}

#[test]
fn a_missing_file_blocks() {
    let result = Command::new("bash")
        .arg(script())
        .arg("/nonexistent/review.txt")
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
}

#[test]
fn the_script_needs_exactly_one_argument() {
    let result = Command::new("bash").arg(script()).output().unwrap();
    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("usage"));
}

#[test]
fn the_hook_calls_the_script_and_does_not_reimplement_the_check() {
    let hook = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join(".cargo-husky/hooks/pre-commit"),
    )
    .unwrap();
    assert!(!hook.contains("grep -q '^VERDICT"));
    let block = "\"$root/scripts/review-verdict.sh\" \"$review\" || {\n    echo \"Fix the problems above or re-run the review.\"\n    exit 1\n}\n";
    assert!(
        hook.contains(block),
        "the hook must stop when the verdict script fails; expected this block:\n{block}"
    );
}
