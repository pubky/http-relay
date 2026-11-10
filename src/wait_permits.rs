//! Limits how many waiting requests each client can have open.
//!
//! A waiting request is a long-poll `GET`, an `/await`, or a `/link` request.
//! Each one holds a [`WaitPermit`] for as long as it waits. Without this limit,
//! a few clients could take every slot for waiting IDs, and every other
//! client's request for a new ID would get `503`.
//!
//! The permit counts the request itself, not its entry in the waiter map. The
//! request drops its permit when it ends: with a response, a timeout, or a
//! disconnect. The waiter map, in contrast, keeps a disconnected request until
//! its next prune, so a client that reconnects would be blocked by its own
//! earlier requests.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use crate::client_ip::ClientNetwork;

/// Hands out at most `max_per_client` permits to each client at a time.
///
/// Cloning gives another handle to the same counts.
#[derive(Clone)]
pub(crate) struct WaitPermits {
    /// Open permits per client. Clients without open permits have no entry,
    /// so the map doesn't grow with every client ever seen.
    counts: Arc<Mutex<HashMap<ClientNetwork, usize>>>,
    max_per_client: usize,
}

impl WaitPermits {
    pub fn new(max_per_client: usize) -> Self {
        Self {
            counts: Arc::default(),
            max_per_client,
        }
    }

    /// Returns a permit for one more waiting request from `client`, or `None`
    /// if the client already holds `max_per_client` permits.
    pub fn try_acquire(&self, client: ClientNetwork) -> Option<WaitPermit> {
        let mut counts = self.lock_counts();
        let open = counts.get(&client).copied().unwrap_or(0);
        if open >= self.max_per_client {
            return None;
        }
        counts.insert(client, open + 1);
        Some(WaitPermit {
            permits: self.clone(),
            client,
        })
    }

    /// Locks the counts, even if a thread panicked while holding the lock:
    /// every update leaves the counts consistent, and a dropped permit must
    /// always be able to give its slot back.
    fn lock_counts(&self) -> MutexGuard<'_, HashMap<ClientNetwork, usize>> {
        self.counts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One open waiting request of a client. Dropping it frees the client's slot.
pub(crate) struct WaitPermit {
    permits: WaitPermits,
    client: ClientNetwork,
}

impl Drop for WaitPermit {
    fn drop(&mut self) {
        let mut counts = self.permits.lock_counts();
        if let Some(open) = counts.get_mut(&self.client) {
            *open -= 1;
            if *open == 0 {
                counts.remove(&self.client);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_client;

    fn alice() -> ClientNetwork {
        test_client("198.51.100.1")
    }

    fn bob() -> ClientNetwork {
        test_client("198.51.100.2")
    }

    #[test]
    fn client_gets_at_most_max_permits() {
        let permits = WaitPermits::new(2);
        let _first = permits.try_acquire(alice()).unwrap();
        let _second = permits.try_acquire(alice()).unwrap();

        assert!(permits.try_acquire(alice()).is_none());
    }

    #[test]
    fn dropped_permit_frees_its_slot() {
        let permits = WaitPermits::new(1);
        drop(permits.try_acquire(alice()).unwrap());

        assert!(permits.try_acquire(alice()).is_some());
    }

    #[test]
    fn clients_are_counted_separately() {
        let permits = WaitPermits::new(1);
        let _alice = permits.try_acquire(alice()).unwrap();

        assert!(permits.try_acquire(bob()).is_some());
    }

    #[test]
    fn clients_without_open_permits_are_forgotten() {
        let permits = WaitPermits::new(1);
        drop(permits.try_acquire(alice()).unwrap());

        assert!(permits.lock_counts().is_empty());
    }

    #[test]
    fn rejected_client_is_not_remembered() {
        let permits = WaitPermits::new(0);

        assert!(permits.try_acquire(alice()).is_none());
        assert!(permits.lock_counts().is_empty());
    }
}
