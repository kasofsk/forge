use axum::{
    response::Html,
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

mod api;
mod config;
mod consumer;
mod coord;
mod dispatcher;
mod error;
pub mod events;
mod forgejo;
mod graph;
mod monitor;
mod registry;
use config::SidecarConfig;
use coord::Coordinator;
use dashmap::DashMap;
use dispatcher::Dispatcher;
use events::EventSender;
use forgejo::ForgejoClient;
use graph::TaskGraph;
use registry::FactoryRegistry;
use workflow_types::{Job, JobTransition, JournalEntry, WorkerInfo};

/// Shared application state threaded through all axum handlers.
pub struct AppState {
    pub graph: TaskGraph,
    pub coord: Coordinator,
    /// Sync identity — labels, deps, claim management.
    pub forgejo: ForgejoClient,
    /// Dispatcher identity — assignees, comments.
    pub dispatcher_forgejo: ForgejoClient,
    pub config: SidecarConfig,
    pub registry: Arc<FactoryRegistry>,
    /// Dispatcher's worker registry — shared so the API can read it.
    pub dispatch_registry: Arc<DashMap<String, WorkerInfo>>,
    /// Pending help requests: job_key → worker_id. Set when a worker requests help;
    /// cleared when a human responds (response forwarded to the worker).
    pub pending_helps: DashMap<String, String>,
    /// Broadcast channel for SSE events to the graph viewer.
    pub event_tx: EventSender,
}

impl AppState {
    /// Publish a job transition to NATS and emit to SSE listeners.
    pub async fn publish_transition(&self, event: &JobTransition) {
        self.coord.publish_transition(event).await;
        events::emit(
            &self.event_tx,
            events::SseEvent::JobUpdate {
                job: event.job.clone(),
            },
        );
    }

    /// Emit a job update to SSE listeners (without publishing a NATS transition).
    pub fn emit_job(&self, job: &Job) {
        events::emit(
            &self.event_tx,
            events::SseEvent::JobUpdate { job: job.clone() },
        );
    }

    /// Emit a worker update event to SSE listeners.
    pub fn emit_worker(&self, info: &WorkerInfo) {
        events::emit(
            &self.event_tx,
            events::SseEvent::WorkerUpdate {
                worker: info.clone(),
            },
        );
    }

    /// Emit a worker removed event to SSE listeners.
    pub fn emit_worker_removed(&self, worker_id: &str) {
        events::emit(
            &self.event_tx,
            events::SseEvent::WorkerRemoved {
                worker_id: worker_id.to_string(),
            },
        );
    }

    pub async fn journal(
        &self,
        action: &str,
        comment: &str,
        job_key: Option<&str>,
        worker_id: Option<&str>,
    ) {
        let entry = JournalEntry {
            timestamp: chrono::Utc::now(),
            action: action.to_string(),
            comment: comment.to_string(),
            job_key: job_key.map(|s| s.to_string()),
            worker_id: worker_id.map(|s| s.to_string()),
        };
        self.coord.append_journal(&entry).await;
        events::emit(
            &self.event_tx,
            events::SseEvent::JournalEntry {
                entry: entry.clone(),
            },
        );
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let config = SidecarConfig::from_env()?;

    let graph = TaskGraph::open(&config.db_path)?;
    let coord = Coordinator::new(&config.nats.url, config.recording_ttl_secs).await?;
    let forgejo = ForgejoClient::new(&config.forgejo.url, &config.forgejo.token);
    let dispatcher_forgejo = ForgejoClient::new(
        &config.dispatcher.forgejo_url,
        &config.dispatcher.forgejo_token,
    );
    let registry = Arc::new(FactoryRegistry::new());

    let dispatch_registry = Arc::new(DashMap::new());
    let (event_tx, _) = events::channel();
    let state = Arc::new(AppState {
        graph,
        coord,
        forgejo,
        dispatcher_forgejo,
        config,
        registry,
        dispatch_registry,
        pending_helps: DashMap::new(),
        event_tx,
    });

    // Start dispatcher (subscribes to NATS events independently)
    let dispatcher = Arc::new(Dispatcher::new(Arc::clone(&state)));
    Arc::clone(&dispatcher).start().await?;

    // Start background tasks
    tokio::spawn(monitor::run_monitor(Arc::clone(&state)));
    tokio::spawn(watch_sessions(Arc::clone(&state)));
    consumer::start(Arc::clone(&state)).await?;
    Arc::clone(&state.registry).start(Arc::clone(&state)).await;

    let app = build_router(Arc::clone(&state));

    let addr: std::net::SocketAddr = state.config.listen_addr.parse()?;
    tracing::info!("listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/// Watch the `workflow-sessions` NATS KV bucket for changes from interactive
/// workers and emit SSE events so the graph viewer updates in real time.
async fn watch_sessions(state: Arc<AppState>) {
    use async_nats::jetstream::kv;
    use futures::StreamExt;

    let mut watcher = match state.coord.watch_sessions().await {
        Ok(w) => w,
        Err(e) => {
            tracing::error!("failed to start session watcher: {e:#}");
            return;
        }
    };

    while let Some(entry) = watcher.next().await {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("session watch error: {e:#}");
                continue;
            }
        };

        match entry.operation {
            kv::Operation::Put => {
                if let Ok(info) =
                    serde_json::from_slice::<workflow_types::SessionInfo>(&entry.value)
                {
                    events::emit(
                        &state.event_tx,
                        events::SseEvent::SessionUpdate {
                            session: info,
                        },
                    );
                }
            }
            kv::Operation::Delete | kv::Operation::Purge => {
                // Key is NATS-safe (`owner.repo.123`), convert back to job key.
                let job_key = entry.key.replace('.', "/");
                events::emit(
                    &state.event_tx,
                    events::SseEvent::SessionRemoved { job_key },
                );
            }
        }
    }
}

fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        // Job discovery
        .route("/jobs", get(api::list_jobs))
        .route("/jobs/available", get(api::available_jobs))
        .route("/jobs/:owner/:repo/:number", get(api::get_job))
        .route("/jobs/:owner/:repo/:number/deps", get(api::get_deps))
        // Factory observability
        .route("/factories", get(api::list_factories))
        // Users
        .route("/repos/:owner/:repo/users", get(api::list_users))
        // Labels & issue creation
        .route("/repos/:owner/:repo/labels", get(api::list_labels))
        .route("/repos/:owner/:repo/issues", post(api::create_issue))
        // Session recordings
        .route("/sessions/:owner/:repo/:number/:session_id/output", get(api::get_session_recording))
        // Interactive session control
        .route("/interact/attach", post(api::attach_session))
        .route("/interact/detach", post(api::detach_session))
        // SSE event stream for graph viewer
        .route("/events", get(api::sse_events))
        // Graph viewer
        .route(
            "/graph",
            get(|| async { Html(include_str!("../static/graph.html")) }),
        )
        // Terminal wrapper (embeds ttyd in a styled frame)
        .route(
            "/terminal",
            get(|| async { Html(include_str!("../static/terminal.html")) }),
        )
        .with_state(state)
        .layer(
            tower_http::cors::CorsLayer::very_permissive(),
        )
}
