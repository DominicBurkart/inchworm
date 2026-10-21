//! Blocking GitHub client for the MVP journeys. Journeys depend on the
//! [`Github`] trait; [`RestGithub`] is the real implementation over the REST
//! API, plus GraphQL for the two draft-state mutations REST does not offer.

use std::fmt;
use std::io::Read;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

const DEFAULT_BASE_URL: &str = "https://api.github.com";
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_MAX_ATTEMPTS: u32 = 3;
/// Longest we are willing to sleep for one rate-limit retry.
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(60);
const PER_PAGE: usize = 100;
/// Upper bound on pages fetched for any listing.
const MAX_PAGES: usize = 10;
/// Cap on a downloaded job log, so a runaway log cannot exhaust memory.
const MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const MAX_REPO_OWNER_LEN: usize = 39;
const MAX_REPO_NAME_LEN: usize = 100;
const MAX_REPO_ECHO_CHARS: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum GithubError {
    #[error("invalid repo {repo:?}, expected owner/name")]
    InvalidRepo { repo: String },
    #[error("invalid commit sha {sha:?}")]
    InvalidSha { sha: String },
    #[error("GITHUB_TOKEN is not set")]
    MissingToken,
    #[error("GitHub returned {status} for {method} {path}: {body}")]
    Status {
        status: u16,
        method: String,
        path: String,
        body: String,
    },
    #[error(
        "GitHub rate limit exceeded for {path} after {attempts} attempt(s), retry after {retry_after_secs:?}s"
    )]
    RateLimited {
        path: String,
        attempts: u32,
        retry_after_secs: Option<u64>,
    },
    #[error("transport error for {path}: {message}")]
    Transport { path: String, message: String },
    #[error("could not decode GitHub response for {path}: {message}")]
    Decode { path: String, message: String },
    #[error("GitHub GraphQL error: {0}")]
    GraphQl(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub state: String,
    pub html_url: String,
    #[serde(rename = "user")]
    pub author: User,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct User {
    pub login: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    /// GraphQL node id, needed for the draft mutations.
    pub node_id: String,
    pub title: String,
    pub head_sha: String,
    pub head_ref: String,
    pub base_ref: String,
    pub draft: bool,
    pub author: String,
    /// GitHub's `mergeable_state` (`clean`, `dirty`, `blocked`, `unknown`, ...).
    /// `unknown` while GitHub is still computing it.
    pub mergeable_state: String,
    /// At least one approving review and no outstanding change request.
    pub approved: bool,
    pub html_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRun {
    pub id: u64,
    pub name: String,
    /// `queued`, `in_progress` or `completed`.
    pub status: String,
    /// `success`, `failure`, `cancelled`, ... Only set once completed.
    pub conclusion: Option<String>,
    pub details_url: Option<String>,
    /// Actions workflow run this check belongs to, parsed from `details_url`.
    /// `None` for checks not produced by GitHub Actions.
    pub workflow_run_id: Option<u64>,
}

impl CheckRun {
    pub fn failed(&self) -> bool {
        matches!(
            self.conclusion.as_deref(),
            Some("failure" | "timed_out" | "startup_failure")
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedComment {
    pub id: u64,
    pub html_url: String,
}

/// Everything the journeys need from GitHub. Blocking; `repo` is `owner/name`.
pub trait Github {
    /// Login of the user the token belongs to.
    fn authenticated_user(&self) -> Result<String, GithubError>;
    /// Open issues (not PRs) whose text matches `query`.
    fn search_open_issues(&self, repo: &str, query: &str) -> Result<Vec<Issue>, GithubError>;
    fn get_issue(&self, repo: &str, number: u64) -> Result<Issue, GithubError>;
    fn create_issue(&self, repo: &str, title: &str, body: &str) -> Result<Issue, GithubError>;
    /// Comments on an issue or a PR (same endpoint).
    fn comment(&self, repo: &str, number: u64, body: &str) -> Result<CreatedComment, GithubError>;
    fn list_open_prs(&self, repo: &str) -> Result<Vec<PullRequest>, GithubError>;
    fn get_pr(&self, repo: &str, number: u64) -> Result<PullRequest, GithubError>;
    fn list_check_runs(&self, repo: &str, sha: &str) -> Result<Vec<CheckRun>, GithubError>;
    /// Plain-text log of one Actions job (a check run id for Actions checks).
    fn job_log(&self, repo: &str, job_id: u64) -> Result<String, GithubError>;
    fn rerun_failed_jobs(&self, repo: &str, run_id: u64) -> Result<(), GithubError>;
    fn convert_to_draft(&self, node_id: &str) -> Result<(), GithubError>;
    fn mark_ready(&self, node_id: &str) -> Result<(), GithubError>;
}

pub struct RestGithub {
    base_url: String,
    token: String,
    agent: ureq::Agent,
    max_attempts: u32,
    max_backoff: Duration,
}

impl fmt::Debug for RestGithub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RestGithub")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .field("max_attempts", &self.max_attempts)
            .finish()
    }
}

impl RestGithub {
    /// Client for api.github.com using `GITHUB_TOKEN`.
    pub fn from_env() -> Result<Self, GithubError> {
        let token = std::env::var("GITHUB_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty())
            .ok_or(GithubError::MissingToken)?;
        Ok(Self::new(DEFAULT_BASE_URL, token))
    }

    /// `base_url` is configurable so tests can point at a local server.
    pub fn new(base_url: &str, token: impl Into<String>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(DEFAULT_CONNECT_TIMEOUT)
            .timeout(DEFAULT_REQUEST_TIMEOUT)
            .redirects(0)
            .user_agent("inchworm")
            .build();
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.into(),
            agent,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            max_backoff: DEFAULT_MAX_BACKOFF,
        }
    }

    /// Total tries per request (1 disables retry) and the longest sleep
    /// between tries.
    pub fn with_retry(mut self, max_attempts: u32, max_backoff: Duration) -> Self {
        self.max_attempts = max_attempts.max(1);
        self.max_backoff = max_backoff;
        self
    }

    /// Sends one request, retrying while GitHub reports a rate limit.
    /// Returns the body of a 2xx response.
    fn send(&self, method: &str, path: &str, body: Option<&Value>) -> Result<String, GithubError> {
        let url = format!("{}{}", self.base_url, path);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let request = self
                .agent
                .request(method, &url)
                .set("Authorization", &format!("Bearer {}", self.token))
                .set("Accept", "application/vnd.github+json")
                .set("X-GitHub-Api-Version", "2022-11-28");
            let result = match body {
                Some(b) => request.send_json(b),
                None => request.call(),
            };
            match result {
                Ok(response) => return read_body(response, path),
                Err(ureq::Error::Status(status, response)) => {
                    let retry_after = rate_limit_wait(status, &response);
                    let text = response.into_string().unwrap_or_default();
                    match retry_after {
                        Some(wait) if attempt < self.max_attempts => {
                            std::thread::sleep(wait.min(self.max_backoff));
                        }
                        Some(wait) => {
                            return Err(GithubError::RateLimited {
                                path: path.to_string(),
                                attempts: attempt,
                                retry_after_secs: Some(wait.as_secs()),
                            });
                        }
                        None => {
                            return Err(GithubError::Status {
                                status,
                                method: method.to_string(),
                                path: path.to_string(),
                                body: text.chars().take(500).collect(),
                            });
                        }
                    }
                }
                Err(ureq::Error::Transport(t)) => {
                    return Err(GithubError::Transport {
                        path: path.to_string(),
                        message: t.to_string(),
                    });
                }
            }
        }
    }

    fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, GithubError> {
        decode(&self.send("GET", path, None)?, path)
    }

    fn post_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<T, GithubError> {
        decode(&self.send("POST", path, Some(body))?, path)
    }

    /// Fetches every page of a listing. `path` must already contain a query
    /// string; `pick` extracts the items from one page.
    fn paginate<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        pick: impl Fn(Value) -> Value,
    ) -> Result<Vec<T>, GithubError> {
        let mut all = Vec::new();
        for page in 1..=MAX_PAGES {
            let page_path = format!("{path}&per_page={PER_PAGE}&page={page}");
            let items: Vec<T> = decode_value(pick(self.get_json(&page_path)?), &page_path)?;
            let full = items.len() >= PER_PAGE;
            all.extend(items);
            if !full {
                break;
            }
        }
        Ok(all)
    }

    fn graphql(&self, query: &str, node_id: &str) -> Result<(), GithubError> {
        let reply: Value = self.post_json(
            "/graphql",
            &json!({ "query": query, "variables": { "id": node_id } }),
        )?;
        match reply.get("errors") {
            Some(errors) if !errors.is_null() => Err(GithubError::GraphQl(
                errors.to_string().chars().take(500).collect(),
            )),
            _ => Ok(()),
        }
    }

    fn approved(&self, repo: &str, number: u64) -> Result<bool, GithubError> {
        let reviews: Vec<RawReview> = self.paginate(
            &format!("/repos/{repo}/pulls/{number}/reviews?"),
            std::convert::identity,
        )?;
        // The latest decisive review per reviewer counts.
        let mut latest: Vec<(String, String)> = Vec::new();
        for review in reviews {
            if !matches!(
                review.state.as_str(),
                "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED"
            ) {
                continue;
            }
            let login = review.user.map(|u| u.login).unwrap_or_default();
            latest.retain(|(l, _)| *l != login);
            latest.push((login, review.state));
        }
        Ok(latest.iter().any(|(_, s)| s == "APPROVED")
            && !latest.iter().any(|(_, s)| s == "CHANGES_REQUESTED"))
    }

    fn pr_from_raw(&self, repo: &str, raw: RawPr) -> Result<PullRequest, GithubError> {
        let approved = self.approved(repo, raw.number)?;
        Ok(PullRequest {
            number: raw.number,
            node_id: raw.node_id,
            title: raw.title,
            head_sha: raw.head.sha,
            head_ref: raw.head.ref_name,
            base_ref: raw.base.ref_name,
            draft: raw.draft,
            author: raw.user.login,
            mergeable_state: raw.mergeable_state.unwrap_or_else(|| "unknown".into()),
            approved,
            html_url: raw.html_url,
        })
    }
}

impl Github for RestGithub {
    fn authenticated_user(&self) -> Result<String, GithubError> {
        Ok(self.get_json::<User>("/user")?.login)
    }

    fn search_open_issues(&self, repo: &str, query: &str) -> Result<Vec<Issue>, GithubError> {
        validate_repo(repo)?;
        let q = format!("repo:{repo} is:issue is:open {query}");
        self.paginate(&format!("/search/issues?q={}", url_encode(&q)), |page| {
            page.get("items").cloned().unwrap_or(Value::Null)
        })
    }

    fn get_issue(&self, repo: &str, number: u64) -> Result<Issue, GithubError> {
        validate_repo(repo)?;
        self.get_json(&format!("/repos/{repo}/issues/{number}"))
    }

    fn create_issue(&self, repo: &str, title: &str, body: &str) -> Result<Issue, GithubError> {
        validate_repo(repo)?;
        self.post_json(
            &format!("/repos/{repo}/issues"),
            &json!({ "title": title, "body": body }),
        )
    }

    fn comment(&self, repo: &str, number: u64, body: &str) -> Result<CreatedComment, GithubError> {
        validate_repo(repo)?;
        let created: RawComment = self.post_json(
            &format!("/repos/{repo}/issues/{number}/comments"),
            &json!({ "body": body }),
        )?;
        Ok(CreatedComment {
            id: created.id,
            html_url: created.html_url,
        })
    }

    fn list_open_prs(&self, repo: &str) -> Result<Vec<PullRequest>, GithubError> {
        validate_repo(repo)?;
        let listed: Vec<RawPr> = self.paginate(
            &format!("/repos/{repo}/pulls?state=open"),
            std::convert::identity,
        )?;
        // The list endpoint omits mergeable_state, so fetch each PR.
        listed
            .into_iter()
            .map(|p| self.get_pr(repo, p.number))
            .collect()
    }

    fn get_pr(&self, repo: &str, number: u64) -> Result<PullRequest, GithubError> {
        validate_repo(repo)?;
        let raw: RawPr = self.get_json(&format!("/repos/{repo}/pulls/{number}"))?;
        self.pr_from_raw(repo, raw)
    }

    fn list_check_runs(&self, repo: &str, sha: &str) -> Result<Vec<CheckRun>, GithubError> {
        validate_repo(repo)?;
        if sha.len() < 7 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(GithubError::InvalidSha {
                sha: sha.chars().take(MAX_REPO_ECHO_CHARS).collect(),
            });
        }
        let raw: Vec<RawCheckRun> = self.paginate(
            &format!("/repos/{repo}/commits/{sha}/check-runs?filter=latest"),
            |page| page.get("check_runs").cloned().unwrap_or(Value::Null),
        )?;
        Ok(raw
            .into_iter()
            .map(|c| CheckRun {
                workflow_run_id: c.details_url.as_deref().and_then(parse_run_id),
                id: c.id,
                name: c.name,
                status: c.status,
                conclusion: c.conclusion,
                details_url: c.details_url,
            })
            .collect())
    }

    fn job_log(&self, repo: &str, job_id: u64) -> Result<String, GithubError> {
        validate_repo(repo)?;
        let path = format!("/repos/{repo}/actions/jobs/{job_id}/logs");
        // The API answers 302 to a short-lived storage URL. Follow it by hand
        // so the token is never sent to a different host.
        let mut url = format!("{}{}", self.base_url, path);
        let mut authed = true;
        for _ in 0..=MAX_REDIRECTS {
            let mut request = self.agent.get(&url);
            if authed {
                request = request
                    .set("Authorization", &format!("Bearer {}", self.token))
                    .set("Accept", "application/vnd.github+json");
            }
            match request.call() {
                Ok(response) if (300..400).contains(&response.status()) => {
                    url = response
                        .header("Location")
                        .ok_or_else(|| GithubError::Decode {
                            path: path.clone(),
                            message: "redirect without Location".into(),
                        })?
                        .to_string();
                    authed = false;
                }
                Ok(response) => {
                    let mut text = String::new();
                    response
                        .into_reader()
                        .take(MAX_LOG_BYTES)
                        .read_to_string(&mut text)
                        .map_err(|e| GithubError::Transport {
                            path: path.clone(),
                            message: e.to_string(),
                        })?;
                    return Ok(text);
                }
                Err(ureq::Error::Status(status, response)) => {
                    return Err(GithubError::Status {
                        status,
                        method: "GET".into(),
                        path,
                        body: response
                            .into_string()
                            .unwrap_or_default()
                            .chars()
                            .take(500)
                            .collect(),
                    });
                }
                Err(ureq::Error::Transport(t)) => {
                    return Err(GithubError::Transport {
                        path,
                        message: t.to_string(),
                    });
                }
            }
        }
        Err(GithubError::Decode {
            path,
            message: "too many redirects".into(),
        })
    }

    fn rerun_failed_jobs(&self, repo: &str, run_id: u64) -> Result<(), GithubError> {
        validate_repo(repo)?;
        self.send(
            "POST",
            &format!("/repos/{repo}/actions/runs/{run_id}/rerun-failed-jobs"),
            Some(&json!({})),
        )?;
        Ok(())
    }

    fn convert_to_draft(&self, node_id: &str) -> Result<(), GithubError> {
        self.graphql(
            "mutation($id: ID!) { convertPullRequestToDraft(input: {pullRequestId: $id}) { pullRequest { id } } }",
            node_id,
        )
    }

    fn mark_ready(&self, node_id: &str) -> Result<(), GithubError> {
        self.graphql(
            "mutation($id: ID!) { markPullRequestReadyForReview(input: {pullRequestId: $id}) { pullRequest { id } } }",
            node_id,
        )
    }
}

#[derive(Deserialize)]
struct RawPr {
    number: u64,
    node_id: String,
    title: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    mergeable_state: Option<String>,
    user: User,
    head: RawRef,
    base: RawRef,
}

#[derive(Deserialize)]
struct RawRef {
    sha: String,
    #[serde(rename = "ref")]
    ref_name: String,
}

#[derive(Deserialize)]
struct RawReview {
    state: String,
    user: Option<User>,
}

#[derive(Deserialize)]
struct RawComment {
    id: u64,
    html_url: String,
}

#[derive(Deserialize)]
struct RawCheckRun {
    id: u64,
    name: String,
    status: String,
    conclusion: Option<String>,
    details_url: Option<String>,
}

/// `https://github.com/o/r/actions/runs/123/job/456` -> 123.
fn parse_run_id(details_url: &str) -> Option<u64> {
    let rest = details_url.split("/actions/runs/").nth(1)?;
    rest.split(['/', '?']).next()?.parse().ok()
}

/// How long to wait before retrying, if this response is a rate limit.
fn rate_limit_wait(status: u16, response: &ureq::Response) -> Option<Duration> {
    if status != 429 && status != 403 {
        return None;
    }
    if let Some(secs) = response
        .header("Retry-After")
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        return Some(Duration::from_secs(secs));
    }
    if status == 429 {
        return Some(Duration::from_secs(1));
    }
    if response.header("X-RateLimit-Remaining").map(str::trim) == Some("0") {
        let reset = response
            .header("X-RateLimit-Reset")
            .and_then(|v| v.trim().parse::<u64>().ok())?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        return Some(Duration::from_secs(reset.saturating_sub(now)));
    }
    None
}

fn read_body(response: ureq::Response, path: &str) -> Result<String, GithubError> {
    response.into_string().map_err(|e| GithubError::Transport {
        path: path.to_string(),
        message: e.to_string(),
    })
}

fn decode<T: serde::de::DeserializeOwned>(text: &str, path: &str) -> Result<T, GithubError> {
    serde_json::from_str(text).map_err(|e| GithubError::Decode {
        path: path.to_string(),
        message: e.to_string(),
    })
}

fn decode_value<T: serde::de::DeserializeOwned>(
    value: Value,
    path: &str,
) -> Result<T, GithubError> {
    serde_json::from_value(value).map_err(|e| GithubError::Decode {
        path: path.to_string(),
        message: e.to_string(),
    })
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn valid_repo_segment(segment: &str, max_len: usize) -> bool {
    !segment.is_empty()
        && segment.len() <= max_len
        && segment != "."
        && segment != ".."
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn validate_repo(repo: &str) -> Result<(), GithubError> {
    let valid = match repo.split_once('/') {
        Some((owner, name)) => {
            valid_repo_segment(owner, MAX_REPO_OWNER_LEN)
                && valid_repo_segment(name, MAX_REPO_NAME_LEN)
        }
        None => false,
    };
    if valid {
        Ok(())
    } else {
        Err(GithubError::InvalidRepo {
            repo: repo.chars().take(MAX_REPO_ECHO_CHARS).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone)]
    struct Seen {
        url: String,
        auth: Option<String>,
        body: String,
    }

    struct Reply {
        method: &'static str,
        url: &'static str,
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: String,
    }

    fn reply(method: &'static str, url: &'static str, status: u16, body: &str) -> Reply {
        Reply {
            method,
            url,
            status,
            headers: vec![],
            body: body.to_string(),
        }
    }

    /// Serves canned replies; each reply answers one matching request, in order.
    struct Fake {
        base: String,
        seen: Arc<Mutex<Vec<Seen>>>,
        server: Arc<tiny_http::Server>,
    }

    impl Fake {
        fn start(replies: Vec<Reply>) -> Fake {
            let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
            let base = format!("http://{}", server.server_addr().to_ip().unwrap());
            let seen = Arc::new(Mutex::new(Vec::new()));
            let (s, log) = (server.clone(), seen.clone());
            let mut replies = replies;
            std::thread::spawn(move || {
                for mut req in s.incoming_requests() {
                    let mut body = String::new();
                    req.as_reader().read_to_string(&mut body).unwrap();
                    let method = req.method().as_str().to_string();
                    let url = req.url().to_string();
                    let auth = req
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv("Authorization"))
                        .map(|h| h.value.to_string());
                    log.lock().unwrap().push(Seen {
                        url: url.clone(),
                        auth,
                        body,
                    });
                    let pos = replies
                        .iter()
                        .position(|r| r.method == method && url.starts_with(r.url));
                    let response = match pos {
                        Some(i) => {
                            let r = replies.remove(i);
                            let mut resp =
                                tiny_http::Response::from_string(r.body).with_status_code(r.status);
                            for (k, v) in r.headers {
                                resp.add_header(
                                    tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes())
                                        .unwrap(),
                                );
                            }
                            resp
                        }
                        None => tiny_http::Response::from_string("no route").with_status_code(404),
                    };
                    let _ = req.respond(response);
                }
            });
            Fake { base, seen, server }
        }

        fn client(&self) -> RestGithub {
            RestGithub::new(&self.base, "secret-token").with_retry(3, Duration::ZERO)
        }

        fn seen(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.server.unblock();
        }
    }

    const PR_JSON: &str = r#"{"number":7,"node_id":"PR_node","title":"t","html_url":"u","draft":false,
        "mergeable_state":"dirty","user":{"login":"alice"},
        "head":{"sha":"abc1234","ref":"feat"},"base":{"sha":"d","ref":"main"}}"#;

    #[test]
    fn debug_redacts_token() {
        let g = RestGithub::new("http://x", "supersecret");
        let shown = format!("{g:?}");
        assert!(!shown.contains("supersecret"));
        assert!(shown.contains("redacted"));
    }

    #[test]
    fn repo_validation() {
        for ok in ["a/b", "Dominic-Burkart/inchworm", "o/r.rs_1"] {
            assert!(validate_repo(ok).is_ok(), "{ok}");
        }
        for bad in [
            "", "a", "a/b/c", "a/", "/b", "../x", "a/..", "a b/c", "a/b?x=1",
        ] {
            assert!(validate_repo(bad).is_err(), "{bad}");
        }
        let g = RestGithub::new("http://127.0.0.1:1", "t");
        assert!(matches!(
            g.get_issue("nope", 1),
            Err(GithubError::InvalidRepo { .. })
        ));
    }

    #[test]
    fn parses_run_id() {
        assert_eq!(
            parse_run_id("https://github.com/o/r/actions/runs/123/job/456"),
            Some(123)
        );
        assert_eq!(parse_run_id("https://example.com/check"), None);
    }

    #[test]
    fn authenticated_user_sends_token() {
        let fake = Fake::start(vec![reply("GET", "/user", 200, r#"{"login":"bob"}"#)]);
        assert_eq!(fake.client().authenticated_user().unwrap(), "bob");
        assert_eq!(fake.seen()[0].auth.as_deref(), Some("Bearer secret-token"));
    }

    #[test]
    fn search_issues_encodes_query_and_returns_author() {
        let fake = Fake::start(vec![reply(
            "GET",
            "/search/issues",
            200,
            r#"{"items":[{"number":3,"title":"bug","body":null,"state":"open",
                "html_url":"h","user":{"login":"alice"}}]}"#,
        )]);
        let issues = fake
            .client()
            .search_open_issues("o/r", "label:x y")
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].author.login, "alice");
        let url = &fake.seen()[0].url;
        assert!(url.contains("q=repo%3Ao%2Fr%20is%3Aissue%20is%3Aopen%20label%3Ax%20y"));
    }

    #[test]
    fn list_open_prs_fills_mergeable_state_and_approval() {
        let fake = Fake::start(vec![
            reply(
                "GET",
                "/repos/o/r/pulls?state=open",
                200,
                &format!("[{PR_JSON}]"),
            ),
            reply(
                "GET",
                "/repos/o/r/pulls/7/reviews",
                200,
                r#"[
                {"state":"CHANGES_REQUESTED","user":{"login":"carol"}},
                {"state":"COMMENTED","user":{"login":"dave"}},
                {"state":"APPROVED","user":{"login":"carol"}}]"#,
            ),
            reply("GET", "/repos/o/r/pulls/7", 200, PR_JSON),
        ]);
        let prs = fake.client().list_open_prs("o/r").unwrap();
        assert_eq!(prs.len(), 1);
        let pr = &prs[0];
        assert_eq!(pr.head_sha, "abc1234");
        assert_eq!(pr.mergeable_state, "dirty");
        assert_eq!(pr.author, "alice");
        assert!(!pr.draft);
        assert!(pr.approved);
    }

    #[test]
    fn outstanding_change_request_is_not_approved() {
        let fake = Fake::start(vec![
            reply(
                "GET",
                "/repos/o/r/pulls/7/reviews",
                200,
                r#"[
                {"state":"APPROVED","user":{"login":"a"}},
                {"state":"CHANGES_REQUESTED","user":{"login":"b"}}]"#,
            ),
            reply("GET", "/repos/o/r/pulls/7", 200, PR_JSON),
        ]);
        assert!(!fake.client().get_pr("o/r", 7).unwrap().approved);
    }

    #[test]
    fn check_runs_carry_conclusion_and_run_id() {
        let fake = Fake::start(vec![reply(
            "GET",
            "/repos/o/r/commits/abc1234/check-runs",
            200,
            r#"{"total_count":2,"check_runs":[
              {"id":9,"name":"test","status":"completed","conclusion":"failure",
               "details_url":"https://github.com/o/r/actions/runs/55/job/9"},
              {"id":10,"name":"lint","status":"in_progress","conclusion":null,"details_url":null}]}"#,
        )]);
        let runs = fake.client().list_check_runs("o/r", "abc1234").unwrap();
        assert_eq!(runs[0].workflow_run_id, Some(55));
        assert!(runs[0].failed());
        assert_eq!(runs[1].conclusion, None);
        assert!(!runs[1].failed());
        assert!(matches!(
            fake.client().list_check_runs("o/r", "../x"),
            Err(GithubError::InvalidSha { .. })
        ));
    }

    #[test]
    fn job_log_follows_redirect_without_token() {
        let storage = Fake::start(vec![reply("GET", "/blob", 200, "step failed\n")]);
        let mut redirect = reply("GET", "/repos/o/r/actions/jobs/9/logs", 302, "");
        redirect
            .headers
            .push(("Location", format!("{}/blob", storage.base)));
        let api = Fake::start(vec![redirect]);
        let log = api.client().job_log("o/r", 9).unwrap();
        assert_eq!(log, "step failed\n");
        assert_eq!(api.seen()[0].auth.as_deref(), Some("Bearer secret-token"));
        assert_eq!(storage.seen()[0].auth, None);
    }

    #[test]
    fn rerun_and_comment_and_create_issue() {
        let fake = Fake::start(vec![
            reply(
                "POST",
                "/repos/o/r/actions/runs/55/rerun-failed-jobs",
                201,
                "",
            ),
            reply(
                "POST",
                "/repos/o/r/issues/7/comments",
                201,
                r#"{"id":1,"html_url":"c"}"#,
            ),
            reply(
                "POST",
                "/repos/o/r/issues",
                201,
                r#"{"number":8,"title":"T","body":"B","state":"open","html_url":"h","user":{"login":"me"}}"#,
            ),
        ]);
        let g = fake.client();
        g.rerun_failed_jobs("o/r", 55).unwrap();
        assert_eq!(g.comment("o/r", 7, "hi").unwrap().id, 1);
        assert_eq!(g.create_issue("o/r", "T", "B").unwrap().number, 8);
        let seen = fake.seen();
        assert!(seen[1].body.contains(r#""body":"hi""#));
        assert!(seen[2].body.contains(r#""title":"T""#));
    }

    #[test]
    fn graphql_draft_mutations() {
        let fake = Fake::start(vec![
            reply("POST", "/graphql", 200, r#"{"data":{}}"#),
            reply("POST", "/graphql", 200, r#"{"data":{}}"#),
            reply(
                "POST",
                "/graphql",
                200,
                r#"{"errors":[{"message":"nope"}]}"#,
            ),
        ]);
        let g = fake.client();
        g.convert_to_draft("PR_node").unwrap();
        g.mark_ready("PR_node").unwrap();
        assert!(matches!(
            g.mark_ready("PR_node"),
            Err(GithubError::GraphQl(m)) if m.contains("nope")
        ));
        let seen = fake.seen();
        assert!(seen[0].body.contains("convertPullRequestToDraft"));
        assert!(seen[0].body.contains("PR_node"));
        assert!(seen[1].body.contains("markPullRequestReadyForReview"));
    }

    #[test]
    fn retries_rate_limit_then_succeeds() {
        let mut limited = reply("GET", "/user", 403, "rate limited");
        limited.headers.push(("Retry-After", "0".into()));
        let fake = Fake::start(vec![
            limited,
            reply("GET", "/user", 200, r#"{"login":"bob"}"#),
        ]);
        assert_eq!(fake.client().authenticated_user().unwrap(), "bob");
        assert_eq!(fake.seen().len(), 2);
    }

    #[test]
    fn rate_limit_gives_up_after_max_attempts() {
        let limited = || {
            let mut r = reply("GET", "/user", 429, "slow down");
            r.headers.push(("Retry-After", "0".into()));
            r
        };
        let fake = Fake::start(vec![limited(), limited()]);
        let err = fake
            .client()
            .with_retry(2, Duration::ZERO)
            .authenticated_user()
            .unwrap_err();
        assert!(matches!(err, GithubError::RateLimited { attempts: 2, .. }));
        assert_eq!(fake.seen().len(), 2);
    }

    #[test]
    fn plain_403_is_not_retried() {
        let fake = Fake::start(vec![reply("GET", "/user", 403, "forbidden")]);
        let err = fake.client().authenticated_user().unwrap_err();
        assert!(matches!(err, GithubError::Status { status: 403, .. }));
        assert_eq!(fake.seen().len(), 1);
    }
}
