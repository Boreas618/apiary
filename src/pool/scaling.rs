use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use super::manager::{Pool, PoolError, SANDBOX_ACQUIRE_TIMEOUT, SCALER_INTERVAL};

impl Pool {
    /// Try to pop an idle sandbox, or scale up if possible, or wait.
    ///
    /// 1. Pop from `idle_queue` -> done.
    /// 2. If `total < max_sandboxes` -> create a new sandbox inline and also
    ///    spawn background creation of `scale_up_step - 1` more.
    /// 3. If at capacity -> wait up to [`SANDBOX_ACQUIRE_TIMEOUT`].
    pub(super) async fn acquire_sandbox(&self) -> Result<Arc<crate::sandbox::Sandbox>, PoolError> {
        if let Some(sandbox) = self.try_pop_idle() {
            return Ok(sandbox);
        }

        let total = self.sandboxes.read().len();
        if total < self.config.max_sandboxes {
            let sandbox = self.create_sandbox().await?;
            let sandbox_id = sandbox.id().to_string();
            self.sandboxes
                .write()
                .insert(sandbox_id.clone(), sandbox.clone());

            *self.last_scale_event.lock() = Instant::now();
            tracing::info!(
                sandbox_id = %sandbox_id,
                total = total + 1,
                "scaled up: created sandbox on demand"
            );

            let extra = self
                .config
                .scale_up_step
                .saturating_sub(1)
                .min(self.config.max_sandboxes.saturating_sub(total + 1));
            if extra > 0 {
                let pool = self.clone();
                tokio::spawn(async move {
                    for _ in 0..extra {
                        if pool.shutdown.load(Ordering::Relaxed) {
                            break;
                        }
                        if pool.sandboxes.read().len() >= pool.config.max_sandboxes {
                            break;
                        }

                        match pool.create_sandbox().await {
                            Ok(sandbox) => {
                                let sandbox_id = sandbox.id().to_string();
                                pool.store_idle_sandbox(sandbox, Instant::now());
                                tracing::info!(
                                    sandbox_id = %sandbox_id,
                                    total = pool.sandboxes.read().len(),
                                    "scaled up: pre-warmed sandbox"
                                );
                            }
                            Err(error) => {
                                tracing::error!("failed to pre-warm sandbox: {error}");
                                break;
                            }
                        }
                    }
                });
            }

            return Ok(sandbox);
        }

        tracing::debug!(
            "pool at max capacity ({}), waiting for idle sandbox",
            self.config.max_sandboxes
        );
        let deadline = tokio::time::Instant::now() + SANDBOX_ACQUIRE_TIMEOUT;

        loop {
            if let Some(sandbox) = self.try_pop_idle() {
                return Ok(sandbox);
            }

            if self.shutdown.load(Ordering::Relaxed) {
                return Err(PoolError::ShuttingDown);
            }

            let notified = self.idle_notify.notified();
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(PoolError::NoIdleSandbox(SANDBOX_ACQUIRE_TIMEOUT.as_secs()));
            }

            match tokio::time::timeout(remaining, notified).await {
                Ok(()) => continue,
                Err(_) => return Err(PoolError::NoIdleSandbox(SANDBOX_ACQUIRE_TIMEOUT.as_secs())),
            }
        }
    }

    /// Pop one sandbox from the idle queue (non-blocking).
    pub(super) fn try_pop_idle(&self) -> Option<Arc<crate::sandbox::Sandbox>> {
        loop {
            let sandbox_id = self.idle_queue.lock().pop_front()?;
            self.idle_since.write().remove(&sandbox_id);
            if let Some(sandbox) = self.sandboxes.read().get(&sandbox_id).cloned() {
                return Some(sandbox);
            }
        }
    }

    /// Return a sandbox to the idle pool.
    pub(super) fn return_to_idle(&self, sandbox_id: &str) {
        self.idle_since
            .write()
            .insert(sandbox_id.to_string(), Instant::now());
        self.idle_queue.lock().push_back(sandbox_id.to_string());
        self.idle_notify.notify_one();
    }

    /// Create a replacement sandbox after a failed reset, keeping the same ID.
    pub(super) async fn replace_sandbox(&self, sandbox_id: &str) -> Result<(), PoolError> {
        tracing::info!(sandbox_id = %sandbox_id, "creating replacement sandbox");

        let sandbox = self
            .create_sandbox_with_id(sandbox_id.to_string())
            .await
            .map_err(|error| PoolError::InitFailed(format!("replacement init failed: {error}")))?;

        self.store_idle_sandbox(sandbox, Instant::now());
        tracing::info!(sandbox_id = %sandbox_id, "replacement sandbox ready");
        Ok(())
    }

    /// Remove a sandbox from the pool entirely (cleanup + deregister).
    pub(super) fn remove_sandbox(&self, sandbox_id: &str) {
        if let Some(sandbox) = self.sandboxes.write().remove(sandbox_id) {
            if let Err(error) = sandbox.cleanup() {
                tracing::debug!(
                    %error,
                    sandbox_id = %sandbox.id(),
                    "failed to cleanup sandbox while removing it from the pool"
                );
            }
        }
        self.idle_since.write().remove(sandbox_id);
    }

    /// Spawn a background tokio task that periodically:
    ///   - Scales down idle sandboxes beyond `min_sandboxes` that have exceeded
    ///     `idle_timeout`.
    ///   - Pro-actively scales up when idle count hits 0 and pool has capacity.
    pub(super) fn spawn_scaler_task(&self) {
        let pool = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SCALER_INTERVAL).await;

                if pool.shutdown.load(Ordering::Relaxed) {
                    tracing::debug!("scaler: shutdown detected, exiting");
                    break;
                }

                pool.scaler_tick().await;
            }
        });
    }

    pub(super) fn cooldown_elapsed(&self) -> bool {
        self.last_scale_event.lock().elapsed() >= self.config.cooldown
    }

    pub(super) async fn scaler_tick(&self) {
        let total = self.sandboxes.read().len();
        let idle_count = self.idle_queue.lock().len();

        if total > self.config.min_sandboxes && idle_count > 0 && self.cooldown_elapsed() {
            self.try_scale_down(total);
        }

        if idle_count == 0 && total < self.config.max_sandboxes && self.cooldown_elapsed() {
            self.try_proactive_scale_up(total).await;
        }
    }

    /// Remove the sandbox that has been idle longest, provided it has exceeded
    /// `idle_timeout` and the pool would still be at or above `min_sandboxes`.
    pub(super) fn try_scale_down(&self, total: usize) {
        if total <= self.config.min_sandboxes {
            return;
        }

        let now = Instant::now();
        let idle_timeout = self.config.idle_timeout;
        let mut idle_queue = self.idle_queue.lock();
        let mut oldest_idx = None;
        let mut oldest_time = now;

        {
            let idle_since = self.idle_since.read();
            for (idx, sandbox_id) in idle_queue.iter().enumerate() {
                if let Some(&since) = idle_since.get(sandbox_id) {
                    if now.duration_since(since) >= idle_timeout && since < oldest_time {
                        oldest_idx = Some(idx);
                        oldest_time = since;
                    }
                }
            }
        }

        if let Some(idx) = oldest_idx {
            if let Some(sandbox_id) = idle_queue.remove(idx) {
                drop(idle_queue);

                self.remove_sandbox(&sandbox_id);
                *self.last_scale_event.lock() = Instant::now();
                tracing::info!(
                    sandbox_id = %sandbox_id,
                    total = self.sandboxes.read().len(),
                    "scaled down: removed idle sandbox"
                );
            }
        }
    }

    /// Pre-create one sandbox so the next `create_session` doesn't have to
    /// wait for sandbox initialisation.
    pub(super) async fn try_proactive_scale_up(&self, total: usize) {
        if total >= self.config.max_sandboxes {
            return;
        }

        match self.create_sandbox().await {
            Ok(sandbox) => {
                let sandbox_id = sandbox.id().to_string();
                self.store_idle_sandbox(sandbox, Instant::now());
                *self.last_scale_event.lock() = Instant::now();

                tracing::info!(
                    sandbox_id = %sandbox_id,
                    total = self.sandboxes.read().len(),
                    "scaled up: proactive warm-up"
                );
            }
            Err(error) => {
                tracing::error!("proactive scale-up failed: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Pool;
    use crate::config::PoolConfig;
    use crate::sandbox::Sandbox;
    use parking_lot::{Mutex, RwLock};
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;
    use tokio::sync::Notify;

    fn test_pool() -> (Pool, TempDir) {
        let tempdir = TempDir::new().expect("tempdir should be created");
        let rootfs = tempdir.path().join("rootfs");
        let overlay_dir = tempdir.path().join("overlays");
        std::fs::create_dir_all(&rootfs).expect("rootfs directory should be created");

        let config = PoolConfig::builder()
            .min_sandboxes(1)
            .max_sandboxes(3)
            .idle_timeout(Duration::from_secs(30))
            .cooldown(Duration::from_secs(0))
            .base_image(&rootfs)
            .overlay_dir(&overlay_dir)
            .build()
            .expect("test config should build");

        let pool = Pool {
            config: Arc::new(config),
            sandboxes: Arc::new(RwLock::new(HashMap::new())),
            idle_queue: Arc::new(Mutex::new(VecDeque::new())),
            idle_notify: Arc::new(Notify::new()),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            shutdown: Arc::new(AtomicBool::new(false)),
            next_sandbox_id: Arc::new(AtomicUsize::new(0)),
            last_scale_event: Arc::new(Mutex::new(Instant::now() - Duration::from_secs(60))),
            idle_since: Arc::new(RwLock::new(HashMap::new())),
            process_monitor: None,
        };

        (pool, tempdir)
    }

    fn new_sandbox(pool: &Pool, sandbox_id: &str) -> Arc<Sandbox> {
        Arc::new(
            Sandbox::new(sandbox_id.to_string(), pool.config.as_ref())
                .expect("sandbox should be created"),
        )
    }

    #[test]
    fn try_pop_idle_skips_stale_ids() {
        let (pool, _tempdir) = test_pool();
        let sandbox = new_sandbox(&pool, "sandbox-1");
        pool.sandboxes
            .write()
            .insert("sandbox-1".to_string(), sandbox.clone());
        pool.idle_queue
            .lock()
            .extend(["stale".to_string(), "sandbox-1".to_string()]);
        pool.idle_since
            .write()
            .insert("sandbox-1".to_string(), Instant::now());

        let popped = pool
            .try_pop_idle()
            .expect("valid sandbox should be popped after stale entries");
        assert_eq!(popped.id(), "sandbox-1");
        assert!(pool.idle_queue.lock().is_empty());
    }

    #[test]
    fn try_scale_down_removes_oldest_expired_idle_sandbox() {
        let (pool, _tempdir) = test_pool();
        let oldest = new_sandbox(&pool, "sandbox-oldest");
        let newest = new_sandbox(&pool, "sandbox-newest");

        pool.store_idle_sandbox(oldest, Instant::now() - Duration::from_secs(60));
        pool.store_idle_sandbox(newest, Instant::now() - Duration::from_secs(5));

        pool.try_scale_down(2);

        assert!(!pool.sandboxes.read().contains_key("sandbox-oldest"));
        assert!(pool.sandboxes.read().contains_key("sandbox-newest"));
        assert_eq!(
            pool.idle_queue.lock().iter().cloned().collect::<Vec<_>>(),
            vec!["sandbox-newest".to_string()]
        );
    }
}
