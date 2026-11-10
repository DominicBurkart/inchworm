use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::config::{Flow, FlowFile};
use crate::github::{FakeGitHub, Issue, PrState, PullRequest};
use crate::judges::{LlmError, Scorer};
use crate::outcome::{CiPolicy, OutcomeEnv};

pub struct FakeScorer {
    replies: Mutex<VecDeque<Result<String, LlmError>>>,
    prompts: Mutex<Vec<String>>,
}

impl FakeScorer {
    pub fn new() -> FakeScorer {
        FakeScorer::replying(Vec::new())
    }

    pub fn replying(replies: Vec<Result<String, LlmError>>) -> FakeScorer {
        FakeScorer {
            replies: Mutex::new(replies.into()),
            prompts: Mutex::new(Vec::new()),
        }
    }

    pub fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

impl Scorer for FakeScorer {
    async fn complete(&self, _model: &str, prompt: &str) -> Result<String, LlmError> {
        self.prompts.lock().unwrap().push(prompt.to_string());
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(r#"{"score": 9, "reason": "fine"}"#.to_string()))
    }
}

pub fn env(scorer: &FakeScorer) -> OutcomeEnv<'_> {
    OutcomeEnv {
        scorer,
        model_permit: Arc::new(Semaphore::new(1)),
        ci: CiPolicy {
            poll_interval: Duration::from_secs(10),
            timeout: Duration::from_secs(60),
        },
    }
}

pub fn flow_with(max_attempts: u32, retry_on: &str, extra: &str) -> Flow {
    FlowFile::parse(&format!(
        r#"
[[flow]]
name = "issue"
max_attempts = {max_attempts}
retry_on = [{retry_on}]
trigger = {{ kind = "label", label = "inchworm" }}
[[flow.step]]
name = "implement"
identity = "rust-implementer"
[[flow.step]]
name = "publish"
identity = "pr-shepherd"
{extra}
"#
    ))
    .unwrap()
    .flow
    .remove(0)
}

pub const DRAFT_OUTCOME: &str = "[flow.outcome]\nmode = \"draft_pr\"\n";

pub fn github_with_issue() -> FakeGitHub {
    let github = FakeGitHub::new();
    github.add_issue(
        "o/r",
        Issue {
            number: 5,
            title: "Do it".into(),
            body: "please".into(),
            labels: vec!["inchworm".into()],
            url: "https://github.com/o/r/issues/5".into(),
        },
    );
    github
}

pub fn pr_at(number: u64, sha: &str) -> PullRequest {
    PullRequest {
        number,
        head_sha: sha.into(),
        is_draft: true,
        state: PrState::Open,
        url: format!("https://github.com/o/r/pull/{number}"),
    }
}
