//! Async image-load jobs.
//!
//! Submitting a list of images to load via [`ImageJobs::submit`] returns
//! a job id immediately (synchronous ack). A background tokio task walks
//! the queued images through [`ImageLoader::load_one`] with bounded
//! concurrency, updating per-image progress as it goes.
//!
//! Clients poll [`ImageJobs::status`] with the returned id to observe
//! progress and final outcome. Completed jobs are retained in memory for
//! [`JOB_RETENTION`] before being garbage-collected by the reaper.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tracing::{debug, info};
use uuid::Uuid;

use crate::images::{ImageLoader, ImageStage, LoadOutcome, ImageRegistry};

/// How long completed jobs are kept around before the reaper drops them.
pub const JOB_RETENTION: Duration = Duration::from_secs(60 * 60);

/// How often the reaper wakes up to look for expired jobs.
const REAP_INTERVAL: Duration = Duration::from_secs(60);

/// Identifier for a submitted image-load job.
pub type JobId = String;

/// Synchronous acknowledgement returned from `POST /api/v1/images`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobAck {
    pub job_id: JobId,
    /// Images that were not yet registered and have been queued for loading.
    pub queued: Vec<String>,
    /// Images that were already registered and skipped.
    pub already_present: Vec<String>,
}

/// Terminal/non-terminal lifecycle state for a job as a whole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageJobState {
    /// At least one image is still queued or in-flight.
    Running,
    /// All images reached a terminal state and at least one succeeded.
    Done,
    /// All images failed (or every image failed when none were already present).
    Failed,
}

/// Per-image progress entry inside a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum ImageProgress {
    /// Waiting for a load slot (in the queue).
    Queued,
    /// `docker pull` in progress.
    Pulling,
    /// Layers being extracted; `done`/`total` may be 0 if unknown.
    Extracting {
        layers_done: usize,
        layers_total: usize,
    },
    /// Image was already in the registry on submission.
    AlreadyPresent,
    /// Successfully registered.
    Done,
    /// Terminal failure with a human-readable reason.
    Failed { reason: String },
}

impl ImageProgress {
    fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::AlreadyPresent | Self::Done | Self::Failed { .. }
        )
    }

    fn is_failure(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }

    fn is_success(&self) -> bool {
        matches!(self, Self::AlreadyPresent | Self::Done)
    }
}

/// Snapshot of a single image-load job, suitable for HTTP responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageJob {
    pub job_id: JobId,
    pub state: ImageJobState,
    /// RFC 3339 timestamp.
    pub started_at: String,
    /// RFC 3339 timestamp.
    pub updated_at: String,
    pub per_image: BTreeMap<String, ImageProgress>,
    pub failed_images: Vec<FailedImage>,
}

/// Per-image failure record (also accessible via `per_image[name]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailedImage {
    pub name: String,
    pub reason: String,
}

/// Internal mutable state for a job.
struct JobState {
    job_id: JobId,
    state: ImageJobState,
    started_at: String,
    updated_at: String,
    per_image: BTreeMap<String, ImageProgress>,
    completed_at: Option<Instant>,
    handle: Option<JoinHandle<()>>,
}

impl JobState {
    fn snapshot(&self) -> ImageJob {
        let failed_images: Vec<FailedImage> = self
            .per_image
            .iter()
            .filter_map(|(name, prog)| match prog {
                ImageProgress::Failed { reason } => Some(FailedImage {
                    name: name.clone(),
                    reason: reason.clone(),
                }),
                _ => None,
            })
            .collect();
        ImageJob {
            job_id: self.job_id.clone(),
            state: self.state.clone(),
            started_at: self.started_at.clone(),
            updated_at: self.updated_at.clone(),
            per_image: self.per_image.clone(),
            failed_images,
        }
    }

    fn touch(&mut self) {
        self.updated_at = now_iso();
    }
}

/// Pool-owned async image-load job tracker.
#[derive(Clone)]
pub struct ImageJobs {
    inner: Arc<Inner>,
}

struct Inner {
    loader: ImageLoader,
    registry: Arc<ImageRegistry>,
    jobs: RwLock<HashMap<JobId, Arc<RwLock<JobState>>>>,
    shutdown: AtomicBool,
    reaper: parking_lot::Mutex<Option<JoinHandle<()>>>,
}

impl ImageJobs {
    /// Construct a new tracker. Caller is responsible for keeping the
    /// returned handle alive for as long as jobs may be polled.
    pub fn new(loader: ImageLoader, registry: Arc<ImageRegistry>) -> Self {
        let this = Self {
            inner: Arc::new(Inner {
                loader,
                registry,
                jobs: RwLock::new(HashMap::new()),
                shutdown: AtomicBool::new(false),
                reaper: parking_lot::Mutex::new(None),
            }),
        };
        this.spawn_reaper();
        this
    }

    fn spawn_reaper(&self) {
        let weak = Arc::downgrade(&self.inner);
        let handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(REAP_INTERVAL).await;
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                if inner.shutdown.load(Ordering::Relaxed) {
                    return;
                }
                let now = Instant::now();
                let mut jobs = inner.jobs.write();
                jobs.retain(|_, state| {
                    let state = state.read();
                    match state.completed_at {
                        Some(completed) => now.duration_since(completed) < JOB_RETENTION,
                        None => true,
                    }
                });
            }
        });
        *self.inner.reaper.lock() = Some(handle);
    }

    /// Submit a list of images. Returns the job id and the partition
    /// between queued (work to do) and already-present (no work needed).
    /// If every image is already present a job is still recorded with
    /// terminal state `Done`.
    pub fn submit(&self, images: Vec<String>) -> JobAck {
        let job_id = Uuid::new_v4().to_string();
        let started = now_iso();

        let mut already_present = Vec::new();
        let mut queued = Vec::new();
        let mut per_image: BTreeMap<String, ImageProgress> = BTreeMap::new();

        for name in images {
            if per_image.contains_key(&name) {
                continue;
            }
            if self.inner.registry.contains(&name) {
                per_image.insert(name.clone(), ImageProgress::AlreadyPresent);
                already_present.push(name);
            } else {
                per_image.insert(name.clone(), ImageProgress::Queued);
                queued.push(name);
            }
        }

        let initial_state = if queued.is_empty() {
            ImageJobState::Done
        } else {
            ImageJobState::Running
        };
        let completed_at = if queued.is_empty() {
            Some(Instant::now())
        } else {
            None
        };

        let job_state = Arc::new(RwLock::new(JobState {
            job_id: job_id.clone(),
            state: initial_state,
            started_at: started.clone(),
            updated_at: started,
            per_image,
            completed_at,
            handle: None,
        }));
        self.inner
            .jobs
            .write()
            .insert(job_id.clone(), job_state.clone());

        if !queued.is_empty() {
            let inner = Arc::clone(&self.inner);
            let queued_for_task = queued.clone();
            let job_state_for_task = job_state.clone();
            let job_id_for_task = job_id.clone();
            let handle = tokio::spawn(async move {
                run_job(inner, job_id_for_task, job_state_for_task, queued_for_task).await;
            });
            job_state.write().handle = Some(handle);
        }

        info!(
            %job_id,
            queued = queued.len(),
            already_present = already_present.len(),
            "image load job submitted"
        );

        JobAck {
            job_id,
            queued,
            already_present,
        }
    }

    /// Fetch a snapshot of the named job, or `None` if it has been
    /// reaped or never existed.
    pub fn status(&self, job_id: &str) -> Option<ImageJob> {
        let jobs = self.inner.jobs.read();
        jobs.get(job_id).map(|state| state.read().snapshot())
    }

    /// Cancel any in-flight extraction tasks. Existing snapshots remain
    /// queryable; `is_terminal` will be unchanged unless the worker
    /// completes naturally between the cancel and the read.
    pub async fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.inner.reaper.lock().take() {
            handle.abort();
        }
        let handles: Vec<_> = {
            let jobs = self.inner.jobs.read();
            jobs.values()
                .filter_map(|state| state.write().handle.take())
                .collect()
        };
        for h in handles {
            h.abort();
        }
    }
}

async fn run_job(
    inner: Arc<Inner>,
    job_id: JobId,
    job_state: Arc<RwLock<JobState>>,
    queued: Vec<String>,
) {
    let mut tasks = Vec::with_capacity(queued.len());
    for name in queued {
        let loader = inner.loader.clone();
        let job_state = job_state.clone();
        let job_id = job_id.clone();
        let name_for_task = name.clone();
        tasks.push(tokio::spawn(async move {
            let progress_state = job_state.clone();
            let progress_name = name_for_task.clone();
            let outcome = loader
                .load_one(&name_for_task, move |stage| {
                    let mut state = progress_state.write();
                    if let Some(slot) = state.per_image.get_mut(&progress_name) {
                        if let Some(new) = stage_to_progress(stage) {
                            // Don't overwrite a terminal state with a
                            // pre-terminal progress signal that arrives
                            // late.
                            if !slot.is_terminal() || new.is_terminal() {
                                *slot = new;
                            }
                        }
                    }
                    state.touch();
                })
                .await;

            let final_progress = match outcome {
                LoadOutcome::AlreadyPresent => ImageProgress::AlreadyPresent,
                LoadOutcome::Loaded => ImageProgress::Done,
                LoadOutcome::Failed(reason) => ImageProgress::Failed { reason },
            };
            {
                let mut state = job_state.write();
                if let Some(slot) = state.per_image.get_mut(&name) {
                    *slot = final_progress;
                }
                state.touch();
                debug!(%job_id, image = %name, "image load entry finalised");
            }
        }));
    }

    for t in tasks {
        let _ = t.await;
    }

    // Finalise overall job state.
    let mut state = job_state.write();
    let mut any_success = false;
    let mut any_failure = false;
    for prog in state.per_image.values() {
        if prog.is_success() {
            any_success = true;
        }
        if prog.is_failure() {
            any_failure = true;
        }
    }
    state.state = match (any_success, any_failure) {
        (true, _) => ImageJobState::Done,
        (false, true) => ImageJobState::Failed,
        // Defensive: should never hit because we always have at least
        // one queued image to reach this point.
        (false, false) => ImageJobState::Done,
    };
    state.completed_at = Some(Instant::now());
    state.touch();
    info!(
        job_id = %job_id,
        state = ?state.state,
        "image load job finished"
    );
}

fn stage_to_progress(stage: ImageStage) -> Option<ImageProgress> {
    Some(match stage {
        ImageStage::Queued => ImageProgress::Queued,
        ImageStage::Pulling => ImageProgress::Pulling,
        ImageStage::Extracting {
            layers_done,
            layers_total,
        } => ImageProgress::Extracting {
            layers_done,
            layers_total,
        },
        ImageStage::Done => ImageProgress::Done,
        ImageStage::Failed(reason) => ImageProgress::Failed { reason },
    })
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::extractor::LayerExtractor;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn build_jobs() -> (ImageJobs, Arc<ImageRegistry>) {
        let tmp = tempdir().unwrap();
        let registry = Arc::new(ImageRegistry::new(tmp.path()));
        let extractor = Arc::new(
            LayerExtractor::new(tmp.path().to_path_buf(), "docker-not-installed".to_string())
                .expect("layer extractor should construct"),
        );
        let loader = ImageLoader::new(
            registry.clone(),
            extractor,
            "docker-not-installed".to_string(),
            4,
        );
        let jobs = ImageJobs::new(loader, registry.clone());
        std::mem::forget(tmp);
        (jobs, registry)
    }

    #[tokio::test]
    async fn submit_with_no_queued_images_is_immediately_done() {
        let (jobs, registry) = build_jobs();
        registry.insert("img:tag".to_string(), vec![PathBuf::from("/a")]);
        let ack = jobs.submit(vec!["img:tag".to_string()]);
        assert!(ack.queued.is_empty());
        assert_eq!(ack.already_present, vec!["img:tag".to_string()]);

        let snap = jobs.status(&ack.job_id).expect("job should exist");
        assert_eq!(snap.state, ImageJobState::Done);
        assert!(matches!(
            snap.per_image.get("img:tag"),
            Some(ImageProgress::AlreadyPresent)
        ));

        jobs.shutdown().await;
    }

    #[tokio::test]
    async fn failed_load_yields_failed_state() {
        let (jobs, _registry) = build_jobs();
        let ack = jobs.submit(vec!["bogus:tag".to_string()]);
        assert_eq!(ack.queued, vec!["bogus:tag".to_string()]);

        // Wait for the job to finalise.
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let snap = jobs.status(&ack.job_id).expect("job should exist");
            if snap.state != ImageJobState::Running {
                break;
            }
        }
        let snap = jobs.status(&ack.job_id).expect("job should exist");
        assert_eq!(snap.state, ImageJobState::Failed);
        assert!(matches!(
            snap.per_image.get("bogus:tag"),
            Some(ImageProgress::Failed { .. })
        ));
        assert_eq!(snap.failed_images.len(), 1);
        assert_eq!(snap.failed_images[0].name, "bogus:tag");

        jobs.shutdown().await;
    }

    #[tokio::test]
    async fn partial_success_yields_done_state() {
        let (jobs, registry) = build_jobs();
        // Pre-register one of the two images so it becomes already_present.
        registry.insert("img:1".to_string(), vec![PathBuf::from("/a")]);
        let ack = jobs.submit(vec!["img:1".to_string(), "bogus:tag".to_string()]);
        assert_eq!(ack.already_present, vec!["img:1".to_string()]);
        assert_eq!(ack.queued, vec!["bogus:tag".to_string()]);

        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let snap = jobs.status(&ack.job_id).expect("job should exist");
            if snap.state != ImageJobState::Running {
                break;
            }
        }
        let snap = jobs.status(&ack.job_id).expect("job should exist");
        // already_present counts as success, failed counts as failure,
        // partial → terminal Done.
        assert_eq!(snap.state, ImageJobState::Done);
        assert!(matches!(
            snap.per_image.get("img:1"),
            Some(ImageProgress::AlreadyPresent)
        ));
        assert!(matches!(
            snap.per_image.get("bogus:tag"),
            Some(ImageProgress::Failed { .. })
        ));

        jobs.shutdown().await;
    }

    #[tokio::test]
    async fn unknown_job_returns_none() {
        let (jobs, _) = build_jobs();
        assert!(jobs.status("does-not-exist").is_none());
        jobs.shutdown().await;
    }

    #[tokio::test]
    async fn duplicate_images_in_request_are_collapsed() {
        let (jobs, _) = build_jobs();
        let ack = jobs.submit(vec![
            "bogus:tag".to_string(),
            "bogus:tag".to_string(),
            "bogus:tag".to_string(),
        ]);
        assert_eq!(ack.queued.len(), 1);
        jobs.shutdown().await;
    }
}
