use std::fmt::Write;

use thiserror::Error;

use crate::model::FailureClass;
use crate::store::{Store, StoreError};

pub const WINDOWS: &[(&str, Option<i64>)] = &[
    ("24h", Some(24 * 60 * 60)),
    ("7d", Some(7 * 24 * 60 * 60)),
    ("30d", Some(30 * 24 * 60 * 60)),
    ("all", None),
];

pub const RECENT_FAILURES: usize = 10;
pub const RECENT_RUNS: usize = 20;

#[derive(Debug, Error)]
pub enum StatsError {
    #[error("unknown window `{0}` (expected 24h, 7d, 30d or all)")]
    UnknownWindow(String),
    #[error("unknown failure class `{0}` (expected one of: {1})")]
    UnknownClass(String, String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// ```
/// use inchworm::stats::parse_class;
///
/// assert!(parse_class("ci_red").is_ok());
/// assert!(parse_class("nope").unwrap_err().to_string().contains("ci_red"));
/// ```
pub fn parse_class(text: &str) -> Result<FailureClass, StatsError> {
    text.parse().map_err(|_| {
        let all: Vec<&str> = FailureClass::ALL.iter().map(|c| c.as_str()).collect();
        StatsError::UnknownClass(text.to_string(), all.join(", "))
    })
}

/// ```
/// use inchworm::model::RunState;
/// use inchworm::stats::render_stats;
/// use inchworm::store::Store;
///
/// let store = Store::open_in_memory().unwrap();
/// let run = store.enqueue_run("o/r", 1, "issue", 100).unwrap().unwrap();
/// store.set_run_state(run, RunState::Failed, 100).unwrap();
/// let text = render_stats(&store, 200, Some("all")).unwrap();
/// assert!(text.contains("o/r") && text.contains("0.0%"));
/// ```
pub fn render_stats(store: &Store, now: i64, window: Option<&str>) -> Result<String, StatsError> {
    let selected: Vec<&(&str, Option<i64>)> = match window {
        None => WINDOWS.iter().collect(),
        Some(name) => {
            let found = WINDOWS
                .iter()
                .find(|(label, _)| *label == name)
                .ok_or_else(|| StatsError::UnknownWindow(name.to_string()))?;
            vec![found]
        }
    };
    let mut out = String::new();
    let mut any = false;
    writeln!(
        out,
        "{:<28} {:<14} {:<7} {:>5} {:>9} {:>8}",
        "repo", "flow", "window", "runs", "succeeded", "rate"
    )
    .ok();
    for (label, seconds) in selected {
        for row in store.success_rates(now, *seconds)? {
            any = true;
            writeln!(
                out,
                "{:<28} {:<14} {:<7} {:>5} {:>9} {:>7.1}%",
                row.repo,
                row.flow,
                label,
                row.total,
                row.succeeded,
                row.rate() * 100.0
            )
            .ok();
        }
    }
    if !any {
        out.push_str("no finished runs in the selected window\n");
    }
    Ok(out)
}

/// ```
/// use inchworm::model::FailureClass;
/// use inchworm::stats::render_failures;
/// use inchworm::store::Store;
///
/// let store = Store::open_in_memory().unwrap();
/// let run = store.enqueue_run("o/r", 1, "issue", 0).unwrap().unwrap();
/// let attempt = store.start_attempt(run, 1).unwrap();
/// store.finish_attempt_failed(attempt.id, FailureClass::CiRed, "check build failed", 2).unwrap();
/// let text = render_failures(&store, None).unwrap();
/// assert!(text.contains("ci_red") && text.contains("check build failed"));
/// ```
pub fn render_failures(store: &Store, class: Option<FailureClass>) -> Result<String, StatsError> {
    let mut out = String::new();
    let histogram = store.failure_histogram(class)?;
    if histogram.is_empty() {
        out.push_str("no failures recorded\n");
        return Ok(out);
    }
    out.push_str("failure classes\n");
    for entry in histogram {
        writeln!(out, "  {:<26} {:>5}", entry.class, entry.count).ok();
    }
    out.push_str("recent failures\n");
    for failure in store.recent_failures(class, RECENT_FAILURES)? {
        let raw = failure.raw_error.replace('\n', " ");
        writeln!(
            out,
            "  run {} {}#{} attempt {} {} {}",
            failure.run_id, failure.repo, failure.issue, failure.attempt, failure.class, raw
        )
        .ok();
    }
    Ok(out)
}

/// ```
/// use inchworm::stats::render_status;
/// use inchworm::store::Store;
///
/// let store = Store::open_in_memory().unwrap();
/// assert!(render_status(&store).unwrap().contains("no runs"));
/// ```
pub fn render_status(store: &Store) -> Result<String, StatsError> {
    let runs = store.recent_runs(RECENT_RUNS)?;
    if runs.is_empty() {
        return Ok("no runs recorded yet\n".to_string());
    }
    let mut out = String::new();
    writeln!(
        out,
        "{:>5} {:<28} {:>6} {:<14} {:<12}",
        "run", "repo", "issue", "flow", "state"
    )
    .ok();
    for run in runs {
        writeln!(
            out,
            "{:>5} {:<28} {:>6} {:<14} {:<12}",
            run.id, run.repo, run.issue, run.flow, run.state
        )
        .ok();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RunState;

    fn finished(store: &Store, repo: &str, issue: u64, end: RunState, at: i64) {
        let id = store
            .enqueue_run(repo, issue, "issue", at)
            .unwrap()
            .unwrap();
        store.set_run_state(id, RunState::Running, at).unwrap();
        if end == RunState::PrOpen {
            store.set_run_state(id, RunState::AwaitingCi, at).unwrap();
            store.set_run_state(id, RunState::Judging, at).unwrap();
        }
        store.set_run_state(id, end, at).unwrap();
    }

    #[test]
    fn stats_report_each_window_and_filter_by_name() {
        let store = Store::open_in_memory().unwrap();
        finished(&store, "o/r", 1, RunState::PrOpen, 1_000_000);
        finished(&store, "o/r", 2, RunState::Failed, 100);
        let all = render_stats(&store, 1_000_100, None).unwrap();
        assert!(all.contains("24h") && all.contains("30d") && all.contains("all"));
        assert!(all.contains("50.0%"), "{all}");
        let day = render_stats(&store, 1_000_100, Some("24h")).unwrap();
        assert!(day.contains("100.0%") && !day.contains("50.0%"), "{day}");
        let empty = render_stats(&Store::open_in_memory().unwrap(), 5, Some("7d")).unwrap();
        assert!(empty.contains("no finished runs"));
        assert!(matches!(
            render_stats(&store, 5, Some("1y")),
            Err(StatsError::UnknownWindow(_))
        ));
    }

    #[test]
    fn failures_filter_by_class_and_keep_raw_errors_on_one_line() {
        let store = Store::open_in_memory().unwrap();
        let run = store.enqueue_run("o/r", 3, "issue", 0).unwrap().unwrap();
        for (class, raw) in [
            (FailureClass::CiRed, "line one\nline two"),
            (FailureClass::Timeout, "slow"),
        ] {
            let attempt = store.start_attempt(run, 1).unwrap();
            store
                .finish_attempt_failed(attempt.id, class, raw, 2)
                .unwrap();
        }
        let all = render_failures(&store, None).unwrap();
        assert!(all.contains("ci_red") && all.contains("timeout"));
        assert!(all.contains("line one line two"));
        let only = render_failures(&store, Some(FailureClass::Timeout)).unwrap();
        assert!(!only.contains("ci_red"));
        let none = render_failures(&store, Some(FailureClass::Cancelled)).unwrap();
        assert!(none.contains("no failures"));
    }

    #[test]
    fn status_lists_recent_runs_with_their_state() {
        let store = Store::open_in_memory().unwrap();
        finished(&store, "o/r", 7, RunState::PrOpen, 5);
        let text = render_status(&store).unwrap();
        assert!(text.contains("o/r") && text.contains("pr_open") && text.contains('7'));
    }

    #[test]
    fn class_names_parse_or_list_the_valid_ones() {
        for class in FailureClass::ALL {
            assert_eq!(parse_class(class.as_str()).unwrap(), *class);
        }
        let err = parse_class("bogus").unwrap_err().to_string();
        assert!(err.contains("bogus") && err.contains("merge_blocked"));
    }
}
