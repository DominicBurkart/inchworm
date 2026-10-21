//! The per-user daemon loop: watch registered directories, keep their configs
//! fresh, and tick every enabled journey. Deliberately thin so the control
//! flow can move elsewhere later; all behavior lives behind traits.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use crate::config::{CONFIG_FILE, Config};
use crate::registry::Registry;

/// Default time between ticks.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Longest single sleep, so a stop request is noticed promptly.
const SLEEP_SLICE: Duration = Duration::from_millis(250);

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct JourneyError(pub String);

/// One of the inchworm journeys, driven once per tick per directory.
pub trait Journey {
    fn name(&self) -> &str;
    fn enabled(&self, config: &Config) -> bool;
    fn tick(&self, dir: &Path, config: &Config) -> Result<(), JourneyError>;
}

/// Time source, injectable so tests do not really sleep.
pub trait Clock {
    fn now(&self) -> Instant;
    fn sleep(&self, duration: Duration);
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Identifies a version of a config file on disk.
type Stamp = (SystemTime, u64);

#[derive(Default)]
struct Entry {
    /// Stamp of the file as last read (outer `None`: never read), whether or
    /// not it parsed. The inner `None` means the file was unreadable.
    stamp: Option<Option<Stamp>>,
    /// The last config that parsed.
    good: Option<Config>,
}

pub struct Daemon {
    registry: Registry,
    journeys: Vec<Box<dyn Journey>>,
    cache: HashMap<PathBuf, Entry>,
    poll_interval: Duration,
    log: Box<dyn Fn(&str)>,
}

impl Daemon {
    pub fn new(registry: Registry, journeys: Vec<Box<dyn Journey>>) -> Self {
        Self {
            registry,
            journeys,
            cache: HashMap::new(),
            poll_interval: DEFAULT_POLL_INTERVAL,
            log: Box::new(|msg| eprintln!("{msg}")),
        }
    }

    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    pub fn with_logger(mut self, log: impl Fn(&str) + 'static) -> Self {
        self.log = Box::new(log);
        self
    }

    /// Refresh the cached config for `dir`, returning the config to use, if
    /// any. A broken file is reported once and the last good config is kept.
    fn refresh(&mut self, dir: &Path) -> Option<&Config> {
        let entry = self.cache.entry(dir.to_path_buf()).or_default();
        let file = dir.join(CONFIG_FILE);
        let stamp = std::fs::metadata(&file)
            .and_then(|m| Ok((m.modified()?, m.len())))
            .ok();
        // A missing file (stamp None) is a state like any other, so it is
        // reported once rather than every tick.
        if entry.stamp != Some(stamp) {
            entry.stamp = Some(stamp);
            match Config::load(dir) {
                Ok(cfg) => entry.good = Some(cfg),
                Err(e) => (self.log)(&format!("{}: {e}", dir.display())),
            }
        }
        entry.good.as_ref()
    }

    /// Run one pass over every registered directory.
    pub fn tick(&mut self, stop: &AtomicBool) {
        let dirs = match self.registry.list() {
            Ok(dirs) => dirs,
            Err(e) => {
                (self.log)(&format!("reading registry: {e}"));
                return;
            }
        };
        self.cache.retain(|dir, _| dirs.contains(dir));
        for dir in &dirs {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            self.refresh(dir);
            let Some(config) = self.cache.get(dir).and_then(|e| e.good.as_ref()) else {
                continue;
            };
            for journey in self.journeys.iter().filter(|j| j.enabled(config)) {
                if let Err(e) = journey.tick(dir, config) {
                    (self.log)(&format!("{}: {}: {e}", dir.display(), journey.name()));
                }
            }
        }
    }

    /// Tick until `stop` is set, sleeping `poll_interval` between passes
    /// (minus the time the pass took).
    pub fn run(&mut self, clock: &dyn Clock, stop: &AtomicBool) {
        while !stop.load(Ordering::Relaxed) {
            let started = clock.now();
            self.tick(stop);
            let deadline = started + self.poll_interval;
            while !stop.load(Ordering::Relaxed) {
                let remaining = deadline.saturating_duration_since(clock.now());
                if remaining.is_zero() {
                    break;
                }
                clock.sleep(remaining.min(SLEEP_SLICE));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use super::*;

    type Calls = Rc<RefCell<Vec<String>>>;

    struct Probe {
        calls: Calls,
        enabled: fn(&Config) -> bool,
        fail: bool,
    }

    impl Journey for Probe {
        fn name(&self) -> &str {
            "probe"
        }
        fn enabled(&self, c: &Config) -> bool {
            (self.enabled)(c)
        }
        fn tick(&self, dir: &Path, c: &Config) -> Result<(), JourneyError> {
            let name = dir.file_name().unwrap().to_string_lossy();
            self.calls
                .borrow_mut()
                .push(format!("{name}:{}", c.issues.enabled));
            if self.fail {
                Err(JourneyError("boom".into()))
            } else {
                Ok(())
            }
        }
    }

    struct Fixture {
        tmp: tempfile::TempDir,
        calls: Calls,
        logs: Calls,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                tmp: tempfile::tempdir().unwrap(),
                calls: Calls::default(),
                logs: Calls::default(),
            }
        }

        fn project(&self, name: &str, toml: &str) -> PathBuf {
            let dir = self.tmp.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(CONFIG_FILE), toml).unwrap();
            self.registry().register(&dir).unwrap();
            dir.canonicalize().unwrap()
        }

        fn registry(&self) -> Registry {
            Registry::new(self.tmp.path().join("state/registry"))
        }

        fn daemon(&self, enabled: fn(&Config) -> bool, fail: bool) -> Daemon {
            let probe = Probe {
                calls: self.calls.clone(),
                enabled,
                fail,
            };
            let logs = self.logs.clone();
            Daemon::new(self.registry(), vec![Box::new(probe)])
                .with_logger(move |m| logs.borrow_mut().push(m.to_string()))
        }

        fn take_calls(&self) -> Vec<String> {
            std::mem::take(&mut *self.calls.borrow_mut())
        }
    }

    fn always(_: &Config) -> bool {
        true
    }

    /// Rewrite a config and bump its mtime so the change is always visible.
    fn rewrite(dir: &Path, toml: &str) {
        let file = dir.join(CONFIG_FILE);
        std::fs::write(&file, toml).unwrap();
        let t = SystemTime::now() + Duration::from_secs(60 * rewrites());
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn rewrites() -> u64 {
        thread_local!(static N: Cell<u64> = const { Cell::new(0) });
        N.with(|n| {
            n.set(n.get() + 1);
            n.get()
        })
    }

    #[test]
    fn ticks_enabled_journeys_per_dir() {
        let f = Fixture::new();
        f.project("a", "");
        f.project("b", "[issues]\nenabled = false\n");
        let mut d = f.daemon(|c| c.issues.enabled, false);
        d.tick(&AtomicBool::new(false));
        assert_eq!(f.take_calls(), ["a:true"]);
    }

    #[test]
    fn journey_error_is_logged_and_does_not_stop_others() {
        let f = Fixture::new();
        f.project("a", "");
        f.project("b", "");
        let mut d = f.daemon(always, true);
        d.tick(&AtomicBool::new(false));
        assert_eq!(f.take_calls().len(), 2);
        let logs = f.logs.borrow();
        assert_eq!(logs.len(), 2);
        assert!(logs[0].contains("probe: boom"));
    }

    #[test]
    fn changed_config_is_reloaded() {
        let f = Fixture::new();
        let a = f.project("a", "");
        let mut d = f.daemon(always, false);
        let stop = AtomicBool::new(false);
        d.tick(&stop);
        rewrite(&a, "[issues]\nenabled = false\n");
        d.tick(&stop);
        assert_eq!(f.take_calls(), ["a:true", "a:false"]);
    }

    #[test]
    fn broken_config_keeps_last_good_and_reports_once() {
        let f = Fixture::new();
        let a = f.project("a", "");
        let mut d = f.daemon(always, false);
        let stop = AtomicBool::new(false);
        d.tick(&stop);
        rewrite(&a, "not = [valid");
        d.tick(&stop);
        d.tick(&stop);
        assert_eq!(f.take_calls(), ["a:true"; 3]);
        assert_eq!(f.logs.borrow().len(), 1, "reported once, not per tick");
        rewrite(&a, "[issues]\nenabled = false\n");
        d.tick(&stop);
        assert_eq!(f.take_calls(), ["a:false"]);
    }

    #[test]
    fn broken_from_the_start_is_skipped() {
        let f = Fixture::new();
        f.project("a", "not = [valid");
        let mut d = f.daemon(always, false);
        let stop = AtomicBool::new(false);
        d.tick(&stop);
        d.tick(&stop);
        assert!(f.take_calls().is_empty());
        assert_eq!(f.logs.borrow().len(), 1);
    }

    #[test]
    fn unregistered_dirs_are_dropped() {
        let f = Fixture::new();
        let a = f.project("a", "");
        let mut d = f.daemon(always, false);
        let stop = AtomicBool::new(false);
        d.tick(&stop);
        assert_eq!(d.cache.len(), 1);
        f.registry().unregister(&a).unwrap();
        d.tick(&stop);
        assert!(d.cache.is_empty());
        assert_eq!(f.take_calls(), ["a:true"]);
    }

    struct FakeClock {
        now: Cell<Instant>,
        slept: RefCell<Vec<Duration>>,
        stop_after: usize,
        stop: Rc<AtomicBool>,
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            self.now.get()
        }
        fn sleep(&self, d: Duration) {
            self.now.set(self.now.get() + d);
            self.slept.borrow_mut().push(d);
            if self.slept.borrow().len() >= self.stop_after {
                self.stop.store(true, Ordering::Relaxed);
            }
        }
    }

    #[test]
    fn run_loops_with_poll_interval_until_stopped() {
        let f = Fixture::new();
        f.project("a", "");
        let stop = Rc::new(AtomicBool::new(false));
        let clock = FakeClock {
            now: Cell::new(Instant::now()),
            slept: RefCell::default(),
            stop_after: 3,
            stop: stop.clone(),
        };
        let mut d = f
            .daemon(always, false)
            .with_poll_interval(Duration::from_millis(100));
        d.run(&clock, &stop);
        // Each pass sleeps one 100ms interval; the stop lands mid-sleep
        // on the third.
        assert_eq!(*clock.slept.borrow(), [Duration::from_millis(100); 3]);
        assert_eq!(f.take_calls().len(), 3);
    }

    #[test]
    fn long_intervals_sleep_in_slices() {
        let f = Fixture::new();
        let stop = Rc::new(AtomicBool::new(false));
        let clock = FakeClock {
            now: Cell::new(Instant::now()),
            slept: RefCell::default(),
            stop_after: 4,
            stop: stop.clone(),
        };
        let mut d = f
            .daemon(always, false)
            .with_poll_interval(Duration::from_secs(60));
        d.run(&clock, &stop);
        assert_eq!(*clock.slept.borrow(), [SLEEP_SLICE; 4]);
    }

    #[test]
    fn stop_flag_set_before_run_does_nothing() {
        let f = Fixture::new();
        f.project("a", "");
        let stop = Rc::new(AtomicBool::new(true));
        let clock = FakeClock {
            now: Cell::new(Instant::now()),
            slept: RefCell::default(),
            stop_after: 1,
            stop: stop.clone(),
        };
        f.daemon(always, false).run(&clock, &stop);
        assert!(f.take_calls().is_empty());
        assert!(clock.slept.borrow().is_empty());
    }
}
