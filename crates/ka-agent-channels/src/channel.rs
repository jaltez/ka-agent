//! The channel abstraction (SEED.md §3.6, Phase 6): one inbound/outbound
//! adapter for one chat network. A channel owns its transport, gates
//! inbound identities against the store's `identities` table, and drives
//! the same runtime command/message paths as every other channel.

use std::future::Future;
use std::pin::Pin;
use tokio::sync::watch;

/// A channel adapter. `run` owns the transport until `shutdown` flips;
/// it must return promptly afterwards (the daemon waits on it).
pub trait Channel: Send + 'static {
    /// The channel's name, matching the identities table (`telegram`,
    /// `matrix`, …) and audit rows.
    fn name(&self) -> &'static str;

    /// Serve inbound traffic until shutdown.
    fn run(self, shutdown: watch::Receiver<bool>) -> Pin<Box<dyn Future<Output = ()> + Send>>
    where
        Self: Sized;
}
