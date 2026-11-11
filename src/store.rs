use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, params};
use thiserror::Error;

use crate::judges::Verdict;
use crate::model::{FailureClass, InvalidTransition, RunState};

pub type RunId = i64;
pub type AttemptId = i64;

const MIGRATIONS: &[&str] = &[
    "
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
",
    "
ALTER TABLE attempts ADD COLUMN nanna_denials INTEGER;
ALTER TABLE attempts ADD COLUMN nanna_qa_ok INTEGER;
CREATE TABLE verdicts (
    id INTEGER PRIMARY KEY,
    attempt_id INTEGER NOT NULL REFERENCES attempts(id),
    head_sha TEXT NOT NULL,
    judge TEXT NOT NULL,
    passed INTEGER NOT NULL,
    score INTEGER,
    detail TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX verdicts_by_attempt ON verdicts (attempt_id);
CREATE INDEX attempts_by_failure ON attempts (failure_class);
",
];

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    InvalidTransition(#[from] InvalidTransition),
    #[error("run {0} not found")]
    RunNotFound(RunId),
    #[error("attempt {0} not found")]
    AttemptNotFound(AttemptId),
    #[error("attempt {0} is not dispatched")]
    AttemptNotDispatched(AttemptId),
    #[error("stored value is corrupt: {0}")]
    Corrupt(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptState {
    Dispatched,
    Published,
    Succeeded,
    Failed,
}

impl AttemptState {
    const fn as_str(self) -> &'static str {
        match self {
            AttemptState::Dispatched => "dispatched",
            AttemptState::Published => "published",
            AttemptState::Succeeded => "succeeded",
            AttemptState::Failed => "failed",
        }
    }

    fn parse(text: &str) -> Result<AttemptState, StoreError> {
        match text {
            "dispatched" => Ok(AttemptState::Dispatched),
            "published" => Ok(AttemptState::Published),
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
    pub issue: u64,
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
    pub nanna_denials: Option<u64>,
    pub nanna_qa_ok: Option<bool>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

pub struct Store {
    conn: Connection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateRow {
    pub repo: String,
    pub flow: String,
    pub total: u64,
    pub succeeded: u64,
}

impl RateRow {
    pub fn rate(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.succeeded as f64 / self.total as f64
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureCount {
    pub class: FailureClass,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureRow {
    pub run_id: RunId,
    pub repo: String,
    pub issue: u64,
    pub attempt: u32,
    pub class: FailureClass,
    pub raw_error: String,
    pub ended_at: i64,
}

fn corrupt<E: std::fmt::Display>(what: &str) -> impl FnOnce(E) -> StoreError + '_ {
    move |err| StoreError::Corrupt(format!("{what}: {err}"))
}

fn run_from_row(row: &Row<'_>) -> Result<Run, StoreError> {
    let state: String = row.get("state")?;
    let issue: i64 = row.get("issue")?;
    Ok(Run {
        id: row.get("id")?,
        repo: row.get("repo")?,
        issue: u64::try_from(issue).map_err(corrupt("issue"))?,
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
    let denials: Option<i64> = row.get("nanna_denials")?;
    let qa_ok: Option<i64> = row.get("nanna_qa_ok")?;
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
        nanna_denials: denials
            .map(u64::try_from)
            .transpose()
            .map_err(corrupt("nanna denials"))?,
        nanna_qa_ok: qa_ok.map(|v| v != 0),
        started_at: row.get("started_at")?,
        ended_at: row.get("ended_at")?,
    })
}

impl Store {
    /// ```
    /// use inchworm::model::RunState;
    /// use inchworm::store::Store;
    ///
    /// let store = Store::open_in_memory().unwrap();
    /// let id = store.enqueue_run("o/r", 7, "issue", 100).unwrap().unwrap();
    /// assert!(store.enqueue_run("o/r", 7, "issue", 101).unwrap().is_none());
    /// store.set_run_state(id, RunState::Running, 102).unwrap();
    /// assert_eq!(store.run(id).unwrap().state, RunState::Running);
    /// ```
    pub fn open_in_memory() -> Result<Store, StoreError> {
        Store::from_connection(Connection::open_in_memory()?)
    }

    pub fn open(path: &Path) -> Result<Store, StoreError> {
        Store::from_connection(Connection::open(path)?)
    }

    fn from_connection(conn: Connection) -> Result<Store, StoreError> {
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    pub fn schema_version(&self) -> Result<usize, StoreError> {
        let version: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        usize::try_from(version).map_err(corrupt("user_version"))
    }

    fn migrate(&self) -> Result<(), StoreError> {
        for (index, sql) in MIGRATIONS.iter().enumerate() {
            if self.schema_version()? > index {
                continue;
            }
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            let current: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
            if usize::try_from(current).unwrap_or(0) > index {
                continue;
            }
            tx.execute_batch(sql)?;
            tx.pragma_update(
                None,
                "user_version",
                i64::try_from(index + 1).unwrap_or(i64::MAX),
            )?;
            tx.commit()?;
        }
        Ok(())
    }

    pub fn enqueue_run(
        &self,
        repo: &str,
        issue: u64,
        flow: &str,
        now: i64,
    ) -> Result<Option<RunId>, StoreError> {
        let issue = i64::try_from(issue).map_err(corrupt("issue"))?;
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO runs (repo, issue, flow, state, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![repo, issue, flow, RunState::Queued.as_str(), now],
        )?;
        Ok((changed == 1).then(|| self.conn.last_insert_rowid()))
    }

    pub fn run(&self, id: RunId) -> Result<Run, StoreError> {
        self.conn
            .query_row("SELECT * FROM runs WHERE id = ?1", [id], |row| {
                Ok(run_from_row(row))
            })
            .optional()?
            .ok_or(StoreError::RunNotFound(id))?
    }

    pub fn set_run_state(&self, id: RunId, to: RunState, now: i64) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let current = self.run(id)?;
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
                "SELECT * FROM runs WHERE repo = ?1 AND state = 'queued' ORDER BY id LIMIT 1",
                [repo],
                |row| Ok(run_from_row(row)),
            )
            .optional()?
            .transpose()
    }

    pub fn active_run(&self, repo: &str) -> Result<Option<Run>, StoreError> {
        self.conn
            .query_row(
                "SELECT * FROM runs WHERE repo = ?1
                 AND state IN ('running', 'awaiting_ci', 'judging') ORDER BY id LIMIT 1",
                [repo],
                |row| Ok(run_from_row(row)),
            )
            .optional()?
            .transpose()
    }

    pub fn start_attempt(&self, run_id: RunId, now: i64) -> Result<Attempt, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        self.run(run_id)?;
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
        tx.commit()?;
        self.attempt(id)
    }

    pub fn attempt(&self, id: AttemptId) -> Result<Attempt, StoreError> {
        self.conn
            .query_row("SELECT * FROM attempts WHERE id = ?1", [id], |row| {
                Ok(attempt_from_row(row))
            })
            .optional()?
            .ok_or(StoreError::AttemptNotFound(id))?
    }

    /// ```
    /// use inchworm::store::Store;
    ///
    /// let store = Store::open_in_memory().unwrap();
    /// let run = store.enqueue_run("o/r", 1, "issue", 0).unwrap().unwrap();
    /// assert_eq!(store.issue_attempt_count("o/r", 1).unwrap(), 0);
    /// store.start_attempt(run, 1).unwrap();
    /// assert_eq!(store.issue_attempt_count("o/r", 1).unwrap(), 1);
    /// assert_eq!(store.issue_attempt_count("o/r", 2).unwrap(), 0);
    /// ```
    pub fn issue_attempt_count(&self, repo: &str, issue: u64) -> Result<u32, StoreError> {
        let issue = i64::try_from(issue).map_err(corrupt("issue"))?;
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM attempts a JOIN runs r ON r.id = a.run_id
             WHERE r.repo = ?1 AND r.issue = ?2",
            params![repo, issue],
            |row| row.get(0),
        )?;
        u32::try_from(count).map_err(corrupt("attempt count"))
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
            "UPDATE attempts SET branch = ?1, head_sha = ?2, pr_number = ?3 WHERE id = ?4",
            params![branch, head_sha, pr, id],
        )?;
        if changed == 0 {
            return Err(StoreError::AttemptNotFound(id));
        }
        Ok(())
    }

    pub fn mark_published(
        &self,
        id: AttemptId,
        denials: u64,
        qa_ok: Option<bool>,
    ) -> Result<(), StoreError> {
        let denials = i64::try_from(denials).map_err(corrupt("nanna denials"))?;
        let changed = self.conn.execute(
            "UPDATE attempts SET state = ?1, nanna_denials = ?2, nanna_qa_ok = ?3
             WHERE id = ?4 AND state = 'dispatched'",
            params![AttemptState::Published.as_str(), denials, qa_ok, id],
        )?;
        if changed == 1 {
            return Ok(());
        }
        self.attempt(id)?;
        Err(StoreError::AttemptNotDispatched(id))
    }

    pub fn record_verdicts(
        &self,
        attempt_id: AttemptId,
        head_sha: &str,
        verdicts: &[Verdict],
        now: i64,
    ) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        for verdict in verdicts {
            tx.execute(
                "INSERT INTO verdicts (attempt_id, head_sha, judge, passed, score, detail, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    attempt_id,
                    head_sha,
                    verdict.judge,
                    verdict.passed,
                    verdict.score,
                    verdict.detail,
                    now
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn verdicts(&self, attempt_id: AttemptId) -> Result<Vec<Verdict>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT judge, passed, score, detail FROM verdicts WHERE attempt_id = ?1 ORDER BY id",
        )?;
        let rows = statement.query_map([attempt_id], |row| {
            let score: Option<i64> = row.get("score")?;
            Ok(Verdict::from_parts(
                row.get("judge")?,
                row.get("passed")?,
                score.and_then(|s| u8::try_from(s).ok()),
                row.get("detail")?,
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn runs_in_states(&self, states: &[RunState]) -> Result<Vec<Run>, StoreError> {
        let wanted: Vec<String> = states.iter().map(|s| format!("'{}'", s.as_str())).collect();
        let mut statement = self.conn.prepare(&format!(
            "SELECT * FROM runs WHERE state IN ({}) ORDER BY id",
            wanted.join(",")
        ))?;
        let rows = statement.query_map([], |row| Ok(run_from_row(row)))?;
        rows.map(|row| row?).collect()
    }

    pub fn recent_runs(&self, limit: usize) -> Result<Vec<Run>, StoreError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut statement = self
            .conn
            .prepare("SELECT * FROM runs ORDER BY updated_at DESC, id DESC LIMIT ?1")?;
        let rows = statement.query_map([limit], |row| Ok(run_from_row(row)))?;
        rows.map(|row| row?).collect()
    }

    /// ```
    /// use inchworm::model::RunState;
    /// use inchworm::store::Store;
    ///
    /// let store = Store::open_in_memory().unwrap();
    /// let run = store.enqueue_run("o/r", 1, "issue", 0).unwrap().unwrap();
    /// store.set_run_state(run, RunState::Failed, 5).unwrap();
    /// let rows = store.success_rates(10, None).unwrap();
    /// assert_eq!((rows[0].total, rows[0].succeeded), (1, 0));
    /// assert_eq!(rows[0].rate(), 0.0);
    /// assert!(store.success_rates(1000, Some(10)).unwrap().is_empty());
    /// ```
    pub fn success_rates(
        &self,
        now: i64,
        window_secs: Option<i64>,
    ) -> Result<Vec<RateRow>, StoreError> {
        let since = window_secs.map(|w| now.saturating_sub(w));
        let mut statement = self.conn.prepare(
            "SELECT repo, flow, COUNT(*), COALESCE(SUM(state IN ('merged', 'pr_open')), 0)
             FROM runs
             WHERE state IN ('merged', 'pr_open', 'failed', 'escalated')
               AND (?1 IS NULL OR updated_at >= ?1)
             GROUP BY repo, flow ORDER BY repo, flow",
        )?;
        let rows = statement.query_map([since], |row| {
            let total: i64 = row.get(2)?;
            let succeeded: i64 = row.get(3)?;
            Ok(RateRow {
                repo: row.get(0)?,
                flow: row.get(1)?,
                total: u64::try_from(total).unwrap_or(0),
                succeeded: u64::try_from(succeeded).unwrap_or(0),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn failure_histogram(
        &self,
        class: Option<FailureClass>,
    ) -> Result<Vec<FailureCount>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT failure_class, COUNT(*) FROM attempts
             WHERE failure_class IS NOT NULL AND (?1 IS NULL OR failure_class = ?1)
             GROUP BY failure_class ORDER BY COUNT(*) DESC, failure_class",
        )?;
        let rows = statement.query_map([class.map(FailureClass::as_str)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut counts = Vec::new();
        for row in rows {
            let (name, count) = row?;
            counts.push(FailureCount {
                class: name.parse().map_err(corrupt("failure class"))?,
                count: u64::try_from(count).map_err(corrupt("count"))?,
            });
        }
        Ok(counts)
    }

    pub fn recent_failures(
        &self,
        class: Option<FailureClass>,
        limit: usize,
    ) -> Result<Vec<FailureRow>, StoreError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut statement = self.conn.prepare(
            "SELECT r.id, r.repo, r.issue, a.n, a.failure_class, COALESCE(a.raw_error, ''),
                    COALESCE(a.ended_at, a.started_at)
             FROM attempts a JOIN runs r ON r.id = a.run_id
             WHERE a.failure_class IS NOT NULL AND (?1 IS NULL OR a.failure_class = ?1)
             ORDER BY a.ended_at DESC, a.id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![class.map(FailureClass::as_str), limit], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?;
        let mut failures = Vec::new();
        for row in rows {
            let (run_id, repo, issue, attempt, class, raw_error, ended_at) = row?;
            failures.push(FailureRow {
                run_id,
                repo,
                issue: u64::try_from(issue).map_err(corrupt("issue"))?,
                attempt: u32::try_from(attempt).map_err(corrupt("attempt"))?,
                class: class.parse().map_err(corrupt("failure class"))?,
                raw_error,
                ended_at,
            });
        }
        Ok(failures)
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
             WHERE id = ?5 AND state IN ('dispatched', 'published')",
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

    /// ```
    /// use inchworm::model::FailureClass;
    /// use inchworm::store::{AttemptState, Store};
    ///
    /// let store = Store::open_in_memory().unwrap();
    /// let run = store.enqueue_run("o/r", 1, "issue", 0).unwrap().unwrap();
    /// let attempt = store.start_attempt(run, 1).unwrap();
    /// assert_eq!(store.recover_after_crash(2).unwrap(), 1);
    /// let lost = store.attempt(attempt.id).unwrap();
    /// assert_eq!(lost.state, AttemptState::Failed);
    /// assert_eq!(lost.failure_class, Some(FailureClass::OrchestratorLost));
    /// assert_eq!(store.recover_after_crash(3).unwrap(), 0);
    /// ```
    pub fn recover_after_crash(&self, now: i64) -> Result<usize, StoreError> {
        Ok(self.conn.execute(
            "UPDATE attempts SET state = 'failed', failure_class = ?1, raw_error = ?2, ended_at = ?3
             WHERE state = 'dispatched'",
            params![
                FailureClass::OrchestratorLost.as_str(),
                "inchworm stopped while this attempt was dispatched",
                now
            ],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn store_with_run() -> (Store, RunId) {
        let store = Store::open_in_memory().unwrap();
        let id = store.enqueue_run("o/r", 1, "issue", 10).unwrap().unwrap();
        (store, id)
    }

    #[test]
    fn migrations_set_the_version_and_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let first = Store::open(&path).unwrap();
        assert_eq!(first.schema_version().unwrap(), MIGRATIONS.len());
        let id = first.enqueue_run("o/r", 1, "issue", 1).unwrap().unwrap();
        drop(first);
        let second = Store::open(&path).unwrap();
        assert_eq!(second.schema_version().unwrap(), MIGRATIONS.len());
        assert_eq!(second.run(id).unwrap().repo, "o/r");
    }

    #[test]
    fn enqueue_deduplicates_on_repo_issue_flow() {
        let (store, id) = store_with_run();
        assert!(store.enqueue_run("o/r", 1, "issue", 11).unwrap().is_none());
        assert!(store.enqueue_run("o/r", 1, "other", 11).unwrap().is_some());
        assert!(store.enqueue_run("o/r", 2, "issue", 11).unwrap().is_some());
        assert!(store.enqueue_run("o/x", 1, "issue", 11).unwrap().is_some());
        assert_eq!(store.run(id).unwrap().state, RunState::Queued);
    }

    #[test]
    fn a_finished_issue_can_be_enqueued_again_but_an_open_one_cannot() {
        let (store, first) = store_with_run();
        store.set_run_state(first, RunState::Running, 11).unwrap();
        assert!(store.enqueue_run("o/r", 1, "issue", 12).unwrap().is_none());
        for state in [RunState::AwaitingCi, RunState::Judging] {
            store.set_run_state(first, state, 13).unwrap();
            assert!(store.enqueue_run("o/r", 1, "issue", 14).unwrap().is_none());
        }
        store.set_run_state(first, RunState::Failed, 15).unwrap();

        let second = store.enqueue_run("o/r", 1, "issue", 16).unwrap().unwrap();
        assert_ne!(first, second);
        assert_eq!(store.run(first).unwrap().state, RunState::Failed);
        assert_eq!(store.next_queued("o/r").unwrap().unwrap().id, second);
        assert!(store.enqueue_run("o/r", 1, "issue", 17).unwrap().is_none());
    }

    #[test]
    fn every_terminal_state_allows_a_rerun() {
        for terminal in [
            RunState::PrOpen,
            RunState::Merged,
            RunState::Escalated,
            RunState::Failed,
        ] {
            let (store, run) = store_with_run();
            store.set_run_state(run, RunState::Running, 11).unwrap();
            match terminal {
                RunState::PrOpen | RunState::Merged => {
                    store.set_run_state(run, RunState::AwaitingCi, 12).unwrap();
                    store.set_run_state(run, RunState::Judging, 13).unwrap();
                }
                _ => {}
            }
            store.set_run_state(run, terminal, 14).unwrap();
            assert!(
                store.enqueue_run("o/r", 1, "issue", 15).unwrap().is_some(),
                "{terminal}"
            );
        }
    }

    #[test]
    fn run_fields_round_trip() {
        let (store, id) = store_with_run();
        let run = store.run(id).unwrap();
        assert_eq!(
            run,
            Run {
                id,
                repo: "o/r".into(),
                issue: 1,
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
    fn next_queued_is_fifo_per_repo_and_active_run_sees_in_flight_states() {
        let store = Store::open_in_memory().unwrap();
        let a1 = store.enqueue_run("o/a", 1, "issue", 1).unwrap().unwrap();
        let b1 = store.enqueue_run("o/b", 1, "issue", 2).unwrap().unwrap();
        let a2 = store.enqueue_run("o/a", 2, "issue", 3).unwrap().unwrap();
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
    fn attempts_are_numbered_per_run() {
        let store = Store::open_in_memory().unwrap();
        let r1 = store.enqueue_run("o/r", 1, "issue", 1).unwrap().unwrap();
        let r2 = store.enqueue_run("o/r", 2, "issue", 1).unwrap().unwrap();
        assert_eq!(store.start_attempt(r1, 2).unwrap().n, 1);
        assert_eq!(store.start_attempt(r1, 3).unwrap().n, 2);
        assert_eq!(store.start_attempt(r2, 4).unwrap().n, 1);
        let all = store.attempts(r1).unwrap();
        assert_eq!(all.iter().map(|a| a.n).collect::<Vec<_>>(), vec![1, 2]);
        assert!(all.iter().all(|a| a.state == AttemptState::Dispatched));
    }

    #[test]
    fn failed_attempts_keep_class_and_raw_error() {
        let (store, run) = store_with_run();
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
        let (store, run) = store_with_run();
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
    }

    #[test]
    fn finishing_twice_or_unknown_attempts_is_an_error() {
        let (store, run) = store_with_run();
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
        assert_eq!(store.attempt(attempt.id).unwrap().ended_at, Some(12));
        assert!(matches!(
            store.finish_attempt_ok(999, 1),
            Err(StoreError::AttemptNotFound(999))
        ));
        assert!(matches!(
            store.record_pr(999, "b", "s", 1),
            Err(StoreError::AttemptNotFound(999))
        ));
    }

    #[test]
    fn crash_recovery_only_touches_dispatched_attempts() {
        let (store, run) = store_with_run();
        let done = store.start_attempt(run, 11).unwrap();
        store.finish_attempt_ok(done.id, 12).unwrap();
        let failed = store.start_attempt(run, 13).unwrap();
        store
            .finish_attempt_failed(failed.id, FailureClass::ModelError, "boom", 14)
            .unwrap();
        let lost = store.start_attempt(run, 15).unwrap();

        assert_eq!(store.recover_after_crash(20).unwrap(), 1);

        assert_eq!(
            store.attempt(done.id).unwrap().state,
            AttemptState::Succeeded
        );
        assert_eq!(
            store.attempt(failed.id).unwrap().failure_class,
            Some(FailureClass::ModelError)
        );
        let lost = store.attempt(lost.id).unwrap();
        assert_eq!(lost.state, AttemptState::Failed);
        assert_eq!(lost.failure_class, Some(FailureClass::OrchestratorLost));
        assert_eq!(lost.ended_at, Some(20));
        assert_eq!(store.recover_after_crash(21).unwrap(), 0);
    }

    #[test]
    fn state_survives_reopen_so_a_restart_can_resume() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let store = Store::open(&path).unwrap();
        let run = store.enqueue_run("o/r", 5, "issue", 1).unwrap().unwrap();
        store.set_run_state(run, RunState::Running, 2).unwrap();
        let attempt = store.start_attempt(run, 3).unwrap();
        drop(store);

        let store = Store::open(&path).unwrap();
        assert_eq!(store.recover_after_crash(10).unwrap(), 1);
        assert_eq!(store.run(run).unwrap().state, RunState::Running);
        assert_eq!(store.active_run("o/r").unwrap().unwrap().id, run);
        assert_eq!(
            store.attempt(attempt.id).unwrap().failure_class,
            Some(FailureClass::OrchestratorLost)
        );
    }

    #[test]
    fn concurrent_opens_of_a_fresh_database_all_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let handles: Vec<_> = (0..6)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || Store::open(&path).map(|s| s.schema_version().unwrap()))
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.join().unwrap().unwrap(), MIGRATIONS.len());
        }
    }

    #[test]
    fn publishing_keeps_the_attempt_out_of_crash_recovery_and_records_the_nanna_report() {
        let (store, run) = store_with_run();
        let attempt = store.start_attempt(run, 11).unwrap();
        store.record_pr(attempt.id, "b", "sha", 3).unwrap();
        store.mark_published(attempt.id, 2, Some(false)).unwrap();
        assert_eq!(store.recover_after_crash(20).unwrap(), 0);
        let published = store.attempt(attempt.id).unwrap();
        assert_eq!(published.state, AttemptState::Published);
        assert_eq!(published.nanna_denials, Some(2));
        assert_eq!(published.nanna_qa_ok, Some(false));
        assert!(matches!(
            store.mark_published(attempt.id, 0, None),
            Err(StoreError::AttemptNotDispatched(_))
        ));
        assert!(matches!(
            store.mark_published(999, 0, None),
            Err(StoreError::AttemptNotFound(999))
        ));
        store
            .finish_attempt_failed(attempt.id, FailureClass::CiRed, "check x failed", 21)
            .unwrap();
        let failed = store.attempt(attempt.id).unwrap();
        assert_eq!(failed.state, AttemptState::Failed);
        assert_eq!(failed.failure_class, Some(FailureClass::CiRed));
    }

    #[test]
    fn corrupt_rows_are_reported_not_trusted() {
        let (store, run) = store_with_run();
        let attempt = store.start_attempt(run, 1).unwrap();
        store
            .conn
            .execute(
                "UPDATE attempts SET state = 'bogus' WHERE id = ?1",
                [attempt.id],
            )
            .unwrap();
        let err = store.attempt(attempt.id).unwrap_err();
        assert!(matches!(err, StoreError::Corrupt(m) if m.contains("bogus")));
        store
            .conn
            .execute(
                "UPDATE attempts SET state = 'dispatched', pr_number = -4 WHERE id = ?1",
                [attempt.id],
            )
            .unwrap();
        let err = store.attempt(attempt.id).unwrap_err();
        assert!(matches!(err, StoreError::Corrupt(m) if m.contains("pr number")));
    }

    #[test]
    fn unknown_report_values_stay_none() {
        let (store, run) = store_with_run();
        let attempt = store.start_attempt(run, 11).unwrap();
        store.mark_published(attempt.id, 0, None).unwrap();
        let published = store.attempt(attempt.id).unwrap();
        assert_eq!(
            (published.nanna_denials, published.nanna_qa_ok),
            (Some(0), None)
        );
        assert_eq!(
            store
                .attempt(store.start_attempt(run, 12).unwrap().id)
                .unwrap()
                .nanna_denials,
            None
        );
    }

    #[test]
    fn verdicts_round_trip_in_order_per_attempt() {
        let (store, run) = store_with_run();
        let first = store.start_attempt(run, 11).unwrap();
        let second = store.start_attempt(run, 12).unwrap();
        let verdicts = [
            Verdict::pass("diff", "3 lines"),
            Verdict::fail("llm", "too vague").with_score(4),
        ];
        store
            .record_verdicts(first.id, "sha1", &verdicts, 13)
            .unwrap();
        store
            .record_verdicts(second.id, "sha2", &verdicts[..1], 14)
            .unwrap();
        assert_eq!(store.verdicts(first.id).unwrap(), verdicts);
        assert_eq!(store.verdicts(second.id).unwrap(), &verdicts[..1]);
        assert!(store.verdicts(999).unwrap().is_empty());
        assert!(store.record_verdicts(999, "s", &verdicts, 1).is_err());
    }

    #[test]
    fn runs_in_states_and_recent_runs() {
        let store = Store::open_in_memory().unwrap();
        let a = store.enqueue_run("o/r", 1, "issue", 1).unwrap().unwrap();
        let b = store.enqueue_run("o/r", 2, "issue", 2).unwrap().unwrap();
        store.set_run_state(b, RunState::Running, 5).unwrap();
        let queued = store.runs_in_states(&[RunState::Queued]).unwrap();
        assert_eq!(queued.iter().map(|r| r.id).collect::<Vec<_>>(), [a]);
        let open = store
            .runs_in_states(&[RunState::Queued, RunState::Running])
            .unwrap();
        assert_eq!(open.len(), 2);
        assert!(store.runs_in_states(&[]).unwrap().is_empty());
        let recent = store.recent_runs(1).unwrap();
        assert_eq!(recent[0].id, b);
    }

    fn finish_run(store: &Store, repo: &str, issue: u64, flow: &str, end: RunState, at: i64) {
        let id = store.enqueue_run(repo, issue, flow, at).unwrap().unwrap();
        store.set_run_state(id, RunState::Running, at).unwrap();
        match end {
            RunState::Failed | RunState::Escalated => store.set_run_state(id, end, at).unwrap(),
            _ => {
                store.set_run_state(id, RunState::AwaitingCi, at).unwrap();
                store.set_run_state(id, RunState::Judging, at).unwrap();
                store.set_run_state(id, end, at).unwrap();
            }
        }
    }

    #[test]
    fn success_rates_aggregate_per_repo_flow_and_window() {
        let store = Store::open_in_memory().unwrap();
        finish_run(&store, "o/a", 1, "issue", RunState::PrOpen, 100);
        finish_run(&store, "o/a", 2, "issue", RunState::Merged, 200);
        finish_run(&store, "o/a", 3, "issue", RunState::Failed, 300);
        finish_run(&store, "o/a", 4, "other", RunState::Escalated, 300);
        finish_run(&store, "o/b", 1, "issue", RunState::Failed, 50);
        store.enqueue_run("o/b", 9, "issue", 400).unwrap();

        let all = store.success_rates(1000, None).unwrap();
        let summary: Vec<_> = all
            .iter()
            .map(|r| (r.repo.as_str(), r.flow.as_str(), r.total, r.succeeded))
            .collect();
        assert_eq!(
            summary,
            [
                ("o/a", "issue", 3, 2),
                ("o/a", "other", 1, 0),
                ("o/b", "issue", 1, 0)
            ]
        );
        assert!((all[0].rate() - 2.0 / 3.0).abs() < 1e-9);

        let recent = store.success_rates(1000, Some(750)).unwrap();
        let summary: Vec<_> = recent.iter().map(|r| (r.repo.as_str(), r.total)).collect();
        assert_eq!(summary, [("o/a", 1), ("o/a", 1)]);
        assert_eq!(
            RateRow {
                repo: String::new(),
                flow: String::new(),
                total: 0,
                succeeded: 0
            }
            .rate(),
            0.0
        );
    }

    #[test]
    fn failure_histogram_and_recent_failures_filter_by_class() {
        let (store, run) = store_with_run();
        for (class, raw, at) in [
            (FailureClass::CiRed, "check a red", 20),
            (FailureClass::ModelError, "oom", 21),
            (FailureClass::CiRed, "check b red", 22),
        ] {
            let attempt = store.start_attempt(run, at).unwrap();
            store
                .finish_attempt_failed(attempt.id, class, raw, at)
                .unwrap();
        }
        let ok = store.start_attempt(run, 30).unwrap();
        store.finish_attempt_ok(ok.id, 31).unwrap();

        let histogram = store.failure_histogram(None).unwrap();
        assert_eq!(
            histogram,
            [
                FailureCount {
                    class: FailureClass::CiRed,
                    count: 2
                },
                FailureCount {
                    class: FailureClass::ModelError,
                    count: 1
                }
            ]
        );
        let only = store
            .failure_histogram(Some(FailureClass::ModelError))
            .unwrap();
        assert_eq!(only.len(), 1);

        let recent = store.recent_failures(None, 2).unwrap();
        assert_eq!(
            recent
                .iter()
                .map(|f| f.raw_error.as_str())
                .collect::<Vec<_>>(),
            ["check b red", "oom"]
        );
        assert_eq!(
            (recent[0].repo.as_str(), recent[0].issue, recent[0].attempt),
            ("o/r", 1, 3)
        );
        let red = store
            .recent_failures(Some(FailureClass::CiRed), 10)
            .unwrap();
        assert_eq!(red.len(), 2);
        assert!(
            store
                .recent_failures(Some(FailureClass::Timeout), 10)
                .unwrap()
                .is_empty()
        );
    }

    proptest! {
        #[test]
        fn histogram_counts_match_the_attempts_recorded(classes in prop::collection::vec(0..FailureClass::ALL.len(), 0..20)) {
            let (store, run) = store_with_run();
            for index in &classes {
                let attempt = store.start_attempt(run, 1).unwrap();
                store.finish_attempt_failed(attempt.id, FailureClass::ALL[*index], "x", 2).unwrap();
            }
            let total: u64 = store.failure_histogram(None).unwrap().iter().map(|c| c.count).sum();
            prop_assert_eq!(total, classes.len() as u64);
        }

        #[test]
        fn verdicts_round_trip(passed in any::<bool>(), score in prop::option::of(0u8..=10), detail in ".{0,40}") {
            let (store, run) = store_with_run();
            let attempt = store.start_attempt(run, 1).unwrap();
            let verdict = Verdict::from_parts("llm".into(), passed, score, detail);
            store.record_verdicts(attempt.id, "sha", std::slice::from_ref(&verdict), 2).unwrap();
            prop_assert_eq!(store.verdicts(attempt.id).unwrap(), vec![verdict]);
        }

        #[test]
        fn attempt_numbers_are_sequential(count in 1usize..12) {
            let (store, run) = store_with_run();
            for expected in 1..=count {
                prop_assert_eq!(store.start_attempt(run, 1).unwrap().n as usize, expected);
            }
        }

        #[test]
        fn store_follows_the_model_state_machine(steps in prop::collection::vec(0..RunState::ALL.len(), 0..16)) {
            let (store, run) = store_with_run();
            let mut model = RunState::Queued;
            for index in steps {
                let target = RunState::ALL[index];
                let result = store.set_run_state(run, target, 1);
                match model.transition(target) {
                    Ok(next) => {
                        prop_assert!(result.is_ok());
                        model = next;
                    }
                    Err(_) => prop_assert!(result.is_err()),
                }
                prop_assert_eq!(store.run(run).unwrap().state, model);
            }
        }

        #[test]
        fn failure_class_and_text_round_trip(index in 0..FailureClass::ALL.len(), raw in ".{0,40}") {
            let (store, run) = store_with_run();
            let attempt = store.start_attempt(run, 1).unwrap();
            let class = FailureClass::ALL[index];
            store.finish_attempt_failed(attempt.id, class, &raw, 2).unwrap();
            let done = store.attempt(attempt.id).unwrap();
            prop_assert_eq!(done.failure_class, Some(class));
            prop_assert_eq!(done.raw_error.as_deref(), Some(raw.as_str()));
        }
    }
}
