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
//! The `persist` feature selects the SQLite backend; without it, entries live
//! in a `HashMap` and are lost on restart. Both backends have the same
//! interface, and the tests in this module run against whichever is compiled.

use std::time::Duration;

use bytes::Bytes;

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

/// Result of [`EntryRepository::insert`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InsertOutcome {
    /// The message is stored. `removed_ids` lists the expired and ACKed
    /// entries deleted to make room.
    Stored { removed_ids: Vec<String> },
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

/// Creates an empty in-memory repository of the compiled backend.
#[cfg(test)]
pub(crate) fn test_repository(max_entries: usize) -> EntryRepository {
    #[cfg(feature = "persist")]
    return EntryRepository::open(None, max_entries).expect("in-memory SQLite opens");
    #[cfg(not(feature = "persist"))]
    return EntryRepository::new(max_entries);
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

    #[test]
    fn stored_message_is_returned() {
        let mut repository = test_repository(10);

        repository.insert("id", &message("body"), TTL).unwrap();

        assert_eq!(repository.get("id").unwrap(), unacked("body"));
        assert_eq!(repository.get("other").unwrap(), None);
    }

    #[test]
    fn storing_again_replaces_the_message_and_its_ack() {
        let mut repository = test_repository(10);
        repository.insert("id", &message("first"), TTL).unwrap();
        repository.ack("id").unwrap();

        repository.insert("id", &message("second"), TTL).unwrap();

        assert_eq!(repository.get("id").unwrap(), unacked("second"));
    }

    #[test]
    fn ack_deletes_the_body() {
        let mut repository = test_repository(10);
        repository.insert("id", &message("body"), TTL).unwrap();

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
            .insert("id", &message("body"), Duration::ZERO)
            .unwrap();

        assert_eq!(repository.get("id").unwrap(), None);
        assert!(!repository.ack("id").unwrap());
    }

    #[test]
    fn remove_expired_returns_only_expired_ids() {
        let mut repository = test_repository(10);
        repository
            .insert("expired", &message("old"), Duration::ZERO)
            .unwrap();
        repository.insert("live", &message("new"), TTL).unwrap();

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
            assert_eq!(repository.insert(id, &message(id), TTL).unwrap(), stored());
        }

        let outcome = repository.insert("id4", &message("id4"), TTL).unwrap();

        assert_eq!(outcome, InsertOutcome::StoreFull);
        assert_eq!(repository.get("id4").unwrap(), None);
        for id in ["id1", "id2", "id3"] {
            assert_eq!(repository.get(id).unwrap(), unacked(id));
        }
    }

    #[test]
    fn full_store_removes_expired_entries_first() {
        let mut repository = test_repository(3);
        repository.insert("acked", &message("acked"), TTL).unwrap();
        repository.ack("acked").unwrap();
        repository
            .insert("expired", &message("old"), Duration::ZERO)
            .unwrap();
        repository.insert("live", &message("live"), TTL).unwrap();

        let outcome = repository.insert("new", &message("new"), TTL).unwrap();

        assert_eq!(outcome, removed(&["expired"]));
        assert_eq!(repository.get("acked").unwrap(), Some(StoredEntry::Acked));
        assert_eq!(repository.get("new").unwrap(), unacked("new"));
    }

    #[test]
    fn full_store_removes_the_oldest_acked_entry() {
        let mut repository = test_repository(3);
        for id in ["unacked", "older-acked", "newer-acked"] {
            repository.insert(id, &message(id), TTL).unwrap();
        }
        repository.ack("older-acked").unwrap();
        repository.ack("newer-acked").unwrap();

        let outcome = repository.insert("new", &message("new"), TTL).unwrap();

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
            repository.insert(id, &message(id), TTL).unwrap();
        }

        let outcome = repository.insert("A", &message("updated"), TTL).unwrap();

        assert_eq!(outcome, stored());
        assert_eq!(repository.get("A").unwrap(), unacked("updated"));
        assert_eq!(repository.get("B").unwrap(), unacked("B"));
        assert_eq!(repository.get("C").unwrap(), unacked("C"));
    }
}
