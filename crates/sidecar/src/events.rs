//! Server-Sent Events (SSE) for the graph viewer.
//!
//! A broadcast channel carries all UI-relevant state changes. The SSE handler
//! sends a full snapshot on connect, then streams incremental events. If a
//! receiver falls behind (lagged), it gets a fresh snapshot instead of the
//! missed events.

use serde::Serialize;
use tokio::sync::broadcast;
use workflow_types::{Job, JournalEntry, SessionInfo, WorkerInfo};

/// Capacity of the broadcast channel. When a receiver falls behind by this
/// many events it receives a `Lagged` error and we resend a full snapshot.
const CHANNEL_CAPACITY: usize = 256;

/// A UI-relevant event sent over the SSE stream.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum SseEvent {
    /// A job changed state (or was first seen).
    #[serde(rename = "job_update")]
    JobUpdate { job: Job },

    /// A worker's registry entry changed.
    #[serde(rename = "worker_update")]
    WorkerUpdate { worker: WorkerInfo },

    /// A worker was pruned from the registry.
    #[serde(rename = "worker_removed")]
    WorkerRemoved { worker_id: String },

    /// A new dispatcher journal entry was recorded.
    #[serde(rename = "journal_entry")]
    JournalEntry { entry: JournalEntry },

    /// An interactive session was created or updated.
    #[serde(rename = "session_update")]
    SessionUpdate { session: SessionInfo },

    /// An interactive session was removed.
    #[serde(rename = "session_removed")]
    SessionRemoved { job_key: String },
}

pub type EventSender = broadcast::Sender<SseEvent>;

/// Create a new broadcast channel for SSE events.
pub fn channel() -> (EventSender, broadcast::Receiver<SseEvent>) {
    broadcast::channel(CHANNEL_CAPACITY)
}

/// Send an event, ignoring errors (no receivers connected).
pub fn emit(tx: &EventSender, event: SseEvent) {
    let _ = tx.send(event);
}
