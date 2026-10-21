//! Executing an offramp when a golden path falls off.
//!
//! GitHub and frontier-agent transport sit behind narrow traits so this module
//! stays testable and does no HTTP of its own.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::config::Offramp;
use crate::path::Outcome;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SinkError(pub String);

/// A failure report destined for a GitHub issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueDraft {
    /// Stable per journey and step; an open issue carrying it gets a comment
    /// instead of a duplicate. It is also embedded in the body.
    pub dedupe_key: String,
    /// Deterministic: the same failure always yields the same title.
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueAction {
    Created(String),
    Commented(String),
}

/// Creates an issue in `repo` (`owner/name`), or comments on the open one that
/// already has the same dedupe key.
pub trait IssueSink {
    fn create_or_comment(&self, repo: &str, draft: &IssueDraft) -> Result<IssueAction, SinkError>;
}

/// A hand-off to a frontier agent. `body` is already redacted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Escalation {
    pub journey: String,
    pub repo: String,
    pub body: String,
}

pub trait EscalationSink {
    /// Returns a short receipt (e.g. "exit status: 0") for the report.
    fn escalate(&self, escalation: &Escalation) -> Result<String, SinkError>;
}

/// Where escalations go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EscalationTarget {
    /// Program and arguments; the body is written to stdin.
    Command(Vec<String>),
    /// POSTed as JSON with `curl`, so no HTTP client is linked in.
    Webhook(String),
}

pub struct ProcessEscalation(pub EscalationTarget);

impl EscalationSink for ProcessEscalation {
    fn escalate(&self, e: &Escalation) -> Result<String, SinkError> {
        let (mut cmd, input) = match &self.0 {
            EscalationTarget::Command(argv) => {
                let (prog, args) = argv
                    .split_first()
                    .ok_or_else(|| SinkError("escalation command is empty".into()))?;
                let mut cmd = Command::new(prog);
                cmd.args(args)
                    .env("INCHWORM_JOURNEY", &e.journey)
                    .env("INCHWORM_REPO", &e.repo);
                (cmd, e.body.clone())
            }
            EscalationTarget::Webhook(url) => {
                let mut cmd = Command::new("curl");
                cmd.args([
                    "-sS",
                    "--fail",
                    "-X",
                    "POST",
                    "-H",
                    "Content-Type: application/json",
                    "--data-binary",
                    "@-",
                ])
                .arg(url);
                let json = format!(
                    "{{\"journey\":{},\"repo\":{},\"body\":{}}}",
                    json_str(&e.journey),
                    json_str(&e.repo),
                    json_str(&e.body)
                );
                (cmd, json)
            }
        };
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| SinkError(format!("spawning escalation: {err}")))?;
        if let Some(mut stdin) = child.stdin.take() {
            // A child that exits without reading is reported via its status.
            let _ = stdin.write_all(input.as_bytes());
        }
        let out = child
            .wait_with_output()
            .map_err(|err| SinkError(format!("waiting for escalation: {err}")))?;
        if out.status.success() {
            Ok(out.status.to_string())
        } else {
            let stderr = redact(String::from_utf8_lossy(&out.stderr).trim());
            Err(SinkError(format!("escalation {}: {stderr}", out.status)))
        }
    }
}

fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

const REDACTED: &str = "<redacted>";

/// Key names whose values are masked in `key=value`, `key: value` and
/// `"key": "value"` forms.
const SECRET_KEYS: &[&str] = &[
    "authorization",
    "token",
    "secret",
    "password",
    "passwd",
    "api_key",
    "api-key",
    "apikey",
];

/// Prefixes of well-known token shapes.
const TOKEN_PREFIXES: &[&str] = &[
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "sk-",
    "xox",
];

/// Mask obvious secrets: `Authorization` headers and bearer credentials,
/// `token`/`secret`/`password`/`api_key` assignments, URL userinfo, and
/// well-known token prefixes (GitHub, `sk-`, Slack). Best effort, not a
/// security boundary.
pub fn redact(text: &str) -> String {
    let mut out = text.lines().map(redact_line).collect::<Vec<_>>().join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn redact_line(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    // Key/value forms mask the rest of the line after the separator (which
    // also covers "Authorization: Bearer <tok>"); the earliest key wins.
    let mut cut: Option<usize> = None;
    for key in SECRET_KEYS {
        let mut from = 0;
        while let Some(i) = lower[from..].find(key) {
            let end = from + i + key.len();
            let rest = &line[end..];
            let unquoted = rest.strip_prefix(['"', '\'']).unwrap_or(rest);
            let trimmed = unquoted.trim_start();
            if trimmed.starts_with([':', '=']) {
                let value_at = line.len() - trimmed.len() + 1;
                if cut.is_none_or(|c| value_at < c) {
                    cut = Some(value_at);
                }
                break;
            }
            from = end;
        }
    }
    let mut out = match cut {
        Some(at) => format!("{} {REDACTED}", line[..at].trim_end()),
        None => line.to_string(),
    };
    out = mask_bearer(&out);
    out = mask_userinfo(&out);
    mask_prefixed_tokens(&out)
}

fn mask_bearer(s: &str) -> String {
    match s.to_ascii_lowercase().find("bearer ") {
        Some(i) => {
            let start = i + "bearer ".len();
            let end = s[start..]
                .find(char::is_whitespace)
                .map_or(s.len(), |n| start + n);
            format!("{}{REDACTED}{}", &s[..start], &s[end..])
        }
        None => s.to_string(),
    }
}

fn mask_userinfo(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let authority_end = tail.find(['/', ' ', '\t']).unwrap_or(tail.len());
        match tail[..authority_end].rfind('@') {
            Some(at) => {
                out.push_str(REDACTED);
                rest = &tail[at..];
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    out
}

fn mask_prefixed_tokens(s: &str) -> String {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let flush = |word: &mut String, out: &mut String| {
        let is_token = word.len() >= 12 && TOKEN_PREFIXES.iter().any(|p| word.starts_with(p));
        out.push_str(if is_token { REDACTED } else { word });
        word.clear();
    };
    let mut out = String::new();
    let mut word = String::new();
    for c in s.chars() {
        if is_word(c) {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// What `execute` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfframpResult {
    /// The path completed; no offramp ran.
    NotNeeded,
    /// The offramp was `noop`. Reported so skipped failures stay visible.
    Noop,
    Issue(IssueAction),
    Escalated(String),
    /// The offramp was attempted and its sink failed.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfframpReport {
    pub journey: String,
    pub offramp: Option<Offramp>,
    pub result: OfframpResult,
}

/// Run the offramp chosen by `outcome`. Sink failures are captured in the
/// report rather than returned, so the caller can log them and move on.
pub fn execute(
    outcome: &Outcome,
    journey: &str,
    repo: &str,
    issues: &dyn IssueSink,
    escalations: &dyn EscalationSink,
) -> OfframpReport {
    let report = |offramp, result| OfframpReport {
        journey: journey.into(),
        offramp,
        result,
    };
    let Outcome::FellOff {
        step,
        fault,
        offramp,
    } = outcome
    else {
        return report(None, OfframpResult::NotNeeded);
    };
    let detail = redact(&format!(
        "Journey `{journey}` fell off its golden path at step `{step}`.\n\nFault: {}\n",
        fault.0
    ));
    let result = match offramp {
        Offramp::Noop => OfframpResult::Noop,
        Offramp::Issue => {
            let dedupe_key = format!("inchworm:{journey}:{step}");
            let draft = IssueDraft {
                title: format!("inchworm: {journey} fell off at {step}"),
                body: format!("{detail}\n<!-- inchworm-dedupe: {dedupe_key} -->\n"),
                dedupe_key,
            };
            match issues.create_or_comment(repo, &draft) {
                Ok(action) => OfframpResult::Issue(action),
                Err(e) => OfframpResult::Failed(e.0),
            }
        }
        Offramp::Escalate => {
            let escalation = Escalation {
                journey: journey.into(),
                repo: repo.into(),
                body: detail,
            };
            match escalations.escalate(&escalation) {
                Ok(receipt) => OfframpResult::Escalated(receipt),
                Err(e) => OfframpResult::Failed(redact(&e.0)),
            }
        }
    };
    report(Some(*offramp), result)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::path::Fault;

    /// Remembers open issues by dedupe key, like the real sink must.
    #[derive(Default)]
    struct FakeIssues {
        open: RefCell<Vec<IssueDraft>>,
        fail: bool,
    }

    impl IssueSink for FakeIssues {
        fn create_or_comment(&self, _: &str, d: &IssueDraft) -> Result<IssueAction, SinkError> {
            if self.fail {
                return Err(SinkError("403".into()));
            }
            let mut open = self.open.borrow_mut();
            match open.iter().position(|o| o.dedupe_key == d.dedupe_key) {
                Some(i) => Ok(IssueAction::Commented(format!("#{}", i + 1))),
                None => {
                    open.push(d.clone());
                    Ok(IssueAction::Created(format!("#{}", open.len())))
                }
            }
        }
    }

    #[derive(Default)]
    struct FakeEscalation {
        sent: RefCell<Vec<Escalation>>,
    }

    impl EscalationSink for FakeEscalation {
        fn escalate(&self, e: &Escalation) -> Result<String, SinkError> {
            self.sent.borrow_mut().push(e.clone());
            Ok("ok".into())
        }
    }

    fn fell_off(offramp: Offramp, fault: &str) -> Outcome {
        Outcome::FellOff {
            step: "plan".into(),
            fault: Fault(fault.into()),
            offramp,
        }
    }

    #[test]
    fn completed_needs_no_offramp() {
        let (i, e) = (FakeIssues::default(), FakeEscalation::default());
        let r = execute(&Outcome::Completed, "issues", "o/r", &i, &e);
        assert_eq!(r.result, OfframpResult::NotNeeded);
        assert_eq!(r.offramp, None);
    }

    #[test]
    fn noop_does_nothing_but_is_reported() {
        let (i, e) = (FakeIssues::default(), FakeEscalation::default());
        let r = execute(&fell_off(Offramp::Noop, "x"), "ci", "o/r", &i, &e);
        assert_eq!(r.result, OfframpResult::Noop);
        assert_eq!(r.offramp, Some(Offramp::Noop));
        assert!(i.open.borrow().is_empty() && e.sent.borrow().is_empty());
    }

    #[test]
    fn issue_is_created_once_then_commented() {
        let (i, e) = (FakeIssues::default(), FakeEscalation::default());
        let o = fell_off(Offramp::Issue, "model gave up");
        let first = execute(&o, "issues", "o/r", &i, &e);
        let second = execute(&o, "issues", "o/r", &i, &e);
        assert_eq!(
            first.result,
            OfframpResult::Issue(IssueAction::Created("#1".into()))
        );
        assert_eq!(
            second.result,
            OfframpResult::Issue(IssueAction::Commented("#1".into()))
        );
        let open = i.open.borrow();
        assert_eq!(open[0].title, "inchworm: issues fell off at plan");
        assert_eq!(open[0].dedupe_key, "inchworm:issues:plan");
        assert!(
            open[0]
                .body
                .contains("inchworm-dedupe: inchworm:issues:plan")
        );
        assert!(e.sent.borrow().is_empty());
    }

    #[test]
    fn issue_sink_failure_is_reported() {
        let i = FakeIssues {
            fail: true,
            ..Default::default()
        };
        let e = FakeEscalation::default();
        let r = execute(&fell_off(Offramp::Issue, "x"), "ci", "o/r", &i, &e);
        assert_eq!(r.result, OfframpResult::Failed("403".into()));
    }

    #[test]
    fn escalation_body_is_redacted() {
        let (i, e) = (FakeIssues::default(), FakeEscalation::default());
        let fault = "push failed\nAuthorization: Bearer abc123\nusing ghp_0123456789abcdefghij";
        let o = fell_off(Offramp::Escalate, fault);
        let r = execute(&o, "conflicts", "o/r", &i, &e);
        assert_eq!(r.result, OfframpResult::Escalated("ok".into()));
        let sent = e.sent.borrow();
        assert_eq!(
            (sent[0].journey.as_str(), sent[0].repo.as_str()),
            ("conflicts", "o/r")
        );
        assert!(sent[0].body.contains("push failed"));
        assert!(!sent[0].body.contains("abc123") && !sent[0].body.contains("ghp_"));
        assert!(i.open.borrow().is_empty());
    }

    #[test]
    fn issue_body_is_redacted_too() {
        let (i, e) = (FakeIssues::default(), FakeEscalation::default());
        execute(
            &fell_off(Offramp::Issue, "token=s3cr3t"),
            "ci",
            "o/r",
            &i,
            &e,
        );
        assert!(!i.open.borrow()[0].body.contains("s3cr3t"));
    }

    #[test]
    fn redacts_common_shapes() {
        for (input, secret) in [
            ("Authorization: Basic dXNlcjpwdw==", "dXNlcjpwdw"),
            ("curl -H 'authorization: Bearer abc.def'", "abc.def"),
            ("GITHUB_TOKEN=ghp_abcdefghijklmnop", "abcdefghijklmnop"),
            ("{\"api_key\": \"k-12345\"}", "k-12345"),
            ("password = hunter2", "hunter2"),
            ("clone https://bob:pw@host/r.git", "bob:pw"),
            ("saw sk-abcdefghijklmnop in logs", "abcdefghijklmnop"),
            (
                "saw github_pat_11ABCDEFG0123456789 here",
                "11ABCDEFG0123456789",
            ),
        ] {
            let out = redact(input);
            assert!(!out.contains(secret), "{input:?} -> {out:?}");
            assert!(out.contains("<redacted>"), "{input:?} -> {out:?}");
        }
    }

    #[test]
    fn redact_leaves_ordinary_text_alone() {
        let text = "tests failed: 3 of 10\nsee src/main.rs\nskip-this and sk-short\n";
        assert_eq!(redact(text), text);
    }

    #[test]
    fn command_sink_pipes_body_to_stdin() {
        let e = Escalation {
            journey: "ci".into(),
            repo: "o/r".into(),
            body: "hi".into(),
        };
        let ok = ProcessEscalation(EscalationTarget::Command(vec!["cat".into()]));
        assert!(ok.escalate(&e).is_ok());
        let fail = ProcessEscalation(EscalationTarget::Command(vec!["false".into()]));
        assert!(fail.escalate(&e).is_err());
        let empty = ProcessEscalation(EscalationTarget::Command(vec![]));
        assert!(empty.escalate(&e).is_err());
    }
}
