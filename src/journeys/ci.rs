//! Journey 2: CI failure root cause.
//!
//! For every open PR head sha that has not been analysed yet and whose checks
//! have finished with a failure, gather the failed-job logs, ask a nanna
//! identity to classify the failure and write a root-cause report, then act on
//! it per `[ci]` config: re-run CI (at most once per sha) when the class
//! allows it, and post the report as a PR comment.
//!
//! The flow is a [`GoldenPath`] with an auditor after classification. When it
//! falls off, the [`Outcome`] is returned in the [`PrReport`] so the caller can
//! run the configured offramp.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{CiConfig, Config, Offramp, Retrigger};
use crate::nanna::{Nanna, TaskRequest};
use crate::path::{Agent, Auditor, Fault, GoldenPath, Outcome};

/// The nanna identity that classifies failures.
pub const IDENTITY: &str = "inchworm-ci-root-cause";

/// Total budget for log text sent to nanna and quoted in comments.
const MAX_LOG_BYTES: usize = 24_000;
/// Smallest share of the budget a single failed check gets.
const MIN_PER_CHECK_LOG_BYTES: usize = 2_000;
/// Bounds on the length of an accepted report.
const MIN_REPORT_CHARS: usize = 20;
const MAX_REPORT_CHARS: usize = 4_000;
const REDACTED: &str = "[REDACTED]";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("ci host: {0}")]
pub struct HostError(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPr {
    pub number: u64,
    pub head_sha: String,
    /// Head branch name, used as the base for nanna's worktree.
    pub head_ref: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conclusion {
    Success,
    Failure,
    TimedOut,
    Cancelled,
    Neutral,
    Skipped,
}

impl Conclusion {
    fn is_failure(self) -> bool {
        matches!(self, Self::Failure | Self::TimedOut)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRun {
    pub name: String,
    /// Workflow run id, used to fetch logs and re-run failed jobs.
    pub run_id: u64,
    /// `None` while the check is still queued or in progress.
    pub conclusion: Option<Conclusion>,
}

/// The slice of GitHub that this journey needs, scoped to the user's repo.
pub trait CiHost {
    fn open_prs(&self) -> Result<Vec<OpenPr>, HostError>;
    fn check_runs(&self, sha: &str) -> Result<Vec<CheckRun>, HostError>;
    /// Log text of the failed jobs of a workflow run.
    fn failed_job_logs(&self, run_id: u64) -> Result<String, HostError>;
    fn rerun_failed_jobs(&self, run_id: u64) -> Result<(), HostError>;
    fn comment(&self, pr: u64, body: &str) -> Result<(), HostError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    Flake,
    TransientNetwork,
    Ambiguous,
    Real,
}

impl FailureClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Flake => "flake",
            Self::TransientNetwork => "transient_network",
            Self::Ambiguous => "ambiguous",
            Self::Real => "real",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        [
            Self::Flake,
            Self::TransientNetwork,
            Self::Ambiguous,
            Self::Real,
        ]
        .into_iter()
        .find(|c| c.as_str() == s)
    }

    /// Whether `[ci.retrigger]` allows re-running CI for this class.
    pub fn retrigger_allowed(self, r: &Retrigger) -> bool {
        match self {
            Self::Flake => r.flake,
            Self::TransientNetwork => r.transient_network,
            Self::Ambiguous => r.ambiguous,
            Self::Real => false,
        }
    }
}

/// Nanna's parsed answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub class: FailureClass,
    pub report: String,
}

/// Per-repo journey state: which commit heads already have a report. Shas are
/// recorded before acting, so a head is analysed and re-run at most once.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CiState {
    pub analysed: BTreeSet<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct PrReport {
    pub pr: u64,
    pub head_sha: String,
    pub status: Status,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    /// All checks finished without failures; nothing to do.
    Green,
    Analysed {
        class: Option<FailureClass>,
        retriggered: bool,
        commented: bool,
        /// `FellOff` means the caller should run the offramp.
        outcome: Outcome,
    },
    /// The host failed before analysis began; the sha will be retried.
    HostError(String),
}

/// Everything the path's agents read and write for one PR head.
struct Run<'a> {
    host: &'a dyn CiHost,
    nanna: &'a dyn Nanna,
    ci: &'a CiConfig,
    repo_path: &'a Path,
    pr: OpenPr,
    failed: Vec<CheckRun>,
    /// Redacted, truncated failed-job logs.
    logs: String,
    verdict: Option<Verdict>,
    retriggered: bool,
    commented: bool,
}

fn host_fault(what: &str, e: HostError) -> Fault {
    Fault(format!("{what}: {e}"))
}

struct GatherLogs;

impl Agent<Run<'_>> for GatherLogs {
    fn name(&self) -> &str {
        "gather-logs"
    }

    fn run(&self, s: &mut Run<'_>) -> Result<(), Fault> {
        let mut runs: Vec<u64> = s.failed.iter().map(|c| c.run_id).collect();
        runs.dedup();
        let per_run = (MAX_LOG_BYTES / runs.len().max(1)).max(MIN_PER_CHECK_LOG_BYTES);
        let mut logs = String::new();
        for id in runs {
            let raw = s
                .host
                .failed_job_logs(id)
                .map_err(|e| host_fault("fetching logs", e))?;
            logs.push_str(&format!("=== workflow run {id} ===\n"));
            logs.push_str(&truncate_log(&redact(&raw), per_run));
            logs.push('\n');
        }
        s.logs = logs;
        Ok(())
    }
}

struct Classify;

impl Agent<Run<'_>> for Classify {
    fn name(&self) -> &str {
        "classify"
    }

    fn run(&self, s: &mut Run<'_>) -> Result<(), Fault> {
        let names: Vec<&str> = s.failed.iter().map(|c| c.name.as_str()).collect();
        let request = TaskRequest {
            identity: IDENTITY.into(),
            description: task_description(s.pr.number, &s.pr.head_sha, &names, &s.logs),
            repo_path: s.repo_path.to_path_buf(),
            branch: s.pr.head_ref.clone(),
        };
        let out = s
            .nanna
            .run(&request)
            .map_err(|e| Fault(format!("nanna: {e}")))?;
        s.verdict = Some(parse_reply(&out.summary)?);
        Ok(())
    }
}

/// Sanity-checks the verdict against the evidence before anything acts on it.
struct AuditVerdict;

impl Auditor<Run<'_>> for AuditVerdict {
    fn audit(&self, s: &Run<'_>) -> Result<(), Fault> {
        let v = s
            .verdict
            .as_ref()
            .ok_or_else(|| Fault("no verdict to audit".into()))?;
        let chars = v.report.chars().count();
        if !(MIN_REPORT_CHARS..=MAX_REPORT_CHARS).contains(&chars) {
            return Err(Fault(format!("report length {chars} out of bounds")));
        }
        if redact(&v.report) != v.report {
            return Err(Fault("report contains what looks like a secret".into()));
        }
        // A model claiming a network blip must be able to point at one.
        let logs = s.logs.to_ascii_lowercase();
        if v.class == FailureClass::TransientNetwork
            && !NETWORK_MARKERS.iter().any(|m| logs.contains(m))
        {
            return Err(Fault(
                "classified transient_network but logs show no network error".into(),
            ));
        }
        Ok(())
    }
}

const NETWORK_MARKERS: [&str; 10] = [
    "timed out",
    "timeout",
    "connection reset",
    "connection refused",
    "could not resolve",
    "temporary failure in name resolution",
    "network is unreachable",
    "tls handshake",
    "503",
    "502",
];

struct RerunCi;

impl Agent<Run<'_>> for RerunCi {
    fn name(&self) -> &str {
        "retrigger"
    }

    fn run(&self, s: &mut Run<'_>) -> Result<(), Fault> {
        let class = s.verdict.as_ref().map(|v| v.class);
        if !class.is_some_and(|c| c.retrigger_allowed(&s.ci.retrigger)) {
            return Ok(());
        }
        let mut runs: Vec<u64> = s.failed.iter().map(|c| c.run_id).collect();
        runs.dedup();
        for id in runs {
            s.host
                .rerun_failed_jobs(id)
                .map_err(|e| host_fault("re-running jobs", e))?;
        }
        s.retriggered = true;
        Ok(())
    }
}

struct Comment;

impl Agent<Run<'_>> for Comment {
    fn name(&self) -> &str {
        "comment"
    }

    fn run(&self, s: &mut Run<'_>) -> Result<(), Fault> {
        if !s.ci.comment {
            return Ok(());
        }
        let v = s
            .verdict
            .as_ref()
            .ok_or_else(|| Fault("no verdict to post".into()))?;
        let body = comment_body(&s.pr.head_sha, v, s.retriggered);
        s.host
            .comment(s.pr.number, &body)
            .map_err(|e| host_fault("posting comment", e))?;
        s.commented = true;
        Ok(())
    }
}

fn golden_path<'a>() -> GoldenPath<Run<'a>> {
    GoldenPath::new("ci-root-cause")
        .step(GatherLogs)
        .audited_step(Classify, AuditVerdict)
        .step(RerunCi)
        .step(Comment)
}

/// The CI root-cause journey for one repository.
pub struct CiRootCause<'a> {
    host: &'a dyn CiHost,
    nanna: &'a dyn Nanna,
    config: &'a Config,
    repo_path: PathBuf,
}

impl<'a> CiRootCause<'a> {
    pub fn new(
        host: &'a dyn CiHost,
        nanna: &'a dyn Nanna,
        config: &'a Config,
        repo_path: PathBuf,
    ) -> Self {
        Self {
            host,
            nanna,
            config,
            repo_path,
        }
    }

    /// The offramp to run when a [`Status::Analysed`] outcome fell off.
    pub fn offramp(&self) -> Offramp {
        self.config
            .offramp
            .ci
            .unwrap_or(self.config.offramp.default)
    }

    /// One pass over the open PRs. Heads that are still running, or already
    /// in `state`, are skipped; everything else gets a [`PrReport`]. Fails only
    /// if the PR list cannot be fetched.
    pub fn tick(&self, state: &mut CiState) -> Result<Vec<PrReport>, HostError> {
        if !self.config.ci.enabled {
            return Ok(vec![]);
        }
        let path = golden_path();
        let mut reports = vec![];
        for pr in self.host.open_prs()? {
            if state.analysed.contains(&pr.head_sha) {
                continue;
            }
            let checks = match self.host.check_runs(&pr.head_sha) {
                Ok(c) => c,
                Err(e) => {
                    reports.push(PrReport {
                        pr: pr.number,
                        head_sha: pr.head_sha,
                        status: Status::HostError(e.to_string()),
                    });
                    continue;
                }
            };
            if checks.iter().any(|c| c.conclusion.is_none()) {
                continue;
            }
            let failed: Vec<CheckRun> = checks
                .into_iter()
                .filter(|c| c.conclusion.is_some_and(Conclusion::is_failure))
                .collect();
            let (number, sha) = (pr.number, pr.head_sha.clone());
            state.analysed.insert(sha.clone());
            if failed.is_empty() {
                reports.push(PrReport {
                    pr: number,
                    head_sha: sha,
                    status: Status::Green,
                });
                continue;
            }
            let mut run = Run {
                host: self.host,
                nanna: self.nanna,
                ci: &self.config.ci,
                repo_path: &self.repo_path,
                pr,
                failed,
                logs: String::new(),
                verdict: None,
                retriggered: false,
                commented: false,
            };
            let outcome = path.run(&mut run, self.offramp());
            reports.push(PrReport {
                pr: number,
                head_sha: sha,
                status: Status::Analysed {
                    class: run.verdict.map(|v| v.class),
                    retriggered: run.retriggered,
                    commented: run.commented,
                    outcome,
                },
            });
        }
        Ok(reports)
    }
}

fn task_description(pr: u64, sha: &str, checks: &[&str], logs: &str) -> String {
    format!(
        "CI failed on pull request #{pr} at commit {sha}.\n\
         Failed checks: {}\n\n\
         Classify the failure as exactly one of:\n\
         - flake: a nondeterministic test or race that likely passes on re-run\n\
         - transient_network: a network or registry outage unrelated to the change\n\
         - ambiguous: possibly transient network/compute trouble, but unclear\n\
         - real: caused by the change itself\n\n\
         Then write a concise root-cause report. Reply in exactly this format and nothing else:\n\
         CLASS: <flake|transient_network|ambiguous|real>\n\
         REPORT:\n\
         <report text>\n\n\
         Failed-job logs (secrets redacted, possibly truncated):\n{logs}",
        checks.join(", "),
    )
}

/// Strictly parse `CLASS: <class>` then `REPORT:` then a non-empty body.
/// Anything else is a fault, so garbage never reaches the repo.
fn parse_reply(reply: &str) -> Result<Verdict, Fault> {
    let bad = |why: &str| Fault(format!("unparseable nanna reply: {why}"));
    let mut lines = reply.lines().skip_while(|l| l.trim().is_empty());
    let class = lines
        .next()
        .and_then(|l| l.trim().strip_prefix("CLASS:"))
        .ok_or_else(|| bad("missing CLASS line"))?;
    let class = FailureClass::parse(class.trim()).ok_or_else(|| bad("unknown class"))?;
    let marker = lines.find(|l| !l.trim().is_empty());
    if marker.map(str::trim) != Some("REPORT:") {
        return Err(bad("missing REPORT marker"));
    }
    let report = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    if report.is_empty() {
        return Err(bad("empty report"));
    }
    Ok(Verdict { class, report })
}

fn comment_body(sha: &str, v: &Verdict, retriggered: bool) -> String {
    let short = &sha[..sha.len().min(7)];
    let action = if retriggered {
        "\n\nFailed jobs were re-run once."
    } else {
        ""
    };
    format!(
        "<!-- inchworm-ci-root-cause {sha} -->\n\
         ### CI root cause for `{short}`\n\n\
         **Classification:** `{}`\n\n{}{action}",
        v.class.as_str(),
        redact(&v.report),
    )
}

const TOKEN_PREFIXES: [&str; 12] = [
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "sk-",
    "AKIA",
    "xoxb-",
    "xoxp-",
    "npm_",
];
const SECRET_KEYS: [&str; 7] = [
    "token",
    "secret",
    "password",
    "passwd",
    "api_key",
    "apikey",
    "credential",
];
/// Fewer characters than this after a token prefix is not a token.
const MIN_TOKEN_TAIL: usize = 8;

/// Mask common credentials: `Authorization` headers, bearer tokens, well-known
/// token prefixes, `secret=value` pairs and `user:pass@` URL userinfo.
pub fn redact(text: &str) -> String {
    text.lines().map(redact_line).collect::<Vec<_>>().join("\n")
}

fn redact_line(line: &str) -> String {
    const HEADER: &str = "authorization:";
    if let Some(i) = line.to_ascii_lowercase().find(HEADER) {
        return format!("{} {REDACTED}", &line[..i + HEADER.len()]);
    }
    let mut redact_next = false;
    let mut bare_key = false;
    let words: Vec<String> = line
        .split(' ')
        .map(|word| {
            if word.is_empty() {
                return String::new();
            }
            if redact_next && !matches!(word, "=" | ":") {
                redact_next = false;
                return REDACTED.into();
            }
            if bare_key && matches!(word, "=" | ":") {
                redact_next = true;
            }
            let word = redact_userinfo(&redact_tokens(word));
            let lower = word.to_ascii_lowercase();
            bare_key = !word.contains(['=', ':']) && SECRET_KEYS.iter().any(|k| lower.contains(k));
            if lower.trim_matches(|c: char| !c.is_alphanumeric()) == "bearer" {
                redact_next = true;
            } else if let Some(pos) = word.find(['=', ':']) {
                let (key, val) = (lower[..pos].to_string(), &word[pos + 1..]);
                if SECRET_KEYS.iter().any(|k| key.contains(k)) {
                    if val.trim_matches(['"', '\'']).is_empty() {
                        redact_next = true;
                    } else {
                        return format!("{}{REDACTED}", &word[..pos + 1]);
                    }
                }
            }
            word
        })
        .collect();
    words.join(" ")
}

/// Replace each token with a known prefix, starting at a word boundary.
fn redact_tokens(word: &str) -> String {
    let is_tail = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.');
    let mut out = String::new();
    let mut rest = word;
    'scan: while !rest.is_empty() {
        for (i, _) in rest.char_indices() {
            let boundary = out.chars().chain(rest[..i].chars()).last();
            if boundary.is_some_and(|b| b.is_ascii_alphanumeric()) {
                continue;
            }
            let here = &rest[i..];
            if let Some(p) = TOKEN_PREFIXES.iter().find(|p| here.starts_with(**p)) {
                let tail = here[p.len()..]
                    .find(|c| !is_tail(c))
                    .unwrap_or(here.len() - p.len());
                if tail >= MIN_TOKEN_TAIL {
                    out.push_str(&rest[..i]);
                    out.push_str(REDACTED);
                    rest = &here[p.len() + tail..];
                    continue 'scan;
                }
            }
        }
        break;
    }
    out.push_str(rest);
    out
}

/// `https://user:pass@host` becomes `https://[REDACTED]@host`.
fn redact_userinfo(word: &str) -> String {
    if let Some(s) = word.find("://") {
        let after = &word[s + 3..];
        if let Some(at) = after.find('@')
            && !after[..at].contains('/')
        {
            return format!("{}{REDACTED}{}", &word[..s + 3], &after[at..]);
        }
    }
    word.to_string()
}

/// Keep the head and most of the tail (failures live at the end of a log).
fn truncate_log(log: &str, max: usize) -> String {
    if log.len() <= max {
        return log.to_string();
    }
    let head_len = max / 5;
    let tail_len = max - head_len;
    let mut head_end = head_len;
    while !log.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = log.len() - tail_len;
    while !log.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!(
        "{}\n[... {} bytes truncated ...]\n{}",
        &log[..head_end],
        tail_start - head_end,
        &log[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::*;
    use crate::nanna::{NannaError, TaskOutput};

    const SHA: &str = "abcdef1234567890";

    #[derive(Default)]
    struct FakeHost {
        prs: Vec<OpenPr>,
        checks: HashMap<String, Vec<CheckRun>>,
        logs: String,
        fail_rerun: bool,
        reruns: RefCell<Vec<u64>>,
        comments: RefCell<Vec<(u64, String)>>,
    }

    impl CiHost for FakeHost {
        fn open_prs(&self) -> Result<Vec<OpenPr>, HostError> {
            Ok(self.prs.clone())
        }
        fn check_runs(&self, sha: &str) -> Result<Vec<CheckRun>, HostError> {
            Ok(self.checks.get(sha).cloned().unwrap_or_default())
        }
        fn failed_job_logs(&self, _: u64) -> Result<String, HostError> {
            Ok(self.logs.clone())
        }
        fn rerun_failed_jobs(&self, run_id: u64) -> Result<(), HostError> {
            if self.fail_rerun {
                return Err(HostError("403".into()));
            }
            self.reruns.borrow_mut().push(run_id);
            Ok(())
        }
        fn comment(&self, pr: u64, body: &str) -> Result<(), HostError> {
            self.comments.borrow_mut().push((pr, body.into()));
            Ok(())
        }
    }

    struct FakeNanna {
        reply: Result<String, NannaError>,
        requests: RefCell<Vec<TaskRequest>>,
    }

    impl FakeNanna {
        fn replying(reply: &str) -> Self {
            Self {
                reply: Ok(reply.into()),
                requests: RefCell::default(),
            }
        }
    }

    impl Nanna for FakeNanna {
        fn run(&self, request: &TaskRequest) -> Result<TaskOutput, NannaError> {
            self.requests.borrow_mut().push(request.clone());
            self.reply.clone().map(|summary| TaskOutput {
                summary,
                patch: None,
            })
        }
    }

    fn check(name: &str, run_id: u64, conclusion: Option<Conclusion>) -> CheckRun {
        CheckRun {
            name: name.into(),
            run_id,
            conclusion,
        }
    }

    fn host_with(checks: Vec<CheckRun>, logs: &str) -> FakeHost {
        FakeHost {
            prs: vec![OpenPr {
                number: 7,
                head_sha: SHA.into(),
                head_ref: "feature".into(),
            }],
            checks: HashMap::from([(SHA.to_string(), checks)]),
            logs: logs.into(),
            ..Default::default()
        }
    }

    fn failing_host(logs: &str) -> FakeHost {
        host_with(
            vec![
                check("build", 11, Some(Conclusion::Success)),
                check("test", 12, Some(Conclusion::Failure)),
            ],
            logs,
        )
    }

    fn reply(class: &str) -> String {
        format!("CLASS: {class}\nREPORT:\nThe integration test raced on a port; passes on re-run.")
    }

    fn tick(
        host: &FakeHost,
        nanna: &FakeNanna,
        config: &Config,
        state: &mut CiState,
    ) -> Vec<PrReport> {
        CiRootCause::new(host, nanna, config, "/repo".into())
            .tick(state)
            .unwrap()
    }

    fn analysed(r: &PrReport) -> (Option<FailureClass>, bool, bool, &Outcome) {
        match &r.status {
            Status::Analysed {
                class,
                retriggered,
                commented,
                outcome,
            } => (*class, *retriggered, *commented, outcome),
            s => panic!("not analysed: {s:?}"),
        }
    }

    #[test]
    fn flake_is_rerun_and_commented_once_per_sha() {
        let host = failing_host("test foo ... FAILED");
        let nanna = FakeNanna::replying(&reply("flake"));
        let config = Config::default();
        let mut state = CiState::default();

        let r = tick(&host, &nanna, &config, &mut state);
        assert_eq!(r.len(), 1);
        let (class, rerun, commented, outcome) = analysed(&r[0]);
        assert_eq!(class, Some(FailureClass::Flake));
        assert!(rerun && commented);
        assert_eq!(*outcome, Outcome::Completed);
        assert_eq!(*host.reruns.borrow(), [12]);
        let comments = host.comments.borrow();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].0, 7);
        assert!(comments[0].1.contains("`flake`") && comments[0].1.contains("re-run once"));
        let req = nanna.requests.borrow();
        assert_eq!(req[0].identity, IDENTITY);
        assert_eq!(req[0].branch, "feature");
        assert!(req[0].description.contains("test foo ... FAILED"));
        drop((comments, req));

        // Same head again: nothing happens.
        assert!(tick(&host, &nanna, &config, &mut state).is_empty());
        assert_eq!(host.reruns.borrow().len(), 1);
        assert_eq!(host.comments.borrow().len(), 1);
        assert_eq!(nanna.requests.borrow().len(), 1);
    }

    #[test]
    fn retrigger_follows_config() {
        for (class, expect) in [
            ("flake", true),
            ("transient_network", true),
            ("ambiguous", false),
            ("real", false),
        ] {
            let host = failing_host("connection reset by peer");
            let nanna = FakeNanna::replying(&reply(class));
            let r = tick(&host, &nanna, &Config::default(), &mut CiState::default());
            assert_eq!(analysed(&r[0]).1, expect, "{class}");
            assert_eq!(host.reruns.borrow().is_empty(), !expect, "{class}");
            assert_eq!(host.comments.borrow().len(), 1, "{class}");
        }

        let mut config = Config::default();
        config.ci.retrigger.ambiguous = true;
        config.ci.retrigger.flake = false;
        let host = failing_host("x");
        let r = tick(
            &host,
            &FakeNanna::replying(&reply("ambiguous")),
            &config,
            &mut CiState::default(),
        );
        assert!(analysed(&r[0]).1);
        let host = failing_host("x");
        let r = tick(
            &host,
            &FakeNanna::replying(&reply("flake")),
            &config,
            &mut CiState::default(),
        );
        assert!(!analysed(&r[0]).1);
    }

    #[test]
    fn comment_can_be_disabled() {
        let mut config = Config::default();
        config.ci.comment = false;
        let host = failing_host("x");
        let r = tick(
            &host,
            &FakeNanna::replying(&reply("real")),
            &config,
            &mut CiState::default(),
        );
        assert!(!analysed(&r[0]).2);
        assert!(host.comments.borrow().is_empty());
    }

    #[test]
    fn disabled_journey_does_nothing() {
        let mut config = Config::default();
        config.ci.enabled = false;
        let host = failing_host("x");
        let nanna = FakeNanna::replying(&reply("flake"));
        assert!(tick(&host, &nanna, &config, &mut CiState::default()).is_empty());
        assert!(nanna.requests.borrow().is_empty());
    }

    #[test]
    fn garbage_reply_fails_closed() {
        for bad in [
            "",
            "sure! it was a flake",
            "CLASS: flaky\nREPORT:\nlong enough report text here",
            "CLASS: flake\nlong enough report text here",
            "CLASS: flake\nREPORT:\n   \n",
            "REPORT:\nCLASS: flake",
        ] {
            let mut config = Config::default();
            config.offramp.ci = Some(Offramp::Issue);
            let host = failing_host("x");
            let r = tick(
                &host,
                &FakeNanna::replying(bad),
                &config,
                &mut CiState::default(),
            );
            let (class, rerun, commented, outcome) = analysed(&r[0]);
            assert_eq!((class, rerun, commented), (None, false, false), "{bad:?}");
            assert!(
                matches!(outcome, Outcome::FellOff { step, offramp: Offramp::Issue, .. } if step == "classify"),
                "{bad:?}"
            );
            assert!(host.reruns.borrow().is_empty() && host.comments.borrow().is_empty());
        }
    }

    #[test]
    fn nanna_error_falls_off() {
        let nanna = FakeNanna {
            reply: Err(NannaError::Cancelled),
            requests: RefCell::default(),
        };
        let host = failing_host("x");
        let r = tick(&host, &nanna, &Config::default(), &mut CiState::default());
        assert!(matches!(analysed(&r[0]).3, Outcome::FellOff { .. }));
    }

    #[test]
    fn auditor_rejects_unsupported_network_claim() {
        let host = failing_host("assertion failed: left == right");
        let r = tick(
            &host,
            &FakeNanna::replying(&reply("transient_network")),
            &Config::default(),
            &mut CiState::default(),
        );
        let (class, rerun, _, outcome) = analysed(&r[0]);
        assert!(!rerun && class.is_some());
        assert!(matches!(outcome, Outcome::FellOff { step, .. } if step == "classify"));
        assert!(host.reruns.borrow().is_empty());
    }

    #[test]
    fn auditor_rejects_short_or_leaky_report() {
        for body in ["tiny", "leaked ghp_abcdefghijklmnop1234 in the report"] {
            let host = failing_host("x");
            let nanna = FakeNanna::replying(&format!("CLASS: real\nREPORT:\n{body}"));
            let r = tick(&host, &nanna, &Config::default(), &mut CiState::default());
            assert!(
                matches!(analysed(&r[0]).3, Outcome::FellOff { .. }),
                "{body}"
            );
            assert!(host.comments.borrow().is_empty());
        }
    }

    #[test]
    fn pending_and_green_heads() {
        let nanna = FakeNanna::replying(&reply("flake"));
        let config = Config::default();

        let host = host_with(
            vec![
                check("test", 1, Some(Conclusion::Failure)),
                check("lint", 2, None),
            ],
            "x",
        );
        let mut state = CiState::default();
        assert!(tick(&host, &nanna, &config, &mut state).is_empty());
        assert!(state.analysed.is_empty(), "retried once checks finish");

        let host = host_with(
            vec![
                check("a", 1, Some(Conclusion::Success)),
                check("b", 2, Some(Conclusion::Skipped)),
            ],
            "x",
        );
        let r = tick(&host, &nanna, &config, &mut state);
        assert_eq!(r[0].status, Status::Green);
        assert!(state.analysed.contains(SHA));
        assert!(nanna.requests.borrow().is_empty());
    }

    #[test]
    fn new_head_is_analysed_again() {
        let mut host = failing_host("x");
        let nanna = FakeNanna::replying(&reply("real"));
        let config = Config::default();
        let mut state = CiState::default();
        tick(&host, &nanna, &config, &mut state);
        host.prs[0].head_sha = "ffff".into();
        host.checks.insert(
            "ffff".into(),
            vec![check("test", 99, Some(Conclusion::TimedOut))],
        );
        let r = tick(&host, &nanna, &config, &mut state);
        assert_eq!(r[0].head_sha, "ffff");
        assert_eq!(host.comments.borrow().len(), 2);
    }

    #[test]
    fn rerun_failure_falls_off_without_comment() {
        let mut host = failing_host("x");
        host.fail_rerun = true;
        let r = tick(
            &host,
            &FakeNanna::replying(&reply("flake")),
            &Config::default(),
            &mut CiState::default(),
        );
        let (_, rerun, commented, outcome) = analysed(&r[0]);
        assert!(!rerun && !commented);
        assert!(matches!(outcome, Outcome::FellOff { step, .. } if step == "retrigger"));
    }

    #[test]
    fn secrets_never_reach_nanna_or_comments() {
        let logs = "Authorization: Bearer abc123\n\
                    GITHUB_TOKEN=ghp_abcdefghijklmnop1234\n\
                    fetching https://user:hunter2@example.com/x timed out";
        let host = failing_host(logs);
        let nanna = FakeNanna::replying(&reply("transient_network"));
        tick(&host, &nanna, &Config::default(), &mut CiState::default());
        let sent = &nanna.requests.borrow()[0].description;
        for secret in ["abc123", "ghp_abcdefghijklmnop1234", "hunter2"] {
            assert!(!sent.contains(secret), "{secret}");
        }
        assert!(sent.contains("timed out"));
    }

    #[test]
    fn redaction_patterns() {
        let cases = [
            ("Authorization: token xyz", "Authorization: [REDACTED]"),
            (
                "curl -H 'authorization: Basic Zm9v'",
                "curl -H 'authorization: [REDACTED]",
            ),
            ("export API_KEY=sekrit", "export API_KEY=[REDACTED]"),
            ("password: sekrit", "password: [REDACTED]"),
            ("password = sekrit", "password = [REDACTED]"),
            ("Bearer eyJhbGciOi.x.y", "Bearer [REDACTED]"),
            (
                "pushed with ghp_0123456789abcdef ok",
                "pushed with [REDACTED] ok",
            ),
            (
                "key=github_pat_11AAAAAAAA_bbbbbbbbbb done",
                "key=[REDACTED] done",
            ),
            ("https://u:p@host/path", "https://[REDACTED]@host/path"),
            (
                "nothing secret here: ghp_short, task-runner-abcdefghij",
                "nothing secret here: ghp_short, task-runner-abcdefghij",
            ),
        ];
        for (input, want) in cases {
            assert_eq!(redact(input), want, "{input}");
        }
    }

    #[test]
    fn truncation_keeps_head_and_tail() {
        let log = format!("HEAD{}TAIL", "x".repeat(10_000));
        let t = truncate_log(&log, 1_000);
        assert!(t.starts_with("HEAD") && t.ends_with("TAIL"));
        assert!(t.len() < 1_100 && t.contains("truncated"));
        assert_eq!(truncate_log("short", 1_000), "short");
        let multi = "é".repeat(5_000);
        assert!(truncate_log(&multi, 1_001).contains("truncated"));
    }

    #[test]
    fn huge_logs_are_bounded_before_nanna() {
        let host = failing_host(&"boom\n".repeat(100_000));
        let nanna = FakeNanna::replying(&reply("real"));
        tick(&host, &nanna, &Config::default(), &mut CiState::default());
        assert!(nanna.requests.borrow()[0].description.len() < MAX_LOG_BYTES + 2_000);
    }

    #[test]
    fn state_roundtrips() {
        let mut s = CiState::default();
        s.analysed.insert(SHA.into());
        let toml = toml::to_string(&s).unwrap();
        assert_eq!(toml::from_str::<CiState>(&toml).unwrap(), s);
    }
}
