use anyhow::{Context, Result};
use async_nats::jetstream::{self, kv, object_store};
use bytes::Bytes;
use futures::StreamExt;
use workflow_types::{ClaimState, JobTransition, JournalEntry, SessionInfo};

const CLAIMS_BUCKET: &str = "workflow-claims";
const JOURNAL_BUCKET: &str = "workflow-dispatch-journal";
/// 7-day TTL for journal entries.
const JOURNAL_TTL_SECS: u64 = 7 * 86_400;
pub const SESSIONS_BUCKET: &str = "workflow-sessions";
const RECORDINGS_BUCKET: &str = "workflow-session-recordings";
const REWORKS_BUCKET: &str = "workflow-pending-reworks";
const BLACKLIST_BUCKET: &str = "workflow-abandon-blacklist";
/// 1-hour TTL for abandon blacklist entries.
const BLACKLIST_TTL_SECS: u64 = 3_600;

pub struct Coordinator {
    kv_claims: kv::Store,
    kv_journal: kv::Store,
    kv_sessions: kv::Store,
    kv_reworks: kv::Store,
    kv_blacklist: kv::Store,
    obj_recordings: object_store::ObjectStore,
    nats: async_nats::Client,
}

impl Coordinator {
    pub async fn new(nats_url: &str, recording_ttl_secs: u64) -> Result<Self> {
        let client = async_nats::connect(nats_url)
            .await
            .context("connect to NATS")?;
        let js = jetstream::new(client.clone());

        let kv_claims = js
            .create_key_value(kv::Config {
                bucket: CLAIMS_BUCKET.to_string(),
                history: 1,
                ..Default::default()
            })
            .await
            .context("create workflow-claims KV bucket")?;

        let kv_journal = js
            .create_key_value(kv::Config {
                bucket: JOURNAL_BUCKET.to_string(),
                history: 1,
                max_age: std::time::Duration::from_secs(JOURNAL_TTL_SECS),
                ..Default::default()
            })
            .await
            .context("create workflow-dispatch-journal KV bucket")?;

        let kv_sessions = js
            .create_key_value(kv::Config {
                bucket: SESSIONS_BUCKET.to_string(),
                history: 1,
                ..Default::default()
            })
            .await
            .context("create workflow-sessions KV bucket")?;

        let kv_reworks = js
            .create_key_value(kv::Config {
                bucket: REWORKS_BUCKET.to_string(),
                history: 1,
                ..Default::default()
            })
            .await
            .context("create workflow-pending-reworks KV bucket")?;

        let kv_blacklist = js
            .create_key_value(kv::Config {
                bucket: BLACKLIST_BUCKET.to_string(),
                history: 1,
                max_age: std::time::Duration::from_secs(BLACKLIST_TTL_SECS),
                ..Default::default()
            })
            .await
            .context("create workflow-abandon-blacklist KV bucket")?;

        let mut recordings_config = object_store::Config {
            bucket: RECORDINGS_BUCKET.to_string(),
            ..Default::default()
        };
        if recording_ttl_secs > 0 {
            recordings_config.max_age = std::time::Duration::from_secs(recording_ttl_secs);
        }
        let obj_recordings = js
            .create_object_store(recordings_config)
            .await
            .context("create workflow-session-recordings object store")?;

        Ok(Self {
            kv_claims,
            kv_journal,
            kv_sessions,
            kv_reworks,
            kv_blacklist,
            obj_recordings,
            nats: client,
        })
    }

    /// Returns a reference to the raw NATS client for publishing events.
    pub fn nats_client(&self) -> &async_nats::Client {
        &self.nats
    }

    // ── Pending reworks (NATS KV) ───────────────────────────────────────────

    /// Queue a rework for a job to be assigned to a specific worker when it becomes idle.
    pub async fn put_pending_rework(&self, job_key: &str, worker_id: &str) -> Result<()> {
        let nats_key = job_key.replace('/', ".");
        self.kv_reworks
            .put(&nats_key, Bytes::from(worker_id.to_string()))
            .await
            .context("put pending rework")?;
        Ok(())
    }

    /// Get and remove a pending rework for a job. Returns the preferred worker_id.
    pub async fn take_pending_rework(&self, job_key: &str) -> Result<Option<String>> {
        let nats_key = job_key.replace('/', ".");
        match self.kv_reworks.get(&nats_key).await {
            Ok(Some(bytes)) => {
                let worker_id = String::from_utf8_lossy(&bytes).to_string();
                let _ = self.kv_reworks.purge(&nats_key).await;
                Ok(Some(worker_id))
            }
            _ => Ok(None),
        }
    }

    /// Remove a pending rework entry (e.g. when the worker yields after completing rework).
    pub async fn remove_pending_rework(&self, job_key: &str) -> Result<()> {
        let nats_key = job_key.replace('/', ".");
        let _ = self.kv_reworks.purge(&nats_key).await;
        Ok(())
    }

    /// Check if a pending rework exists for a job.
    pub async fn has_pending_rework(&self, job_key: &str) -> bool {
        let nats_key = job_key.replace('/', ".");
        matches!(self.kv_reworks.get(&nats_key).await, Ok(Some(_)))
    }

    // ── Abandon blacklist (NATS KV with TTL) ──────────────────────────────

    /// Add a (job_key, worker_id) pair to the abandon blacklist.
    /// Entries auto-expire after 1 hour via bucket-level TTL.
    pub async fn blacklist_abandon(&self, job_key: &str, worker_id: &str) -> Result<()> {
        // Key: "job_key::worker_id" with slashes replaced by dots.
        let nats_key = format!("{}.{}", job_key.replace('/', "."), worker_id);
        self.kv_blacklist
            .put(&nats_key, Bytes::from("1"))
            .await
            .context("put abandon blacklist entry")?;
        Ok(())
    }

    /// Check if a (job_key, worker_id) pair is blacklisted.
    pub async fn is_blacklisted(&self, job_key: &str, worker_id: &str) -> bool {
        let nats_key = format!("{}.{}", job_key.replace('/', "."), worker_id);
        matches!(self.kv_blacklist.get(&nats_key).await, Ok(Some(_)))
    }

    // ── Session recordings (Object Store) ──────────────────────────────────

    /// Store a session recording in NATS Object Store.
    pub async fn put_recording(&self, key: &str, data: Bytes) -> Result<()> {
        use tokio::io::AsyncReadExt;
        let mut reader = &data[..];
        self.obj_recordings
            .put(key, &mut reader)
            .await
            .context("put session recording")?;
        Ok(())
    }

    /// Retrieve a session recording from NATS Object Store.
    pub async fn get_recording(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let mut result = match self.obj_recordings.get(key).await {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        result.read_to_end(&mut buf).await.context("read recording")?;
        Ok(Some(buf))
    }

    // ── Claim operations ─────────────────────────────────────────────────────

    /// Attempt an exclusive claim using NATS KV CAS semantics.
    ///
    /// Returns `Some(ClaimState)` on success, `None` if already claimed.
    pub async fn try_claim(
        &self,
        key: &str,
        worker_id: String,
        timeout_secs: u64,
        lease_secs: u64,
    ) -> Result<Option<ClaimState>> {
        let claim = ClaimState::new(worker_id, timeout_secs, lease_secs);
        let bytes: Bytes = serde_json::to_vec(&claim)?.into();

        match self.kv_claims.entry(key).await? {
            // No prior entry at all — use create (CAS: last revision = 0)
            None => match self.kv_claims.create(key, bytes).await {
                Ok(_) => Ok(Some(claim)),
                Err(_) => Ok(None),
            },
            // Tombstone from a previous release — overwrite it with a CAS update
            Some(entry)
                if entry.operation == kv::Operation::Delete
                    || entry.operation == kv::Operation::Purge =>
            {
                match self.kv_claims.update(key, bytes, entry.revision).await {
                    Ok(_) => Ok(Some(claim)),
                    Err(_) => Ok(None),
                }
            }
            // Active claim already exists
            Some(_) => Ok(None),
        }
    }

    /// Update the heartbeat timestamp for an active claim (CAS update).
    ///
    /// Returns `true` if the heartbeat was recorded, `false` if the claim no
    /// longer belongs to `worker_id` or doesn't exist.
    pub async fn heartbeat(&self, key: &str, worker_id: &str) -> Result<bool> {
        let entry = match self.kv_claims.entry(key).await? {
            Some(e) if e.operation == kv::Operation::Put => e,
            _ => return Ok(false),
        };

        let mut claim: ClaimState = serde_json::from_slice(&entry.value)?;
        if claim.worker_id != worker_id {
            return Ok(false);
        }

        claim.renew_lease();
        let bytes: Bytes = serde_json::to_vec(&claim)?.into();

        match self.kv_claims.update(key, bytes, entry.revision).await {
            Ok(_) => Ok(true),
            // Revision conflict means a concurrent heartbeat won the race;
            // the claim is still valid so we treat this as success.
            Err(_) => Ok(true),
        }
    }

    /// Release a claim (soft-delete the KV entry).
    pub async fn release(&self, key: &str) -> Result<()> {
        self.kv_claims.delete(key).await.context("release claim")?;
        Ok(())
    }

    /// Get the current claim for a job, if any.
    pub async fn get_claim(&self, key: &str) -> Result<Option<ClaimState>> {
        match self.kv_claims.entry(key).await? {
            Some(entry) if entry.operation == kv::Operation::Put => {
                let claim = serde_json::from_slice(&entry.value)?;
                Ok(Some(claim))
            }
            _ => Ok(None),
        }
    }

    /// Iterate all active claims. Used by the timeout monitor.
    pub async fn all_claims(&self) -> Result<Vec<(String, ClaimState)>> {
        let mut keys_stream = self.kv_claims.keys().await?;
        let mut result = Vec::new();
        while let Some(key_result) = keys_stream.next().await {
            let key = key_result?;
            if let Ok(Some(claim)) = self.get_claim(&key).await {
                result.push((key, claim));
            }
        }
        Ok(result)
    }

    // ── NATS pub/sub ─────────────────────────────────────────────────────────

    /// Publish a webhook event to `workflow.events.issue.{action}`.
    pub async fn publish_event(&self, action: &str, payload: Bytes) -> Result<()> {
        let subject = format!("workflow.events.issue.{action}");
        self.nats
            .publish(subject, payload)
            .await
            .context("publish NATS event")?;
        Ok(())
    }

    // ── Journal persistence ────────────────────────────────────────────────

    /// Append a journal entry to NATS KV. Errors are logged, not propagated.
    pub async fn append_journal(&self, entry: &JournalEntry) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let ts = entry.timestamp.timestamp_millis();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let key = format!("{ts}.{seq}");

        match serde_json::to_vec(entry) {
            Ok(payload) => {
                if let Err(e) = self.kv_journal.put(&key, Bytes::from(payload)).await {
                    tracing::warn!(key, error = %e, "failed to persist journal entry");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to serialize journal entry");
            }
        }
    }

    /// Read recent journal entries from NATS KV, sorted newest-first.
    pub async fn list_journal(&self, limit: usize) -> Vec<JournalEntry> {
        let mut entries = Vec::new();

        let keys = match self.kv_journal.keys().await {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(error = %e, "failed to list journal keys");
                return entries;
            }
        };

        let mut all_keys: Vec<String> = Vec::new();
        let mut keys = keys;
        while let Some(Ok(key)) = keys.next().await {
            all_keys.push(key);
        }

        // Keys are "{timestamp_millis}.{seq}" — reverse sort gives newest first.
        let mut sorted_keys = all_keys;
        sorted_keys.sort_unstable_by(|a, b| b.cmp(a));
        sorted_keys.truncate(limit);

        for key in &sorted_keys {
            match self.kv_journal.entry(key).await {
                Ok(Some(entry)) if entry.operation == kv::Operation::Put => {
                    match serde_json::from_slice::<JournalEntry>(&entry.value) {
                        Ok(je) => entries.push(je),
                        Err(e) => tracing::warn!(key, error = %e, "bad journal entry"),
                    }
                }
                _ => {}
            }
        }

        entries
    }

    /// Fire-and-forget publish of a state-transition event.
    ///
    /// Used by the consumer, API handlers, and monitor so that the
    /// dispatcher (and future reactors) can react to state changes
    /// without coupling to the mutation site.
    pub async fn publish_transition(&self, event: &JobTransition) {
        match serde_json::to_vec(event) {
            Ok(payload) => {
                if let Err(e) = self
                    .nats
                    .publish("workflow.jobs.transition", Bytes::from(payload))
                    .await
                {
                    tracing::warn!(
                        job = %event.job.key(),
                        error = %e,
                        "failed to publish transition event"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    job = %event.job.key(),
                    error = %e,
                    "failed to serialize transition event"
                );
            }
        }
    }

    // ── Session registry ─────────────────────────────────────────────────────

    /// Register an active interactive session in the KV store.
    /// Key is the NATS-safe job key (`owner.repo.123`).
    pub async fn put_session(&self, nats_key: &str, info: &SessionInfo) -> Result<()> {
        let bytes = Bytes::from(serde_json::to_vec(info)?);
        self.kv_sessions
            .put(nats_key, bytes)
            .await
            .context("put session")?;
        Ok(())
    }

    /// Remove a session from the registry when the worker finishes.
    pub async fn delete_session(&self, nats_key: &str) -> Result<()> {
        self.kv_sessions
            .purge(nats_key)
            .await
            .context("delete session")?;
        Ok(())
    }

    /// Watch for session changes. Returns a stream of KV entries.
    pub async fn watch_sessions(&self) -> Result<async_nats::jetstream::kv::Watch> {
        self.kv_sessions
            .watch_all()
            .await
            .context("watch sessions KV")
    }

    /// List all active sessions.
    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let mut keys_stream = self.kv_sessions.keys().await?;
        let mut result = Vec::new();
        while let Some(key_result) = keys_stream.next().await {
            let key = key_result?;
            if let Some(entry) = self.kv_sessions.entry(&key).await? {
                if entry.operation == kv::Operation::Put {
                    if let Ok(info) = serde_json::from_slice::<SessionInfo>(&entry.value) {
                        result.push(info);
                    }
                }
            }
        }
        Ok(result)
    }
}
