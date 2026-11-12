use std::collections::HashSet;
use std::fs::{File, Metadata, OpenOptions, TryLockError};
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
const MAX_BRANCH_BYTES: usize = 255;
const MAX_HEAD_SHA_BYTES: usize = 64;

fn states_sql(states: &[RunState]) -> String {
    states
        .iter()
        .map(|state| format!("'{}'", state.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Migration 1 is frozen: it must never be edited once a database may have
/// been created from it. A change to the schema or to the run states needs a
/// new entry in this list that alters or rebuilds the tables.
const MIGRATIONS: &[&str] = &["
CREATE TABLE runs (
    id INTEGER PRIMARY KEY,
    repo TEXT NOT NULL,
    subject_kind TEXT NOT NULL CHECK (subject_kind IN ('issue', 'pr', 'commit')),
    subject_ref TEXT NOT NULL CHECK (length(subject_ref) > 0),
    flow TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('queued', 'running', 'awaiting_ci', 'judging', 'merged', 'pr_open', 'failed', 'escalated')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX runs_one_open_per_subject
    ON runs (repo, subject_kind, subject_ref, flow)
    WHERE state IN ('queued', 'running', 'awaiting_ci', 'judging');
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
"];

const FROZEN_OBJECTS: &[(&str, &str, &str)] = &[
    (
        "table",
        "runs",
        "CREATE TABLE runs (
    id INTEGER PRIMARY KEY,
    repo TEXT NOT NULL,
    subject_kind TEXT NOT NULL CHECK (subject_kind IN ('issue', 'pr', 'commit')),
    subject_ref TEXT NOT NULL CHECK (length(subject_ref) > 0),
    flow TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('queued', 'running', 'awaiting_ci', 'judging', 'merged', 'pr_open', 'failed', 'escalated')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
)",
    ),
    (
        "index",
        "runs_one_open_per_subject",
        "CREATE UNIQUE INDEX runs_one_open_per_subject
    ON runs (repo, subject_kind, subject_ref, flow)
    WHERE state IN ('queued', 'running', 'awaiting_ci', 'judging')",
    ),
    (
        "table",
        "attempts",
        "CREATE TABLE attempts (
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
)",
    ),
];

fn normalise_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

const EXPECTED_RUN_COLUMNS: &[&str] = &[
    "id",
    "repo",
    "subject_kind",
    "subject_ref",
    "flow",
    "state",
    "created_at",
    "updated_at",
];

const EXPECTED_ATTEMPT_COLUMNS: &[&str] = &[
    "id",
    "run_id",
    "n",
    "state",
    "branch",
    "head_sha",
    "pr_number",
    "failure_class",
    "raw_error",
    "started_at",
    "ended_at",
];

const EXPECTED_INDEX_COLUMNS: &[&str] = &["repo", "subject_kind", "subject_ref", "flow"];

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
    #[error(
        "the database schema does not match this build ({0}); it was probably created by an unreleased development build: delete the file and start again"
    )]
    SchemaMismatch(String),
    #[error("invalid subject: {0}")]
    InvalidSubject(String),
    #[error("invalid pull request record: {0}")]
    InvalidPullRequest(String),
    #[error("another inchworm daemon holds {0}")]
    AlreadyRunning(PathBuf),
    #[error("the daemon lock is for {lock}, not for the database {store}")]
    LockForDifferentDatabase { lock: PathBuf, store: PathBuf },
    #[error("only a database file can be recovered, not an in-memory store")]
    RecoveryNeedsFileStore,
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
/// A subject is valid by construction and has exactly one spelling: issue and
/// pull request numbers are positive decimals without leading zeros, and a
/// commit is a full SHA-1 or SHA-256 in lowercase hex. Two spellings of one
/// subject therefore cannot both be opened.
///
/// ```
/// use inchworm::store::{Subject, SubjectKind};
///
/// let issue = Subject::issue(7).unwrap();
/// assert_eq!(issue.kind(), SubjectKind::Issue);
/// assert_eq!(issue.reference(), "7");
/// assert!(Subject::issue(0).is_err());
/// assert!(Subject::commit("0123abc").is_err());
/// let sha = "0123456789ABCDEF0123456789abcdef01234567";
/// assert_eq!(Subject::commit(sha).unwrap().reference(), sha.to_ascii_lowercase());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    kind: SubjectKind,
    reference: String,
}

impl Subject {
    pub fn issue(number: u64) -> Result<Subject, StoreError> {
        Subject::from_parts(SubjectKind::Issue, &number.to_string())
    }

    pub fn pr(number: u64) -> Result<Subject, StoreError> {
        Subject::from_parts(SubjectKind::Pr, &number.to_string())
    }

    pub fn commit(sha: &str) -> Result<Subject, StoreError> {
        Subject::from_parts(SubjectKind::Commit, &sha.to_ascii_lowercase())
    }

    /// Builds a subject from its stored parts, accepting only the canonical
    /// spelling.
    pub fn from_parts(kind: SubjectKind, reference: &str) -> Result<Subject, StoreError> {
        let valid = match kind {
            SubjectKind::Issue | SubjectKind::Pr => {
                reference.parse::<u64>().is_ok()
                    && reference.bytes().all(|b| b.is_ascii_digit())
                    && !reference.starts_with('0')
            }
            SubjectKind::Commit => {
                matches!(reference.len(), 40 | 64)
                    && reference
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            }
        };
        if !valid {
            return Err(StoreError::InvalidSubject(format!(
                "`{reference}` is not a canonical {} reference",
                kind.as_str()
            )));
        }
        Ok(Subject {
            kind,
            reference: reference.to_string(),
        })
    }

    pub fn kind(&self) -> SubjectKind {
        self.kind
    }

    pub fn reference(&self) -> &str {
        &self.reference
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

/// Proof that this process is the only daemon using one state database.
///
/// On unix the lock is an exclusive `flock` on the database file itself, which
/// is created empty when missing, and the lock is identified by the file's
/// device and inode. Symlinks, hard links and a dangling symlink that is later
/// filled in therefore all lead to one lock, and there is no side file to
/// delete. [`Store::recover_after_crash`] refuses a lock that belongs to a
/// different file. Elsewhere the lock is a `<database>.daemon.lock` file next
/// to the canonical path and is identified by that path.
///
/// Names that SQLite treats as in-memory (`:memory:`, the empty name and
/// `file:` URIs naming a memory database) have no file and are refused.
///
/// The lock excludes only other processes that also take it: a process that
/// dispatches attempts without calling [`DaemonLock::acquire`] is not stopped.
/// Dropping the lock closes a descriptor on the database file, so drop it only
/// when no store of the same process is in the middle of a transaction.
///
/// ```
/// use inchworm::store::DaemonLock;
///
/// let dir = tempfile::tempdir().unwrap();
/// let db = dir.path().join("state.db");
/// let first = DaemonLock::acquire(&db).unwrap();
/// assert!(DaemonLock::acquire(&db).is_err());
/// assert!(first.database().ends_with("state.db"));
/// drop(first);
/// assert!(DaemonLock::acquire(&db).is_ok());
/// ```
#[derive(Debug)]
pub struct DaemonLock {
    _file: File,
    database: PathBuf,
    identity: FileIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    path: PathBuf,
}

#[cfg(unix)]
fn file_identity(meta: &Metadata, _canonical: &Path) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    FileIdentity {
        device: meta.dev(),
        inode: meta.ino(),
    }
}

#[cfg(not(unix))]
fn file_identity(_meta: &Metadata, canonical: &Path) -> FileIdentity {
    FileIdentity {
        path: canonical.to_path_buf(),
    }
}

#[cfg(unix)]
fn lock_file_path(database: &Path) -> PathBuf {
    database.to_path_buf()
}

#[cfg(not(unix))]
fn lock_file_path(database: &Path) -> PathBuf {
    let mut name = database.as_os_str().to_owned();
    name.push(".daemon.lock");
    PathBuf::from(name)
}

fn is_in_memory(path: &Path) -> bool {
    let Some(text) = path.to_str() else {
        return false;
    };
    text.is_empty()
        || text == ":memory:"
        || (text.starts_with("file:")
            && (text.contains(":memory:") || text.contains("mode=memory")))
}

fn canonical_db_path(path: &Path) -> std::io::Result<PathBuf> {
    match path.canonicalize() {
        Ok(canonical) => Ok(canonical),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let name = path.file_name().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "the database path has no file name",
                )
            })?;
            Ok(parent.canonicalize()?.join(name))
        }
        Err(err) => Err(err),
    }
}

impl DaemonLock {
    pub fn acquire(db_path: &Path) -> Result<DaemonLock, StoreError> {
        if is_in_memory(db_path) {
            return Err(StoreError::RecoveryNeedsFileStore);
        }
        let database = canonical_db_path(db_path).map_err(|source| StoreError::LockIo {
            path: db_path.to_path_buf(),
            source,
        })?;
        let path = lock_file_path(&database);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|source| StoreError::LockIo {
                path: path.clone(),
                source,
            })?;
        let database = canonical_db_path(&database).unwrap_or(database);
        let identity = file
            .metadata()
            .map(|meta| file_identity(&meta, &database))
            .map_err(|source| StoreError::LockIo {
                path: path.clone(),
                source,
            })?;
        match file.try_lock() {
            Ok(()) => Ok(DaemonLock {
                _file: file,
                database,
                identity,
            }),
            Err(TryLockError::WouldBlock) => Err(StoreError::AlreadyRunning(path)),
            Err(TryLockError::Error(source)) => Err(StoreError::LockIo { path, source }),
        }
    }

    pub fn database(&self) -> &Path {
        &self.database
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Recovery {
    pub attempts_failed: usize,
    pub runs_requeued: usize,
    pub resumable: Vec<RunId>,
}

struct DatabaseFile {
    path: PathBuf,
    identity: FileIdentity,
}

pub struct Store {
    conn: Connection,
    file: Option<DatabaseFile>,
}

fn corrupt<E: std::fmt::Display>(what: &str) -> impl FnOnce(E) -> StoreError + '_ {
    move |err| StoreError::Corrupt(format!("{what}: {err}"))
}

fn check_text(field: &str, value: &str, max_bytes: usize) -> Result<(), StoreError> {
    let problem = if value.trim().is_empty() {
        Some("must not be blank".to_string())
    } else if value.len() > max_bytes {
        Some(format!("must be at most {max_bytes} bytes"))
    } else if value.chars().any(char::is_control) {
        Some("must not contain control characters".to_string())
    } else {
        None
    };
    match problem {
        Some(problem) => Err(StoreError::InvalidPullRequest(format!("{field} {problem}"))),
        None => Ok(()),
    }
}

fn run_from_row(row: &Row<'_>) -> Result<Run, StoreError> {
    let state: String = row.get("state")?;
    let kind: String = row.get("subject_kind")?;
    Ok(Run {
        id: row.get("id")?,
        repo: row.get("repo")?,
        subject: Subject::from_parts(
            SubjectKind::parse(&kind)?,
            &row.get::<_, String>("subject_ref")?,
        )
        .map_err(|err| StoreError::Corrupt(err.to_string()))?,
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

fn enable_wal(conn: &Connection, timeout: Duration) -> Result<(), StoreError> {
    let deadline = Instant::now() + timeout;
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
    /// let subject = Subject::issue(7).unwrap();
    /// let id = store.enqueue_run("o/r", &subject, "issue", 100).unwrap().unwrap();
    /// assert!(store.enqueue_run("o/r", &subject, "issue", 101).unwrap().is_none());
    /// store.set_run_state(id, RunState::Running, 102).unwrap();
    /// assert_eq!(store.run(id).unwrap().state, RunState::Running);
    /// ```
    pub fn open_in_memory() -> Result<Store, StoreError> {
        Store::from_connection(Connection::open_in_memory()?, false, BUSY_TIMEOUT)
    }

    pub fn open(path: &Path) -> Result<Store, StoreError> {
        Store::open_with_busy_timeout(path, BUSY_TIMEOUT)
    }

    pub(crate) fn open_with_busy_timeout(
        path: &Path,
        timeout: Duration,
    ) -> Result<Store, StoreError> {
        let conn = Connection::open(path)?;
        let in_memory = is_in_memory(path);
        let mut store = Store::from_connection(conn, !in_memory, timeout)?;
        if !in_memory {
            let canonical = canonical_db_path(path).unwrap_or_else(|_| path.to_path_buf());
            let meta = std::fs::metadata(path).map_err(|source| StoreError::LockIo {
                path: path.to_path_buf(),
                source,
            })?;
            store.file = Some(DatabaseFile {
                identity: file_identity(&meta, &canonical),
                path: canonical,
            });
        }
        Ok(store)
    }

    fn from_connection(
        conn: Connection,
        use_wal: bool,
        timeout: Duration,
    ) -> Result<Store, StoreError> {
        conn.busy_timeout(timeout)?;
        if use_wal {
            enable_wal(&conn, timeout)?;
        }
        conn.pragma_update(None, "foreign_keys", true)?;
        let mut store = Store { conn, file: None };
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
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = Store::version_in(&tx)?;
        if current > MIGRATIONS.len() {
            return Err(StoreError::SchemaTooNew {
                found: current,
                supported: MIGRATIONS.len(),
            });
        }
        for (index, sql) in MIGRATIONS.iter().enumerate().skip(current) {
            tx.execute_batch(sql)?;
            tx.pragma_update(
                None,
                "user_version",
                i64::try_from(index + 1).unwrap_or(i64::MAX),
            )?;
        }
        tx.commit()?;
        self.verify_schema()
    }

    fn verify_schema(&self) -> Result<(), StoreError> {
        let columns = |table: &str| -> Result<Vec<String>, StoreError> {
            let mut statement = self
                .conn
                .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?;
            let names = statement
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            Ok(names)
        };
        let mismatch = |detail: String| Err(StoreError::SchemaMismatch(detail));
        for (table, expected) in [
            ("runs", EXPECTED_RUN_COLUMNS),
            ("attempts", EXPECTED_ATTEMPT_COLUMNS),
        ] {
            let found = columns(table)?;
            if found != expected {
                return mismatch(format!(
                    "table `{table}` has columns {found:?}, expected {expected:?}"
                ));
            }
        }
        let index: Option<(bool, bool)> = self
            .conn
            .query_row(
                "SELECT \"unique\", partial FROM pragma_index_list('runs') WHERE name = 'runs_one_open_per_subject'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if index != Some((true, true)) {
            return mismatch(
                "the partial unique index `runs_one_open_per_subject` is missing or different"
                    .into(),
            );
        }
        let mut statement = self.conn.prepare(
            "SELECT name FROM pragma_index_info('runs_one_open_per_subject') ORDER BY seqno",
        )?;
        let indexed: Vec<String> = statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        if indexed != EXPECTED_INDEX_COLUMNS {
            return mismatch(format!(
                "the index `runs_one_open_per_subject` covers {indexed:?}, expected {EXPECTED_INDEX_COLUMNS:?}"
            ));
        }
        for (kind, name, expected) in FROZEN_OBJECTS {
            let stored: Option<String> = self
                .conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type = ?1 AND name = ?2",
                    [kind, name],
                    |row| row.get(0),
                )
                .optional()?;
            if stored.as_deref().map(normalise_sql) != Some(normalise_sql(expected)) {
                return mismatch(format!(
                    "the definition of the {kind} `{name}` differs from migration 1"
                ));
            }
        }
        Ok(())
    }

    pub fn enqueue_run(
        &self,
        repo: &str,
        subject: &Subject,
        flow: &str,
        now: i64,
    ) -> Result<Option<RunId>, StoreError> {
        let tx = self.immediate()?;
        let inserted = tx.execute(
            "INSERT INTO runs (repo, subject_kind, subject_ref, flow, state, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                repo,
                subject.kind().as_str(),
                subject.reference(),
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

    /// Records the branch, head commit and pull request an attempt produced.
    ///
    /// The recorded pull request number is what sends a `running` run to
    /// `awaiting_ci` during recovery, so it is validated: `pr_number` must be
    /// positive, `branch` must not be blank and at most 255 bytes, `head_sha`
    /// must not be blank and at most 64 bytes, and neither text may contain a
    /// control character. Anything else is [`StoreError::InvalidPullRequest`]
    /// and nothing is written.
    ///
    /// ```
    /// use inchworm::model::RunState;
    /// use inchworm::store::{Store, StoreError, Subject};
    ///
    /// let store = Store::open_in_memory().unwrap();
    /// let subject = Subject::issue(1).unwrap();
    /// let run = store.enqueue_run("o/r", &subject, "issue", 0).unwrap().unwrap();
    /// store.set_run_state(run, RunState::Running, 1).unwrap();
    /// let attempt = store.start_attempt(run, 2).unwrap();
    /// assert!(matches!(
    ///     store.record_pr(attempt.id, "feature", "abc123", 0),
    ///     Err(StoreError::InvalidPullRequest(_))
    /// ));
    /// store.record_pr(attempt.id, "feature", "abc123", 7).unwrap();
    /// assert_eq!(store.attempt(attempt.id).unwrap().pr_number, Some(7));
    /// ```
    pub fn record_pr(
        &self,
        id: AttemptId,
        branch: &str,
        head_sha: &str,
        pr_number: u64,
    ) -> Result<(), StoreError> {
        if pr_number == 0 {
            return Err(StoreError::InvalidPullRequest(
                "the pull request number must be positive".into(),
            ));
        }
        check_text("branch", branch, MAX_BRANCH_BYTES)?;
        check_text("head sha", head_sha, MAX_HEAD_SHA_BYTES)?;
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

    /// Recovers the runs a dead daemon left in flight. Every run in `running`,
    /// `awaiting_ci` or `judging` belonged to it, so recovery decides by run
    /// state, never by whether the run still owns a dispatched attempt:
    ///
    /// - every dispatched attempt is failed as `orchestrator_lost`;
    /// - a `running` run whose latest attempt already recorded a pull request
    ///   (finished, or lost after `record_pr`) moves to `awaiting_ci`, because
    ///   the work exists and must not be redone;
    /// - any other `running` run goes back to `queued`, so its subject is not
    ///   wedged behind an open run;
    /// - `awaiting_ci` and `judging` runs keep their state and are listed in
    ///   [`Recovery::resumable`] with the `awaiting_ci` ones recovered above,
    ///   for the daemon to continue from the stored pull request and head SHA;
    /// - `queued` and terminal runs are not touched.
    ///
    /// Calling it again changes nothing and reports the same resumable runs.
    ///
    /// The lock must be the one for this database file (see [`DaemonLock`]); an
    /// in-memory store cannot be recovered.
    ///
    /// ```
    /// use inchworm::model::{FailureClass, RunState};
    /// use inchworm::store::{DaemonLock, Store, Subject};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let db = dir.path().join("state.db");
    /// let store = Store::open(&db).unwrap();
    /// let subject = Subject::issue(1).unwrap();
    /// let run = store.enqueue_run("o/r", &subject, "issue", 0).unwrap().unwrap();
    /// store.set_run_state(run, RunState::Running, 1).unwrap();
    /// let attempt = store.start_attempt(run, 2).unwrap();
    ///
    /// let lock = DaemonLock::acquire(&db).unwrap();
    /// let report = store.recover_after_crash(&lock, 3).unwrap();
    /// assert_eq!((report.attempts_failed, report.runs_requeued), (1, 1));
    /// assert!(report.resumable.is_empty());
    /// assert_eq!(store.run(run).unwrap().state, RunState::Queued);
    /// let lost = store.attempt(attempt.id).unwrap();
    /// assert_eq!(lost.failure_class, Some(FailureClass::OrchestratorLost));
    /// ```
    pub fn recover_after_crash(
        &self,
        daemon: &DaemonLock,
        now: i64,
    ) -> Result<Recovery, StoreError> {
        let own = self
            .file
            .as_ref()
            .ok_or(StoreError::RecoveryNeedsFileStore)?;
        if daemon.identity != own.identity {
            return Err(StoreError::LockForDifferentDatabase {
                lock: daemon.database().to_path_buf(),
                store: own.path.clone(),
            });
        }
        let tx = self.immediate()?;
        let mut statement = tx.prepare("SELECT id FROM attempts WHERE state = 'dispatched'")?;
        let lost: HashSet<AttemptId> = statement
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

        let sql = format!(
            "SELECT * FROM runs WHERE state IN ({}) ORDER BY id",
            states_sql(RunState::ACTIVE)
        );
        let mut statement = tx.prepare(&sql)?;
        let runs: Vec<Run> = statement
            .query_map([], |row| Ok(run_from_row(row)))?
            .map(|row| row?)
            .collect::<Result<_, _>>()?;
        drop(statement);

        let mut recovery = Recovery {
            attempts_failed,
            ..Recovery::default()
        };
        for run in runs {
            if run.state != RunState::Running {
                recovery.resumable.push(run.id);
                continue;
            }
            let latest = tx
                .query_row(
                    "SELECT * FROM attempts WHERE run_id = ?1 ORDER BY n DESC LIMIT 1",
                    [run.id],
                    |row| Ok(attempt_from_row(row)),
                )
                .optional()?
                .transpose()?;
            let published = latest.is_some_and(|attempt| {
                attempt.pr_number.is_some()
                    && (attempt.state == AttemptState::Succeeded || lost.contains(&attempt.id))
            });
            let to = if published {
                recovery.resumable.push(run.id);
                RunState::AwaitingCi
            } else {
                recovery.runs_requeued += 1;
                RunState::Queued
            };
            tx.execute(
                "UPDATE runs SET state = ?1, updated_at = ?2 WHERE id = ?3",
                params![to.as_str(), now, run.id],
            )?;
        }
        tx.commit()?;
        Ok(recovery)
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

    const SHA1: &str = "0123456789abcdef0123456789abcdef01234567";
    const SHA256: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn issue(n: u64) -> Subject {
        Subject::issue(n).unwrap()
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
        assert_eq!(first.schema_version().unwrap(), MIGRATIONS.len());
        let id = first
            .enqueue_run("o/r", &issue(1), "issue", 1)
            .unwrap()
            .unwrap();
        drop(first);
        let second = Store::open(&path).unwrap();
        assert_eq!(second.schema_version().unwrap(), MIGRATIONS.len());
        assert_eq!(second.run(id).unwrap().repo, "o/r");
    }

    const UNRELEASED_130EFD6A_SCHEMA: &str = "
CREATE TABLE runs (
    id INTEGER PRIMARY KEY,
    repo TEXT NOT NULL,
    issue INTEGER NOT NULL,
    flow TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX runs_one_open_per_issue
    ON runs (repo, issue, flow)
    WHERE state IN ('queued', 'running', 'awaiting_ci', 'judging');
CREATE TABLE attempts (
    id INTEGER PRIMARY KEY,
    run_id INTEGER NOT NULL REFERENCES runs(id),
    n INTEGER NOT NULL,
    state TEXT NOT NULL,
    branch TEXT,
    head_sha TEXT,
    pr_number INTEGER,
    failure_class TEXT,
    raw_error TEXT,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    UNIQUE (run_id, n)
);
";

    fn mismatch(result: Result<Store, StoreError>) -> String {
        match result {
            Err(error @ StoreError::SchemaMismatch(_)) => error.to_string(),
            Err(other) => panic!("expected SchemaMismatch, got {other}"),
            Ok(_) => panic!("a database with the wrong schema must not open"),
        }
    }

    #[test]
    fn a_database_made_by_the_unreleased_130efd6a_build_is_refused_with_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        let old = Connection::open(&path).unwrap();
        old.execute_batch(UNRELEASED_130EFD6A_SCHEMA).unwrap();
        old.pragma_update(None, "user_version", 1).unwrap();
        drop(old);

        let message = mismatch(Store::open(&path));
        assert!(message.contains("unreleased"), "{message}");
        assert!(message.contains("delete"), "{message}");
        assert!(message.contains("subject_kind"), "{message}");
    }

    #[test]
    fn a_damaged_current_schema_is_refused_whichever_part_is_wrong() {
        let damage = [
            ("missing table", "DROP TABLE attempts"),
            ("missing index", "DROP INDEX runs_one_open_per_subject"),
            (
                "index that is not partial",
                "DROP INDEX runs_one_open_per_subject;
                 CREATE UNIQUE INDEX runs_one_open_per_subject ON runs (repo, subject_kind, subject_ref, flow)",
            ),
            (
                "index that is not unique",
                "DROP INDEX runs_one_open_per_subject;
                 CREATE INDEX runs_one_open_per_subject ON runs (repo, subject_kind, subject_ref, flow)
                 WHERE state IN ('queued', 'running', 'awaiting_ci', 'judging')",
            ),
            (
                "index on other columns",
                "DROP INDEX runs_one_open_per_subject;
                 CREATE UNIQUE INDEX runs_one_open_per_subject ON runs (repo, flow)
                 WHERE state IN ('queued', 'running', 'awaiting_ci', 'judging')",
            ),
            ("missing column", "ALTER TABLE attempts DROP COLUMN raw_error"),
            ("extra column", "ALTER TABLE runs ADD COLUMN extra TEXT"),
        ];
        for (name, sql) in damage {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("damaged.db");
            let store = Store::open(&path).unwrap();
            store.raw().execute_batch(sql).unwrap();
            drop(store);
            let message = mismatch(Store::open(&path));
            assert!(message.contains("delete"), "{name}: {message}");
        }
    }

    fn database_made_from(migration: &str, dir: &tempfile::TempDir) -> PathBuf {
        let path = dir.path().join("variant.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(migration).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        path
    }

    fn altered(from: &str, to: &str) -> String {
        assert_eq!(MIGRATIONS[0].matches(from).count(), 1, "{from}");
        MIGRATIONS[0].replace(from, to)
    }

    #[test]
    fn a_schema_with_the_same_columns_but_different_constraints_or_predicate_is_refused() {
        let variants = [
            (
                "index predicate narrowed",
                altered(
                    "WHERE state IN ('queued', 'running', 'awaiting_ci', 'judging')",
                    "WHERE state IN ('queued')",
                ),
            ),
            (
                "runs state check without escalated",
                altered("'failed', 'escalated')", "'failed')"),
            ),
            (
                "subject_ref check removed",
                altered(" CHECK (length(subject_ref) > 0)", ""),
            ),
            (
                "subject_kind check removed",
                altered(" CHECK (subject_kind IN ('issue', 'pr', 'commit'))", ""),
            ),
            (
                "attempts state check removed",
                altered(
                    " CHECK (state IN ('dispatched', 'succeeded', 'failed'))",
                    "",
                ),
            ),
            (
                "unique run_id and n removed",
                altered(",\n    UNIQUE (run_id, n)", ""),
            ),
            ("foreign key removed", altered(" REFERENCES runs(id)", "")),
        ];
        for (name, migration) in variants {
            let dir = tempfile::tempdir().unwrap();
            let path = database_made_from(&migration, &dir);
            let message = mismatch(Store::open(&path));
            assert!(message.contains("delete"), "{name}: {message}");
            assert!(message.contains("definition"), "{name}: {message}");
        }
    }

    #[test]
    fn a_schema_that_differs_only_in_whitespace_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let spaced = MIGRATIONS[0].replace("    ", "\t").replace(", ", ",\n  ");
        let path = database_made_from(&spaced, &dir);
        assert_eq!(
            Store::open(&path).unwrap().schema_version().unwrap(),
            MIGRATIONS.len()
        );
    }

    #[test]
    fn a_healthy_database_reopens_after_the_schema_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fine.db");
        drop(Store::open(&path).unwrap());
        assert!(Store::open(&path).is_ok());
    }

    fn state_names_after(sql: &str, marker: &str) -> std::collections::BTreeSet<String> {
        let start = sql.find(marker).expect("marker present") + marker.len();
        let list = &sql[start..sql[start..].find(')').unwrap() + start];
        list.split(',')
            .map(|name| name.trim().trim_matches('\'').to_string())
            .collect()
    }

    #[test]
    fn migration_1_is_frozen_and_must_agree_with_the_current_run_states() {
        let sql = MIGRATIONS[0];
        let all: std::collections::BTreeSet<String> = RunState::ALL
            .iter()
            .map(|s| s.as_str().to_string())
            .collect();
        let open: std::collections::BTreeSet<String> = RunState::OPEN
            .iter()
            .map(|s| s.as_str().to_string())
            .collect();
        assert_eq!(
            state_names_after(sql, "CHECK (state IN ("),
            all,
            "RunState changed: add a NEW migration that rebuilds the table, do not edit migration 1"
        );
        assert_eq!(
            state_names_after(sql, "WHERE state IN ("),
            open,
            "RunState::OPEN changed: add a NEW migration that rebuilds the index, do not edit migration 1"
        );
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
                assert_eq!(supported, MIGRATIONS.len());
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
            assert_eq!(version, MIGRATIONS.len());
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
                        let subject = issue(worker * 1000 + n + 1);
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

    fn hold_exclusive_lock(path: &Path) -> Connection {
        let holder = Connection::open(path).unwrap();
        holder
            .execute_batch("CREATE TABLE IF NOT EXISTS t (x); BEGIN EXCLUSIVE")
            .unwrap();
        holder
    }

    #[test]
    fn switching_to_wal_retries_while_the_database_is_busy_and_gives_up_at_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("busy.db");
        let holder = hold_exclusive_lock(&path);
        let waiter = Connection::open(&path).unwrap();
        waiter.busy_timeout(Duration::ZERO).unwrap();

        let started = Instant::now();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(enable_wal(&waiter, Duration::from_millis(150)));
        });
        let err = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("enable_wal ignored its deadline")
            .unwrap_err();
        assert!(
            matches!(&err, StoreError::Sqlite(inner) if is_busy(inner)),
            "{err}"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "gave up too early"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "ignored the deadline"
        );
        drop(holder);
    }

    #[test]
    fn switching_to_wal_succeeds_once_the_lock_is_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("released.db");
        let holder = hold_exclusive_lock(&path);
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            holder.execute_batch("COMMIT").unwrap();
        });
        let waiter = Connection::open(&path).unwrap();
        waiter.busy_timeout(Duration::ZERO).unwrap();
        enable_wal(&waiter, Duration::from_secs(5)).expect("must retry until the lock is gone");
        releaser.join().unwrap();
        let mode: String = waiter
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn switching_to_wal_reports_a_non_busy_error_at_once_without_retrying() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("readonly.db");
        drop(hold_exclusive_lock(&path));
        let readonly = Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();

        let started = Instant::now();
        let err = enable_wal(&readonly, Duration::from_secs(5)).unwrap_err();
        assert!(
            matches!(&err, StoreError::Sqlite(inner) if !is_busy(inner)),
            "{err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "retried a non-busy error"
        );
    }

    #[test]
    fn busy_and_locked_are_retryable_and_other_errors_are_not() {
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        );
        let locked = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_LOCKED),
            None,
        );
        let constraint = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
            None,
        );
        assert!(is_busy(&busy) && is_busy(&locked));
        assert!(!is_busy(&constraint));
        assert!(!is_busy(&rusqlite::Error::QueryReturnedNoRows));
    }

    fn within_ten_seconds<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(work());
        });
        receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("the call hung")
    }

    #[test]
    fn a_database_one_version_beyond_the_supported_one_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("next.db");
        drop(Store::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", MIGRATIONS.len() as i64 + 1)
            .unwrap();
        drop(conn);
        match Store::open(&path) {
            Err(StoreError::SchemaTooNew { found, supported }) => {
                assert_eq!((found, supported), (MIGRATIONS.len() + 1, MIGRATIONS.len()));
            }
            Err(other) => panic!("unexpected error: {other}"),
            Ok(_) => panic!("a newer schema must not open"),
        }
    }

    #[test]
    fn opening_a_locked_database_with_a_short_timeout_fails_fast() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked.db");
        let holder = hold_exclusive_lock(&path);
        let started = Instant::now();
        let result = within_ten_seconds(move || {
            Store::open_with_busy_timeout(&path, Duration::from_millis(100)).map(|_| ())
        });
        assert!(result.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited {:?} instead of about 100 ms",
            started.elapsed()
        );
        drop(holder);
    }

    #[test]
    fn opening_retries_the_wal_switch_while_a_reader_holds_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reader.db");
        let reader = Connection::open(&path).unwrap();
        reader
            .execute_batch("CREATE TABLE t (x); INSERT INTO t VALUES (1); BEGIN; SELECT * FROM t;")
            .unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            reader.execute_batch("COMMIT").unwrap();
        });
        let store = within_ten_seconds(move || {
            Store::open_with_busy_timeout(&path, Duration::from_secs(5))
        })
        .expect("open must keep retrying the WAL switch until the reader is gone");
        releaser.join().unwrap();
        assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len());
    }

    #[test]
    fn opening_waits_for_a_lock_that_is_released_within_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("released-open.db");
        let holder = hold_exclusive_lock(&path);
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            holder.execute_batch("COMMIT").unwrap();
        });
        let store = within_ten_seconds(move || {
            Store::open_with_busy_timeout(&path, Duration::from_secs(5))
        })
        .expect("open must wait for the lock instead of giving up");
        releaser.join().unwrap();
        assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len());
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
                .enqueue_run("o/r", &Subject::pr(1).unwrap(), "issue", 11)
                .unwrap()
                .is_some()
        );
        let commit = Subject::commit(SHA1).unwrap();
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
    fn issue_and_pr_subjects_need_a_positive_number_and_have_one_spelling() {
        assert!(matches!(
            Subject::issue(0),
            Err(StoreError::InvalidSubject(_))
        ));
        assert!(matches!(Subject::pr(0), Err(StoreError::InvalidSubject(_))));
        let seven = Subject::issue(7).unwrap();
        assert_eq!(seven.kind(), SubjectKind::Issue);
        assert_eq!(seven.reference(), "7");
        assert_eq!(Subject::pr(7).unwrap().kind(), SubjectKind::Pr);
        let largest = Subject::issue(u64::MAX).unwrap();
        assert_eq!(largest.reference(), "18446744073709551615");
        assert_eq!(
            Subject::from_parts(SubjectKind::Issue, largest.reference()).unwrap(),
            largest
        );
    }

    #[test]
    fn non_canonical_numbers_are_rejected_when_built_from_parts() {
        for bad in [
            "",
            "0",
            "00",
            "07",
            "+7",
            "-1",
            " 7",
            "7 ",
            "7\n",
            "not a number",
            "1.5",
            "0x7",
            "\u{663}",
            "18446744073709551616",
            "99999999999999999999999",
        ] {
            for kind in [SubjectKind::Issue, SubjectKind::Pr] {
                assert!(
                    matches!(
                        Subject::from_parts(kind, bad),
                        Err(StoreError::InvalidSubject(_))
                    ),
                    "{kind:?} {bad:?}"
                );
            }
        }
        for good in ["1", "7", "10", "123456789"] {
            assert!(
                Subject::from_parts(SubjectKind::Issue, good).is_ok(),
                "{good}"
            );
        }
    }

    #[test]
    fn commit_subjects_must_be_a_full_sha1_or_sha256_in_lowercase_hex() {
        for bad in [
            "",
            "ZZZ",
            "abcdef",
            "0123abc",
            &SHA1[..39],
            &SHA1[..7],
            &SHA256[..41],
            &SHA256[..63],
        ] {
            assert!(Subject::commit(bad).is_err(), "{bad:?}");
        }
        let too_long = format!("{SHA256}0");
        assert!(Subject::commit(&too_long).is_err());
        let not_hex = format!("g{}", &SHA1[1..]);
        assert!(Subject::commit(&not_hex).is_err());
        assert!(Subject::commit(SHA1).is_ok());
        assert!(Subject::commit(SHA256).is_ok());
        assert_eq!(
            Subject::commit(&SHA1.to_ascii_uppercase())
                .unwrap()
                .reference(),
            SHA1
        );
    }

    #[test]
    fn from_parts_only_accepts_the_canonical_spelling() {
        let upper = SHA1.to_ascii_uppercase();
        assert!(Subject::from_parts(SubjectKind::Commit, &upper).is_err());
        assert!(Subject::from_parts(SubjectKind::Commit, SHA1).is_ok());
        assert!(Subject::from_parts(SubjectKind::Commit, "7").is_err());
        assert!(Subject::from_parts(SubjectKind::Issue, SHA1).is_err());
    }

    #[test]
    fn one_subject_cannot_be_opened_twice_under_two_spellings() {
        let store = Store::open_in_memory().unwrap();
        let first = Subject::issue(7).unwrap();
        let again = Subject::from_parts(SubjectKind::Issue, "7").unwrap();
        assert!(
            store
                .enqueue_run("o/r", &first, "issue", 1)
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .enqueue_run("o/r", &again, "issue", 2)
                .unwrap()
                .is_none()
        );
        assert!(Subject::from_parts(SubjectKind::Issue, "07").is_err());
        let lower = Subject::commit(SHA1).unwrap();
        let upper = Subject::commit(&SHA1.to_ascii_uppercase()).unwrap();
        assert!(
            store
                .enqueue_run("o/r", &lower, "issue", 3)
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .enqueue_run("o/r", &upper, "issue", 4)
                .unwrap()
                .is_none()
        );
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

    fn invalid_pr(result: Result<(), StoreError>) -> String {
        match result {
            Err(StoreError::InvalidPullRequest(detail)) => detail,
            other => panic!("expected InvalidPullRequest, got {other:?}"),
        }
    }

    #[test]
    fn record_pr_rejects_pr_zero_blank_fields_control_characters_and_absurd_lengths() {
        let (store, run) = running_store();
        let attempt = store.start_attempt(run, 11).unwrap();
        let long_branch = "b".repeat(256);
        let long_sha = "a".repeat(65);
        let cases: Vec<(&str, &str, &str, u64, &str)> = vec![
            ("pr 0", "feature", "abc123", 0, "pull request number"),
            ("empty branch", "", "abc123", 1, "branch"),
            ("blank branch", "   ", "abc123", 1, "branch"),
            ("tab-only branch", "\t", "abc123", 1, "branch"),
            ("newline in branch", "a\nb", "abc123", 1, "branch"),
            ("nul in branch", "a\0b", "abc123", 1, "branch"),
            ("delete in branch", "a\x7fb", "abc123", 1, "branch"),
            ("long branch", &long_branch, "abc123", 1, "branch"),
            ("empty sha", "feature", "", 1, "head sha"),
            ("blank sha", "feature", "   ", 1, "head sha"),
            ("newline in sha", "feature", "ab\nc", 1, "head sha"),
            ("long sha", "feature", &long_sha, 1, "head sha"),
        ];
        for (name, branch, sha, pr, field) in cases {
            let detail = invalid_pr(store.record_pr(attempt.id, branch, sha, pr));
            assert!(detail.contains(field), "{name}: {detail}");
            let unchanged = store.attempt(attempt.id).unwrap();
            assert_eq!(
                (unchanged.branch, unchanged.head_sha, unchanged.pr_number),
                (None, None, None),
                "{name}"
            );
        }
    }

    #[test]
    fn record_pr_accepts_values_at_the_bounds() {
        let (store, run) = running_store();
        let attempt = store.start_attempt(run, 11).unwrap();
        let branch = "b".repeat(255);
        let sha = "a".repeat(64);
        store.record_pr(attempt.id, &branch, &sha, 1).unwrap();
        let stored = store.attempt(attempt.id).unwrap();
        assert_eq!(stored.branch.as_deref(), Some(branch.as_str()));
        assert_eq!(stored.head_sha.as_deref(), Some(sha.as_str()));
        assert_eq!(stored.pr_number, Some(1));
        store
            .record_pr(attempt.id, "feat/x y", "abc", u64::MAX >> 1)
            .unwrap();
    }

    #[test]
    fn a_recorded_pr_number_is_never_zero_so_recovery_cannot_be_diverted() {
        let dir = tempfile::tempdir().unwrap();
        let (store, lock) = file_store(&dir);
        let run = running_in(&store);
        let attempt = store.start_attempt(run, 12).unwrap();
        invalid_pr(store.record_pr(attempt.id, "feature", "abc123", 0));
        store.recover_after_crash(&lock, 20).unwrap();
        assert_eq!(store.run(run).unwrap().state, RunState::Queued);
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

    fn file_store(dir: &tempfile::TempDir) -> (Store, DaemonLock) {
        let path = dir.path().join("recovery.db");
        let store = Store::open(&path).unwrap();
        let lock = DaemonLock::acquire(&path).unwrap();
        (store, lock)
    }

    fn running_in(store: &Store) -> RunId {
        let id = store
            .enqueue_run("o/r", &issue(1), "issue", 10)
            .unwrap()
            .unwrap();
        store.set_run_state(id, RunState::Running, 11).unwrap();
        id
    }

    #[test]
    fn recovery_fails_dispatched_attempts_and_requeues_the_run_so_the_subject_is_not_wedged() {
        let dir = tempfile::tempdir().unwrap();
        let (store, lock) = file_store(&dir);
        let run = running_in(&store);
        let lost = store.start_attempt(run, 12).unwrap();
        assert!(
            store
                .enqueue_run("o/r", &issue(1), "issue", 13)
                .unwrap()
                .is_none()
        );

        let report = store.recover_after_crash(&lock, 20).unwrap();
        assert_eq!(
            report,
            Recovery {
                attempts_failed: 1,
                runs_requeued: 1,
                resumable: Vec::new()
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
        let (store, lock) = file_store(&dir);
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

        let report = store.recover_after_crash(&lock, 30).unwrap();
        assert_eq!(
            report,
            Recovery {
                attempts_failed: 0,
                runs_requeued: 0,
                resumable: vec![done_run]
            }
        );

        assert_eq!(store.attempt(ok.id).unwrap().state, AttemptState::Succeeded);
        assert_eq!(
            store.attempt(bad.id).unwrap().failure_class,
            Some(FailureClass::ModelError)
        );
        assert_eq!(store.run(done_run).unwrap().state, RunState::AwaitingCi);
        assert_eq!(store.run(queued).unwrap().state, RunState::Queued);
    }

    type Case = (
        &'static str,
        &'static str,
        Vec<(&'static str, Option<i64>)>,
        RunState,
        bool,
    );

    struct Seeded {
        state: &'static str,
        attempts: Vec<(&'static str, Option<i64>)>,
    }

    fn seed(store: &Store, subject: u64, spec: &Seeded) -> RunId {
        let id = store
            .enqueue_run("o/r", &issue(subject), "issue", 1)
            .unwrap()
            .unwrap();
        force_state(store, id, spec.state);
        for (n, (state, pr)) in spec.attempts.iter().enumerate() {
            let class = (*state == "failed").then_some("model_error");
            store
                .raw()
                .execute(
                    "INSERT INTO attempts (run_id, n, state, pr_number, failure_class, started_at, ended_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)",
                    params![id, n as i64 + 1, state, pr, class, (*state != "dispatched").then_some(2)],
                )
                .unwrap();
        }
        id
    }

    #[test]
    fn recovery_is_decided_by_run_state_for_every_state_with_and_without_a_dispatched_attempt() {
        use RunState::*;
        let d = ("dispatched", None);
        let cases: Vec<Case> = vec![
            ("queued, none", "queued", vec![], Queued, false),
            ("queued, dispatched", "queued", vec![d], Queued, false),
            ("running, none", "running", vec![], Queued, false),
            ("running, dispatched", "running", vec![d], Queued, false),
            (
                "running, published and finished",
                "running",
                vec![("succeeded", Some(7))],
                AwaitingCi,
                true,
            ),
            (
                "running, finished without a pr",
                "running",
                vec![("succeeded", None)],
                Queued,
                false,
            ),
            (
                "running, earlier failure after a pr",
                "running",
                vec![("failed", Some(7))],
                Queued,
                false,
            ),
            (
                "running, pr recorded then crashed",
                "running",
                vec![("dispatched", Some(7))],
                AwaitingCi,
                true,
            ),
            (
                "running, published once then a retry crashed",
                "running",
                vec![("succeeded", Some(7)), d],
                Queued,
                false,
            ),
            ("awaiting_ci, none", "awaiting_ci", vec![], AwaitingCi, true),
            (
                "awaiting_ci, dispatched",
                "awaiting_ci",
                vec![d],
                AwaitingCi,
                true,
            ),
            ("judging, none", "judging", vec![], Judging, true),
            ("judging, dispatched", "judging", vec![d], Judging, true),
            ("merged, none", "merged", vec![], Merged, false),
            ("merged, dispatched", "merged", vec![d], Merged, false),
            ("pr_open, dispatched", "pr_open", vec![d], PrOpen, false),
            ("failed, dispatched", "failed", vec![d], Failed, false),
            (
                "escalated, dispatched",
                "escalated",
                vec![d],
                Escalated,
                false,
            ),
        ];
        for (name, state, attempts, expected, resumable) in cases {
            let dir = tempfile::tempdir().unwrap();
            let (store, lock) = file_store(&dir);
            let run = seed(
                &store,
                1,
                &Seeded {
                    state,
                    attempts: attempts.clone(),
                },
            );
            let dispatched = attempts.iter().filter(|a| a.0 == "dispatched").count();
            let report = store.recover_after_crash(&lock, 50).unwrap();

            assert_eq!(store.run(run).unwrap().state, expected, "{name}");
            assert_eq!(report.resumable.contains(&run), resumable, "{name}");
            assert_eq!(report.attempts_failed, dispatched, "{name}");
            assert_eq!(
                report.runs_requeued,
                usize::from(expected == Queued && state == "running"),
                "{name}"
            );
            for attempt in store.attempts(run).unwrap() {
                assert_ne!(attempt.state, AttemptState::Dispatched, "{name}");
            }
            let again = store.recover_after_crash(&lock, 60).unwrap();
            assert_eq!(
                (again.attempts_failed, again.runs_requeued),
                (0, 0),
                "{name}"
            );
            assert_eq!(again.resumable, report.resumable, "{name}");
            assert_eq!(store.run(run).unwrap().state, expected, "{name}");
            if expected.is_open() {
                assert!(
                    store
                        .enqueue_run("o/r", &issue(1), "issue", 70)
                        .unwrap()
                        .is_none(),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn a_running_run_with_no_attempt_does_not_wedge_its_subject() {
        let dir = tempfile::tempdir().unwrap();
        let (store, lock) = file_store(&dir);
        let run = running_in(&store);
        assert!(
            store
                .enqueue_run("o/r", &issue(1), "issue", 13)
                .unwrap()
                .is_none()
        );
        store.recover_after_crash(&lock, 20).unwrap();
        assert_eq!(store.run(run).unwrap().state, RunState::Queued);
        assert_eq!(store.next_queued("o/r").unwrap().unwrap().id, run);
        assert!(
            store
                .enqueue_run("o/r", &issue(1), "issue", 21)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_lock_for_another_database_does_not_authorise_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.db");
        let b = dir.path().join("b.db");
        let store_a = Store::open(&a).unwrap();
        let run = running_in(&store_a);
        let attempt = store_a.start_attempt(run, 12).unwrap();
        let lock_b = DaemonLock::acquire(&b).unwrap();

        let err = store_a.recover_after_crash(&lock_b, 20).unwrap_err();
        match &err {
            StoreError::LockForDifferentDatabase { lock, store } => {
                assert!(lock.ends_with("b.db") && store.ends_with("a.db"), "{err}");
            }
            other => panic!("expected LockForDifferentDatabase, got {other}"),
        }
        assert_eq!(store_a.run(run).unwrap().state, RunState::Running);
        assert_eq!(
            store_a.attempt(attempt.id).unwrap().state,
            AttemptState::Dispatched
        );
    }

    #[test]
    fn an_in_memory_store_cannot_recover() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let lock = DaemonLock::acquire(&dir.path().join("any.db")).unwrap();
        assert!(matches!(
            store.recover_after_crash(&lock, 1),
            Err(StoreError::RecoveryNeedsFileStore)
        ));
    }

    #[test]
    fn the_lock_reports_the_canonical_path_and_can_be_taken_before_the_database_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("later.db");
        let lock = DaemonLock::acquire(&path).unwrap();
        assert_eq!(
            lock.database(),
            dir.path().canonicalize().unwrap().join("later.db")
        );
        let store = Store::open(&path).unwrap();
        let run = running_in(&store);
        store.start_attempt(run, 2).unwrap();
        assert_eq!(
            store.recover_after_crash(&lock, 3).unwrap().attempts_failed,
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_new_database_behind_a_symlinked_directory_shares_the_real_lock() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("real");
        std::fs::create_dir(&real_dir).unwrap();
        let link_dir = dir.path().join("link");
        std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();

        let via_link = DaemonLock::acquire(&link_dir.join("new.db")).unwrap();
        assert_eq!(
            via_link.database(),
            real_dir.canonicalize().unwrap().join("new.db")
        );
        assert!(matches!(
            DaemonLock::acquire(&real_dir.join("new.db")),
            Err(StoreError::AlreadyRunning(_))
        ));
        drop(via_link);

        let lock = DaemonLock::acquire(&link_dir.join("new.db")).unwrap();
        let store = Store::open(&real_dir.join("new.db")).unwrap();
        let run = running_in(&store);
        store.start_attempt(run, 2).unwrap();
        assert_eq!(
            store.recover_after_crash(&lock, 3).unwrap().attempts_failed,
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_database_path_shares_one_lock_with_the_real_path() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.db");
        drop(Store::open(&real).unwrap());
        let link = dir.path().join("link.db");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let first = DaemonLock::acquire(&real).unwrap();
        assert!(matches!(
            DaemonLock::acquire(&link),
            Err(StoreError::AlreadyRunning(_))
        ));
        drop(first);
        let via_link = DaemonLock::acquire(&link).unwrap();
        assert!(matches!(
            DaemonLock::acquire(&real),
            Err(StoreError::AlreadyRunning(_))
        ));
        assert_eq!(via_link.database(), real.canonicalize().unwrap());

        let store = Store::open(&real).unwrap();
        let run = running_in(&store);
        store.start_attempt(run, 2).unwrap();
        assert_eq!(
            store
                .recover_after_crash(&via_link, 3)
                .unwrap()
                .attempts_failed,
            1
        );

        let opened_through_the_link = Store::open(&link).unwrap();
        let other = opened_through_the_link
            .enqueue_run("o/r", &issue(2), "issue", 4)
            .unwrap()
            .unwrap();
        opened_through_the_link
            .set_run_state(other, RunState::Running, 5)
            .unwrap();
        let report = opened_through_the_link
            .recover_after_crash(&via_link, 6)
            .expect("a store opened through a symlink is the same database as the lock");
        assert_eq!(report.runs_requeued, 1);
    }

    #[cfg(unix)]
    #[test]
    fn two_hard_links_to_one_database_share_one_lock() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.db");
        let b = dir.path().join("b.db");
        drop(Store::open(&a).unwrap());
        std::fs::hard_link(&a, &b).unwrap();

        let first = DaemonLock::acquire(&a).unwrap();
        assert!(matches!(
            DaemonLock::acquire(&b),
            Err(StoreError::AlreadyRunning(_))
        ));
        let store_b = Store::open(&b).unwrap();
        let run = running_in(&store_b);
        store_b.start_attempt(run, 2).unwrap();
        assert_eq!(
            store_b
                .recover_after_crash(&first, 3)
                .unwrap()
                .attempts_failed,
            1
        );
        drop(first);
        assert!(DaemonLock::acquire(&b).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_lock_taken_through_a_dangling_symlink_is_the_lock_of_the_real_database() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.db");
        let link = dir.path().join("link.db");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let via_link = DaemonLock::acquire(&link).unwrap();
        let store = Store::open(&real).unwrap();
        assert!(matches!(
            DaemonLock::acquire(&real),
            Err(StoreError::AlreadyRunning(_))
        ));
        let run = running_in(&store);
        store.start_attempt(run, 2).unwrap();
        assert_eq!(
            store
                .recover_after_crash(&via_link, 3)
                .unwrap()
                .attempts_failed,
            1
        );
    }

    #[test]
    fn removing_a_lock_side_file_does_not_let_a_second_holder_in() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let first = DaemonLock::acquire(&db).unwrap();
        let mut side = first.database().as_os_str().to_owned();
        side.push(".daemon.lock");
        let _ = std::fs::remove_file(PathBuf::from(side));
        assert!(matches!(
            DaemonLock::acquire(&db),
            Err(StoreError::AlreadyRunning(_))
        ));
        drop(first);
        assert!(DaemonLock::acquire(&db).is_ok());
    }

    #[test]
    fn in_memory_names_have_no_file_to_lock_and_cannot_recover() {
        let dir = tempfile::tempdir().unwrap();
        let other = DaemonLock::acquire(&dir.path().join("other.db")).unwrap();
        for name in [":memory:", "", "file::memory:", "file:private?mode=memory"] {
            assert!(
                matches!(
                    DaemonLock::acquire(Path::new(name)),
                    Err(StoreError::RecoveryNeedsFileStore)
                ),
                "{name:?}"
            );
            let store = Store::open(Path::new(name)).unwrap();
            let run = running_in(&store);
            store.start_attempt(run, 2).unwrap();
            assert!(
                matches!(
                    store.recover_after_crash(&other, 3),
                    Err(StoreError::RecoveryNeedsFileStore)
                ),
                "{name:?}"
            );
            assert_eq!(store.run(run).unwrap().state, RunState::Running, "{name:?}");
        }
    }

    #[test]
    fn recovery_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let (store, lock) = file_store(&dir);
        let run = running_in(&store);
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
        let (store, lock) = file_store(&dir);
        let run = running_in(&store);
        store.start_attempt(run, 12).unwrap();
        force_state(&store, run, "judging");
        let report = store.recover_after_crash(&lock, 20).unwrap();
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
                assert_eq!(path, lock_file_path(&db.canonicalize().unwrap()));
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
            "UPDATE runs SET subject_ref = '07'",
            "UPDATE runs SET subject_ref = '0'",
            "UPDATE runs SET subject_ref = 'not a number'",
            "UPDATE runs SET subject_kind = 'commit', subject_ref = 'abc1234'",
            "UPDATE runs SET subject_kind = 'commit', subject_ref = '0123456789ABCDEF0123456789ABCDEF01234567'",
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
                    "UPDATE runs SET subject_kind = 'issue', subject_ref = '1';
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
        fn full_hex_commit_ids_are_accepted_and_lowercased(sha in "[0-9a-fA-F]{40}|[0-9a-fA-F]{64}") {
            let subject = Subject::commit(&sha).unwrap();
            prop_assert_eq!(subject.reference(), sha.to_ascii_lowercase());
        }

        #[test]
        fn commit_ids_of_any_other_length_or_alphabet_are_rejected(
            sha in "[0-9a-f]{0,39}|[0-9a-f]{41,63}|[0-9a-f]{65,70}|[0-9a-f]{10,30}[g-z][0-9a-f]{10,30}",
        ) {
            prop_assert!(Subject::commit(&sha).is_err());
        }

        #[test]
        fn canonical_numbers_round_trip_and_leading_zeros_are_rejected(
            number in 1u64..,
            zeros in "0{1,3}",
        ) {
            let subject = Subject::issue(number).unwrap();
            prop_assert_eq!(subject.reference(), number.to_string());
            prop_assert_eq!(Subject::from_parts(SubjectKind::Issue, subject.reference()).unwrap(), subject);
            let padded = format!("{zeros}{number}");
            prop_assert!(Subject::from_parts(SubjectKind::Issue, &padded).is_err());
        }

        #[test]
        fn arbitrary_text_never_builds_a_non_canonical_subject(text in ".{0,70}") {
            for kind in [SubjectKind::Issue, SubjectKind::Pr, SubjectKind::Commit] {
                if let Ok(subject) = Subject::from_parts(kind, &text) {
                    prop_assert_eq!(subject.reference(), text.as_str());
                    prop_assert_eq!(subject.kind(), kind);
                }
            }
        }
    }
}
