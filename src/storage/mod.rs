//! Storage for relayed messages.
//!
//! Each ID has at most one entry: the latest message stored for it, until its
//! consumer ACKs it. An entry expires a TTL after it was stored. Expired
//! entries are never returned, even before [`EntryRepository::remove_expired`]
//! deletes them.
//!
//! The store holds at most `max_entries` entries. When it is full, storing a
//! new ID deletes entries that no longer hold a message: expired ones first,
//! then the ACKed ones stored longest ago. Entries with an unACKed message are never
//! deleted to make room; if there is no other way, the new ID is rejected.
//!
//! Because unACKed entries can't be deleted, one client could fill the store
//! and keep everyone else out. So each entry records its owner, the client
//! network that stored its message, and a client may own at most
//! `max_entries_per_client` live unACKed entries. Storing a message for an ID
//! makes the storing client its owner. Replacing an entry the client already
//! owns doesn't count against its limit.
//!
//! The `persist` feature selects the SQLite backend; without it, entries live
//! in a `HashMap` and are lost on restart. Both backends have the same
//! interface, and the tests in this module run against whichever is compiled.

use std::time::Duration;

use bytes::Bytes;

#[cfg(test)]
use crate::client_ip::{ClientIp, ClientNetwork};

#[cfg(not(feature = "persist"))]
mod memory;
#[cfg(feature = "persist")]
mod sqlite;

#[cfg(not(feature = "persist"))]
pub(crate) use memory::EntryRepository;
#[cfg(feature = "persist")]
pub(crate) use sqlite::EntryRepository;

/// A message from a producer, with the Content-Type it was sent with.
///
/// Uses `Bytes` for zero-copy cloning when fanning out to concurrent waiters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Message {
    pub body: Bytes,
    pub content_type: Option<String>,
}

/// A live (unexpired) entry.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StoredEntry {
    /// The message waits for its consumer.
    Unacked(Message),
    /// The consumer ACKed the message, and its body was deleted.
    Acked,
}

/// How many entries the store holds; see the module docs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Capacity {
    pub max_entries: usize,
    pub max_entries_per_client: usize,
}

/// Result of [`EntryRepository::insert`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InsertOutcome {
    /// The message is stored. `removed_ids` lists the expired and ACKed
    /// entries deleted to make room.
    Stored { removed_ids: Vec<String> },
    /// The client already owns `max_entries_per_client` other live unACKed
    /// entries. Nothing was changed.
    ClientLimitReached,
    /// Every entry holds an unACKed message, so there is no room for a new ID.
    /// Nothing was changed.
    StoreFull,
}

/// Returns the current Unix timestamp in milliseconds.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("System time before Unix epoch")
        .as_millis() as i64
}

/// Returns when an entry stored now with `ttl` expires, in Unix milliseconds.
fn expires_at(ttl: Duration) -> i64 {
    now_millis().saturating_add(i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX))
}

/// Creates an empty in-memory repository of the compiled backend, with no
/// limit per client.
#[cfg(test)]
pub(crate) fn test_repository(max_entries: usize) -> EntryRepository {
    test_repository_with_capacity(Capacity {
        max_entries,
        max_entries_per_client: usize::MAX,
    })
}

/// Creates an empty in-memory repository of the compiled backend.
#[cfg(all(test, feature = "persist"))]
pub(crate) fn test_repository_with_capacity(capacity: Capacity) -> EntryRepository {
    EntryRepository::open(None, capacity).expect("in-memory SQLite opens")
}

/// Creates an empty in-memory repository of the compiled backend.
#[cfg(all(test, not(feature = "persist")))]
pub(crate) fn test_repository_with_capacity(capacity: Capacity) -> EntryRepository {
    EntryRepository::new(capacity)
}

/// Returns the network of the client at `ip`.
#[cfg(test)]
pub(crate) fn test_client(ip: &str) -> ClientNetwork {
    ClientIp(ip.parse().expect("test IP addresses are valid")).network()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(60);

    fn message(body: &'static str) -> Message {
        Message {
            body: Bytes::from_static(body.as_bytes()),
            content_type: Some("text/plain".to_string()),
        }
    }

    fn unacked(body: &'static str) -> Option<StoredEntry> {
        Some(StoredEntry::Unacked(message(body)))
    }

    fn alice() -> ClientNetwork {
        test_client("198.51.100.1")
    }

    fn bob() -> ClientNetwork {
        test_client("198.51.100.2")
    }

    #[test]
    fn stored_message_is_returned() {
        let mut repository = test_repository(10);

        repository
            .insert("id", &message("body"), TTL, alice())
            .unwrap();

        assert_eq!(repository.get("id").unwrap(), unacked("body"));
        assert_eq!(repository.get("other").unwrap(), None);
    }

    #[test]
    fn storing_again_replaces_the_message_and_its_ack() {
        let mut repository = test_repository(10);
        repository
            .insert("id", &message("first"), TTL, alice())
            .unwrap();
        repository.ack("id").unwrap();

        repository
            .insert("id", &message("second"), TTL, alice())
            .unwrap();

        assert_eq!(repository.get("id").unwrap(), unacked("second"));
    }

    #[test]
    fn ack_deletes_the_body() {
        let mut repository = test_repository(10);
        repository
            .insert("id", &message("body"), TTL, alice())
            .unwrap();

        assert!(repository.ack("id").unwrap());

        assert_eq!(repository.get("id").unwrap(), Some(StoredEntry::Acked));
    }

    #[test]
    fn ack_of_missing_entry_fails() {
        let mut repository = test_repository(10);

        assert!(!repository.ack("missing").unwrap());
    }

    #[test]
    fn expired_entry_is_gone_before_it_is_removed() {
        let mut repository = test_repository(10);

        repository
            .insert("id", &message("body"), Duration::ZERO, alice())
            .unwrap();

        assert_eq!(repository.get("id").unwrap(), None);
        assert!(!repository.ack("id").unwrap());
    }

    #[test]
    fn remove_expired_returns_only_expired_ids() {
        let mut repository = test_repository(10);
        repository
            .insert("expired", &message("old"), Duration::ZERO, alice())
            .unwrap();
        repository
            .insert("live", &message("new"), TTL, alice())
            .unwrap();

        assert_eq!(repository.remove_expired().unwrap(), vec!["expired"]);
        assert_eq!(repository.remove_expired().unwrap(), Vec::<String>::new());
        assert_eq!(repository.get("live").unwrap(), unacked("new"));
    }

    fn stored() -> InsertOutcome {
        InsertOutcome::Stored {
            removed_ids: Vec::new(),
        }
    }

    fn removed(ids: &[&str]) -> InsertOutcome {
        InsertOutcome::Stored {
            removed_ids: ids.iter().map(|id| id.to_string()).collect(),
        }
    }

    #[test]
    fn full_store_rejects_a_new_id_and_keeps_unacked_messages() {
        let mut repository = test_repository(3);
        for id in ["id1", "id2", "id3"] {
            assert_eq!(
                repository.insert(id, &message(id), TTL, alice()).unwrap(),
                stored()
            );
        }

        let outcome = repository
            .insert("id4", &message("id4"), TTL, alice())
            .unwrap();

        assert_eq!(outcome, InsertOutcome::StoreFull);
        assert_eq!(repository.get("id4").unwrap(), None);
        for id in ["id1", "id2", "id3"] {
            assert_eq!(repository.get(id).unwrap(), unacked(id));
        }
    }

    #[test]
    fn full_store_removes_expired_entries_first() {
        let mut repository = test_repository(3);
        repository
            .insert("acked", &message("acked"), TTL, alice())
            .unwrap();
        repository.ack("acked").unwrap();
        repository
            .insert("expired", &message("old"), Duration::ZERO, alice())
            .unwrap();
        repository
            .insert("live", &message("live"), TTL, alice())
            .unwrap();

        let outcome = repository
            .insert("new", &message("new"), TTL, alice())
            .unwrap();

        assert_eq!(outcome, removed(&["expired"]));
        assert_eq!(repository.get("acked").unwrap(), Some(StoredEntry::Acked));
        assert_eq!(repository.get("new").unwrap(), unacked("new"));
    }

    #[test]
    fn full_store_removes_the_oldest_acked_entry() {
        let mut repository = test_repository(3);
        for id in ["unacked", "older-acked", "newer-acked"] {
            repository.insert(id, &message(id), TTL, alice()).unwrap();
        }
        repository.ack("older-acked").unwrap();
        repository.ack("newer-acked").unwrap();

        let outcome = repository
            .insert("new", &message("new"), TTL, alice())
            .unwrap();

        assert_eq!(outcome, removed(&["older-acked"]));
        assert_eq!(repository.get("unacked").unwrap(), unacked("unacked"));
        assert_eq!(
            repository.get("newer-acked").unwrap(),
            Some(StoredEntry::Acked)
        );
        assert_eq!(repository.get("new").unwrap(), unacked("new"));
    }

    #[test]
    fn replacing_an_entry_in_a_full_store_removes_nothing() {
        let mut repository = test_repository(3);
        for id in ["A", "B", "C"] {
            repository.insert(id, &message(id), TTL, alice()).unwrap();
        }

        let outcome = repository
            .insert("A", &message("updated"), TTL, alice())
            .unwrap();

        assert_eq!(outcome, stored());
        assert_eq!(repository.get("A").unwrap(), unacked("updated"));
        assert_eq!(repository.get("B").unwrap(), unacked("B"));
        assert_eq!(repository.get("C").unwrap(), unacked("C"));
    }

    // ========== Limit per client ==========

    /// Holds 10 entries, at most 2 of them unACKed per client.
    fn repository_with_limit_of_two() -> EntryRepository {
        test_repository_with_capacity(Capacity {
            max_entries: 10,
            max_entries_per_client: 2,
        })
    }

    #[test]
    fn client_at_its_limit_is_rejected_and_nothing_changes() {
        let mut repository = repository_with_limit_of_two();
        repository
            .insert("a1", &message("a1"), TTL, alice())
            .unwrap();
        repository
            .insert("a2", &message("a2"), TTL, alice())
            .unwrap();

        let outcome = repository
            .insert("a3", &message("a3"), TTL, alice())
            .unwrap();

        assert_eq!(outcome, InsertOutcome::ClientLimitReached);
        assert_eq!(repository.get("a3").unwrap(), None);
        assert_eq!(repository.get("a1").unwrap(), unacked("a1"));
    }

    #[test]
    fn other_client_is_not_affected_by_a_full_client() {
        let mut repository = repository_with_limit_of_two();
        repository
            .insert("a1", &message("a1"), TTL, alice())
            .unwrap();
        repository
            .insert("a2", &message("a2"), TTL, alice())
            .unwrap();

        let outcome = repository.insert("b1", &message("b1"), TTL, bob()).unwrap();

        assert_eq!(outcome, stored());
    }

    #[test]
    fn client_at_its_limit_can_replace_its_own_entry() {
        let mut repository = repository_with_limit_of_two();
        repository
            .insert("a1", &message("a1"), TTL, alice())
            .unwrap();
        repository
            .insert("a2", &message("a2"), TTL, alice())
            .unwrap();

        let outcome = repository
            .insert("a1", &message("updated"), TTL, alice())
            .unwrap();

        assert_eq!(outcome, stored());
        assert_eq!(repository.get("a1").unwrap(), unacked("updated"));
    }

    #[test]
    fn acked_and_expired_entries_do_not_count() {
        let mut repository = repository_with_limit_of_two();
        repository
            .insert("acked", &message("acked"), TTL, alice())
            .unwrap();
        repository.ack("acked").unwrap();
        repository
            .insert("expired", &message("old"), Duration::ZERO, alice())
            .unwrap();
        repository
            .insert("a1", &message("a1"), TTL, alice())
            .unwrap();

        let outcome = repository
            .insert("a2", &message("a2"), TTL, alice())
            .unwrap();

        assert_eq!(outcome, stored());
    }

    #[test]
    fn storing_under_another_clients_id_moves_it_to_your_count() {
        let mut repository = repository_with_limit_of_two();
        repository
            .insert("a1", &message("a1"), TTL, alice())
            .unwrap();
        repository
            .insert("a2", &message("a2"), TTL, alice())
            .unwrap();
        repository.insert("b1", &message("b1"), TTL, bob()).unwrap();

        repository
            .insert("a1", &message("bob"), TTL, bob())
            .unwrap();

        assert_eq!(
            repository
                .insert("a3", &message("a3"), TTL, alice())
                .unwrap(),
            stored()
        );
        assert_eq!(
            repository.insert("b2", &message("b2"), TTL, bob()).unwrap(),
            InsertOutcome::ClientLimitReached
        );
    }

    #[test]
    fn limit_above_max_entries_is_off() {
        let mut repository = test_repository_with_capacity(Capacity {
            max_entries: 2,
            max_entries_per_client: 3,
        });
        repository
            .insert("a1", &message("a1"), TTL, alice())
            .unwrap();
        repository
            .insert("a2", &message("a2"), TTL, alice())
            .unwrap();

        let outcome = repository
            .insert("a3", &message("a3"), TTL, alice())
            .unwrap();

        assert_eq!(outcome, InsertOutcome::StoreFull);
    }
}
