use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use veilid_core::{VeilidAPI, VeilidConfig, VeilidUpdate, api_startup};

use crate::error::{CoreError, CoreResult};

/// Poll interval while waiting for the node to attach to the network.
const ATTACH_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Node storage and identity settings.
#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub program_name: String,
    pub namespace: String,
    /// Override for the veilid table/protected store location.
    /// If not set, a per-program folder inside the OS temp dir is used.
    pub storage_dir: Option<PathBuf>,
}

impl NodeConfig {
    pub fn new(program_name: &str) -> Self {
        Self {
            program_name: program_name.to_string(),
            namespace: String::new(),
            storage_dir: None,
        }
    }

    /// Default storage dir, isolated per program name in the OS temp dir.
    pub fn default_storage_dir(&self) -> PathBuf {
        std::env::temp_dir().join(&self.program_name)
    }
}

/// A running veilid node handle with a buffered update stream.
pub struct Node {
    api: VeilidAPI,
    updates: Option<mpsc::UnboundedReceiver<VeilidUpdate>>,
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node")
            .field("program", &"mesh_taxi")
            .finish()
    }
}

impl Node {
    /// Starts the veilid core and attaches to the network.
    pub async fn start(config: NodeConfig) -> CoreResult<Node> {
        let storage_dir = config
            .storage_dir
            .clone()
            .unwrap_or_else(|| config.default_storage_dir());

        let mut veilid_config = VeilidConfig::new(
            &config.program_name,
            "MeshTaxi",
            "dev.mesh_taxi",
            Some(storage_dir.to_string_lossy().as_ref()),
            None,
        );
        veilid_config.namespace = config.namespace.clone();

        let (tx, updates) = mpsc::unbounded_channel::<VeilidUpdate>();
        let update_callback: veilid_core::UpdateCallback = Arc::new(move |update: VeilidUpdate| {
            let _ = tx.send(update);
        });

        let api = api_startup(update_callback, veilid_config).await?;
        api.attach().await?;

        Ok(Node {
            api,
            updates: Some(updates),
        })
    }

    pub fn api(&self) -> &VeilidAPI {
        &self.api
    }

    /// Drains the node update stream (attachment states, app messages /
    /// calls over imported routes, value changes, ...).
    ///
    /// The returned receiver keeps collecting updates for the node lifetime.
    pub fn take_update_stream(&mut self) -> Option<mpsc::UnboundedReceiver<VeilidUpdate>> {
        self.updates.take()
    }

    pub async fn routing_context(&self) -> CoreResult<veilid_core::RoutingContext> {
        Ok(self.api.routing_context()?)
    }

    /// Waits until the node is attached and the public internet routing
    /// domain is ready for network operations.
    pub async fn wait_network_ready(&self, timeout: Duration) -> CoreResult<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let state = self.api.get_state().await?;
            if state.attachment.public_internet_ready && state.attachment.state.is_attached() {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(CoreError::Timeout(timeout));
            }
            tokio::time::sleep(ATTACH_POLL_INTERVAL).await;
        }
    }

    /// Graceful detach and shutdown of the node.
    pub async fn shutdown(self) {
        self.api.detach().await.ok();
        tokio::time::sleep(Duration::from_millis(100)).await;
        self.api.shutdown().await;
    }
}
