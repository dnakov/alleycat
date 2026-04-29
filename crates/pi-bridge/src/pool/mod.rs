//! `PiPool` — owns the set of live pi-coding-agent subprocesses and routes
//! codex thread ids to the right process.
//!
//! Per the design (`~/.claude/plans/i-wanna-design-a-smooth-horizon.md`,
//! "Multi-cwd / multi-process pool"):
//!
//! - **One pi process per codex thread.** Pi binds `process.cwd` per-process
//!   via `process.chdir`, and a single pi process holds exactly one active
//!   session at a time. So even when two codex threads share a `cwd`, each
//!   gets its own pi child.
//! - **Idle reaping.** A thread with no in-flight turn for [`Self::idle_ttl`]
//!   is reaped: stdin is closed (pi exits cleanly, JSONL persists for resume).
//! - **Bounded.** [`Self::max_processes`] caps concurrency; LRU-evicts the
//!   least-recently-active idle thread when a new acquire would exceed the
//!   cap. Active threads (turn in progress) are never evicted — over-cap
//!   acquires fail with [`PoolError::Capacity`] in that case.
//!
//! The pool exposes only structural operations (spawn, attach to thread,
//! lookup, release, evict). Sending pi commands is the caller's job via the
//! returned [`Arc<PiProcessHandle>`].

pub mod pi_protocol;
pub mod process;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alleycat_bridge_core::{LocalLauncher, ProcessLauncher};
use thiserror::Error;
use tokio::sync::Mutex;
use uuid::Uuid;

pub use pi_protocol::*;
pub use process::{PiProcessError, PiProcessHandle};

/// Codex thread identifier as it appears on the wire (UUID-shaped string).
///
/// We don't introduce a newtype here because handlers/turn.rs and
/// handlers/thread.rs deal with the raw string form directly when forwarding
/// notifications; an extra wrapper would just churn at the boundary. If
/// codex_proto later adds a strong type, swap this alias.
pub type ThreadId = String;

/// Bounded pool default — 16 concurrent pi processes. Generous enough for
/// a typical workflow, low enough that a runaway client can't exhaust
/// system resources.
pub const DEFAULT_MAX_PROCESSES: usize = 16;

/// Idle reap interval default — 10 minutes. After this long without an
/// `acquire`, `mark_active`, or in-flight turn marker, the pi child is
/// shut down. Resume rehydrates from the persisted JSONL session.
pub const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Error)]
pub enum PoolError {
    #[error("pool is at capacity ({0} processes); no idle thread to evict")]
    Capacity(usize),

    #[error("thread {0} already exists in the pool")]
    DuplicateThread(ThreadId),

    #[error(transparent)]
    Spawn(#[from] anyhow::Error),
}

/// Per-thread bookkeeping the pool keeps alongside each [`PiProcessHandle`].
#[derive(Debug)]
struct PoolEntry {
    handle: Arc<PiProcessHandle>,
    cwd: PathBuf,
    last_active: Instant,
    /// True while a turn is being driven through this thread. The reaper
    /// never evicts threads with `active=true` regardless of TTL.
    active: bool,
}

#[derive(Debug)]
struct PoolInner {
    processes: HashMap<ThreadId, PoolEntry>,
    by_cwd: HashMap<PathBuf, HashSet<ThreadId>>,
    max_processes: usize,
    idle_ttl: Duration,
}

impl PoolInner {
    fn insert(&mut self, thread_id: ThreadId, entry: PoolEntry) {
        self.by_cwd
            .entry(entry.cwd.clone())
            .or_default()
            .insert(thread_id.clone());
        self.processes.insert(thread_id, entry);
    }

    fn remove(&mut self, thread_id: &str) -> Option<PoolEntry> {
        let entry = self.processes.remove(thread_id)?;
        if let Some(set) = self.by_cwd.get_mut(&entry.cwd) {
            set.remove(thread_id);
            if set.is_empty() {
                self.by_cwd.remove(&entry.cwd);
            }
        }
        Some(entry)
    }

    /// Pick the least-recently-active *idle* thread for eviction. Returns
    /// `None` when every thread currently has a turn in flight.
    fn pick_lru_idle(&self) -> Option<ThreadId> {
        self.processes
            .iter()
            .filter(|(_, e)| !e.active)
            .min_by_key(|(_, e)| e.last_active)
            .map(|(id, _)| id.clone())
    }

    fn collect_expired(&self, now: Instant) -> Vec<ThreadId> {
        self.processes
            .iter()
            .filter(|(_, e)| !e.active && now.duration_since(e.last_active) >= self.idle_ttl)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

/// Thread-safe pool of pi processes.
#[derive(Clone)]
pub struct PiPool {
    inner: Arc<Mutex<PoolInner>>,
    pi_bin: PathBuf,
    launcher: Arc<dyn ProcessLauncher>,
}

impl std::fmt::Debug for PiPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PiPool")
            .field("pi_bin", &self.pi_bin)
            .finish_non_exhaustive()
    }
}

impl PiPool {
    /// Compatibility constructor: build a pool that uses `LocalLauncher`. Kept
    /// so the daemon's existing `agents.rs` callsite (`PiPool::new(bin)`)
    /// keeps compiling unchanged through the A2 → A5 sequence; A5 will
    /// migrate that callsite to `PiBridge::builder().launcher(...)` and
    /// drop this shim.
    pub fn new(pi_bin: impl Into<PathBuf>) -> Self {
        Self::with_launcher(pi_bin, Arc::new(LocalLauncher))
    }

    /// Build a pool that launches `pi-coding-agent` through `launcher` with
    /// the default cap + idle TTL. Daemon path uses `Arc::new(LocalLauncher)`;
    /// Litter substitutes `Arc::new(SshLauncher::new(...))`.
    pub fn with_launcher(pi_bin: impl Into<PathBuf>, launcher: Arc<dyn ProcessLauncher>) -> Self {
        Self::with_launcher_and_limits(pi_bin, launcher, DEFAULT_MAX_PROCESSES, DEFAULT_IDLE_TTL)
    }

    pub fn with_limits(
        pi_bin: impl Into<PathBuf>,
        max_processes: usize,
        idle_ttl: Duration,
    ) -> Self {
        Self::with_launcher_and_limits(pi_bin, Arc::new(LocalLauncher), max_processes, idle_ttl)
    }

    pub fn with_launcher_and_limits(
        pi_bin: impl Into<PathBuf>,
        launcher: Arc<dyn ProcessLauncher>,
        max_processes: usize,
        idle_ttl: Duration,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(PoolInner {
                processes: HashMap::new(),
                by_cwd: HashMap::new(),
                max_processes: max_processes.max(1),
                idle_ttl,
            })),
            pi_bin: pi_bin.into(),
            launcher,
        }
    }

    /// Path of the pi binary this pool spawns.
    pub fn pi_bin(&self) -> &Path {
        &self.pi_bin
    }

    /// Process launcher used to spawn pi children.
    pub fn launcher(&self) -> &Arc<dyn ProcessLauncher> {
        &self.launcher
    }

    /// Spawn a fresh pi process for a brand-new codex thread, mint a thread
    /// id, and return both. The handler is responsible for sending pi the
    /// `new_session` (and any `set_model`/`set_thinking_level` overrides)
    /// before the first `prompt`.
    pub async fn acquire_for_new_thread(
        &self,
        cwd: impl AsRef<Path>,
    ) -> Result<(ThreadId, Arc<PiProcessHandle>), PoolError> {
        let thread_id = Uuid::now_v7().to_string();
        let handle = self
            .spawn_with_capacity_check(thread_id.clone(), cwd.as_ref())
            .await?;
        Ok((thread_id, handle))
    }

    /// Spawn a fresh pi process bound to `cwd` for an explicit `thread_id`,
    /// e.g. when resuming a thread that already exists in the bridge index.
    /// Errors if the pool already tracks `thread_id` — callers should `get`
    /// first and only fall back to acquire if the existing process exited.
    pub async fn acquire_for_resume(
        &self,
        thread_id: ThreadId,
        cwd: impl AsRef<Path>,
    ) -> Result<Arc<PiProcessHandle>, PoolError> {
        {
            let inner = self.inner.lock().await;
            if inner.processes.contains_key(&thread_id) {
                return Err(PoolError::DuplicateThread(thread_id));
            }
        }
        self.spawn_with_capacity_check(thread_id, cwd.as_ref())
            .await
    }

    /// Borrow a pi process for a one-shot, connection-scoped query
    /// (`model/list`, `skills/list`). Pi processes one command at a time on
    /// stdin, so utility queries serialize behind any in-flight turn on the
    /// same handle — but `get_available_models` / `get_commands` are fast and
    /// non-blocking on pi's side, so the wait is bounded.
    ///
    /// Reuse strategy:
    /// 1. If `cwd` is supplied and a thread-bound pi process exists for that
    ///    cwd, return its handle (no spawn). This matches `skills/list`
    ///    semantics — the catalog is per-cwd.
    /// 2. Otherwise, if any thread-bound pi process exists, return the
    ///    least-recently-active one (suitable for `model/list` which is
    ///    cwd-independent).
    /// 3. Otherwise, spawn a fresh pi process tagged with a synthetic
    ///    `utility_<uuid>` thread id. The handle rides the normal idle TTL —
    ///    next `reap_idle()` call (or the next acquire that hits capacity)
    ///    sweeps it. Callers should not call `mark_active` on a utility
    ///    handle so it stays eligible for reaping.
    ///
    /// `cwd` defaults to the bridge process's current directory when `None`
    /// and a fresh spawn is required, since pi binds `process.chdir` at
    /// startup and `model/list` doesn't care which cwd it sees.
    pub async fn acquire_utility(
        &self,
        cwd: Option<&Path>,
    ) -> Result<Arc<PiProcessHandle>, PoolError> {
        // (1) cwd-scoped reuse.
        if let Some(target) = cwd {
            let reused = {
                let mut inner = self.inner.lock().await;
                inner
                    .by_cwd
                    .get(target)
                    .and_then(|set| set.iter().next().cloned())
                    .and_then(|id| inner.processes.get_mut(&id).map(|e| e.handle.clone()))
            };
            if let Some(handle) = reused {
                return Ok(handle);
            }
        }
        // (2) cwd-agnostic reuse: any LRU thread-bound process.
        let reused_any = {
            let inner = self.inner.lock().await;
            inner
                .processes
                .iter()
                .min_by_key(|(_, e)| e.last_active)
                .map(|(_, e)| e.handle.clone())
        };
        if let Some(handle) = reused_any {
            return Ok(handle);
        }
        // (3) fresh spawn under a synthetic id.
        let cwd = cwd
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let synthetic_id = format!("utility_{}", Uuid::now_v7());
        self.spawn_with_capacity_check(synthetic_id, &cwd).await
    }

    /// Look up the pi process that owns `thread_id`, refreshing its
    /// last-active timestamp so the reaper won't pick it up immediately.
    pub async fn get(&self, thread_id: &str) -> Option<Arc<PiProcessHandle>> {
        let mut inner = self.inner.lock().await;
        let entry = inner.processes.get_mut(thread_id)?;
        entry.last_active = Instant::now();
        Some(entry.handle.clone())
    }

    /// Mark a thread as currently driving a turn (or any other long-running
    /// operation). Active threads are not eligible for LRU eviction or idle
    /// reaping until [`Self::mark_idle`] is called.
    pub async fn mark_active(&self, thread_id: &str) {
        let mut inner = self.inner.lock().await;
        if let Some(entry) = inner.processes.get_mut(thread_id) {
            entry.active = true;
            entry.last_active = Instant::now();
        }
    }

    /// Inverse of [`Self::mark_active`]; refreshes `last_active`.
    pub async fn mark_idle(&self, thread_id: &str) {
        let mut inner = self.inner.lock().await;
        if let Some(entry) = inner.processes.get_mut(thread_id) {
            entry.active = false;
            entry.last_active = Instant::now();
        }
    }

    /// Explicitly release a thread's pi process (e.g. user closed the
    /// thread). Sends EOF on stdin and reaps the child. No-op if the
    /// thread isn't in the pool.
    pub async fn release(&self, thread_id: &str) {
        let entry = {
            let mut inner = self.inner.lock().await;
            inner.remove(thread_id)
        };
        if let Some(entry) = entry {
            entry.handle.shutdown().await;
        }
    }

    /// All thread ids currently tracked by the pool.
    pub async fn loaded_thread_ids(&self) -> Vec<ThreadId> {
        self.inner.lock().await.processes.keys().cloned().collect()
    }

    /// Thread ids running in the given `cwd`.
    pub async fn threads_for_cwd(&self, cwd: impl AsRef<Path>) -> Vec<ThreadId> {
        let cwd = cwd.as_ref();
        self.inner
            .lock()
            .await
            .by_cwd
            .get(cwd)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Count of live pi processes (== number of tracked threads).
    pub async fn len(&self) -> usize {
        self.inner.lock().await.processes.len()
    }

    /// Returns true when the pool has no live processes.
    pub async fn is_empty(&self) -> bool {
        self.inner.lock().await.processes.is_empty()
    }

    /// Sweep idle threads whose `last_active` is older than `idle_ttl`.
    /// Returns the thread ids that were reaped. Callers may run this on a
    /// timer; it's also called opportunistically before each new acquire.
    pub async fn reap_idle(&self) -> Vec<ThreadId> {
        let now = Instant::now();
        let expired: Vec<ThreadId> = {
            let inner = self.inner.lock().await;
            inner.collect_expired(now)
        };
        let mut reaped = Vec::with_capacity(expired.len());
        for id in expired {
            let entry = self.inner.lock().await.remove(&id);
            if let Some(entry) = entry {
                entry.handle.shutdown().await;
                reaped.push(id);
            }
        }
        reaped
    }

    /// Spawn `pi-coding-agent --mode rpc` for the given thread/cwd. Performs
    /// idle-reaping and at-cap LRU eviction first; bails with
    /// [`PoolError::Capacity`] only if every tracked thread is currently
    /// active.
    async fn spawn_with_capacity_check(
        &self,
        thread_id: ThreadId,
        cwd: &Path,
    ) -> Result<Arc<PiProcessHandle>, PoolError> {
        // Best-effort reap before checking the cap.
        self.reap_idle().await;

        // Capacity check + LRU eviction loop. We re-check after each
        // eviction in case multiple acquires raced.
        loop {
            let evict = {
                let inner = self.inner.lock().await;
                if inner.processes.len() < inner.max_processes {
                    None
                } else {
                    inner.pick_lru_idle()
                }
            };
            match evict {
                Some(victim) => {
                    let entry = self.inner.lock().await.remove(&victim);
                    if let Some(entry) = entry {
                        entry.handle.shutdown().await;
                    }
                }
                None => {
                    let inner = self.inner.lock().await;
                    if inner.processes.len() >= inner.max_processes {
                        return Err(PoolError::Capacity(inner.max_processes));
                    }
                    break;
                }
            }
        }

        let handle = PiProcessHandle::launch_with(self.launcher.as_ref(), cwd, &self.pi_bin)
            .await
            .map_err(PoolError::Spawn)?;
        let handle = Arc::new(handle);
        let entry = PoolEntry {
            handle: handle.clone(),
            cwd: cwd.to_path_buf(),
            last_active: Instant::now(),
            active: false,
        };
        let mut inner = self.inner.lock().await;
        if inner.processes.contains_key(&thread_id) {
            // Race: another acquire raced us. Drop the new handle (Drop will
            // shut it down).
            drop(inner);
            handle.shutdown().await;
            return Err(PoolError::DuplicateThread(thread_id));
        }
        inner.insert(thread_id, entry);
        Ok(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_pi_pool(max: usize, ttl: Duration) -> PiPool {
        // Use a path that doesn't exist; we never call spawn in these tests
        // — they exercise only the bookkeeping helpers via direct insert.
        PiPool::with_limits(PathBuf::from("/usr/bin/false"), max, ttl)
    }

    fn dummy_entry(cwd: PathBuf, active: bool, age: Duration) -> PoolEntry {
        // Build a poisoned PiProcessHandle by skipping spawn: we never use
        // the handle in pure-bookkeeping tests, only check the table state.
        // Using a never-spawned process would dangle channels, so we drop
        // any inserted entry via PoolInner::remove (which clears bookkeeping)
        // — the handle's TaskSet::drop is safe (it just aborts None tasks).
        let (writer_tx, _writer_rx) = tokio::sync::mpsc::unbounded_channel();
        let (events_tx, _) = tokio::sync::broadcast::channel(1);
        let handle = PiProcessHandle::__test_dangling(writer_tx, events_tx, cwd.clone());
        PoolEntry {
            handle: Arc::new(handle),
            cwd,
            last_active: Instant::now() - age,
            active,
        }
    }

    #[tokio::test]
    async fn lru_picks_oldest_idle() {
        let pool = fake_pi_pool(3, Duration::from_secs(60));
        let mut inner = pool.inner.lock().await;
        inner.insert(
            "t1".into(),
            dummy_entry(PathBuf::from("/a"), false, Duration::from_secs(5)),
        );
        inner.insert(
            "t2".into(),
            dummy_entry(PathBuf::from("/b"), false, Duration::from_secs(20)),
        );
        inner.insert(
            "t3".into(),
            dummy_entry(PathBuf::from("/c"), true, Duration::from_secs(60)),
        );
        let victim = inner.pick_lru_idle().unwrap();
        assert_eq!(victim, "t2", "active thread t3 must not be picked");
    }

    #[tokio::test]
    async fn lru_returns_none_when_all_active() {
        let pool = fake_pi_pool(3, Duration::from_secs(60));
        let mut inner = pool.inner.lock().await;
        inner.insert(
            "t1".into(),
            dummy_entry(PathBuf::from("/a"), true, Duration::ZERO),
        );
        inner.insert(
            "t2".into(),
            dummy_entry(PathBuf::from("/b"), true, Duration::ZERO),
        );
        assert!(inner.pick_lru_idle().is_none());
    }

    #[tokio::test]
    async fn collect_expired_respects_active_flag_and_ttl() {
        let pool = fake_pi_pool(8, Duration::from_secs(30));
        let mut inner = pool.inner.lock().await;
        inner.insert(
            "young_idle".into(),
            dummy_entry(PathBuf::from("/a"), false, Duration::from_secs(5)),
        );
        inner.insert(
            "old_active".into(),
            dummy_entry(PathBuf::from("/b"), true, Duration::from_secs(120)),
        );
        inner.insert(
            "old_idle".into(),
            dummy_entry(PathBuf::from("/c"), false, Duration::from_secs(120)),
        );
        let expired = inner.collect_expired(Instant::now());
        assert_eq!(expired, vec!["old_idle".to_string()]);
    }

    #[tokio::test]
    async fn by_cwd_index_tracks_inserts_and_removes() {
        let pool = fake_pi_pool(8, Duration::from_secs(60));
        {
            let mut inner = pool.inner.lock().await;
            inner.insert(
                "t1".into(),
                dummy_entry(PathBuf::from("/x"), false, Duration::ZERO),
            );
            inner.insert(
                "t2".into(),
                dummy_entry(PathBuf::from("/x"), false, Duration::ZERO),
            );
            inner.insert(
                "t3".into(),
                dummy_entry(PathBuf::from("/y"), false, Duration::ZERO),
            );
        }
        let mut x = pool.threads_for_cwd("/x").await;
        x.sort();
        assert_eq!(x, vec!["t1".to_string(), "t2".to_string()]);
        assert_eq!(pool.threads_for_cwd("/y").await, vec!["t3".to_string()]);

        {
            let mut inner = pool.inner.lock().await;
            inner.remove("t1");
        }
        assert_eq!(pool.threads_for_cwd("/x").await, vec!["t2".to_string()]);

        {
            let mut inner = pool.inner.lock().await;
            inner.remove("t2");
        }
        assert!(pool.threads_for_cwd("/x").await.is_empty());
    }

    #[tokio::test]
    async fn mark_active_blocks_lru_pick() {
        let pool = fake_pi_pool(8, Duration::from_secs(60));
        {
            let mut inner = pool.inner.lock().await;
            inner.insert(
                "t1".into(),
                dummy_entry(PathBuf::from("/a"), false, Duration::from_secs(120)),
            );
        }
        // Before mark_active: LRU picks t1.
        assert_eq!(
            pool.inner.lock().await.pick_lru_idle().as_deref(),
            Some("t1")
        );
        pool.mark_active("t1").await;
        assert!(pool.inner.lock().await.pick_lru_idle().is_none());
        pool.mark_idle("t1").await;
        assert_eq!(
            pool.inner.lock().await.pick_lru_idle().as_deref(),
            Some("t1")
        );
    }

    #[tokio::test]
    async fn acquire_utility_reuses_cwd_match_when_present() {
        let pool = fake_pi_pool(8, Duration::from_secs(60));
        let target_handle = {
            let mut inner = pool.inner.lock().await;
            inner.insert(
                "t1".into(),
                dummy_entry(PathBuf::from("/repo"), false, Duration::from_secs(5)),
            );
            inner.insert(
                "t2".into(),
                dummy_entry(PathBuf::from("/other"), false, Duration::from_secs(1)),
            );
            inner.processes.get("t1").unwrap().handle.clone()
        };
        let handle = pool
            .acquire_utility(Some(Path::new("/repo")))
            .await
            .expect("utility");
        // Same Arc identity as the t1 handle.
        assert!(Arc::ptr_eq(&handle, &target_handle));
    }

    #[tokio::test]
    async fn acquire_utility_falls_back_to_lru_when_no_cwd_match() {
        let pool = fake_pi_pool(8, Duration::from_secs(60));
        let lru_handle = {
            let mut inner = pool.inner.lock().await;
            // older last_active, so it should be picked by `min_by_key`.
            inner.insert(
                "older".into(),
                dummy_entry(PathBuf::from("/a"), false, Duration::from_secs(60)),
            );
            inner.insert(
                "newer".into(),
                dummy_entry(PathBuf::from("/b"), false, Duration::from_secs(1)),
            );
            inner.processes.get("older").unwrap().handle.clone()
        };
        // No cwd given → LRU fallback over all processes.
        let handle = pool.acquire_utility(None).await.expect("utility");
        assert!(Arc::ptr_eq(&handle, &lru_handle));
    }

    #[tokio::test]
    async fn acquire_utility_falls_back_to_lru_when_cwd_unmatched() {
        let pool = fake_pi_pool(8, Duration::from_secs(60));
        let lru_handle = {
            let mut inner = pool.inner.lock().await;
            inner.insert(
                "t1".into(),
                dummy_entry(PathBuf::from("/a"), false, Duration::from_secs(5)),
            );
            inner.processes.get("t1").unwrap().handle.clone()
        };
        // cwd /nope has no thread; falls through to LRU which picks t1.
        let handle = pool
            .acquire_utility(Some(Path::new("/nope")))
            .await
            .expect("utility");
        assert!(Arc::ptr_eq(&handle, &lru_handle));
    }

    #[tokio::test]
    async fn loaded_thread_ids_and_len() {
        let pool = fake_pi_pool(8, Duration::from_secs(60));
        assert_eq!(pool.len().await, 0);
        assert!(pool.is_empty().await);
        {
            let mut inner = pool.inner.lock().await;
            inner.insert(
                "alpha".into(),
                dummy_entry(PathBuf::from("/a"), false, Duration::ZERO),
            );
            inner.insert(
                "beta".into(),
                dummy_entry(PathBuf::from("/b"), false, Duration::ZERO),
            );
        }
        assert_eq!(pool.len().await, 2);
        let mut ids = pool.loaded_thread_ids().await;
        ids.sort();
        assert_eq!(ids, vec!["alpha".to_string(), "beta".to_string()]);
    }
}
