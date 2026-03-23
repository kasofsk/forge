use anyhow::{Context, Result};
use std::env;

pub struct SidecarConfig {
    pub forgejo: ForgejoConfig,
    pub dispatcher: DispatcherConfig,
    pub nats: NatsConfig,
    pub db_path: String,
    pub default_timeout_secs: u64,
    pub lease_secs: u64,
    pub heartbeat_interval_secs: u64,
    pub monitor_interval_secs: u64,
    pub worker_prune_secs: u64,
    pub listen_addr: String,
    /// TTL in seconds for session recordings in the object store. 0 = no TTL.
    pub recording_ttl_secs: u64,
}

pub struct ForgejoConfig {
    pub url: String,
    pub token: String,
}

pub struct DispatcherConfig {
    pub forgejo_url: String,
    pub forgejo_token: String,
}

pub struct NatsConfig {
    pub url: String,
}

impl SidecarConfig {
    pub fn from_env() -> Result<Self> {
        let forgejo_url = env::var("FORGEJO_URL").context("FORGEJO_URL required")?;

        Ok(Self {
            forgejo: ForgejoConfig {
                url: forgejo_url.clone(),
                token: env::var("FORGEJO_TOKEN").context("FORGEJO_TOKEN required")?,
            },
            dispatcher: DispatcherConfig {
                forgejo_url: env::var("DISPATCHER_FORGEJO_URL")
                    .unwrap_or_else(|_| forgejo_url.clone()),
                forgejo_token: env::var("DISPATCHER_FORGEJO_TOKEN")
                    .context("DISPATCHER_FORGEJO_TOKEN required")?,
            },
            nats: NatsConfig {
                url: env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string()),
            },
            db_path: env::var("DB_PATH").unwrap_or_else(|_| "./workflow.db".to_string()),
            default_timeout_secs: env_u64("DEFAULT_TIMEOUT_SECS", 3600),
            lease_secs: env_u64("LEASE_SECS", 60),
            heartbeat_interval_secs: env_u64("HEARTBEAT_INTERVAL_SECS", 10),
            monitor_interval_secs: env_u64("MONITOR_INTERVAL_SECS", 15),
            worker_prune_secs: env_u64("WORKER_PRUNE_SECS", 90),
            listen_addr: env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:3000".to_string()),
            recording_ttl_secs: env_u64("RECORDING_TTL_SECS", 0),
        })
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
