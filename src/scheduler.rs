use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

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
        self.claim_seq().map(|(repo, _, item)| (repo, item))
    }

    fn claim_seq(&mut self) -> Option<(String, u64, T)> {
        let repo = self
            .queues
            .iter()
            .filter(|(repo, queue)| !self.active.contains(*repo) && !queue.is_empty())
            .min_by_key(|(_, queue)| queue.front().map(|(seq, _)| *seq))
            .map(|(repo, _)| repo.clone())?;
        let (seq, item) = self.queues.get_mut(&repo)?.pop_front()?;
        self.active.insert(repo.clone());
        Some((repo, seq, item))
    }

    fn requeue_front(&mut self, repo: &str, seq: u64, item: T) {
        self.queues
            .entry(repo.to_string())
            .or_default()
            .push_front((seq, item));
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
    item: Option<T>,
    seq: u64,
    shared: Arc<Shared<T>>,
    permit: Option<OwnedSemaphorePermit>,
    started: bool,
}

impl<T> Lease<T> {
    pub fn repo(&self) -> &str {
        &self.repo
    }

    pub fn item(&self) -> &T {
        self.item
            .as_ref()
            .expect("lease item is present until drop")
    }

    /// ```
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// use inchworm::scheduler::Scheduler;
    ///
    /// let scheduler = Scheduler::new(1);
    /// scheduler.enqueue("a/x", 7u32);
    /// let mut lease = scheduler.next().await;
    /// let permit = lease.take_permit().unwrap();
    /// assert!(lease.take_permit().is_none());
    /// drop(permit);
    /// assert_eq!(scheduler.model_permit().available_permits(), 1);
    /// assert!(scheduler.is_active("a/x"));
    /// drop(lease);
    /// assert_eq!(scheduler.queued_len("a/x"), 0);
    /// # }
    /// ```
    pub fn take_permit(&mut self) -> Option<OwnedSemaphorePermit> {
        self.permit.take()
    }
}

impl<T> Drop for Lease<T> {
    fn drop(&mut self) {
        let started = self.started;
        self.permit.take();
        {
            let mut queues = self.shared.queues();
            if !started && let Some(item) = self.item.take() {
                queues.requeue_front(&self.repo, self.seq, item);
            }
            queues.finish(&self.repo);
        }
        self.shared.wake.notify_waiters();
        self.shared.wake.notify_one();
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
    /// assert_eq!(scheduler.model_permit().available_permits(), 0);
    /// drop(lease);
    /// assert_eq!(scheduler.model_permit().available_permits(), 1);
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
        self.shared.wake.notify_one();
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
            tokio::pin!(notified);
            notified.as_mut().enable();
            let claimed = self.shared.queues().claim_seq();
            if let Some((repo, seq, item)) = claimed {
                let mut lease = Lease {
                    repo,
                    item: Some(item),
                    seq,
                    shared: Arc::clone(&self.shared),
                    permit: None,
                    started: false,
                };
                if let Ok(permit) = Arc::clone(&self.shared.permit).acquire_owned().await {
                    lease.permit = Some(permit);
                    lease.started = true;
                }
                return lease;
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

    #[tokio::test]
    async fn next_waits_for_work() {
        let s: Scheduler<u8> = Scheduler::new(1);
        assert!(timeout(SHORT, s.next()).await.is_err());
        let waiter = {
            let s = s.clone();
            tokio::spawn(async move { *s.next().await.item() })
        };
        tokio::time::sleep(SHORT).await;
        s.enqueue("r", 5);
        assert_eq!(
            timeout(Duration::from_secs(2), waiter)
                .await
                .unwrap()
                .unwrap(),
            5
        );
    }

    #[tokio::test]
    async fn second_run_in_the_same_repo_waits_for_the_lease_to_drop() {
        let s = Scheduler::new(2);
        s.enqueue("r", 1);
        s.enqueue("r", 2);
        let first = s.next().await;
        assert_eq!(*first.item(), 1);
        assert!(s.is_active("r"));
        assert!(timeout(SHORT, s.next()).await.is_err());
        drop(first);
        let second = timeout(Duration::from_secs(2), s.next()).await.unwrap();
        assert_eq!(*second.item(), 2);
    }

    #[tokio::test]
    async fn global_permit_serialises_different_repos() {
        let s = Scheduler::new(1);
        s.enqueue("a", 1);
        s.enqueue("b", 2);
        let first = s.next().await;
        assert!(timeout(SHORT, s.next()).await.is_err());
        drop(first);
        let second = timeout(Duration::from_secs(2), s.next()).await.unwrap();
        assert_eq!(second.repo(), "b");
    }

    #[tokio::test]
    async fn permit_is_shared_with_other_holders_such_as_the_llm_judge() {
        let s = Scheduler::new(1);
        let judge_permit = s.model_permit().acquire_owned().await.unwrap();
        s.enqueue("a", 1);
        assert!(timeout(SHORT, s.next()).await.is_err());
        drop(judge_permit);
        let lease = timeout(Duration::from_secs(2), s.next()).await.unwrap();
        assert_eq!(s.model_permit().available_permits(), 0);
        drop(lease);
        assert_eq!(s.model_permit().available_permits(), 1);
    }

    #[tokio::test]
    async fn an_llm_judge_can_take_the_permit_once_the_lease_hands_it_over() {
        let s = Scheduler::new(1);
        s.enqueue("a", 1);
        let mut lease = s.next().await;
        assert!(
            timeout(SHORT, s.model_permit().acquire_owned())
                .await
                .is_err()
        );
        drop(lease.take_permit());
        let judge = timeout(SHORT, s.model_permit().acquire_owned())
            .await
            .unwrap()
            .unwrap();
        drop(judge);
        assert!(s.is_active("a"));
    }

    #[tokio::test]
    async fn dropping_a_lease_after_handing_over_the_permit_does_not_requeue() {
        let s = Scheduler::new(1);
        s.enqueue("a", 1);
        let mut lease = s.next().await;
        drop(lease.take_permit());
        drop(lease);
        assert_eq!(s.queued_len("a"), 0);
        assert!(!s.is_active("a"));
        assert!(timeout(SHORT, s.next()).await.is_err());
    }

    #[tokio::test]
    async fn cancelled_next_does_not_leave_the_repo_stuck() {
        let s = Scheduler::new(1);
        let judge_permit = s.model_permit().acquire_owned().await.unwrap();
        s.enqueue("a", 1);
        s.enqueue("a", 2);
        assert!(timeout(SHORT, s.next()).await.is_err());
        assert!(!s.is_active("a"));
        assert_eq!(s.queued_len("a"), 2);
        drop(judge_permit);
        assert_eq!(*s.next().await.item(), 1);
    }

    #[tokio::test]
    async fn zero_permits_is_treated_as_one() {
        let s = Scheduler::new(0);
        s.enqueue("a", 1);
        assert_eq!(*timeout(SHORT, s.next()).await.unwrap().item(), 1);
    }
}
