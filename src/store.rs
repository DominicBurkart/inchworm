use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{
    Connection, ErrorCode, OptionalExtension, Row, Transaction, TransactionBehavior, params,
};
use thiserror::Error;

use crate::model::{FailureClass, InvalidTransition, RunState};

pub type RunId = i64;
pub type AttemptId = i64;

const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REFERENCE_LEN: usize = 200;

fn states_sql(states: &[RunState]) -> String {
    states
        .iter()
        .map(|state| format!("'{}'", state.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn migrations() -> Vec<String> {
    vec![format!(
        "
CREATE TABLE runs (
    id INTEGER PRIMARY KEY,
    repo TEXT NOT NULL,
    subject_kind TEXT NOT NULL CHECK (subject_kind IN ('issue', 'pr', 'commit')),
    subject_ref TEXT NOT NULL CHECK (length(subject_ref) > 0),
    flow TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ({all})),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX runs_one_open_per_subject
    ON runs (repo, subject_kind, subject_ref, flow)
    WHERE state IN ({open});
CREATE TABLE attempts (
    id INTEGER PRIMARY KEY,
    run_id INTEGER NOT NULL REFERENCES runs(id),
    n INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('dispatched', 'succeeded', 'failed')),
    branch TEXT,
    head_sha TEXT,
    pr_number INTEGER,
    failure_class TEXT,
    raw_error TEXT,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    UNIQUE (run_id, n)
);
",
        all = states_sql(RunState::ALL),
        open = states_sql(RunState::OPEN),
    )]
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    InvalidTransition(#[from] InvalidTransition),
    #[error("run {0} not found")]
    RunNotFound(RunId),
    #[error("run {run} is {state}, not running, so it cannot start an attempt")]
    RunNotRunning { run: RunId, state: RunState },
    #[error("attempt {0} not found")]
    AttemptNotFound(AttemptId),
    #[error("attempt {0} is not dispatched")]
    AttemptNotDispatched(AttemptId),
    #[error("stored value is corrupt: {0}")]
    Corrupt(String),
    #[error("database schema version {found} is newer than this build supports ({supported})")]
    SchemaTooNew { found: usize, supported: usize },
    #[error("invalid subject: {0}")]
    InvalidSubject(String),
    #[error("another inchworm daemon holds {0}")]
    AlreadyRunning(PathBuf),
    #[error("cannot use lock file {path}: {source}")]
    LockIo {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectKind {
    Issue,
    Pr,
    Commit,
}

impl SubjectKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            SubjectKind::Issue => "issue",
            SubjectKind::Pr => "pr",
            SubjectKind::Commit => "commit",
        }
    }

    fn parse(text: &str) -> Result<SubjectKind, StoreError> {
        match text {
            "issue" => Ok(SubjectKind::Issue),
            "pr" => Ok(SubjectKind::Pr),
            "commit" => Ok(SubjectKind::Commit),
            other => Err(StoreError::Corrupt(format!("subject kind `{other}`"))),
        }
    }
}

/// What a run is about: an issue, a pull request or a commit.
///
/// ```
/// use inchworm::store::{Subject, SubjectKind};
///
/// let issue = Subject::issue(7);
/// assert_eq!(issue.kind, SubjectKind::Issue);
/// assert_eq!(issue.reference, "7");
/// assert!(Subject::commit("not-hex").is_err());
/// assert_eq!(Subject::commit("0123abc").unwrap().kind, SubjectKind::Commit);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    pub kind: SubjectKind,
    pub reference: String,
}

impl Subject {
    pub fn issue(number: u64) -> Subject {
        Subject {
            kind: SubjectKind::Issue,
            reference: number.to_string(),
        }
    }

    pub fn pr(number: u64) -> Subject {
        Subject {
            kind: SubjectKind::Pr,
            reference: number.to_string(),
        }
    }

    pub fn commit(sha: &str) -> Result<Subject, StoreError> {
        let valid = (7..=64).contains(&sha.len()) && sha.chars().all(|c| c.is_ascii_hexdigit());
        if !valid {
            return Err(StoreError::InvalidSubject(format!(
                "`{sha}` is not a 7 to 64 character hex commit id"
            )));
        }
        Ok(Subject {
            kind: SubjectKind::Commit,
            reference: sha.to_ascii_lowercase(),
        })
    }

    fn validate(&self) -> Result<(), StoreError> {
        if self.reference.is_empty()
            || self.reference.len() > MAX_REFERENCE_LEN
            || self.reference.chars().any(char::is_control)
        {
            return Err(StoreError::InvalidSubject(format!(
                "reference must be 1 to {MAX_REFERENCE_LEN} printable characters"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptState {
    Dispatched,
    Succeeded,
    Failed,
}

impl AttemptState {
    const fn as_str(self) -> &'static str {
        match self {
            AttemptState::Dispatched => "dispatched",
            AttemptState::Succeeded => "succeeded",
            AttemptState::Failed => "failed",
        }
    }

    fn parse(text: &str) -> Result<AttemptState, StoreError> {
        match text {
            "dispatched" => Ok(AttemptState::Dispatched),
            "succeeded" => Ok(AttemptState::Succeeded),
            "failed" => Ok(AttemptState::Failed),
            other => Err(StoreError::Corrupt(format!("attempt state `{other}`"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub id: RunId,
    pub repo: String,
    pub subject: Subject,
    pub flow: String,
    pub state: RunState,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub id: AttemptId,
    pub run_id: RunId,
    pub n: u32,
    pub state: AttemptState,
    pub branch: Option<String>,
    pub head_sha: Option<String>,
    pub pr_number: Option<u64>,
    pub failure_class: Option<FailureClass>,
    pub raw_error: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

/// Proof that this process is the only daemon using a state database.
///
/// Recovery after a crash may only be run by the holder, because while the
/// lock is held no other daemon can own a dispatched attempt.
///
/// ```
/// use inchworm::store::DaemonLock;
///
/// let dir = tempfile::tempdir().unwrap();
/// let db = dir.path().join("state.db");
/// let first = DaemonLock::acquire(&db).unwrap();
/// assert!(DaemonLock::acquire(&db).is_err());
/// drop(first);
/// assert!(DaemonLock::acquire(&db).is_ok());
/// ```
#[derive(Debug)]
pub struct DaemonLock {
    _file: File,
}

impl DaemonLock {
    pub fn acquire(db_path: &Path) -> Result<DaemonLock, StoreError> {
        let mut name = db_path.as_os_str().to_owned();
        name.push(".daemon.lock");
        let path = PathBuf::from(name);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|source| StoreError::LockIo {
                path: path.clone(),
                source,
            })?;
        match file.try_lock() {
            Ok(()) => Ok(DaemonLock { _file: file }),
            Err(TryLockError::WouldBlock) => Err(StoreError::AlreadyRunning(path)),
            Err(TryLockError::Error(source)) => Err(StoreError::LockIo { path, source }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Recovery {
    pub attempts_failed: usize,
    pub runs_requeued: usize,
}

pub struct Store {
    conn: Connection,
}

fn corrupt<E: std::fmt::Display>(what: &str) -> impl FnOnce(E) -> StoreError + '_ {
    move |err| StoreError::Corrupt(format!("{what}: {err}"))
}

fn run_from_row(row: &Row<'_>) -> Result<Run, StoreError> {
    let state: String = row.get("state")?;
    let kind: String = row.get("subject_kind")?;
    Ok(Run {
        id: row.get("id")?,
        repo: row.get("repo")?,
        subject: Subject {
            kind: SubjectKind::parse(&kind)?,
            reference: row.get("subject_ref")?,
        },
        flow: row.get("flow")?,
        state: state.parse().map_err(corrupt("run state"))?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

fn attempt_from_row(row: &Row<'_>) -> Result<Attempt, StoreError> {
    let state: String = row.get("state")?;
    let n: i64 = row.get("n")?;
    let pr: Option<i64> = row.get("pr_number")?;
    let class: Option<String> = row.get("failure_class")?;
    Ok(Attempt {
        id: row.get("id")?,
        run_id: row.get("run_id")?,
        n: u32::try_from(n).map_err(corrupt("attempt n"))?,
        state: AttemptState::parse(&state)?,
        branch: row.get("branch")?,
        head_sha: row.get("head_sha")?,
        pr_number: pr
            .map(u64::try_from)
            .transpose()
            .map_err(corrupt("pr number"))?,
        failure_class: class
            .map(|c| c.parse::<FailureClass>())
            .transpose()
            .map_err(corrupt("failure class"))?,
        raw_error: row.get("raw_error")?,
        started_at: row.get("started_at")?,
        ended_at: row.get("ended_at")?,
    })
}

fn run_in(conn: &Connection, id: RunId) -> Result<Run, StoreError> {
    conn.query_row("SELECT * FROM runs WHERE id = ?1", [id], |row| {
        Ok(run_from_row(row))
    })
    .optional()?
    .ok_or(StoreError::RunNotFound(id))?
}

fn attempt_in(conn: &Connection, id: AttemptId) -> Result<Attempt, StoreError> {
    conn.query_row("SELECT * FROM attempts WHERE id = ?1", [id], |row| {
        Ok(attempt_from_row(row))
    })
    .optional()?
    .ok_or(StoreError::AttemptNotFound(id))?
}

fn is_busy(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(failure, _)
            if matches!(failure.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

fn enable_wal(conn: &Connection) -> Result<(), StoreError> {
    let deadline = Instant::now() + BUSY_TIMEOUT;
    let mut pause = Duration::from_millis(2);
    loop {
        let switched = conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0));
        match switched {
            Ok(_) => return Ok(()),
            Err(err) if is_busy(&err) && Instant::now() < deadline => {
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(100));
            }
            Err(err) => return Err(err.into()),
        }
    }
}

fn is_unique_violation(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == ErrorCode::ConstraintViolation
                && failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
    )
}

impl Store {
    /// ```
    /// use inchworm::model::RunState;
    /// use inchworm::store::{Store, Subject};
    ///
    /// let store = Store::open_in_memory().unwrap();
    /// let id = store.enqueue_run("o/r", &Subject::issue(7), "issue", 100).unwrap().unwrap();
    /// assert!(store.enqueue_run("o/r", &Subject::issue(7), "issue", 101).unwrap().is_none());
    /// store.set_run_state(id, RunState::Running, 102).unwrap();
    /// assert_eq!(store.run(id).unwrap().state, RunState::Running);
    /// ```
    pub fn open_in_memory() -> Result<Store, StoreError> {
        Store::from_connection(Connection::open_in_memory()?, false)
    }

    pub fn open(path: &Path) -> Result<Store, StoreError> {
        Store::from_connection(Connection::open(path)?, true)
    }

    fn from_connection(conn: Connection, wal: bool) -> Result<Store, StoreError> {
        conn.busy_timeout(BUSY_TIMEOUT)?;
        if wal {
            enable_wal(&conn)?;
        }
        conn.pragma_update(None, "foreign_keys", true)?;
        let mut store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    fn immediate(&self) -> Result<Transaction<'_>, StoreError> {
        Ok(Transaction::new_unchecked(
            &self.conn,
            TransactionBehavior::Immediate,
        )?)
    }

    pub fn schema_version(&self) -> Result<usize, StoreError> {
        Store::version_in(&self.conn)
    }

    fn version_in(conn: &Connection) -> Result<usize, StoreError> {
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        usize::try_from(version).map_err(corrupt("user_version"))
    }

    fn migrate(&mut self) -> Result<(), StoreError> {
        let steps = migrations();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = Store::version_in(&tx)?;
        if current > steps.len() {
            return Err(StoreError::SchemaTooNew {
                found: current,
                supported: steps.len(),
            });
        }
        for (index, sql) in steps.iter().enumerate().skip(current) {
            tx.execute_batch(sql)?;
            tx.pragma_update(
                None,
                "user_version",
                i64::try_from(index + 1).unwrap_or(i64::MAX),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn enqueue_run(
        &self,
        repo: &str,
        subject: &Subject,
        flow: &str,
        now: i64,
    ) -> Result<Option<RunId>, StoreError> {
        subject.validate()?;
        let tx = self.immediate()?;
        let inserted = tx.execute(
            "INSERT INTO runs (repo, subject_kind, subject_ref, flow, state, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                repo,
                subject.kind.as_str(),
                subject.reference,
                flow,
                RunState::Queued.as_str(),
                now
            ],
        );
        match inserted {
            Ok(_) => {
                let id = tx.last_insert_rowid();
                tx.commit()?;
                Ok(Some(id))
            }
            Err(err) if is_unique_violation(&err) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    pub fn run(&self, id: RunId) -> Result<Run, StoreError> {
        run_in(&self.conn, id)
    }

    pub fn set_run_state(&self, id: RunId, to: RunState, now: i64) -> Result<(), StoreError> {
        let tx = self.immediate()?;
        let current = run_in(&tx, id)?;
        current.state.transition(to)?;
        tx.execute(
            "UPDATE runs SET state = ?1, updated_at = ?2 WHERE id = ?3",
            params![to.as_str(), now, id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn next_queued(&self, repo: &str) -> Result<Option<Run>, StoreError> {
        self.conn
            .query_row(
                "SELECT * FROM runs WHERE repo = ?1 AND state = ?2 ORDER BY id LIMIT 1",
                params![repo, RunState::Queued.as_str()],
                |row| Ok(run_from_row(row)),
            )
            .optional()?
            .transpose()
    }

    pub fn active_run(&self, repo: &str) -> Result<Option<Run>, StoreError> {
        let sql = format!(
            "SELECT * FROM runs WHERE repo = ?1 AND state IN ({}) ORDER BY id LIMIT 1",
            states_sql(RunState::ACTIVE)
        );
        self.conn
            .query_row(&sql, [repo], |row| Ok(run_from_row(row)))
            .optional()?
            .transpose()
    }

    pub fn start_attempt(&self, run_id: RunId, now: i64) -> Result<Attempt, StoreError> {
        let tx = self.immediate()?;
        let run = run_in(&tx, run_id)?;
        if run.state != RunState::Running {
            return Err(StoreError::RunNotRunning {
                run: run_id,
                state: run.state,
            });
        }
        let n: i64 = tx.query_row(
            "SELECT COALESCE(MAX(n), 0) + 1 FROM attempts WHERE run_id = ?1",
            [run_id],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO attempts (run_id, n, state, started_at) VALUES (?1, ?2, ?3, ?4)",
            params![run_id, n, AttemptState::Dispatched.as_str(), now],
        )?;
        let id = tx.last_insert_rowid();
        let attempt = attempt_in(&tx, id)?;
        tx.commit()?;
        Ok(attempt)
    }

    pub fn attempt(&self, id: AttemptId) -> Result<Attempt, StoreError> {
        attempt_in(&self.conn, id)
    }

    pub fn attempts(&self, run_id: RunId) -> Result<Vec<Attempt>, StoreError> {
        let mut statement = self
            .conn
            .prepare("SELECT * FROM attempts WHERE run_id = ?1 ORDER BY n")?;
        let rows = statement.query_map([run_id], |row| Ok(attempt_from_row(row)))?;
        rows.map(|row| row?).collect()
    }

    pub fn record_pr(
        &self,
        id: AttemptId,
        branch: &str,
        head_sha: &str,
        pr_number: u64,
    ) -> Result<(), StoreError> {
        let pr = i64::try_from(pr_number).map_err(corrupt("pr number"))?;
        let changed = self.conn.execute(
            "UPDATE attempts SET branch = ?1, head_sha = ?2, pr_number = ?3
             WHERE id = ?4 AND state = 'dispatched'",
            params![branch, head_sha, pr, id],
        )?;
        if changed == 1 {
            return Ok(());
        }
        self.attempt(id)?;
        Err(StoreError::AttemptNotDispatched(id))
    }

    pub fn finish_attempt_ok(&self, id: AttemptId, now: i64) -> Result<(), StoreError> {
        self.finish(id, AttemptState::Succeeded, None, None, now)
    }

    pub fn finish_attempt_failed(
        &self,
        id: AttemptId,
        class: FailureClass,
        raw_error: &str,
        now: i64,
    ) -> Result<(), StoreError> {
        self.finish(id, AttemptState::Failed, Some(class), Some(raw_error), now)
    }

    fn finish(
        &self,
        id: AttemptId,
        state: AttemptState,
        class: Option<FailureClass>,
        raw_error: Option<&str>,
        now: i64,
    ) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE attempts SET state = ?1, failure_class = ?2, raw_error = ?3, ended_at = ?4
             WHERE id = ?5 AND state = 'dispatched'",
            params![
                state.as_str(),
                class.map(FailureClass::as_str),
                raw_error,
                now,
                id
            ],
        )?;
        if changed == 1 {
            return Ok(());
        }
        self.attempt(id)?;
        Err(StoreError::AttemptNotDispatched(id))
    }

    /// Fails every attempt left dispatched by a dead daemon and puts its run
    /// back in the queue, so the issue is not wedged behind an open run.
    ///
    /// ```
    /// use inchworm::model::{FailureClass, RunState};
    /// use inchworm::store::{DaemonLock, Store, Subject};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let db = dir.path().join("state.db");
    /// let store = Store::open(&db).unwrap();
    /// let run = store.enqueue_run("o/r", &Subject::issue(1), "issue", 0).unwrap().unwrap();
    /// store.set_run_state(run, RunState::Running, 1).unwrap();
    /// let attempt = store.start_attempt(run, 2).unwrap();
    ///
    /// let lock = DaemonLock::acquire(&db).unwrap();
    /// let report = store.recover_after_crash(&lock, 3).unwrap();
    /// assert_eq!((report.attempts_failed, report.runs_requeued), (1, 1));
    /// assert_eq!(store.run(run).unwrap().state, RunState::Queued);
    /// let lost = store.attempt(attempt.id).unwrap();
    /// assert_eq!(lost.failure_class, Some(FailureClass::OrchestratorLost));
    /// ```
    pub fn recover_after_crash(
        &self,
        _daemon: &DaemonLock,
        now: i64,
    ) -> Result<Recovery, StoreError> {
        let tx = self.immediate()?;
        let mut statement = tx.prepare(
            "SELECT DISTINCT run_id FROM attempts WHERE state = 'dispatched' ORDER BY run_id",
        )?;
        let run_ids: Vec<RunId> = statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        drop(statement);

        let attempts_failed = tx.execute(
            "UPDATE attempts SET state = 'failed', failure_class = ?1, raw_error = ?2, ended_at = ?3
             WHERE state = 'dispatched'",
            params![
                FailureClass::OrchestratorLost.as_str(),
                "inchworm stopped while this attempt was dispatched",
                now
            ],
        )?;
        let mut runs_requeued = 0;
        for run_id in run_ids {
            let run = run_in(&tx, run_id)?;
            if run.state.can_transition_to(RunState::Queued) {
                tx.execute(
                    "UPDATE runs SET state = ?1, updated_at = ?2 WHERE id = ?3",
                    params![RunState::Queued.as_str(), now, run_id],
                )?;
                runs_requeued += 1;
            }
        }
        tx.commit()?;
        Ok(Recovery {
            attempts_failed,
            runs_requeued,
        })
    }
}

#[cfg(test)]
impl Store {
    fn raw(&self) -> &Connection {
        &self.conn
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::sync::{Arc, Barrier};

    const EDGES: &[(RunState, RunState)] = &[
        (RunState::Queued, RunState::Running),
        (RunState::Queued, RunState::Failed),
        (RunState::Queued, RunState::Escalated),
        (RunState::Running, RunState::AwaitingCi),
        (RunState::Running, RunState::Failed),
        (RunState::Running, RunState::Escalated),
        (RunState::Running, RunState::Queued),
        (RunState::AwaitingCi, RunState::Judging),
        (RunState::AwaitingCi, RunState::Failed),
        (RunState::AwaitingCi, RunState::Escalated),
        (RunState::Judging, RunState::Merged),
        (RunState::Judging, RunState::PrOpen),
        (RunState::Judging, RunState::Failed),
        (RunState::Judging, RunState::Escalated),
    ];

    fn issue(n: u64) -> Subject {
        Subject::issue(n)
    }

    fn store_with_run() -> (Store, RunId) {
        let store = Store::open_in_memory().unwrap();
        let id = store
            .enqueue_run("o/r", &issue(1), "issue", 10)
            .unwrap()
            .unwrap();
        (store, id)
    }

    fn running_store() -> (Store, RunId) {
        let (store, id) = store_with_run();
        store.set_run_state(id, RunState::Running, 11).unwrap();
        (store, id)
    }

    fn force_state(store: &Store, id: RunId, state: &str) {
        store
            .raw()
            .execute_batch("PRAGMA ignore_check_constraints = ON")
            .unwrap();
        store
            .raw()
            .execute(
                "UPDATE runs SET state = ?1 WHERE id = ?2",
                params![state, id],
            )
            .unwrap();
        store
            .raw()
            .execute_batch("PRAGMA ignore_check_constraints = OFF")
            .unwrap();
    }

    #[test]
    fn migrations_set_the_version_and_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let first = Store::open(&path).unwrap();
        assert_eq!(first.schema_version().unwrap(), migrations().len());
        let id = first
            .enqueue_run("o/r", &issue(1), "issue", 1)
            .unwrap()
            .unwrap();
        drop(first);
        let second = Store::open(&path).unwrap();
        assert_eq!(second.schema_version().unwrap(), migrations().len());
        assert_eq!(second.run(id).unwrap().repo, "o/r");
    }

    #[test]
    fn a_database_from_a_newer_build_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        drop(Store::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        drop(conn);
        match Store::open(&path) {
            Err(StoreError::SchemaTooNew { found, supported }) => {
                assert_eq!(found, 99);
                assert_eq!(supported, migrations().len());
            }
            Err(other) => panic!("unexpected error: {other}"),
            Ok(_) => panic!("a newer schema must not open"),
        }
    }

    #[test]
    fn concurrent_first_opens_all_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    Store::open(&path).map(|store| store.schema_version().unwrap())
                })
            })
            .collect();
        for handle in handles {
            let version = handle.join().unwrap().expect("open must not fail");
            assert_eq!(version, migrations().len());
        }
    }

    #[test]
    fn two_handles_on_one_file_never_report_a_locked_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        drop(Store::open(&path).unwrap());
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = (0..2u64)
            .map(|worker| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let store = Store::open(&path).unwrap();
                    barrier.wait();
                    for n in 0..60 {
                        let subject = issue(worker * 1000 + n);
                        let id = store
                            .enqueue_run("o/r", &subject, "issue", 1)
                            .expect("enqueue must not fail under contention")
                            .unwrap();
                        store.set_run_state(id, RunState::Running, 2).unwrap();
                        store.start_attempt(id, 3).unwrap();
                        store.set_run_state(id, RunState::Failed, 4).unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let store = Store::open(&path).unwrap();
        let rows: i64 = store
            .raw()
            .query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 120);
    }

    #[test]
    fn file_databases_use_wal_and_a_busy_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        let mode: String = store
            .raw()
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let timeout: i64 = store
            .raw()
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .unwrap();
        assert_eq!(timeout, BUSY_TIMEOUT.as_millis() as i64);
        let foreign_keys: i64 = store
            .raw()
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        assert_eq!(foreign_keys, 1);
    }

    #[test]
    fn foreign_keys_are_enforced() {
        let (store, _) = store_with_run();
        let orphan = store.raw().execute(
            "INSERT INTO attempts (run_id, n, state, started_at) VALUES (999, 1, 'dispatched', 1)",
            [],
        );
        assert!(orphan.is_err());
    }

    #[test]
    fn enqueue_deduplicates_on_repo_subject_and_flow() {
        let (store, id) = store_with_run();
        assert!(
            store
                .enqueue_run("o/r", &issue(1), "issue", 11)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .enqueue_run("o/r", &issue(1), "other", 11)
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .enqueue_run("o/r", &issue(2), "issue", 11)
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .enqueue_run("o/x", &issue(1), "issue", 11)
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .enqueue_run("o/r", &Subject::pr(1), "issue", 11)
                .unwrap()
                .is_some()
        );
        let commit = Subject::commit("0123abc").unwrap();
        assert!(
            store
                .enqueue_run("o/r", &commit, "issue", 11)
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .enqueue_run("o/r", &commit, "issue", 12)
                .unwrap()
                .is_none()
        );
        assert_eq!(store.run(id).unwrap().state, RunState::Queued);
    }

    #[test]
    fn only_a_duplicate_open_run_is_reported_as_none() {
        let (store, _) = store_with_run();
        store
            .raw()
            .execute_batch(
                "CREATE TRIGGER refuse BEFORE INSERT ON runs
                 BEGIN SELECT RAISE(ABORT, 'refused'); END;",
            )
            .unwrap();
        let err = store.enqueue_run("o/r", &issue(2), "issue", 1).unwrap_err();
        assert!(matches!(err, StoreError::Sqlite(_)), "{err}");
    }

    #[test]
    fn a_subject_is_validated_before_anything_is_written() {
        let store = Store::open_in_memory().unwrap();
        for reference in ["", "a\nb", "tab\there"] {
            let subject = Subject {
                kind: SubjectKind::Issue,
                reference: reference.to_string(),
            };
            assert!(
                matches!(
                    store.enqueue_run("o/r", &subject, "issue", 1),
                    Err(StoreError::InvalidSubject(_))
                ),
                "{reference:?}"
            );
        }
        let long = Subject {
            kind: SubjectKind::Issue,
            reference: "9".repeat(MAX_REFERENCE_LEN + 1),
        };
        assert!(matches!(
            store.enqueue_run("o/r", &long, "issue", 1),
            Err(StoreError::InvalidSubject(_))
        ));
        let longest = Subject {
            kind: SubjectKind::Issue,
            reference: "9".repeat(MAX_REFERENCE_LEN),
        };
        assert!(
            store
                .enqueue_run("o/r", &longest, "issue", 1)
                .unwrap()
                .is_some()
        );
        let rows: i64 = store
            .raw()
            .query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn commit_subjects_must_be_hex_of_a_sensible_length() {
        assert!(Subject::commit("abcdef").is_err());
        assert!(Subject::commit("abcdefg").is_err());
        assert!(Subject::commit(&"a".repeat(65)).is_err());
        assert!(Subject::commit("").is_err());
        assert_eq!(Subject::commit("ABCDEF1").unwrap().reference, "abcdef1");
        assert!(Subject::commit(&"f".repeat(64)).is_ok());
        assert!(Subject::commit(&"f".repeat(7)).is_ok());
    }

    #[test]
    fn run_fields_round_trip() {
        let (store, id) = store_with_run();
        assert_eq!(
            store.run(id).unwrap(),
            Run {
                id,
                repo: "o/r".into(),
                subject: issue(1),
                flow: "issue".into(),
                state: RunState::Queued,
                created_at: 10,
                updated_at: 10,
            }
        );
    }

    #[test]
    fn unknown_run_is_reported() {
        let store = Store::open_in_memory().unwrap();
        assert!(matches!(store.run(99), Err(StoreError::RunNotFound(99))));
        assert!(matches!(
            store.set_run_state(99, RunState::Running, 1),
            Err(StoreError::RunNotFound(99))
        ));
        assert!(matches!(
            store.start_attempt(99, 1),
            Err(StoreError::RunNotFound(99))
        ));
    }

    #[test]
    fn legal_transitions_persist_and_update_the_timestamp() {
        let (store, id) = store_with_run();
        store.set_run_state(id, RunState::Running, 20).unwrap();
        let run = store.run(id).unwrap();
        assert_eq!(run.state, RunState::Running);
        assert_eq!(run.updated_at, 20);
        assert_eq!(run.created_at, 10);
    }

    #[test]
    fn illegal_transitions_are_rejected_and_leave_state_unchanged() {
        let (store, id) = store_with_run();
        let err = store.set_run_state(id, RunState::Merged, 20).unwrap_err();
        assert!(matches!(err, StoreError::InvalidTransition(_)));
        let run = store.run(id).unwrap();
        assert_eq!(run.state, RunState::Queued);
        assert_eq!(run.updated_at, 10);
    }

    #[test]
    fn a_finished_subject_can_be_enqueued_again_but_an_open_one_cannot() {
        let (store, first) = store_with_run();
        for state in [RunState::Running, RunState::AwaitingCi, RunState::Judging] {
            store.set_run_state(first, state, 11).unwrap();
            assert!(
                store
                    .enqueue_run("o/r", &issue(1), "issue", 12)
                    .unwrap()
                    .is_none()
            );
        }
        store.set_run_state(first, RunState::Failed, 15).unwrap();
        let second = store
            .enqueue_run("o/r", &issue(1), "issue", 16)
            .unwrap()
            .unwrap();
        assert_ne!(first, second);
        assert_eq!(store.run(first).unwrap().state, RunState::Failed);
        assert_eq!(store.next_queued("o/r").unwrap().unwrap().id, second);
    }

    #[test]
    fn every_terminal_state_allows_a_rerun_and_every_open_state_blocks_it() {
        for state in RunState::ALL {
            let (store, id) = store_with_run();
            force_state(&store, id, state.as_str());
            let again = store.enqueue_run("o/r", &issue(1), "issue", 20).unwrap();
            assert_eq!(again.is_some(), state.is_terminal(), "{state}");
            assert_eq!(
                store.active_run("o/r").unwrap().is_some(),
                state.is_active(),
                "{state}"
            );
        }
    }

    #[test]
    fn the_open_state_index_is_derived_from_the_model() {
        let (store, _) = store_with_run();
        let sql: String = store
            .raw()
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'runs_one_open_per_subject'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        for state in RunState::ALL {
            assert_eq!(
                sql.contains(&format!("'{}'", state.as_str())),
                state.is_open(),
                "{state}: {sql}"
            );
        }
    }

    #[test]
    fn next_queued_is_fifo_per_repo_and_active_run_sees_in_flight_states() {
        let store = Store::open_in_memory().unwrap();
        let a1 = store
            .enqueue_run("o/a", &issue(1), "issue", 1)
            .unwrap()
            .unwrap();
        let b1 = store
            .enqueue_run("o/b", &issue(1), "issue", 2)
            .unwrap()
            .unwrap();
        let a2 = store
            .enqueue_run("o/a", &issue(2), "issue", 3)
            .unwrap()
            .unwrap();
        assert_eq!(store.next_queued("o/a").unwrap().unwrap().id, a1);
        assert_eq!(store.next_queued("o/b").unwrap().unwrap().id, b1);
        assert!(store.next_queued("o/none").unwrap().is_none());
        assert!(store.active_run("o/a").unwrap().is_none());

        store.set_run_state(a1, RunState::Running, 4).unwrap();
        assert_eq!(store.active_run("o/a").unwrap().unwrap().id, a1);
        assert_eq!(store.next_queued("o/a").unwrap().unwrap().id, a2);
        assert!(store.active_run("o/b").unwrap().is_none());

        for state in [RunState::AwaitingCi, RunState::Judging] {
            store.set_run_state(a1, state, 5).unwrap();
            assert_eq!(store.active_run("o/a").unwrap().unwrap().id, a1);
        }
        store.set_run_state(a1, RunState::PrOpen, 6).unwrap();
        assert!(store.active_run("o/a").unwrap().is_none());
    }

    #[test]
    fn attempts_need_a_running_run_and_are_numbered_per_run() {
        let store = Store::open_in_memory().unwrap();
        let r1 = store
            .enqueue_run("o/r", &issue(1), "issue", 1)
            .unwrap()
            .unwrap();
        let r2 = store
            .enqueue_run("o/r", &issue(2), "issue", 1)
            .unwrap()
            .unwrap();
        assert!(matches!(
            store.start_attempt(r1, 2),
            Err(StoreError::RunNotRunning {
                state: RunState::Queued,
                ..
            })
        ));
        store.set_run_state(r1, RunState::Running, 2).unwrap();
        store.set_run_state(r2, RunState::Running, 2).unwrap();
        assert_eq!(store.start_attempt(r1, 2).unwrap().n, 1);
        assert_eq!(store.start_attempt(r1, 3).unwrap().n, 2);
        assert_eq!(store.start_attempt(r2, 4).unwrap().n, 1);
        let all = store.attempts(r1).unwrap();
        assert_eq!(all.iter().map(|a| a.n).collect::<Vec<_>>(), vec![1, 2]);
        assert!(all.iter().all(|a| a.state == AttemptState::Dispatched));

        store.set_run_state(r1, RunState::Failed, 5).unwrap();
        let err = store.start_attempt(r1, 6).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::RunNotRunning {
                    state: RunState::Failed,
                    ..
                }
            ),
            "{err}"
        );
        assert_eq!(store.attempts(r1).unwrap().len(), 2);
    }

    #[test]
    fn failed_attempts_keep_class_and_raw_error() {
        let (store, run) = running_store();
        let attempt = store.start_attempt(run, 11).unwrap();
        store
            .finish_attempt_failed(attempt.id, FailureClass::MaxIterations, "ran out", 12)
            .unwrap();
        let done = store.attempt(attempt.id).unwrap();
        assert_eq!(done.state, AttemptState::Failed);
        assert_eq!(done.failure_class, Some(FailureClass::MaxIterations));
        assert_eq!(done.raw_error.as_deref(), Some("ran out"));
        assert_eq!(done.ended_at, Some(12));
    }

    #[test]
    fn successful_attempts_record_the_pr() {
        let (store, run) = running_store();
        let attempt = store.start_attempt(run, 11).unwrap();
        store
            .record_pr(attempt.id, "inchworm/1-a1", "abc123", 42)
            .unwrap();
        store.finish_attempt_ok(attempt.id, 13).unwrap();
        let done = store.attempt(attempt.id).unwrap();
        assert_eq!(done.state, AttemptState::Succeeded);
        assert_eq!(done.branch.as_deref(), Some("inchworm/1-a1"));
        assert_eq!(done.head_sha.as_deref(), Some("abc123"));
        assert_eq!(done.pr_number, Some(42));
        assert_eq!(done.failure_class, None);
        assert_eq!(done.raw_error, None);
    }

    #[test]
    fn finished_attempts_cannot_be_changed_again() {
        let (store, run) = running_store();
        let attempt = store.start_attempt(run, 11).unwrap();
        store.finish_attempt_ok(attempt.id, 12).unwrap();
        assert!(matches!(
            store.finish_attempt_ok(attempt.id, 13),
            Err(StoreError::AttemptNotDispatched(_))
        ));
        assert!(matches!(
            store.finish_attempt_failed(attempt.id, FailureClass::Timeout, "x", 13),
            Err(StoreError::AttemptNotDispatched(_))
        ));
        assert!(matches!(
            store.record_pr(attempt.id, "b", "s", 7),
            Err(StoreError::AttemptNotDispatched(_))
        ));
        let unchanged = store.attempt(attempt.id).unwrap();
        assert_eq!(unchanged.ended_at, Some(12));
        assert_eq!(unchanged.pr_number, None);
        assert!(matches!(
            store.finish_attempt_ok(999, 1),
            Err(StoreError::AttemptNotFound(999))
        ));
        assert!(matches!(
            store.record_pr(999, "b", "s", 1),
            Err(StoreError::AttemptNotFound(999))
        ));
    }

    fn lock_for(dir: &tempfile::TempDir) -> DaemonLock {
        DaemonLock::acquire(&dir.path().join("recovery.db")).unwrap()
    }

    #[test]
    fn recovery_fails_dispatched_attempts_and_requeues_the_run_so_the_subject_is_not_wedged() {
        let dir = tempfile::tempdir().unwrap();
        let (store, run) = running_store();
        let lost = store.start_attempt(run, 12).unwrap();
        assert!(
            store
                .enqueue_run("o/r", &issue(1), "issue", 13)
                .unwrap()
                .is_none()
        );

        let report = store.recover_after_crash(&lock_for(&dir), 20).unwrap();
        assert_eq!(
            report,
            Recovery {
                attempts_failed: 1,
                runs_requeued: 1
            }
        );

        let lost = store.attempt(lost.id).unwrap();
        assert_eq!(lost.state, AttemptState::Failed);
        assert_eq!(lost.failure_class, Some(FailureClass::OrchestratorLost));
        assert_eq!(
            lost.raw_error.as_deref(),
            Some("inchworm stopped while this attempt was dispatched")
        );
        assert_eq!(lost.ended_at, Some(20));

        let recovered = store.run(run).unwrap();
        assert_eq!(recovered.state, RunState::Queued);
        assert_eq!(recovered.updated_at, 20);
        assert!(store.active_run("o/r").unwrap().is_none());
        assert_eq!(store.next_queued("o/r").unwrap().unwrap().id, run);
        assert!(
            store
                .enqueue_run("o/r", &issue(1), "issue", 21)
                .unwrap()
                .is_none()
        );

        store.set_run_state(run, RunState::Running, 22).unwrap();
        assert_eq!(store.start_attempt(run, 23).unwrap().n, 2);
    }

    #[test]
    fn recovery_leaves_finished_attempts_and_other_runs_alone() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let done_run = store
            .enqueue_run("o/r", &issue(1), "issue", 1)
            .unwrap()
            .unwrap();
        store.set_run_state(done_run, RunState::Running, 2).unwrap();
        let ok = store.start_attempt(done_run, 3).unwrap();
        store.finish_attempt_ok(ok.id, 4).unwrap();
        let bad = store.start_attempt(done_run, 5).unwrap();
        store
            .finish_attempt_failed(bad.id, FailureClass::ModelError, "boom", 6)
            .unwrap();
        store
            .set_run_state(done_run, RunState::AwaitingCi, 7)
            .unwrap();

        let queued = store
            .enqueue_run("o/r", &issue(2), "issue", 8)
            .unwrap()
            .unwrap();

        let report = store.recover_after_crash(&lock_for(&dir), 30).unwrap();
        assert_eq!(report, Recovery::default());

        assert_eq!(store.attempt(ok.id).unwrap().state, AttemptState::Succeeded);
        assert_eq!(
            store.attempt(bad.id).unwrap().failure_class,
            Some(FailureClass::ModelError)
        );
        assert_eq!(store.run(done_run).unwrap().state, RunState::AwaitingCi);
        assert_eq!(store.run(queued).unwrap().state, RunState::Queued);
    }

    #[test]
    fn recovery_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let lock = lock_for(&dir);
        let (store, run) = running_store();
        store.start_attempt(run, 12).unwrap();
        assert_eq!(
            store
                .recover_after_crash(&lock, 20)
                .unwrap()
                .attempts_failed,
            1
        );
        assert_eq!(
            store.recover_after_crash(&lock, 21).unwrap(),
            Recovery::default()
        );
        assert_eq!(store.run(run).unwrap().updated_at, 20);
    }

    #[test]
    fn recovery_does_not_touch_a_run_that_cannot_be_requeued() {
        let dir = tempfile::tempdir().unwrap();
        let (store, run) = running_store();
        store.start_attempt(run, 12).unwrap();
        force_state(&store, run, "judging");
        let report = store.recover_after_crash(&lock_for(&dir), 20).unwrap();
        assert_eq!(report.attempts_failed, 1);
        assert_eq!(report.runs_requeued, 0);
        assert_eq!(store.run(run).unwrap().state, RunState::Judging);
    }

    #[test]
    fn state_survives_reopen_so_a_restart_can_resume() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let store = Store::open(&path).unwrap();
        let run = store
            .enqueue_run("o/r", &issue(5), "issue", 1)
            .unwrap()
            .unwrap();
        store.set_run_state(run, RunState::Running, 2).unwrap();
        let attempt = store.start_attempt(run, 3).unwrap();
        drop(store);

        let store = Store::open(&path).unwrap();
        let lock = DaemonLock::acquire(&path).unwrap();
        let report = store.recover_after_crash(&lock, 10).unwrap();
        assert_eq!(report.attempts_failed, 1);
        assert_eq!(store.run(run).unwrap().state, RunState::Queued);
        assert_eq!(
            store.attempt(attempt.id).unwrap().failure_class,
            Some(FailureClass::OrchestratorLost)
        );
    }

    #[test]
    fn the_daemon_lock_is_exclusive_and_reports_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let first = DaemonLock::acquire(&db).unwrap();
        match DaemonLock::acquire(&db) {
            Err(StoreError::AlreadyRunning(path)) => {
                assert!(path.ends_with("state.db.daemon.lock"), "{}", path.display());
            }
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
        drop(first);
        assert!(DaemonLock::acquire(&db).is_ok());
        let missing = dir.path().join("no-such-dir").join("state.db");
        assert!(matches!(
            DaemonLock::acquire(&missing),
            Err(StoreError::LockIo { .. })
        ));
    }

    #[test]
    fn corrupt_rows_are_reported_not_trusted() {
        let (store, run) = running_store();
        let attempt = store.start_attempt(run, 12).unwrap();

        force_state(&store, run, "weird");
        assert!(matches!(store.run(run), Err(StoreError::Corrupt(_))));
        force_state(&store, run, "running");

        store
            .raw()
            .execute_batch("PRAGMA ignore_check_constraints = ON")
            .unwrap();
        let cases = [
            "UPDATE runs SET subject_kind = 'weird'",
            "UPDATE attempts SET state = 'weird'",
            "UPDATE attempts SET n = -1",
            "UPDATE attempts SET pr_number = -5",
            "UPDATE attempts SET failure_class = 'not_a_class'",
        ];
        for statement in cases {
            let backup_run = store.run(run).ok();
            let backup_attempt = store.attempt(attempt.id).ok();
            store.raw().execute(statement, []).unwrap();
            let broken = store.run(run).is_err() || store.attempt(attempt.id).is_err();
            assert!(broken, "{statement}");
            assert!(
                matches!(
                    store
                        .run(run)
                        .err()
                        .or_else(|| store.attempt(attempt.id).err()),
                    Some(StoreError::Corrupt(_))
                ),
                "{statement}"
            );
            store
                .raw()
                .execute_batch(
                    "UPDATE runs SET subject_kind = 'issue';
                     UPDATE attempts SET state = 'dispatched', n = 1, pr_number = NULL, failure_class = NULL;",
                )
                .unwrap();
            assert_eq!(store.run(run).ok(), backup_run);
            assert_eq!(store.attempt(attempt.id).ok(), backup_attempt);
        }
    }

    proptest! {
        #[test]
        fn attempt_numbers_are_sequential(count in 1usize..12) {
            let (store, run) = running_store();
            for expected in 1..=count {
                prop_assert_eq!(store.start_attempt(run, 1).unwrap().n as usize, expected);
            }
        }

        #[test]
        fn store_follows_the_documented_edge_table(steps in prop::collection::vec(0..RunState::ALL.len(), 0..16)) {
            let (store, run) = store_with_run();
            let mut state = RunState::Queued;
            for index in steps {
                let target = RunState::ALL[index];
                let legal = EDGES.contains(&(state, target));
                let result = store.set_run_state(run, target, 1);
                prop_assert_eq!(result.is_ok(), legal);
                if legal {
                    state = target;
                }
                prop_assert_eq!(store.run(run).unwrap().state, state);
            }
        }

        #[test]
        fn failure_class_and_text_round_trip(index in 0..FailureClass::ALL.len(), raw in ".{0,40}") {
            let (store, run) = running_store();
            let attempt = store.start_attempt(run, 1).unwrap();
            let class = FailureClass::ALL[index];
            store.finish_attempt_failed(attempt.id, class, &raw, 2).unwrap();
            let done = store.attempt(attempt.id).unwrap();
            prop_assert_eq!(done.failure_class, Some(class));
            prop_assert_eq!(done.raw_error.as_deref(), Some(raw.as_str()));
        }

        #[test]
        fn hex_commit_ids_are_accepted_and_lowercased(sha in "[0-9a-fA-F]{7,64}") {
            let subject = Subject::commit(&sha).unwrap();
            prop_assert_eq!(subject.reference, sha.to_ascii_lowercase());
        }

        #[test]
        fn non_hex_commit_ids_are_rejected(sha in "[0-9a-f]{3,20}[g-z][0-9a-f]{3,20}") {
            prop_assert!(Subject::commit(&sha).is_err());
        }
    }
}
