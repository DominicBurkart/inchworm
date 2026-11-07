use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, params};
use thiserror::Error;

use crate::model::{FailureClass, InvalidTransition, RunState};

pub type RunId = i64;
pub type AttemptId = i64;

const MIGRATIONS: &[&str] = &["
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
"];

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
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

pub struct Store {
    conn: Connection,
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
        let current = self.schema_version()?;
        for (index, sql) in MIGRATIONS.iter().enumerate().skip(current) {
            let tx = self.conn.unchecked_transaction()?;
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

    proptest! {
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
