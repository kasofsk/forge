use anyhow::Result;
use async_trait::async_trait;

use crate::client::SidecarClient;
use crate::forgejo::ForgejoClient;

/// Trait for external work factory processes.
///
/// A factory inspects current job state via the sidecar API and creates new
/// Forgejo issues when appropriate. Factories do **not** claim or execute
/// jobs — that is exclusively for [`crate::worker::Worker`] implementors.
///
/// Each factory supports two composable trigger modes:
/// - **Poll**: runs on a configured interval (implement `poll_interval_secs`)
/// - **Event**: subscribes to NATS subjects (implement `nats_subjects` + `on_event`)
///
/// For in-process factories co-deployed inside the sidecar, use the
/// `WorkFactory` trait in `workflow-sidecar::registry` instead (which has
/// direct access to the sidecar's internal state rather than going through
/// HTTP).
#[async_trait]
pub trait WorkFactory: Send + Sync {
    /// Unique name for this factory (used in the `/factories` API).
    fn name(&self) -> &str;

    /// How often to call `poll`, in seconds. `None` = no poll loop.
    fn poll_interval_secs(&self) -> Option<u64> {
        None
    }

    /// NATS subjects this factory subscribes to.
    /// Patterns like `workflow.events.issue.*` are supported.
    fn nats_subjects(&self) -> Vec<String> {
        vec![]
    }

    /// Called on each poll interval.
    async fn poll(&self, sidecar: &SidecarClient, forgejo: &ForgejoClient) -> Result<()>;

    /// Called when a message arrives on one of `nats_subjects`.
    async fn on_event(
        &self,
        subject: &str,
        payload: &[u8],
        sidecar: &SidecarClient,
        forgejo: &ForgejoClient,
    ) -> Result<()> {
        let _ = (subject, payload, sidecar, forgejo);
        Ok(())
    }
}

/// Runner for external factory processes.
///
/// Handles the poll-loop lifecycle for a set of registered factories.
pub struct FactoryRunner {
    factories: Vec<Box<dyn WorkFactory>>,
    sidecar: SidecarClient,
    forgejo: ForgejoClient,
}

impl FactoryRunner {
    pub fn new(sidecar_url: &str, forgejo_url: &str, forgejo_token: &str) -> Self {
        Self {
            factories: vec![],
            sidecar: SidecarClient::new(sidecar_url),
            forgejo: ForgejoClient::new(forgejo_url, forgejo_token),
        }
    }

    pub fn register(mut self, factory: impl WorkFactory + 'static) -> Self {
        self.factories.push(Box::new(factory));
        self
    }

    /// Spawn a tokio interval task for each poll-based factory.
    pub async fn start(self: std::sync::Arc<Self>) {
        for (i, factory) in self.factories.iter().enumerate() {
            if let Some(interval_secs) = factory.poll_interval_secs() {
                let runner = std::sync::Arc::clone(&self);
                tokio::spawn(async move {
                    let mut interval =
                        tokio::time::interval(std::time::Duration::from_secs(interval_secs));
                    loop {
                        interval.tick().await;
                        if let Some(f) = runner.factories.get(i) {
                            if let Err(e) = f.poll(&runner.sidecar, &runner.forgejo).await {
                                tracing::error!(factory = f.name(), "poll error: {e:#}");
                            }
                        }
                    }
                });
            }
        }
    }
}
