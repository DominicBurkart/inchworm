use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{Notify, Semaphore};

#[derive(Debug)]
pub struct RepoQueues<T> {
    queues: BTreeMap<String, VecDeque<(u64, T)>>,
    active: BTreeSet<String>,
    next_seq: u64,
}

impl<T> Default for RepoQueues<T> {
    fn default() -> Self {
        RepoQueues {
            queues: BTreeMap::new(),
            active: BTreeSet::new(),
            next_seq: 0,
        }
    }
}

impl<T> RepoQueues<T> {
    /// ```
    /// use inchworm::scheduler::RepoQueues;
    ///
    /// let mut queues = RepoQueues::default();
    /// queues.enqueue("a/x", 1);
    /// queues.enqueue("a/x", 2);
    /// assert_eq!(queues.claim(), Some(("a/x".to_string(), 1)));
    /// assert_eq!(queues.claim(), None);
    /// queues.finish("a/x");
    /// assert_eq!(queues.claim(), Some(("a/x".to_string(), 2)));
    /// ```
    pub fn enqueue(&mut self, repo: &str, item: T) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.queues
            .entry(repo.to_string())
            .or_default()
            .push_back((seq, item));
    }

    pub fn claim(&mut self) -> Option<(String, T)> {
        let repo = self
            .queues
            .iter()
            .filter(|(repo, queue)| !self.active.contains(*repo) && !queue.is_empty())
            .min_by_key(|(_, queue)| queue.front().map(|(seq, _)| *seq))
            .map(|(repo, _)| repo.clone())?;
        let (_, item) = self.queues.get_mut(&repo)?.pop_front()?;
        self.active.insert(repo.clone());
        Some((repo, item))
    }

    pub fn finish(&mut self, repo: &str) {
        self.active.remove(repo);
        if self.queues.get(repo).is_some_and(VecDeque::is_empty) {
            self.queues.remove(repo);
        }
    }

    pub fn is_active(&self, repo: &str) -> bool {
        self.active.contains(repo)
    }

    pub fn queued_len(&self, repo: &str) -> usize {
        self.queues.get(repo).map_or(0, VecDeque::len)
    }
}

struct Shared<T> {
    queues: Mutex<RepoQueues<T>>,
    wake: Notify,
    permit: Arc<Semaphore>,
}

impl<T> Shared<T> {
    fn queues(&self) -> MutexGuard<'_, RepoQueues<T>> {
        self.queues
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub struct Scheduler<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Clone for Scheduler<T> {
    fn clone(&self) -> Self {
        Scheduler {
            shared: Arc::clone(&self.shared),
        }
    }
}

pub struct Lease<T> {
    repo: String,
    item: T,
    shared: Arc<Shared<T>>,
}

impl<T> Lease<T> {
    pub fn repo(&self) -> &str {
        &self.repo
    }

    pub fn item(&self) -> &T {
        &self.item
    }
}

impl<T> Drop for Lease<T> {
    fn drop(&mut self) {
        self.shared.queues().finish(&self.repo);
        self.shared.wake.notify_waiters();
    }
}

impl<T> Scheduler<T> {
    /// ```
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// use inchworm::scheduler::Scheduler;
    ///
    /// let scheduler = Scheduler::new(1);
    /// scheduler.enqueue("a/x", 7u32);
    /// let lease = scheduler.next().await;
    /// assert_eq!((lease.repo(), *lease.item()), ("a/x", 7));
    /// assert_eq!(scheduler.model_permit().available_permits(), 1);
    /// assert!(scheduler.is_active("a/x"));
    /// drop(lease);
    /// assert!(!scheduler.is_active("a/x"));
    /// # }
    /// ```
    pub fn new(permits: usize) -> Scheduler<T> {
        Scheduler {
            shared: Arc::new(Shared {
                queues: Mutex::new(RepoQueues::default()),
                wake: Notify::new(),
                permit: Arc::new(Semaphore::new(permits.max(1))),
            }),
        }
    }

    pub fn model_permit(&self) -> Arc<Semaphore> {
        Arc::clone(&self.shared.permit)
    }

    pub fn enqueue(&self, repo: &str, item: T) {
        self.shared.queues().enqueue(repo, item);
        self.shared.wake.notify_waiters();
    }

    pub fn is_active(&self, repo: &str) -> bool {
        self.shared.queues().is_active(repo)
    }

    pub fn queued_len(&self, repo: &str) -> usize {
        self.shared.queues().queued_len(repo)
    }

    pub async fn next(&self) -> Lease<T> {
        loop {
            let notified = self.shared.wake.notified();
            let claimed = self.shared.queues().claim();
            if let Some((repo, item)) = claimed {
                return Lease {
                    repo,
                    item,
                    shared: Arc::clone(&self.shared),
                };
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::time::Duration;
    use tokio::time::timeout;

    const SHORT: Duration = Duration::from_millis(80);

    #[test]
    fn fifo_within_a_repo_and_one_active_at_a_time() {
        let mut q = RepoQueues::default();
        for i in 0..3 {
            q.enqueue("r", i);
        }
        assert_eq!(q.claim(), Some(("r".into(), 0)));
        assert!(q.is_active("r"));
        assert_eq!(q.claim(), None);
        assert_eq!(q.queued_len("r"), 2);
        q.finish("r");
        assert_eq!(q.claim(), Some(("r".into(), 1)));
    }

    #[test]
    fn repos_are_served_oldest_first_across_repos() {
        let mut q = RepoQueues::default();
        q.enqueue("b", "b1");
        q.enqueue("a", "a1");
        q.enqueue("b", "b2");
        assert_eq!(q.claim(), Some(("b".into(), "b1")));
        assert_eq!(q.claim(), Some(("a".into(), "a1")));
        assert_eq!(q.claim(), None);
        q.finish("b");
        assert_eq!(q.claim(), Some(("b".into(), "b2")));
    }

    #[test]
    fn empty_claim_and_spurious_finish_are_harmless() {
        let mut q: RepoQueues<u8> = RepoQueues::default();
        assert_eq!(q.claim(), None);
        q.finish("nope");
        assert!(!q.is_active("nope"));
        assert_eq!(q.queued_len("nope"), 0);
    }

    proptest! {
        #[test]
        fn model_never_runs_two_in_a_repo_and_preserves_order(
            ops in prop::collection::vec((0usize..3, any::<bool>()), 0..60)
        ) {
            let repos = ["a", "b", "c"];
            let mut q = RepoQueues::default();
            let mut next_id = 0u32;
            let mut enqueued: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
            let mut started: BTreeMap<String, Vec<u32>> = BTreeMap::new();
            let mut running: BTreeSet<String> = BTreeSet::new();
            for (repo, is_enqueue) in ops {
                if is_enqueue {
                    q.enqueue(repos[repo], next_id);
                    enqueued.entry(repos[repo]).or_default().push(next_id);
                    next_id += 1;
                } else if running.remove(repos[repo]) {
                    q.finish(repos[repo]);
                }
                while let Some((r, id)) = q.claim() {
                    prop_assert!(running.insert(r.clone()));
                    started.entry(r).or_default().push(id);
                }
            }
            for (repo, ids) in started {
                let all = &enqueued[repo.as_str()];
                prop_assert_eq!(&ids[..], &all[..ids.len()]);
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn next_waits_for_work() {
        let s: Scheduler<u8> = Scheduler::new(1);
        assert!(timeout(SHORT, s.next()).await.is_err());
        let waiter = {
            let s = s.clone();
            tokio::spawn(async move { *s.next().await.item() })
        };
        tokio::time::sleep(SHORT).await;
        assert!(!waiter.is_finished());
        s.enqueue("r", 5);
        assert_eq!(timeout(SHORT, waiter).await.unwrap().unwrap(), 5);
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiter_woken_by_a_lease_drop_takes_the_next_item() {
        let s = Scheduler::new(1);
        s.enqueue("r", 1);
        s.enqueue("r", 2);
        let first = s.next().await;
        let waiter = {
            let s = s.clone();
            tokio::spawn(async move { *s.next().await.item() })
        };
        tokio::time::sleep(SHORT).await;
        assert!(!waiter.is_finished());
        drop(first);
        assert_eq!(timeout(SHORT, waiter).await.unwrap().unwrap(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn several_waiters_are_all_woken_when_a_repo_frees_up() {
        let s = Scheduler::new(1);
        s.enqueue("r", 1);
        let first = s.next().await;
        s.enqueue("r", 2);
        s.enqueue("q", 3);
        let waiters: Vec<_> = (0..2)
            .map(|_| {
                let s = s.clone();
                tokio::spawn(async move { *s.next().await.item() })
            })
            .collect();
        tokio::time::sleep(SHORT).await;
        let mut done = 0;
        for waiter in &waiters {
            done += usize::from(waiter.is_finished());
        }
        assert_eq!(done, 1);
        drop(first);
        let mut got = Vec::new();
        for waiter in waiters {
            got.push(timeout(SHORT, waiter).await.unwrap().unwrap());
        }
        got.sort();
        assert_eq!(got, [2, 3]);
    }

    #[tokio::test(start_paused = true)]
    async fn second_run_in_the_same_repo_waits_for_the_lease_to_drop() {
        let s = Scheduler::new(2);
        s.enqueue("r", 1);
        s.enqueue("r", 2);
        let first = s.next().await;
        assert_eq!(*first.item(), 1);
        assert!(s.is_active("r"));
        assert!(timeout(SHORT, s.next()).await.is_err());
        drop(first);
        assert_eq!(*timeout(SHORT, s.next()).await.unwrap().item(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn different_repos_are_leased_concurrently_and_the_lease_holds_no_model_permit() {
        let s = Scheduler::new(1);
        s.enqueue("a", 1);
        s.enqueue("b", 2);
        let first = s.next().await;
        let second = timeout(SHORT, s.next()).await.unwrap();
        assert_eq!((first.repo(), second.repo()), ("a", "b"));
        assert_eq!(s.model_permit().available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_holding_a_lease_can_still_take_the_model_permit() {
        let s = Scheduler::new(1);
        s.enqueue("a", 1);
        let lease = s.next().await;
        let permit = timeout(SHORT, s.model_permit().acquire_owned())
            .await
            .unwrap()
            .unwrap();
        assert!(s.is_active(lease.repo()));
        drop(permit);
        assert_eq!(s.model_permit().available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn n_leased_runs_can_all_use_n_permits_without_deadlock() {
        let s = Scheduler::new(2);
        for repo in ["a", "b", "c"] {
            s.enqueue(repo, repo.to_string());
        }
        let leases = [s.next().await, s.next().await, s.next().await];
        for lease in &leases {
            let permit = timeout(SHORT, s.model_permit().acquire_owned())
                .await
                .unwrap()
                .unwrap();
            drop(permit);
            assert!(s.is_active(lease.repo()));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_a_lease_frees_the_repo_and_prunes_its_empty_queue() {
        let s = Scheduler::new(1);
        s.enqueue("a", 1);
        let lease = s.next().await;
        assert!(s.is_active("a"));
        assert_eq!(s.queued_len("a"), 0);
        drop(lease);
        assert!(!s.is_active("a"));
        assert!(s.shared.queues().queues.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_next_leaves_queued_work_untouched() {
        let s = Scheduler::new(1);
        assert!(timeout(SHORT, s.next()).await.is_err());
        s.enqueue("a", 1);
        s.enqueue("a", 2);
        drop(s.next().await);
        assert_eq!(s.queued_len("a"), 1);
        assert!(!s.is_active("a"));
        assert_eq!(*s.next().await.item(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn every_waiter_blocked_on_an_empty_queue_is_woken_by_an_enqueue_in_turn() {
        let s = Scheduler::new(1);
        let waiters: Vec<_> = (0..3)
            .map(|_| {
                let s = s.clone();
                tokio::spawn(async move {
                    let lease = s.next().await;
                    *lease.item()
                })
            })
            .collect();
        tokio::time::sleep(SHORT).await;
        for (i, repo) in ["a", "b", "c"].into_iter().enumerate() {
            s.enqueue(repo, i);
        }
        let mut got = Vec::new();
        for waiter in waiters {
            got.push(timeout(SHORT, waiter).await.unwrap().unwrap());
        }
        got.sort();
        assert_eq!(got, [0, 1, 2]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn no_wakeup_is_lost_when_enqueues_and_lease_drops_race_with_claims() {
        let s: Scheduler<usize> = Scheduler::new(1);
        let total = 400usize;
        let consumers: Vec<_> = (0..4)
            .map(|_| {
                let s = s.clone();
                tokio::spawn(async move {
                    let mut seen = Vec::new();
                    loop {
                        let lease = s.next().await;
                        if *lease.item() == usize::MAX {
                            return seen;
                        }
                        seen.push(*lease.item());
                        tokio::task::yield_now().await;
                    }
                })
            })
            .collect();
        let producer = {
            let s = s.clone();
            tokio::spawn(async move {
                for i in 0..total {
                    s.enqueue(["a", "b"][i % 2], i);
                    if i % 7 == 0 {
                        tokio::task::yield_now().await;
                    }
                }
            })
        };
        timeout(Duration::from_secs(60), producer)
            .await
            .unwrap()
            .unwrap();
        loop {
            if s.queued_len("a") + s.queued_len("b") == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        for _ in 0..4 {
            s.enqueue("stop", usize::MAX);
        }
        let mut all = Vec::new();
        for consumer in consumers {
            all.extend(
                timeout(Duration::from_secs(60), consumer)
                    .await
                    .expect("a consumer was left sleeping while work was queued")
                    .unwrap(),
            );
        }
        all.sort();
        assert_eq!(all, (0..total).collect::<Vec<_>>());
    }

    struct Fixed;

    impl crate::judges::Scorer for Fixed {
        async fn complete(
            &self,
            _model: &str,
            _prompt: &str,
        ) -> Result<String, crate::judges::LlmError> {
            Ok(r#"{"score":9,"reason":"ok"}"#.to_string())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_llm_judge_runs_under_a_lease_on_the_shared_permit() {
        let s = Scheduler::new(1);
        s.enqueue("a", 1);
        let lease = s.next().await;
        let verdict = timeout(
            SHORT,
            crate::judges::llm_judge(&Fixed, &s.model_permit(), "r", "m", 5, "+x"),
        )
        .await
        .expect("the judge must not deadlock against its own lease");
        assert!(verdict.passed);
        assert!(s.is_active(lease.repo()));
        assert_eq!(s.model_permit().available_permits(), 1);
    }

    #[tokio::test]
    async fn zero_permits_is_treated_as_one() {
        let s: Scheduler<u8> = Scheduler::new(0);
        assert_eq!(s.model_permit().available_permits(), 1);
    }
}
