//! Counts the open waiting requests of each client.
//!
//! A waiting request (a long-poll, `/await`, or a `/link` request) takes one of
//! the shared slots for waiting IDs. Without a limit per client, one client
//! could take them all, and every request for a new ID would get `503`.
//!
//! The count follows the request, not the waiter map: a [`WaitPermit`] lives as
//! long as the request and gives its slot back when dropped, whether the request
//! got a response, timed out, or its client disconnected. The waiter map only
//! notices a disconnected client at its next prune, so counting its entries
//! would block a reconnecting client with its own dead requests.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
};

use crate::client_ip::ClientNetwork;

/// The number of open waiting requests of each client, at most
/// `max_per_client`.
#[derive(Clone)]
pub(crate) struct WaitingClients {
    /// Only clients with at least one open waiting request have an entry.
    counts: Arc<Mutex<HashMap<ClientNetwork, usize>>>,
    max_per_client: usize,
}

impl WaitingClients {
    pub fn new(max_per_client: usize) -> Self {
        Self {
            counts: Arc::default(),
            max_per_client,
        }
    }

    /// Counts one more waiting request from `client`, until the returned permit
    /// is dropped. Returns `None` if the client already has `max_per_client`.
    pub fn try_start(&self, client: ClientNetwork) -> Option<WaitPermit> {
        let mut counts = self.lock_counts();
        let count = counts.entry(client).or_insert(0);
        if *count >= self.max_per_client {
            return None;
        }
        *count += 1;
        Some(WaitPermit {
            clients: self.clone(),
            client,
        })
    }

    /// Locks the counts, also after a panic elsewhere: every update leaves them
    /// consistent, and a permit must be able to give its slot back.
    fn lock_counts(&self) -> std::sync::MutexGuard<'_, HashMap<ClientNetwork, usize>> {
        self.counts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One open waiting request. Dropping it gives the client's slot back.
pub(crate) struct WaitPermit {
    clients: WaitingClients,
    client: ClientNetwork,
}

impl Drop for WaitPermit {
    fn drop(&mut self) {
        let mut counts = self.clients.lock_counts();
        if let Some(count) = counts.get_mut(&self.client) {
            *count -= 1;
            if *count == 0 {
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
        let clients = WaitingClients::new(2);
        let _first = clients.try_start(alice()).unwrap();
        let _second = clients.try_start(alice()).unwrap();

        assert!(clients.try_start(alice()).is_none());
    }

    #[test]
    fn dropped_permit_frees_its_slot() {
        let clients = WaitingClients::new(1);
        drop(clients.try_start(alice()).unwrap());

        assert!(clients.try_start(alice()).is_some());
    }

    #[test]
    fn clients_are_counted_separately() {
        let clients = WaitingClients::new(1);
        let _alice = clients.try_start(alice()).unwrap();

        assert!(clients.try_start(bob()).is_some());
    }

    #[test]
    fn clients_without_permits_are_forgotten() {
        let clients = WaitingClients::new(1);
        drop(clients.try_start(alice()).unwrap());

        assert!(clients.lock_counts().is_empty());
    }
}
