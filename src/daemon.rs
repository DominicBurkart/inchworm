use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::Notify;

use crate::clock::Clock;
use crate::config::{FlowFile, GlobalConfig, RepoEntry, Trigger};
use crate::flow::Dispatcher;
use crate::github::GitHub;
use crate::judges::DynScorer;
use crate::model::RunState;
use crate::nanna_client::{AssignRequest, ClientError, NannaClient, TaskOutcome};
use crate::outcome::{CiPolicy, OutcomeEnv};
use crate::policy::HumanGate;
use crate::run::{RunContext, enqueue_issue, execute_run};
use crate::scheduler::Scheduler;
use crate::store::{RunId, Store, StoreError};

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("cannot start the worker: {0}")]
    Worker(String),
    #[error("the worker thread stopped unexpectedly")]
    WorkerDied,
    #[error("another inchworm daemon holds {0}; refusing to start a second instance")]
    AlreadyRunning(PathBuf),
    #[error("cannot create the instance lock {path}: {detail}")]
    Lock { path: PathBuf, detail: String },
}

#[derive(Debug)]
pub struct InstanceLock {
    _file: std::fs::File,
    path: PathBuf,
}

impl InstanceLock {
    /// ```
    /// use inchworm::daemon::InstanceLock;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let db = dir.path().join("state.db");
    /// let first = InstanceLock::acquire(&db).unwrap();
    /// assert!(InstanceLock::acquire(&db).is_err());
    /// drop(first);
    /// assert!(InstanceLock::acquire(&db).is_ok());
    /// ```
    pub fn acquire(state_db: &std::path::Path) -> Result<InstanceLock, DaemonError> {
        let mut name = state_db.as_os_str().to_owned();
        name.push(".lock");
        let path = PathBuf::from(name);
        let lock_error = |err: std::io::Error| DaemonError::Lock {
            path: path.clone(),
            detail: err.to_string(),
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(lock_error)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(lock_error)?;
        match file.try_lock() {
            Ok(()) => Ok(InstanceLock { _file: file, path }),
            Err(std::fs::TryLockError::WouldBlock) => Err(DaemonError::AlreadyRunning(path)),
            Err(std::fs::TryLockError::Error(err)) => Err(lock_error(err)),
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

#[derive(Debug, Clone)]
pub struct DaemonSettings {
    pub repos: Vec<RepoEntry>,
    pub state_db: PathBuf,
    pub poll: Duration,
    pub concurrency: usize,
    pub ci: CiPolicy,
    pub grace: Duration,
}

impl DaemonSettings {
    /// ```
    /// use inchworm::config::GlobalConfig;
    /// use inchworm::daemon::DaemonSettings;
    ///
    /// let config = GlobalConfig::parse("nanna_bin = \"nanna\"\npoll_secs = 7\n[[repo]]\nslug = \"o/r\"\npath = \"/r\"\n").unwrap();
    /// let settings = DaemonSettings::from_config(&config);
    /// assert_eq!(settings.poll.as_secs(), 7);
    /// assert_eq!(settings.concurrency, 1);
    /// assert_eq!(settings.repos.len(), 1);
    /// ```
    pub fn from_config(config: &GlobalConfig) -> DaemonSettings {
        DaemonSettings {
            repos: config.repo.clone(),
            state_db: config.state_db.clone(),
            poll: Duration::from_secs(config.poll_secs),
            concurrency: config.concurrency,
            ci: CiPolicy::default(),
            grace: Duration::from_secs(20),
        }
    }
}

pub trait DispatcherFactory: Send + Sync {
    fn make(&self) -> Result<Box<dyn Dispatcher>, String>;
}

pub struct NannaFactory {
    bin: PathBuf,
}

impl NannaFactory {
    pub fn new(bin: PathBuf) -> NannaFactory {
        NannaFactory { bin }
    }
}

impl DispatcherFactory for NannaFactory {
    fn make(&self) -> Result<Box<dyn Dispatcher>, String> {
        let mut client = NannaClient::spawn(&self.bin)
            .map_err(|err| format!("cannot start `{} mcp-serve`: {err}", self.bin.display()))?;
        client
            .initialize()
            .map_err(|err| format!("nanna initialize failed: {err}"))?;
        Ok(Box::new(client))
    }
}

struct Reconnecting {
    factory: Arc<dyn DispatcherFactory>,
    inner: Option<Box<dyn Dispatcher>>,
}

impl Dispatcher for Reconnecting {
    fn dispatch(
        &mut self,
        request: &AssignRequest,
        timeout: Duration,
    ) -> Result<TaskOutcome, ClientError> {
        if self.inner.is_none() {
            match self.factory.make() {
                Ok(dispatcher) => self.inner = Some(dispatcher),
                Err(message) => return Err(ClientError::Protocol(message)),
            }
        }
        let result = match self.inner.as_mut() {
            Some(dispatcher) => dispatcher.dispatch(request, timeout),
            None => return Err(ClientError::Protocol("no dispatcher".into())),
        };
        if result.is_err() {
            self.inner = None;
        }
        result
    }
}

pub struct DaemonDeps {
    pub github: Arc<dyn GitHub + Send + Sync>,
    pub clock: Arc<dyn Clock>,
    pub dispatchers: Arc<dyn DispatcherFactory>,
    pub scorer: Arc<dyn DynScorer>,
}

#[derive(Debug, Clone)]
struct Job {
    run_id: RunId,
    flow: String,
    repo: RepoEntry,
}

fn log(message: &str) {
    eprintln!("inchworm: {message}");
}

fn load_flows(repo: &RepoEntry) -> Result<FlowFile, String> {
    let path = repo.path.join(".inchworm/flows.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    FlowFile::parse(&text).map_err(|err| format!("{}: {err}", path.display()))
}

async fn process(
    job: Job,
    store: &Store,
    deps: &DaemonDeps,
    scheduler: &Scheduler<Job>,
    ci: CiPolicy,
    _lease: crate::scheduler::Lease<Job>,
) -> Option<Job> {
    let repo = &job.repo;
    let file = match load_flows(repo) {
        Ok(file) => file,
        Err(err) => {
            log(&format!("{}: {err}; will retry", repo.slug));
            return Some(job);
        }
    };
    let Some(flow) = file.flow.iter().find(|flow| flow.name == job.flow) else {
        log(&format!(
            "{}: run {} uses flow `{}` which is no longer configured",
            repo.slug, job.run_id, job.flow
        ));
        return None;
    };
    let ctx = RunContext {
        store,
        github: deps.github.as_ref(),
        slug: &repo.slug,
        repo_path: &repo.path,
        clock: deps.clock.as_ref(),
    };
    let env = OutcomeEnv {
        scorer: deps.scorer.as_ref(),
        model_permit: scheduler.model_permit(),
        ci,
        human_gate: HumanGate::NotConsulted,
    };
    let mut dispatcher = Reconnecting {
        factory: Arc::clone(&deps.dispatchers),
        inner: None,
    };
    match execute_run(&ctx, &env, flow, job.run_id, &mut dispatcher, None).await {
        Ok(summary) => log(&format!(
            "{}: run {} finished as {}",
            repo.slug, job.run_id, summary.state
        )),
        Err(err) => log(&format!("{}: run {} errored: {err}", repo.slug, job.run_id)),
    }
    None
}

async fn worker(
    settings: DaemonSettings,
    store: Store,
    deps: Arc<DaemonDeps>,
    scheduler: Scheduler<Job>,
    stop: Arc<Notify>,
) {
    let mut deferred: Vec<Job> = Vec::new();
    loop {
        let lease = tokio::select! {
            () = stop.notified() => return,
            lease = scheduler.next() => lease,
            () = deps.clock.sleep(settings.poll), if !deferred.is_empty() => {
                for job in deferred.drain(..) {
                    let repo = job.repo.slug.clone();
                    scheduler.enqueue(&repo, job);
                }
                continue;
            }
        };
        let job = lease.item().clone();
        if let Some(retry) = process(job, &store, &deps, &scheduler, settings.ci, lease).await {
            deferred.push(retry);
        }
    }
}

type Unswapped = HashSet<(String, u64)>;

fn poll_repo(
    repo: &RepoEntry,
    store: &Store,
    deps: &DaemonDeps,
    scheduler: &Scheduler<Job>,
    unswapped: &mut Unswapped,
) -> Result<(), String> {
    let file = load_flows(repo)?;
    let ctx = RunContext {
        store,
        github: deps.github.as_ref(),
        slug: &repo.slug,
        repo_path: &repo.path,
        clock: deps.clock.as_ref(),
    };
    for flow in &file.flow {
        let Trigger::Label { label } = &flow.trigger;
        let issues = deps
            .github
            .open_issues_with_label(&repo.slug, label)
            .map_err(|err| err.to_string())?;
        let listed: HashSet<u64> = issues.iter().map(|issue| issue.number).collect();
        unswapped.retain(|(slug, number)| *slug != repo.slug || listed.contains(number));
        for issue in issues {
            let key = (repo.slug.clone(), issue.number);
            if unswapped.contains(&key) {
                continue;
            }
            let mut warnings = Vec::new();
            match enqueue_issue(&ctx, flow, issue.number, &mut warnings) {
                Ok(Some(run_id)) => {
                    if !warnings.is_empty() {
                        unswapped.insert(key);
                    }
                    log(&format!(
                        "{}#{}: queued run {run_id} for flow `{}`",
                        repo.slug, issue.number, flow.name
                    ));
                    scheduler.enqueue(
                        &repo.slug,
                        Job {
                            run_id,
                            flow: flow.name.clone(),
                            repo: repo.clone(),
                        },
                    );
                }
                Ok(None) => {}
                Err(err) => return Err(format!("{}#{}: {err}", repo.slug, issue.number)),
            }
            for warning in warnings {
                log(&format!("{}#{}: {warning}", repo.slug, issue.number));
            }
        }
    }
    Ok(())
}

fn poll_all(
    settings: &DaemonSettings,
    deps: &DaemonDeps,
    scheduler: &Scheduler<Job>,
    unswapped: &mut Unswapped,
) {
    let store = match Store::open(&settings.state_db) {
        Ok(store) => store,
        Err(err) => return log(&format!("cannot open the state database to poll: {err}")),
    };
    for repo in &settings.repos {
        if let Err(err) = poll_repo(repo, &store, deps, scheduler, unswapped) {
            log(&format!("{}: {err}", repo.slug));
        }
    }
}

fn resume(
    settings: &DaemonSettings,
    store: &Store,
    deps: &DaemonDeps,
    scheduler: &Scheduler<Job>,
) -> Result<(), StoreError> {
    let lost = store.recover_after_crash(deps.clock.now())?;
    if lost > 0 {
        log(&format!(
            "marked {lost} dispatched attempt(s) as orchestrator_lost"
        ));
    }
    let open = store.runs_in_states(&[
        RunState::Queued,
        RunState::Running,
        RunState::AwaitingCi,
        RunState::Judging,
    ])?;
    for run in open {
        let Some(repo) = settings.repos.iter().find(|repo| repo.slug == run.repo) else {
            log(&format!(
                "run {} ({}#{}) belongs to a repo that is no longer configured; leaving it as {}",
                run.id, run.repo, run.issue, run.state
            ));
            continue;
        };
        {
            log(&format!(
                "resuming run {} ({}#{})",
                run.id, run.repo, run.issue
            ));
            scheduler.enqueue(
                &run.repo,
                Job {
                    run_id: run.id,
                    flow: run.flow,
                    repo: repo.clone(),
                },
            );
        }
    }
    Ok(())
}

pub async fn run_daemon(
    settings: DaemonSettings,
    deps: DaemonDeps,
    shutdown: impl Future<Output = ()>,
) -> Result<(), DaemonError> {
    let _instance = InstanceLock::acquire(&settings.state_db)?;
    let store = Store::open(&settings.state_db)?;
    let scheduler: Scheduler<Job> = Scheduler::new(settings.concurrency);
    resume(&settings, &store, &deps, &scheduler)?;

    let worker_store = Store::open(&settings.state_db)?;
    let deps = Arc::new(deps);
    let stop = Arc::new(Notify::new());
    let handle = {
        let settings = settings.clone();
        let deps = Arc::clone(&deps);
        let scheduler = scheduler.clone();
        let stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("inchworm-worker".into())
            .spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a current-thread runtime can always be built")
                    .block_on(worker(settings, worker_store, deps, scheduler, stop));
            })
            .map_err(|err| DaemonError::Worker(err.to_string()))?
    };

    tokio::pin!(shutdown);
    let mut unswapped = Unswapped::new();
    let outcome = loop {
        let (done, finished) = tokio::sync::oneshot::channel();
        {
            let settings = settings.clone();
            let deps = Arc::clone(&deps);
            let scheduler = scheduler.clone();
            let mut taken = std::mem::take(&mut unswapped);
            std::thread::spawn(move || {
                poll_all(&settings, &deps, &scheduler, &mut taken);
                let _ = done.send(taken);
            });
        }
        tokio::select! {
            () = &mut shutdown => break Ok(()),
            taken = finished => {
                if let Ok(taken) = taken {
                    unswapped = taken;
                } else {
                    log("a poll cycle died; starting over");
                }
            }
        }
        if handle.is_finished() {
            break Err(DaemonError::WorkerDied);
        }
        tokio::select! {
            () = &mut shutdown => break Ok(()),
            () = deps.clock.sleep(settings.poll) => {}
        }
    };

    stop.notify_one();
    let deadline = std::time::Instant::now() + settings.grace;
    while !handle.is_finished() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if handle.is_finished() {
        let _ = handle.join();
    } else {
        log("stopping with a run still in flight; it will be marked orchestrator_lost on restart");
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::SystemClock;
    use crate::github::FakeGitHub;
    use crate::model::FailureClass;
    use crate::nanna_client::fake::{Scenario, connect};
    use crate::policy::{CheckConclusion, CheckRun};
    use crate::store::AttemptState;
    use crate::testing::{FakeScorer, github_with_issue, pr_at};
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Instant;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    const FLOWS: &str = r#"
[[flow]]
name = "issue"
max_attempts = 2
retry_on = ["orchestrator_lost", "judge_rejected"]
trigger = { kind = "label", label = "inchworm" }
[[flow.step]]
name = "implement"
identity = "rust-implementer"
[[flow.step]]
name = "publish"
identity = "pr-shepherd"
[[flow.judge]]
kind = "diff"
max_lines = 50
[flow.outcome]
mode = "draft_pr"
required_checks = ["build"]
"#;

    const PATCH: &str = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -0,0 +1 @@\n+x\n";

    #[derive(Default)]
    struct ScriptedFactory {
        batches: Mutex<VecDeque<Vec<Scenario>>>,
        made: Mutex<usize>,
        failure: Option<String>,
        panics: bool,
    }

    impl ScriptedFactory {
        fn with(batches: Vec<Vec<Scenario>>) -> ScriptedFactory {
            ScriptedFactory {
                batches: Mutex::new(batches.into()),
                ..ScriptedFactory::default()
            }
        }
    }

    impl DispatcherFactory for ScriptedFactory {
        fn make(&self) -> Result<Box<dyn Dispatcher>, String> {
            assert!(!self.panics, "factory panic");
            *self.made.lock().unwrap() += 1;
            if let Some(message) = &self.failure {
                return Err(message.clone());
            }
            let scenarios = self.batches.lock().unwrap().pop_front().unwrap_or_default();
            let (mut client, _fake) = connect(scenarios);
            client.initialize().map_err(|e| e.to_string())?;
            Ok(Box::new(client))
        }
    }

    fn successes() -> Vec<Scenario> {
        vec![
            Scenario::success_with_patch("+x\n", &["src/lib.rs"]),
            Scenario::success_with_patch("+x\n", &["src/lib.rs"]),
        ]
    }

    struct Rig {
        dir: tempfile::TempDir,
        github: Arc<FakeGitHub>,
    }

    impl Rig {
        fn new(flows: &str) -> Rig {
            let dir = tempfile::tempdir().unwrap();
            for repo in ["repo", "other"] {
                std::fs::create_dir_all(dir.path().join(repo).join(".inchworm")).unwrap();
            }
            std::fs::write(dir.path().join("repo/.inchworm/flows.toml"), flows).unwrap();
            Rig {
                dir,
                github: Arc::new(github_with_issue()),
            }
        }

        fn db(&self) -> PathBuf {
            self.dir.path().join("state.db")
        }

        fn store(&self) -> Store {
            Store::open(&self.db()).unwrap()
        }

        fn settings(&self) -> DaemonSettings {
            DaemonSettings {
                repos: vec![RepoEntry {
                    slug: "o/r".into(),
                    path: self.dir.path().join("repo"),
                }],
                state_db: self.db(),
                poll: Duration::from_millis(20),
                concurrency: 1,
                ci: CiPolicy {
                    poll_interval: Duration::from_millis(5),
                    timeout: Duration::from_secs(5),
                },
                grace: Duration::from_millis(300),
            }
        }

        fn green(&self, number: u64, issue: u64, attempt: u32, sha: &str) {
            self.github.add_pr(
                "o/r",
                &format!("inchworm/{issue}-a{attempt}"),
                pr_at(number, sha),
            );
            self.github.set_pr_patch("o/r", number, PATCH);
            self.github.push_check_poll(
                "o/r",
                sha,
                vec![CheckRun {
                    name: "build".into(),
                    head_sha: sha.into(),
                    conclusion: CheckConclusion::Success,
                }],
            );
        }

        fn start(&self, settings: DaemonSettings, factory: ScriptedFactory) -> Running {
            let deps = DaemonDeps {
                github: Arc::clone(&self.github) as Arc<dyn GitHub + Send + Sync>,
                clock: Arc::new(SystemClock),
                dispatchers: Arc::new(factory),
                scorer: Arc::new(FakeScorer::new()),
            };
            let stop = Arc::new(Notify::new());
            let signal = Arc::clone(&stop);
            let handle = std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(run_daemon(settings, deps, async move {
                        signal.notified().await;
                    }))
            });
            Running { stop, handle }
        }
    }

    struct Running {
        stop: Arc<Notify>,
        handle: std::thread::JoinHandle<Result<(), DaemonError>>,
    }

    impl Running {
        fn stop(self) -> Result<(), DaemonError> {
            self.stop.notify_one();
            self.handle.join().unwrap()
        }
    }

    fn wait_for(what: &str, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn states(store: &Store) -> Vec<(u64, RunState)> {
        let mut runs: Vec<_> = store
            .recent_runs(50)
            .unwrap()
            .into_iter()
            .map(|r| (r.issue, r.state))
            .collect();
        runs.sort_by_key(|(issue, _)| *issue);
        runs
    }

    #[test]
    fn a_labelled_issue_is_picked_up_run_judged_and_left_as_a_draft_pr() {
        let rig = Rig::new(FLOWS);
        rig.green(77, 5, 1, SHA);
        let running = rig.start(rig.settings(), ScriptedFactory::with(vec![successes()]));
        wait_for("the run to finish", || {
            states(&rig.store()) == [(5, RunState::PrOpen)]
        });
        running.stop().unwrap();

        let store = rig.store();
        let run = store.recent_runs(1).unwrap().remove(0);
        let attempt = &store.attempts(run.id).unwrap()[0];
        assert_eq!(attempt.state, AttemptState::Succeeded);
        assert_eq!(store.verdicts(attempt.id).unwrap().len(), 1);
        assert_eq!(rig.github.issue_labels("o/r", 5), ["inchworm:done"]);
        assert!(rig.github.pr("o/r", 77).unwrap().is_draft);
        assert!(rig.github.merge_calls().is_empty());
        assert!(
            rig.github
                .comments()
                .iter()
                .any(|c| c.2.contains("draft_pr"))
        );
    }

    #[test]
    fn restart_recovers_lost_dispatches_resumes_queued_and_published_runs() {
        let rig = Rig::new(FLOWS);
        let store = rig.store();
        for issue in [5u64, 6, 7] {
            rig.github.add_issue(
                "o/r",
                crate::github::Issue {
                    number: issue,
                    title: format!("issue {issue}"),
                    body: String::new(),
                    labels: vec!["inchworm:running".into()],
                    url: "u".into(),
                },
            );
        }
        let lost = store.enqueue_run("o/r", 5, "issue", 1).unwrap().unwrap();
        store.set_run_state(lost, RunState::Running, 2).unwrap();
        let dispatched = store.start_attempt(lost, 3).unwrap();
        store.enqueue_run("o/r", 6, "issue", 4).unwrap().unwrap();
        let published = store.enqueue_run("o/r", 7, "issue", 5).unwrap().unwrap();
        store
            .set_run_state(published, RunState::Running, 6)
            .unwrap();
        let attempt = store.start_attempt(published, 7).unwrap();
        store
            .record_pr(attempt.id, "inchworm/7-a1", SHA, 79)
            .unwrap();
        store.mark_published(attempt.id, 0, Some(true)).unwrap();
        store
            .set_run_state(published, RunState::AwaitingCi, 8)
            .unwrap();
        drop(store);

        rig.green(78, 5, 2, SHA);
        rig.green(80, 6, 1, "1111111111111111111111111111111111111111");
        rig.green(79, 7, 1, SHA);
        let running = rig.start(
            rig.settings(),
            ScriptedFactory::with(vec![successes(), successes()]),
        );
        wait_for("all three runs to finish", || {
            states(&rig.store())
                == [
                    (5, RunState::PrOpen),
                    (6, RunState::PrOpen),
                    (7, RunState::PrOpen),
                ]
        });
        running.stop().unwrap();

        let store = rig.store();
        let first = store.attempt(dispatched.id).unwrap();
        assert_eq!(first.failure_class, Some(FailureClass::OrchestratorLost));
        assert_eq!(store.attempts(lost).unwrap().len(), 2);
        assert_eq!(store.attempts(published).unwrap().len(), 1);
    }

    #[test]
    fn an_issue_that_already_has_an_open_run_is_not_enqueued_twice() {
        let rig = Rig::new(FLOWS);
        rig.store()
            .enqueue_run("o/r", 5, "issue", 1)
            .unwrap()
            .unwrap();
        let running = rig.start(
            rig.settings(),
            ScriptedFactory::with(vec![vec![Scenario::Hang]]),
        );
        wait_for("the resumed run to be dispatched", || {
            rig.store()
                .recent_runs(1)
                .unwrap()
                .first()
                .is_some_and(|run| run.state == RunState::Running)
        });
        std::thread::sleep(Duration::from_millis(150));
        running.stop().unwrap();
        assert_eq!(rig.store().recent_runs(10).unwrap().len(), 1);
    }

    #[test]
    fn a_repo_with_broken_flows_does_not_stop_the_others() {
        let rig = Rig::new(FLOWS);
        rig.green(77, 5, 1, SHA);
        let mut settings = rig.settings();
        settings.repos.insert(
            0,
            RepoEntry {
                slug: "o/other".into(),
                path: rig.dir.path().join("other"),
            },
        );
        settings.repos.insert(
            0,
            RepoEntry {
                slug: "o/gone".into(),
                path: rig.dir.path().join("gone"),
            },
        );
        let running = rig.start(settings, ScriptedFactory::with(vec![successes()]));
        wait_for("the healthy repo to finish", || {
            states(&rig.store()) == [(5, RunState::PrOpen)]
        });
        running.stop().unwrap();
    }

    #[test]
    fn two_issues_in_one_repo_run_one_after_the_other() {
        let rig = Rig::new(FLOWS);
        rig.github.add_issue(
            "o/r",
            crate::github::Issue {
                number: 6,
                title: "second".into(),
                body: String::new(),
                labels: vec!["inchworm".into()],
                url: "u".into(),
            },
        );
        rig.green(77, 5, 1, SHA);
        rig.green(78, 6, 1, "2222222222222222222222222222222222222222");
        let running = rig.start(
            rig.settings(),
            ScriptedFactory::with(vec![successes(), successes()]),
        );
        wait_for("both runs to finish", || {
            states(&rig.store()) == [(5, RunState::PrOpen), (6, RunState::PrOpen)]
        });
        running.stop().unwrap();
    }

    #[test]
    fn an_unavailable_nanna_fails_the_run_as_orchestrator_lost() {
        let rig = Rig::new(&FLOWS.replace("max_attempts = 2", "max_attempts = 1"));
        let factory = ScriptedFactory {
            failure: Some("cannot start `nanna mcp-serve`".into()),
            ..ScriptedFactory::default()
        };
        let running = rig.start(rig.settings(), factory);
        wait_for("the run to fail", || {
            states(&rig.store()) == [(5, RunState::Failed)]
        });
        running.stop().unwrap();
        let store = rig.store();
        let run = store.recent_runs(1).unwrap().remove(0);
        let attempt = &store.attempts(run.id).unwrap()[0];
        assert_eq!(attempt.failure_class, Some(FailureClass::OrchestratorLost));
        assert!(attempt.raw_error.as_ref().unwrap().contains("cannot start"));
        assert_eq!(rig.github.issue_labels("o/r", 5), ["inchworm:failed"]);
    }

    #[test]
    fn shutdown_while_idle_is_prompt_and_clean() {
        let rig = Rig::new(FLOWS);
        rig.github.set_labels("o/r", 5, &[], &["inchworm"]).unwrap();
        let running = rig.start(rig.settings(), ScriptedFactory::default());
        std::thread::sleep(Duration::from_millis(80));
        let started = Instant::now();
        running.stop().unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(rig.store().recent_runs(10).unwrap().is_empty());
    }

    #[test]
    fn shutdown_does_not_wait_for_an_in_flight_run_and_leaves_it_recoverable() {
        let rig = Rig::new(FLOWS);
        let running = rig.start(
            rig.settings(),
            ScriptedFactory::with(vec![vec![Scenario::Hang]]),
        );
        wait_for("the attempt to be dispatched", || {
            rig.store()
                .recent_runs(1)
                .unwrap()
                .first()
                .is_some_and(|run| {
                    rig.store()
                        .attempts(run.id)
                        .unwrap()
                        .iter()
                        .any(|a| a.state == AttemptState::Dispatched)
                })
        });
        let started = Instant::now();
        running.stop().unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(rig.store().recover_after_crash(9).unwrap(), 1);
    }

    #[test]
    fn the_instance_lock_lives_next_to_the_state_db_and_is_exclusive() {
        let rig = Rig::new(FLOWS);
        let first = InstanceLock::acquire(&rig.db()).unwrap();
        assert!(first.path().ends_with("state.db.lock"));
        let second = InstanceLock::acquire(&rig.db()).unwrap_err();
        assert!(
            matches!(second, DaemonError::AlreadyRunning(_)),
            "{second:?}"
        );
        assert!(second.to_string().contains("state.db.lock"));
        drop(first);
        assert!(InstanceLock::acquire(&rig.db()).is_ok());
    }

    #[test]
    fn an_unwritable_lock_location_is_a_lock_error() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, "x").unwrap();
        let err = InstanceLock::acquire(&blocker.join("state.db")).unwrap_err();
        assert!(matches!(err, DaemonError::Lock { .. }), "{err:?}");
    }

    #[test]
    fn a_second_daemon_refuses_to_start_and_leaves_the_first_ones_attempt_alone() {
        let rig = Rig::new(FLOWS);
        let store = rig.store();
        let run = store.enqueue_run("o/r", 5, "issue", 1).unwrap().unwrap();
        store.set_run_state(run, RunState::Running, 2).unwrap();
        let attempt = store.start_attempt(run, 3).unwrap();
        let _first = InstanceLock::acquire(&rig.db()).unwrap();
        let second = rig.start(rig.settings(), ScriptedFactory::default());
        let result = second.handle.join().unwrap();
        assert!(
            matches!(result, Err(DaemonError::AlreadyRunning(_))),
            "{result:?}"
        );
        assert_eq!(
            store.attempt(attempt.id).unwrap().state,
            AttemptState::Dispatched
        );
        assert_eq!(store.run(run).unwrap().state, RunState::Running);
    }

    struct HungRunner;

    impl crate::exec::Runner for HungRunner {
        fn run(&self, _: &crate::exec::CmdSpec) -> std::io::Result<crate::exec::CmdOutput> {
            std::thread::sleep(Duration::from_secs(30));
            Err(std::io::Error::other("hung"))
        }
    }

    #[test]
    fn a_stop_request_ends_the_daemon_promptly_even_while_gh_is_hung() {
        let rig = Rig::new(FLOWS);
        let deps = DaemonDeps {
            github: Arc::new(crate::github::GhCli::with_runner(HungRunner)),
            clock: Arc::new(SystemClock),
            dispatchers: Arc::new(ScriptedFactory::default()),
            scorer: Arc::new(FakeScorer::new()),
        };
        let stop = Arc::new(Notify::new());
        let signal = Arc::clone(&stop);
        let settings = rig.settings();
        let handle = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run_daemon(settings, deps, async move {
                    signal.notified().await;
                }))
        });
        std::thread::sleep(Duration::from_millis(150));
        let started = Instant::now();
        stop.notify_one();
        let result = handle.join().unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    struct Flaky(bool);

    impl Dispatcher for Flaky {
        fn dispatch(&mut self, _: &AssignRequest, _: Duration) -> Result<TaskOutcome, ClientError> {
            if self.0 {
                Err(ClientError::ReadTimeout(Duration::from_millis(1)))
            } else {
                Ok(TaskOutcome::TimedOut)
            }
        }
    }

    struct FlakyFactory(Mutex<usize>);

    impl DispatcherFactory for FlakyFactory {
        fn make(&self) -> Result<Box<dyn Dispatcher>, String> {
            let mut made = self.0.lock().unwrap();
            *made += 1;
            Ok(Box::new(Flaky(*made == 1)))
        }
    }

    fn assign() -> AssignRequest {
        AssignRequest {
            description: "d".into(),
            repo_path: PathBuf::from("/r"),
            identity: "i".into(),
            branch: "b".into(),
        }
    }

    #[test]
    fn a_failed_dispatch_drops_the_client_so_the_next_attempt_gets_a_fresh_one() {
        let factory = Arc::new(FlakyFactory(Mutex::new(0)));
        let mut dispatcher = Reconnecting {
            factory: Arc::clone(&factory) as Arc<dyn DispatcherFactory>,
            inner: None,
        };
        let timeout = Duration::from_secs(1);
        assert!(matches!(
            dispatcher.dispatch(&assign(), timeout),
            Err(ClientError::ReadTimeout(_))
        ));
        assert!(matches!(
            dispatcher.dispatch(&assign(), timeout),
            Ok(TaskOutcome::TimedOut)
        ));
        assert!(dispatcher.dispatch(&assign(), timeout).is_ok());
        assert_eq!(*factory.0.lock().unwrap(), 2);
    }

    #[test]
    fn a_factory_failure_is_reported_on_every_dispatch_and_retried() {
        let factory = Arc::new(ScriptedFactory {
            failure: Some("cannot start".into()),
            ..ScriptedFactory::default()
        });
        let mut dispatcher = Reconnecting {
            factory: Arc::clone(&factory) as Arc<dyn DispatcherFactory>,
            inner: None,
        };
        for _ in 0..2 {
            let err = dispatcher
                .dispatch(&assign(), Duration::from_secs(1))
                .unwrap_err();
            assert!(err.to_string().contains("cannot start"));
        }
        assert_eq!(*factory.made.lock().unwrap(), 2);
    }

    #[test]
    fn a_queued_run_waits_out_an_unreadable_flows_file_instead_of_stalling_until_restart() {
        let rig = Rig::new("not toml at all [");
        let store = rig.store();
        store.enqueue_run("o/r", 5, "issue", 1).unwrap().unwrap();
        drop(store);
        rig.green(77, 5, 1, SHA);
        let running = rig.start(rig.settings(), ScriptedFactory::with(vec![successes()]));
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(states(&rig.store()), [(5, RunState::Queued)]);
        std::fs::write(rig.dir.path().join("repo/.inchworm/flows.toml"), FLOWS).unwrap();
        wait_for("the run to finish once the file is readable", || {
            states(&rig.store()) == [(5, RunState::PrOpen)]
        });
        running.stop().unwrap();
    }

    #[test]
    fn a_dead_worker_is_reported_instead_of_hanging() {
        let rig = Rig::new(FLOWS);
        let factory = ScriptedFactory {
            panics: true,
            ..ScriptedFactory::default()
        };
        let running = rig.start(rig.settings(), factory);
        let handle = running.handle;
        let result = handle.join().unwrap();
        assert!(matches!(result, Err(DaemonError::WorkerDied)), "{result:?}");
    }

    fn script(dir: &std::path::Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("nanna");
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn make_with_retry(factory: &NannaFactory) -> Result<Box<dyn Dispatcher>, String> {
        for _ in 0..50 {
            match factory.make() {
                Err(err) if err.contains("Text file busy") => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                other => return other,
            }
        }
        factory.make()
    }

    #[test]
    fn nanna_factory_starts_and_initializes_a_server_or_reports_why_not() {
        let dir = tempfile::tempdir().unwrap();
        let ok = script(
            dir.path(),
            "#!/bin/sh\nread line\nprintf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}'\nread line\nsleep 5\n",
        );
        assert!(make_with_retry(&NannaFactory::new(ok)).is_ok());
        let dead = script(dir.path(), "#!/bin/sh\nexit 3\n");
        let err = make_with_retry(&NannaFactory::new(dead)).err().unwrap();
        assert!(err.contains("initialize failed"), "{err}");
    }

    #[test]
    fn jobs_that_cannot_run_are_logged_and_do_not_block_the_queue() {
        let rig = Rig::new(FLOWS);
        rig.github.add_issue(
            "o/r",
            crate::github::Issue {
                number: 99,
                title: "ghost".into(),
                body: String::new(),
                labels: vec![],
                url: "u".into(),
            },
        );
        let store = rig.store();
        store
            .enqueue_run("o/r", 5, "retired-flow", 1)
            .unwrap()
            .unwrap();
        let missing_issue = store.enqueue_run("o/r", 98, "issue", 2).unwrap().unwrap();
        drop(store);
        rig.green(77, 5, 1, SHA);
        let running = rig.start(rig.settings(), ScriptedFactory::with(vec![successes()]));
        wait_for("the healthy issue to finish", || {
            rig.store()
                .runs_in_states(&[RunState::PrOpen])
                .unwrap()
                .len()
                == 1
        });
        running.stop().unwrap();
        let store = rig.store();
        assert_eq!(store.run(missing_issue).unwrap().state, RunState::Queued);
        assert_eq!(store.runs_in_states(&[RunState::Queued]).unwrap().len(), 2);
    }

    #[test]
    fn a_run_whose_flows_file_disappeared_is_left_queued() {
        let rig = Rig::new(FLOWS);
        rig.store()
            .enqueue_run("o/r", 5, "issue", 1)
            .unwrap()
            .unwrap();
        rig.github.set_labels("o/r", 5, &[], &["inchworm"]).unwrap();
        std::fs::remove_file(rig.dir.path().join("repo/.inchworm/flows.toml")).unwrap();
        let factory = ScriptedFactory::default();
        let running = rig.start(rig.settings(), factory);
        std::thread::sleep(Duration::from_millis(300));
        running.stop().unwrap();
        assert_eq!(
            rig.store()
                .runs_in_states(&[RunState::Queued])
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn label_failures_are_warnings_and_do_not_cause_the_issue_to_be_rerun() {
        let rig = Rig::new(FLOWS);
        rig.github.fail_labels_with("denied");
        rig.green(77, 5, 1, SHA);
        let running = rig.start(rig.settings(), ScriptedFactory::with(vec![successes()]));
        wait_for("the run to finish", || {
            rig.store()
                .runs_in_states(&[RunState::PrOpen])
                .unwrap()
                .len()
                == 1
        });
        std::thread::sleep(Duration::from_millis(200));
        running.stop().unwrap();
        assert_eq!(rig.store().recent_runs(10).unwrap().len(), 1);
    }

    #[test]
    fn nanna_factory_reports_a_missing_binary() {
        let factory = NannaFactory::new(PathBuf::from("/no/such/nanna"));
        let err = factory.make().err().unwrap();
        assert!(err.contains("cannot start"), "{err}");
    }
}
