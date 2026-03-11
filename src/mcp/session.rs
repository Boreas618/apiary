use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::apiary_client::{ApiaryClient, ApiaryError, ExecuteTaskRequest, ExecuteTaskResponse};

const REAPER_INTERVAL: Duration = Duration::from_secs(60);

struct SessionEntry {
    apiary_session_id: String,
    /// Prevents concurrent session creation for the same client_id.
    init_lock: Mutex<()>,
}

struct ClientState {
    refcount: u32,
    detached_at: Option<Instant>,
}

#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<Inner>,
}

struct Inner {
    client: ApiaryClient,
    working_dir: String,
    idle_timeout: Duration,
    sessions: DashMap<String, SessionEntry>,
    states: DashMap<String, ClientState>,
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl SessionManager {
    pub fn new(client: ApiaryClient, working_dir: String, idle_timeout: Duration) -> Self {
        let (shutdown_tx, _) = tokio::sync::watch::channel(false);
        Self {
            inner: Arc::new(Inner {
                client,
                working_dir,
                idle_timeout,
                sessions: DashMap::new(),
                states: DashMap::new(),
                shutdown: shutdown_tx,
            }),
        }
    }

    /// Increment refcount on connection open.
    pub fn attach(&self, client_id: &str) {
        let mut entry = self
            .inner
            .states
            .entry(client_id.to_owned())
            .or_insert(ClientState {
                refcount: 0,
                detached_at: None,
            });
        entry.refcount += 1;
        entry.detached_at = None;
    }

    /// Decrement refcount on connection close.
    pub fn detach(&self, client_id: &str) {
        if let Some(mut entry) = self.inner.states.get_mut(client_id) {
            entry.refcount = entry.refcount.saturating_sub(1);
            if entry.refcount == 0 {
                entry.detached_at = Some(Instant::now());
            }
        }
    }

    /// Ensure the client has an Apiary session, creating one if needed.
    pub async fn ensure_session(&self, client_id: &str) -> Result<String, ApiaryError> {
        // Fast path: session already exists
        if let Some(entry) = self.inner.sessions.get(client_id) {
            if !entry.apiary_session_id.is_empty() {
                return Ok(entry.apiary_session_id.clone());
            }
        }

        // Slow path: insert placeholder, then create under lock
        self.inner
            .sessions
            .entry(client_id.to_owned())
            .or_insert_with(|| SessionEntry {
                apiary_session_id: String::new(),
                init_lock: Mutex::new(()),
            });

        // Acquire the init lock (must drop DashMap ref first to avoid deadlock)
        let lock_guard = {
            let entry = self.inner.sessions.get(client_id).unwrap();
            entry.init_lock.lock().await
        };

        // Double-check after acquiring lock
        {
            let entry = self.inner.sessions.get(client_id).unwrap();
            if !entry.apiary_session_id.is_empty() {
                return Ok(entry.apiary_session_id.clone());
            }
        }

        let session_id = self
            .inner
            .client
            .create_session(&self.inner.working_dir)
            .await?;
        {
            let mut entry = self.inner.sessions.get_mut(client_id).unwrap();
            entry.apiary_session_id = session_id.clone();
        }
        drop(lock_guard);

        let count = self.inner.sessions.len();
        info!(
            session_id = %session_id,
            client_id = %client_id,
            active = count,
            "session created"
        );
        Ok(session_id)
    }

    /// Execute a command. If session 404s, transparently recreate.
    pub async fn execute(
        &self,
        client_id: &str,
        command: &str,
        timeout_ms: Option<u64>,
        working_dir: Option<&str>,
    ) -> Result<ExecuteTaskResponse, ApiaryError> {
        let wrapped = format!("bash -c {}", shell_escape::escape(command.into()));
        let session_id = self.ensure_session(client_id).await?;

        let req = ExecuteTaskRequest {
            command: wrapped.clone(),
            session_id: session_id.clone(),
            timeout_ms,
            working_dir: working_dir.map(|s| s.to_owned()),
            env: Default::default(),
        };

        match self.inner.client.execute(&req).await {
            Ok(resp) => Ok(resp),
            Err(ApiaryError::SessionNotFound(_)) => {
                warn!(
                    session_id = %session_id,
                    client_id = %client_id,
                    "session lost, recreating"
                );
                self.inner.sessions.remove(client_id);
                let new_session_id = self.ensure_session(client_id).await?;
                let req = ExecuteTaskRequest {
                    command: wrapped,
                    session_id: new_session_id,
                    timeout_ms,
                    working_dir: working_dir.map(|s| s.to_owned()),
                    env: Default::default(),
                };
                self.inner.client.execute(&req).await
            }
            Err(e) => Err(e),
        }
    }

    /// Start the background reaper task.
    pub fn start_reaper(&self) {
        let mgr = self.clone();
        let mut shutdown_rx = self.inner.shutdown.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(REAPER_INTERVAL) => {}
                    _ = shutdown_rx.changed() => break,
                }
                let now = Instant::now();
                let idle_timeout = mgr.inner.idle_timeout;
                let mut to_reap = Vec::new();

                for entry in mgr.inner.states.iter() {
                    if entry.refcount == 0 {
                        if let Some(detached) = entry.detached_at {
                            if now.duration_since(detached) >= idle_timeout {
                                to_reap.push(entry.key().clone());
                            }
                        }
                    }
                }

                for cid in to_reap {
                    info!(client_id = %cid, "reaping idle client");
                    mgr.destroy_client(&cid).await;
                }
            }
        });
    }

    async fn destroy_client(&self, client_id: &str) {
        if let Some((_, entry)) = self.inner.sessions.remove(client_id) {
            if !entry.apiary_session_id.is_empty() {
                if let Err(e) = self
                    .inner
                    .client
                    .destroy_session(&entry.apiary_session_id)
                    .await
                {
                    warn!(
                        error = %e,
                        session_id = %entry.apiary_session_id,
                        "failed to destroy apiary session"
                    );
                }
            }
        }
        self.inner.states.remove(client_id);
        let remaining = self.inner.sessions.len();
        info!(client_id = %client_id, remaining, "client destroyed");
    }

    pub async fn shutdown(&self) {
        let _ = self.inner.shutdown.send(true);
        let client_ids: Vec<String> = self
            .inner
            .sessions
            .iter()
            .map(|e| e.key().clone())
            .collect();
        for cid in client_ids {
            self.destroy_client(&cid).await;
        }
    }

    pub fn active_sessions(&self) -> usize {
        self.inner.sessions.len()
    }
}
