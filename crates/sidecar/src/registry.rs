use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;
use workflow_types::FactoryStatus;

use crate::AppState;

// ── WorkFactory trait ─────────────────────────────────────────────────────────

/// A work factory registered with the sidecar.
pub trait WorkFactory: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn poll_interval_secs(&self) -> Option<u64>;
    fn nats_subjects(&self) -> Vec<String> {
        vec![]
    }

    fn poll(
        self: Arc<Self>,
        state: Arc<AppState>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'static>>;

    fn on_event(
        self: Arc<Self>,
        _subject: String,
        _payload: Vec<u8>,
        state: Arc<AppState>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'static>> {
        Box::pin(async move {
            let _ = state;
            Ok(())
        })
    }
}

// ── FactoryRegistry ───────────────────────────────────────────────────────────

struct FactoryEntry {
    factory: Arc<dyn WorkFactory>,
    last_poll: Option<DateTime<Utc>>,
    last_error: Option<String>,
    enabled: bool,
}

pub struct FactoryRegistry {
    entries: Mutex<HashMap<String, FactoryEntry>>,
}

impl FactoryRegistry {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub async fn register(&self, factory: impl WorkFactory) {
        let name = factory.name().to_string();
        let entry = FactoryEntry {
            factory: Arc::new(factory),
            last_poll: None,
            last_error: None,
            enabled: true,
        };
        self.entries.lock().await.insert(name, entry);
    }

    /// Spawn background poll loops and NATS subscriptions for all registered factories.
    pub async fn start(self: Arc<Self>, app_state: Arc<AppState>) {
        let factories: Vec<(String, Arc<dyn WorkFactory>, Option<u64>, Vec<String>)> = {
            let entries = self.entries.lock().await;
            entries
                .values()
                .filter(|e| e.enabled)
                .map(|e| {
                    (
                        e.factory.name().to_string(),
                        Arc::clone(&e.factory),
                        e.factory.poll_interval_secs(),
                        e.factory.nats_subjects(),
                    )
                })
                .collect()
        };

        for (name, factory, interval_secs, subjects) in factories {
            // Poll loop
            if let Some(secs) = interval_secs {
                let registry = Arc::clone(&self);
                let name_c = name.clone();
                let factory_c = Arc::clone(&factory);
                let state = Arc::clone(&app_state);
                tokio::spawn(async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(secs));
                    loop {
                        interval.tick().await;
                        registry
                            .run_factory(&name_c, Arc::clone(&factory_c), Arc::clone(&state))
                            .await;
                    }
                });
            }

            // NATS event subscriptions
            for subject in subjects {
                let registry = Arc::clone(&self);
                let name_c = name.clone();
                let factory_c = Arc::clone(&factory);
                let state = Arc::clone(&app_state);
                let nats = app_state.coord.nats_client().clone();
                let subj = subject.clone();
                tokio::spawn(async move {
                    match nats.subscribe(subj.clone()).await {
                        Ok(mut sub) => {
                            while let Some(msg) = futures::StreamExt::next(&mut sub).await {
                                let payload = msg.payload.to_vec();
                                let factory_c = Arc::clone(&factory_c);
                                let state_c = Arc::clone(&state);
                                let result =
                                    factory_c.on_event(subj.clone(), payload, state_c).await;
                                if let Err(e) = result {
                                    tracing::error!(
                                        factory = name_c,
                                        subject = subj,
                                        "on_event error: {e:#}"
                                    );
                                }
                                // Update last_poll timestamp on event triggers
                                let mut entries = registry.entries.lock().await;
                                if let Some(entry) = entries.get_mut(&name_c) {
                                    entry.last_poll = Some(Utc::now());
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                factory = name_c,
                                subject = subj,
                                "NATS subscribe failed: {e:#}"
                            );
                        }
                    }
                });
            }
        }
    }

    /// Manually trigger a factory poll (used by `POST /factories/:name/poll`).
    pub async fn poll_factory(
        self: &Arc<Self>,
        name: &str,
        state: Arc<AppState>,
    ) -> anyhow::Result<()> {
        let factory = {
            let entries = self.entries.lock().await;
            entries.get(name).map(|e| Arc::clone(&e.factory))
        };
        match factory {
            Some(f) => {
                self.run_factory(name, f, state).await;
                Ok(())
            }
            None => anyhow::bail!("factory not found: {name}"),
        }
    }

    pub async fn list_factories(&self) -> Vec<FactoryStatus> {
        let entries = self.entries.lock().await;
        entries
            .values()
            .map(|e| FactoryStatus {
                name: e.factory.name().to_string(),
                enabled: e.enabled,
                poll_interval_secs: e.factory.poll_interval_secs(),
                last_poll: e.last_poll,
                last_error: e.last_error.clone(),
            })
            .collect()
    }

    async fn run_factory(&self, name: &str, factory: Arc<dyn WorkFactory>, state: Arc<AppState>) {
        let result = factory.poll(state).await;
        let mut entries = self.entries.lock().await;
        if let Some(entry) = entries.get_mut(name) {
            entry.last_poll = Some(Utc::now());
            entry.last_error = result.err().map(|e| {
                tracing::error!(factory = name, "poll error: {e:#}");
                e.to_string()
            });
        }
    }
}

impl Default for FactoryRegistry {
    fn default() -> Self {
        Self::new()
    }
}
