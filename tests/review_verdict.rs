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
fn ordinary_lowercase_prose_about_unsafe_code_is_not_a_verdict() {
    let text: &[u8] =
        b"VERDICT: SAFE\n\nThe change adds no unsafe code and no unsafe blocks.\nTests check unsafe inputs.\n";
    assert_eq!(verdict(text).0, 0);
}

#[test]
fn the_unsafe_token_is_matched_as_a_word_and_not_inside_longer_words() {
    for text in [
        &b"VERDICT: SAFE\nREUNSAFED\n"[..],
        &b"VERDICT: SAFE\nUNSAFELY\n"[..],
        &b"VERDICT: SAFE\nXUNSAFE and UNSAFEY\n"[..],
    ] {
        assert_eq!(verdict(text).0, 0, "{:?}", String::from_utf8_lossy(text));
    }
}

#[test]
fn any_later_unsafe_verdict_blocks_whatever_its_markup() {
    let blocked: &[&[u8]] = &[
        b"VERDICT: SAFE\nVERDICT: UNSAFE: leaked a token\n",
        b"VERDICT: SAFE\n  VERDICT: UNSAFE\n",
        b"VERDICT: SAFE\n**VERDICT: UNSAFE**\n",
        b"VERDICT: SAFE\n- VERDICT: UNSAFE\n",
        b"VERDICT: SAFE\n> Verdict: unsafe\n",
        b"VERDICT: SAFE\nverdict - unsafe\n",
        b"VERDICT: SAFE\nThis commit is UNSAFE to push.\n",
        b"VERDICT: SAFE\nUNSAFE\n",
        b"VERDICT: SAFE\n(UNSAFE)\n",
    ];
    for output in blocked {
        let (code, message) = verdict(output);
        assert_eq!(code, 1, "{:?}", String::from_utf8_lossy(output));
        assert!(message.contains("UNSAFE"), "{message}");
    }
}

#[test]
fn a_line_that_pairs_verdict_with_unsafe_blocks_in_any_order_and_case() {
    let blocked: &[&[u8]] = &[
        b"VERDICT: SAFE\nVerdict: this change is unsafe to push\n",
        b"VERDICT: SAFE\nThe unsafe verdict stands.\n",
        b"VERDICT: SAFE\nmy verdict on this commit: unsafe\n",
        b"VERDICT: SAFE\nunsafe, final verdict\n",
        b"VERDICT: SAFE\nVeRdIcT => UnSaFe\n",
    ];
    for output in blocked {
        let (code, message) = verdict(output);
        assert_eq!(code, 1, "{:?}", String::from_utf8_lossy(output));
        assert!(message.contains("UNSAFE"), "{message}");
    }
}

#[test]
fn saying_the_change_is_not_safe_blocks_but_similar_words_do_not() {
    let blocked: &[&[u8]] = &[
        b"VERDICT: SAFE\nThis change is not safe to push.\n",
        b"VERDICT: SAFE\nNOT SAFE\n",
        b"VERDICT: SAFE\nnot   safe\n",
        b"VERDICT: SAFE\n(not safe)\n",
        b"VERDICT: SAFE\nIt is Not Safe.\n",
    ];
    for output in blocked {
        let (code, message) = verdict(output);
        assert_eq!(code, 1, "{:?}", String::from_utf8_lossy(output));
        assert!(message.contains("not safe"), "{message}");
    }
    let passing: &[&[u8]] = &[
        b"VERDICT: SAFE\nThe handling is not safely wrapped but harmless.\n",
        b"VERDICT: SAFE\nNothing here is a safe-guard issue.\n",
        b"VERDICT: SAFE\nnot-safe-looking names are fine\n",
        b"VERDICT: SAFE\nThe diff is safe, and the notes say nothing unsafe was found.\n",
    ];
    for output in passing {
        assert_eq!(
            verdict(output).0,
            0,
            "{:?}",
            String::from_utf8_lossy(output)
        );
    }
}

#[test]
fn a_nul_byte_anywhere_blocks() {
    for output in [
        &b"VERDICT: SAFE\0\n"[..],
        &b"VERDICT: SAFE\nfine\0 text\n"[..],
        &b"VERDICT: SAFE\n\0"[..],
    ] {
        let (code, message) = verdict(output);
        assert_eq!(code, 1, "{output:?}");
        assert!(message.contains("NUL"), "{message}");
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
    assert!(String::from_utf8_lossy(&result.stderr).contains("does not exist"));
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
