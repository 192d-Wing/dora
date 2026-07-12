//! Server operating mode.
//!
//! The mode is shared between the management API — which changes it in response
//! to the `maintenance-mode`, `drain`, and `shutdown` actions — and the DHCP
//! datapath, which reads it on every new-lease request to decide whether to
//! answer. Reads happen on the hot path, so the shared handle is a lock-free
//! [`AtomicU8`] rather than a mutex.

use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

use serde::{Deserialize, Serialize};

/// The server's operating mode, reported by `GET /v1/server`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerMode {
    /// Normal operation: every request is served.
    Normal,
    /// Maintenance: both new leases and renewals are suppressed — the server is
    /// fully out of service but still running.
    Maintenance,
    /// Drain: new leases are suppressed, but existing clients may still renew so
    /// their bindings age out gracefully.
    Drain,
    /// Shutting down: a graceful shutdown is in progress. New leases are
    /// suppressed (as in drain) until the process exits.
    ShuttingDown,
}

impl ServerMode {
    fn to_u8(self) -> u8 {
        match self {
            ServerMode::Normal => 0,
            ServerMode::Maintenance => 1,
            ServerMode::Drain => 2,
            ServerMode::ShuttingDown => 3,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => ServerMode::Maintenance,
            2 => ServerMode::Drain,
            3 => ServerMode::ShuttingDown,
            // 0 and any unexpected value fall back to Normal
            _ => ServerMode::Normal,
        }
    }

    /// The mode's stable snake_case string form, as persisted in `server_state`
    /// and reported by the API (matches the serde representation).
    pub fn as_str(self) -> &'static str {
        match self {
            ServerMode::Normal => "normal",
            ServerMode::Maintenance => "maintenance",
            ServerMode::Drain => "drain",
            ServerMode::ShuttingDown => "shutting_down",
        }
    }

    /// Parse a persisted mode string; any unknown value falls back to `Normal`
    /// (mirrors [`ServerMode::from_u8`]'s lenience).
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "maintenance" => ServerMode::Maintenance,
            "drain" => ServerMode::Drain,
            "shutting_down" => ServerMode::ShuttingDown,
            _ => ServerMode::Normal,
        }
    }

    /// Whether this mode suppresses brand-new lease acquisition — v4 `DISCOVER`
    /// and v6 `SOLICIT`. True for every non-normal mode.
    pub fn suppresses_new_leases(self) -> bool {
        !matches!(self, ServerMode::Normal)
    }

    /// Whether this mode additionally suppresses renewals of existing leases
    /// (v4 `REQUEST`, v6 `RENEW`/`REBIND`). Only `maintenance` takes the server
    /// fully out of service; `drain` and `shutting_down` keep renewals flowing.
    pub fn suppresses_renewals(self) -> bool {
        matches!(self, ServerMode::Maintenance)
    }
}

/// A cheap, cloneable, lock-free handle to the shared [`ServerMode`]. Cloning
/// shares the same underlying atomic, so a `set` from the management API is
/// immediately visible to the DHCP datapath.
#[derive(Debug, Clone)]
pub struct SharedMode(Arc<AtomicU8>);

impl SharedMode {
    /// Create a new shared mode initialized to `mode`.
    pub fn new(mode: ServerMode) -> Self {
        SharedMode(Arc::new(AtomicU8::new(mode.to_u8())))
    }

    /// Read the current mode.
    pub fn get(&self) -> ServerMode {
        ServerMode::from_u8(self.0.load(Ordering::Relaxed))
    }

    /// Set the current mode.
    pub fn set(&self, mode: ServerMode) {
        self.0.store(mode.to_u8(), Ordering::Relaxed);
    }
}

impl Default for SharedMode {
    fn default() -> Self {
        SharedMode::new(ServerMode::Normal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_through_shared_handle() {
        let mode = SharedMode::new(ServerMode::Normal);
        assert_eq!(mode.get(), ServerMode::Normal);
        mode.set(ServerMode::Drain);
        assert_eq!(mode.get(), ServerMode::Drain);
        // a clone shares the same atomic
        let clone = mode.clone();
        clone.set(ServerMode::Maintenance);
        assert_eq!(mode.get(), ServerMode::Maintenance);
    }

    #[test]
    fn enforcement_predicates() {
        assert!(!ServerMode::Normal.suppresses_new_leases());
        assert!(!ServerMode::Normal.suppresses_renewals());

        assert!(ServerMode::Drain.suppresses_new_leases());
        assert!(!ServerMode::Drain.suppresses_renewals());

        assert!(ServerMode::Maintenance.suppresses_new_leases());
        assert!(ServerMode::Maintenance.suppresses_renewals());

        assert!(ServerMode::ShuttingDown.suppresses_new_leases());
        assert!(!ServerMode::ShuttingDown.suppresses_renewals());
    }

    #[test]
    fn serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&ServerMode::ShuttingDown).unwrap(),
            "\"shutting_down\""
        );
    }
}
