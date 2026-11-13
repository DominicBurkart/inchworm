use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

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
use crate::policy::MergeGrant;
use crate::run::{RunContext, RunError, enqueue_issue, execute_run};
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
    failures: u32,
}

const MAX_BACKOFF: Duration = Duration::from_secs(900);

fn backoff(poll: Duration, failures: u32) -> Duration {
    poll.saturating_mul(1u32 << failures.min(3))
        .min(MAX_BACKOFF)
}

fn log(message: &str) {
    eprintln!("inchworm: {}", crate::model::sanitize_text(message, 4000));
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
) -> Option<Job> {
    let repo = &job.repo;
    let file = match load_flows(repo) {
        Ok(file) => file,
        Err(err) => {
            log(&format!("{}: {err}; will retry", repo.slug));
            return Some(job.failed());
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
        merge_grant: MergeGrant::Denied,
    };
    let mut dispatcher = Reconnecting {
        factory: Arc::clone(&deps.dispatchers),
        inner: None,
    };
    match execute_run(&ctx, &env, flow, job.run_id, &mut dispatcher, None).await {
        Ok(summary) => {
            log(&format!(
                "{}: run {} finished as {}",
                repo.slug, job.run_id, summary.state
            ));
            None
        }
        Err(err @ RunError::NotRunnable { .. }) => {
            log(&format!("{}: run {} errored: {err}", repo.slug, job.run_id));
            None
        }
        Err(err) => {
            log(&format!(
                "{}: run {} errored: {err}; will retry",
                repo.slug, job.run_id
            ));
            Some(job.failed())
        }
    }
}

impl Job {
    fn failed(mut self) -> Job {
        self.failures = self.failures.saturating_add(1);
        self
    }
}

struct InFlight(Arc<AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn run_job(
    job: Job,
    lease: crate::scheduler::Lease<Job>,
    settings: &DaemonSettings,
    deps: &Arc<DaemonDeps>,
    scheduler: &Scheduler<Job>,
    retries: &tokio::sync::mpsc::UnboundedSender<Job>,
    in_flight: &Arc<AtomicUsize>,
) -> std::io::Result<()> {
    in_flight.fetch_add(1, Ordering::SeqCst);
    let guard = InFlight(Arc::clone(in_flight));
    let state_db = settings.state_db.clone();
    let ci = settings.ci;
    let deps = Arc::clone(deps);
    let scheduler = scheduler.clone();
    let retries = retries.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("inchworm-run-{}", job.run_id))
        .spawn(move || {
            let _guard = guard;
            let slug = job.repo.slug.clone();
            let id = job.run_id;
            let fallback = job.clone();
            let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let store = match Store::open(&state_db) {
                    Ok(store) => store,
                    Err(err) => {
                        log(&format!(
                            "{slug}: run {id}: cannot open the state database: {err}; will retry"
                        ));
                        return Some(job.failed());
                    }
                };
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a current-thread runtime can always be built")
                    .block_on(process(job, &store, &deps, &scheduler, ci))
            }));
            let retry = match attempt {
                Ok(retry) => retry,
                Err(_) => {
                    log(&format!("{slug}: run {id} panicked; will retry"));
                    Some(fallback.failed())
                }
            };
            drop(lease);
            if let Some(retry) = retry {
                let _ = retries.send(retry);
            }
        });
    spawned.map(|_| ())
}

async fn worker(
    settings: DaemonSettings,
    deps: Arc<DaemonDeps>,
    scheduler: Scheduler<Job>,
    stop: Arc<Notify>,
) {
    let mut deferred: Vec<(Instant, Job)> = Vec::new();
    let in_flight = Arc::new(AtomicUsize::new(0));
    let (retries, mut returned) = tokio::sync::mpsc::unbounded_channel::<Job>();
    loop {
        tokio::select! {
            () = stop.notified() => break,
            lease = scheduler.next() => {
                let job = lease.item().clone();
                let fallback = job.clone();
                if let Err(err) = run_job(job, lease, &settings, &deps, &scheduler, &retries, &in_flight) {
                    log(&format!("{}: cannot start a run thread: {err}; will retry", fallback.repo.slug));
                    let delay = backoff(settings.poll, fallback.failures);
                    deferred.push((Instant::now() + delay, fallback.failed()));
                }
            }
            Some(job) = returned.recv() => {
                let delay = backoff(settings.poll, job.failures);
                deferred.push((Instant::now() + delay, job));
            }
            () = deps.clock.sleep(settings.poll), if !deferred.is_empty() => {
                let now = Instant::now();
                let (due, waiting): (Vec<_>, Vec<_>) =
                    deferred.drain(..).partition(|(at, _)| *at <= now);
                deferred = waiting;
                for (_, job) in due {
                    let repo = job.repo.slug.clone();
                    scheduler.enqueue(&repo, job);
                }
            }
        }
    }
    while in_flight.load(Ordering::SeqCst) > 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
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
                            failures: 0,
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
                    failures: 0,
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
                    .block_on(worker(settings, deps, scheduler, stop));
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
        delay: Duration,
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
            std::thread::sleep(self.delay);
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
    fn a_merge_when_flow_never_merges_because_the_daemon_passes_a_denied_grant() {
        let rig = Rig::new(&FLOWS.replace("draft_pr", "merge_when"));
        rig.green(77, 5, 1, SHA);
        let running = rig.start(rig.settings(), ScriptedFactory::with(vec![successes()]));
        wait_for("the run to finish", || {
            states(&rig.store()) == [(5, RunState::PrOpen)]
        });
        running.stop().unwrap();
        assert!(rig.github.merge_calls().is_empty());
        assert!(rig.github.ready_calls().is_empty());
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
    fn backoff_doubles_up_to_eight_times_the_poll_interval_and_is_capped() {
        let poll = Duration::from_secs(10);
        let delays: Vec<u64> = (0..6).map(|n| backoff(poll, n).as_secs()).collect();
        assert_eq!(delays, [10, 20, 40, 80, 80, 80]);
        assert_eq!(backoff(Duration::from_secs(600), 3), MAX_BACKOFF);
        assert_eq!(backoff(Duration::from_secs(600), u32::MAX), MAX_BACKOFF);
    }

    #[test]
    fn shutdown_waits_for_an_in_flight_run_that_finishes_within_the_grace_period() {
        let rig = Rig::new(FLOWS);
        rig.green(77, 5, 1, SHA);
        let mut settings = rig.settings();
        settings.grace = Duration::from_secs(10);
        let factory = ScriptedFactory {
            delay: Duration::from_millis(500),
            ..ScriptedFactory::with(vec![successes()])
        };
        let running = rig.start(settings, factory);
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
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(states(&rig.store()), [(5, RunState::PrOpen)]);
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
        rig.github
            .set_labels("o/r", 5, &["inchworm:running"], &["inchworm"])
            .unwrap();
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
    fn a_panicking_run_is_retried_without_killing_the_daemon() {
        let rig = Rig::new(FLOWS);
        let factory = ScriptedFactory {
            panics: true,
            ..ScriptedFactory::default()
        };
        let running = rig.start(rig.settings(), factory);
        wait_for("the run to be queued", || {
            !rig.store().recent_runs(5).unwrap().is_empty()
        });
        std::thread::sleep(Duration::from_millis(400));
        assert!(!running.handle.is_finished());
        running.stop().unwrap();
        assert_eq!(
            rig.store().recent_runs(1).unwrap()[0].state,
            RunState::Running
        );
    }

    #[test]
    fn nanna_factory_starts_and_initializes_a_server_or_reports_why_not() {
        let dir = tempfile::tempdir().unwrap();
        let ok = crate::testing::shim_script(
            dir.path(),
            "ok",
            "read line\nprintf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}'\nread line\nsleep 5\n",
        );
        assert!(NannaFactory::new(ok).make().is_ok());
        let dead = crate::testing::shim_script(dir.path(), "dead", "exit 3\n");
        let err = NannaFactory::new(dead).make().err().unwrap();
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

#[cfg(test)]
mod round2 {
    use super::*;
    use crate::clock::SystemClock;
    use crate::config::MergeMethod;
    use crate::github::{FakeGitHub, GitHubError, Issue, PullRequest};
    use crate::nanna_client::fake::{Scenario, connect};
    use crate::policy::CheckConclusion;
    use crate::policy::CheckRun;
    use crate::testing::{FakeScorer, github_with_issue, pr_at};
    use std::sync::Mutex;
    use std::time::Instant;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const PATCH: &str = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -0,0 +1 @@\n+x\n";
    const FLOWS: &str = r#"
[[flow]]
name = "issue"
max_attempts = 2
retry_on = ["orchestrator_lost"]
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

    struct Flaky {
        inner: FakeGitHub,
        issue_failures: Mutex<u32>,
        pr_failures: Mutex<u32>,
        label_ok_calls: Mutex<Option<u32>>,
    }

    impl GitHub for Flaky {
        fn open_issues_with_label(&self, r: &str, l: &str) -> Result<Vec<Issue>, GitHubError> {
            self.inner.open_issues_with_label(r, l)
        }
        fn issue(&self, r: &str, n: u64) -> Result<Issue, GitHubError> {
            let mut left = self.issue_failures.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                return Err(GitHubError::Command {
                    command: "gh issue view".into(),
                    detail: "HTTP 502".into(),
                });
            }
            self.inner.issue(r, n)
        }
        fn labels(&self, r: &str) -> Result<Vec<String>, GitHubError> {
            self.inner.labels(r)
        }
        fn set_labels(&self, r: &str, n: u64, a: &[&str], d: &[&str]) -> Result<(), GitHubError> {
            let mut ok = self.label_ok_calls.lock().unwrap();
            if let Some(left) = ok.as_mut() {
                if *left == 0 {
                    return Err(GitHubError::Command {
                        command: "gh issue edit".into(),
                        detail: "label inchworm:done not found".into(),
                    });
                }
                *left -= 1;
            }
            self.inner.set_labels(r, n, a, d)
        }
        fn comment(&self, r: &str, n: u64, b: &str) -> Result<(), GitHubError> {
            self.inner.comment(r, n, b)
        }
        fn pr_for_branch(&self, r: &str, b: &str) -> Result<Option<PullRequest>, GitHubError> {
            self.inner.pr_for_branch(r, b)
        }
        fn pr_files(&self, r: &str, n: u64) -> Result<Vec<String>, GitHubError> {
            self.inner.pr_files(r, n)
        }
        fn pr(&self, r: &str, n: u64) -> Result<PullRequest, GitHubError> {
            let mut left = self.pr_failures.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                return Err(GitHubError::Command {
                    command: "gh pr view".into(),
                    detail: "HTTP 502".into(),
                });
            }
            self.inner.pr(r, n)
        }
        fn pr_patch(&self, r: &str, n: u64) -> Result<String, GitHubError> {
            self.inner.pr_patch(r, n)
        }
        fn check_runs(&self, r: &str, s: &str) -> Result<Vec<CheckRun>, GitHubError> {
            self.inner.check_runs(r, s)
        }
        fn mark_ready(&self, r: &str, n: u64) -> Result<(), GitHubError> {
            self.inner.mark_ready(r, n)
        }
        fn mark_draft(&self, r: &str, n: u64) -> Result<(), GitHubError> {
            self.inner.mark_draft(r, n)
        }
        fn merge_pr(&self, r: &str, n: u64, h: &str, m: MergeMethod) -> Result<(), GitHubError> {
            self.inner.merge_pr(r, n, h, m)
        }
        fn required_checks(&self, r: &str, b: &str) -> Result<Option<Vec<String>>, GitHubError> {
            self.inner.required_checks(r, b)
        }
    }

    struct Factory(Mutex<Vec<Vec<Scenario>>>);
    impl DispatcherFactory for Factory {
        fn make(&self) -> Result<Box<dyn Dispatcher>, String> {
            let s = self.0.lock().unwrap().pop().unwrap_or_default();
            let (mut client, _f) = connect(s);
            client.initialize().map_err(|e| e.to_string())?;
            Ok(Box::new(client))
        }
    }

    fn ok() -> Vec<Scenario> {
        vec![
            Scenario::success_with_patch("+x\n", &["src/lib.rs"]),
            Scenario::success_with_patch("+x\n", &["src/lib.rs"]),
        ]
    }

    fn green(g: &FakeGitHub, slug: &str, number: u64, issue: u64) {
        g.add_pr(slug, &format!("inchworm/{issue}-a1"), pr_at(number, SHA));
        g.set_pr_patch(slug, number, PATCH);
        g.push_check_poll(
            slug,
            SHA,
            vec![CheckRun {
                name: "build".into(),
                head_sha: SHA.into(),
                conclusion: CheckConclusion::Success,
            }],
        );
    }

    fn start(
        settings: DaemonSettings,
        github: Arc<dyn GitHub + Send + Sync>,
        batches: Vec<Vec<Scenario>>,
    ) -> (
        Arc<Notify>,
        std::thread::JoinHandle<Result<(), DaemonError>>,
    ) {
        let deps = DaemonDeps {
            github,
            clock: Arc::new(SystemClock),
            dispatchers: Arc::new(Factory(Mutex::new(batches))),
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
                    signal.notified().await
                }))
        });
        (stop, handle)
    }

    fn setup(slugs: &[&str]) -> (tempfile::TempDir, DaemonSettings) {
        let dir = tempfile::tempdir().unwrap();
        let mut repos = Vec::new();
        for (i, slug) in slugs.iter().enumerate() {
            let p = dir.path().join(format!("repo{i}"));
            std::fs::create_dir_all(p.join(".inchworm")).unwrap();
            std::fs::write(p.join(".inchworm/flows.toml"), FLOWS).unwrap();
            repos.push(RepoEntry {
                slug: slug.to_string(),
                path: p,
            });
        }
        let settings = DaemonSettings {
            repos,
            state_db: dir.path().join("state.db"),
            poll: Duration::from_millis(20),
            concurrency: 2,
            ci: CiPolicy {
                poll_interval: Duration::from_millis(5),
                timeout: Duration::from_secs(120),
            },
            grace: Duration::from_millis(300),
        };
        (dir, settings)
    }

    fn issue(n: u64) -> Issue {
        Issue {
            number: n,
            title: "t".into(),
            body: String::new(),
            labels: vec!["inchworm".into()],
            url: "u".into(),
        }
    }

    #[test]
    fn a_transient_gh_error_when_a_run_starts_must_not_strand_it_until_restart() {
        let (_dir, settings) = setup(&["o/r"]);
        let flaky = Arc::new(Flaky {
            inner: github_with_issue(),
            issue_failures: Mutex::new(2),
            pr_failures: Mutex::new(0),
            label_ok_calls: Mutex::new(None),
        });
        green(&flaky.inner, "o/r", 77, 5);
        let db = settings.state_db.clone();
        let (stop, handle) = start(settings, flaky.clone(), vec![ok()]);
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut state = None;
        while Instant::now() < deadline {
            let runs = Store::open(&db).unwrap().recent_runs(5).unwrap();
            state = runs.first().map(|r| r.state);
            if state == Some(RunState::PrOpen) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        stop.notify_one();
        handle.join().unwrap().unwrap();
        assert_eq!(
            state,
            Some(RunState::PrOpen),
            "run stranded after one transient `gh issue view` failure"
        );
    }

    #[test]
    fn a_ci_wait_in_one_repo_must_not_block_every_other_repo() {
        let (_dir, settings) = setup(&["o/a", "o/b"]);
        let github = FakeGitHub::new();
        github.add_issue("o/a", issue(1));
        github.add_issue("o/b", issue(2));
        github.add_pr(
            "o/a",
            "inchworm/1-a1",
            pr_at(10, "1111111111111111111111111111111111111111"),
        );
        github.set_pr_patch("o/a", 10, PATCH);
        green(&github, "o/b", 20, 2);
        let db = settings.state_db.clone();
        let (stop, handle) = start(settings, Arc::new(github), vec![ok(), ok()]);
        let started = Instant::now();
        let mut b_done = None;
        while started.elapsed() < Duration::from_secs(30) {
            let runs = Store::open(&db).unwrap().recent_runs(5).unwrap();
            if runs
                .iter()
                .any(|r| r.repo == "o/b" && r.state == RunState::PrOpen)
            {
                b_done = Some(started.elapsed());
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        stop.notify_one();
        handle.join().unwrap().unwrap();
        assert!(
            b_done.is_some(),
            "o/b was starved for >30s while o/a waited on CI (a 2h wait in production)"
        );
    }

    fn seed_published(db: &std::path::Path, final_state: RunState) -> RunId {
        let store = Store::open(db).unwrap();
        let run = store.enqueue_run("o/r", 5, "issue", 1).unwrap().unwrap();
        store.set_run_state(run, RunState::Running, 2).unwrap();
        let attempt = store.start_attempt(run, 3).unwrap();
        store
            .record_pr(attempt.id, "inchworm/5-a1", SHA, 77)
            .unwrap();
        store.mark_published(attempt.id, 0, Some(true)).unwrap();
        store.set_run_state(run, RunState::AwaitingCi, 4).unwrap();
        if final_state == RunState::Judging {
            store.set_run_state(run, RunState::Judging, 5).unwrap();
        }
        run
    }

    fn flaky_pr(failures: u32) -> Arc<Flaky> {
        let flaky = Arc::new(Flaky {
            inner: github_with_issue(),
            issue_failures: Mutex::new(0),
            pr_failures: Mutex::new(failures),
            label_ok_calls: Mutex::new(None),
        });
        flaky
            .inner
            .set_labels("o/r", 5, &["inchworm:running"], &["inchworm"])
            .unwrap();
        green(&flaky.inner, "o/r", 77, 5);
        flaky
    }

    fn wait_state(db: &std::path::Path, want: RunState) -> Option<RunState> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut state = None;
        while Instant::now() < deadline {
            state = Store::open(db)
                .unwrap()
                .recent_runs(5)
                .unwrap()
                .first()
                .map(|r| r.state);
            if state == Some(want) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        state
    }

    #[test]
    fn a_retried_published_run_resumes_through_legal_edges_and_never_dispatches_again() {
        let (_dir, settings) = setup(&["o/r"]);
        let run = seed_published(&settings.state_db, RunState::AwaitingCi);
        let db = settings.state_db.clone();
        let (stop, handle) = start(settings, flaky_pr(2), vec![]);
        let state = wait_state(&db, RunState::PrOpen);
        stop.notify_one();
        handle.join().unwrap().unwrap();
        assert_eq!(state, Some(RunState::PrOpen));
        let store = Store::open(&db).unwrap();
        let attempts = store.attempts(run).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].state, crate::store::AttemptState::Succeeded);
    }

    #[test]
    fn a_judging_run_is_resumed_after_a_restart() {
        let (_dir, settings) = setup(&["o/r"]);
        let run = seed_published(&settings.state_db, RunState::Judging);
        let db = settings.state_db.clone();
        let (stop, handle) = start(settings, flaky_pr(0), vec![]);
        let state = wait_state(&db, RunState::PrOpen);
        stop.notify_one();
        handle.join().unwrap().unwrap();
        assert_eq!(state, Some(RunState::PrOpen));
        assert_eq!(Store::open(&db).unwrap().attempts(run).unwrap().len(), 1);
    }

    #[test]
    fn a_transient_error_retry_dispatches_exactly_once() {
        let (_dir, settings) = setup(&["o/r"]);
        let flaky = Arc::new(Flaky {
            inner: github_with_issue(),
            issue_failures: Mutex::new(3),
            pr_failures: Mutex::new(0),
            label_ok_calls: Mutex::new(None),
        });
        green(&flaky.inner, "o/r", 77, 5);
        let db = settings.state_db.clone();
        let (stop, handle) = start(settings, flaky, vec![ok()]);
        let state = wait_state(&db, RunState::PrOpen);
        stop.notify_one();
        handle.join().unwrap().unwrap();
        assert_eq!(state, Some(RunState::PrOpen));
        let store = Store::open(&db).unwrap();
        let run = store.recent_runs(5).unwrap().remove(0);
        assert_eq!(store.recent_runs(5).unwrap().len(), 1);
        assert_eq!(store.attempts(run.id).unwrap().len(), 1);
    }
}
