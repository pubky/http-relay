//! In-memory storage for relay entries (when `persist` feature is disabled).
//!
//! Provides a HashMap-based storage bounded by a maximum entry count, with the
//! same capacity rules as the SQLite version. Data is lost on restart.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Mutex;

use super::types::{InsertOutcome, StoredEntry};
use super::unix_timestamp_millis;

/// Default maximum number of entries the store holds.
#[allow(dead_code)]
pub const DEFAULT_MAX_ENTRIES: usize = 10_000;

/// Internal entry with creation timestamp, used to pick the oldest acknowledged
/// entry when making room.
struct Entry {
    message_body: Option<Vec<u8>>,
    content_type: Option<String>,
    acked: bool,
    expires_at: i64,
    created_at: i64,
}

/// In-memory repository for relay entries.
///
/// Thread-safe via internal Mutex. Holds at most `max_entries` entries.
pub struct EntryRepository {
    entries: Mutex<HashMap<String, Entry>>,
    max_entries: usize,
}

impl EntryRepository {
    /// Creates a new in-memory repository.
    ///
    /// # Arguments
    /// * `_path` - Ignored (for API compatibility with SQLite version)
    /// * `max_entries` - Maximum number of stored entries (see [`Self::insert`])
    pub fn new(_path: Option<&std::path::Path>, max_entries: usize) -> Result<Self> {
        Ok(Self {
            entries: Mutex::new(HashMap::new()),
            max_entries,
        })
    }

    /// Inserts or replaces an entry.
    ///
    /// Replacing an existing ID always succeeds. A new ID needs a free slot;
    /// when the store holds `max_entries`, room is made by deleting, in order:
    /// 1. expired entries,
    /// 2. the oldest acknowledged entry (it no longer holds a message).
    ///
    /// Entries with an undelivered, unexpired message are never deleted. If no
    /// room can be made, nothing is written and [`InsertOutcome::StoreFull`]
    /// is returned.
    pub fn insert(
        &self,
        id: &str,
        body: &[u8],
        content_type: Option<&str>,
        expires_at: i64,
    ) -> Result<InsertOutcome> {
        let mut entries = self.entries.lock().expect("Mutex poisoned");
        let created_at = unix_timestamp_millis();

        let is_new_id = !entries.contains_key(id);
        if is_new_id && !self.make_room(&mut entries) {
            return Ok(InsertOutcome::StoreFull);
        }

        entries.insert(
            id.to_string(),
            Entry {
                message_body: Some(body.to_vec()),
                content_type: content_type.map(|s| s.to_string()),
                acked: false,
                expires_at,
                created_at,
            },
        );

        Ok(InsertOutcome::Stored)
    }

    /// Frees one slot if the store is full, deleting only entries that no
    /// longer hold a message. Returns whether a slot is available.
    fn make_room(&self, entries: &mut HashMap<String, Entry>) -> bool {
        if entries.len() < self.max_entries {
            return true;
        }

        Self::delete_expired(entries);
        if entries.len() < self.max_entries {
            return true;
        }

        let oldest_acked_id = entries
            .iter()
            .filter(|(_, entry)| entry.acked)
            .min_by_key(|(_, entry)| entry.created_at)
            .map(|(id, _)| id.clone());
        if let Some(id) = oldest_acked_id {
            entries.remove(&id);
        }
        entries.len() < self.max_entries
    }

    /// Deletes expired entries and returns how many were deleted.
    fn delete_expired(entries: &mut HashMap<String, Entry>) -> usize {
        let current_time = unix_timestamp_millis();
        let count_before = entries.len();
        entries.retain(|_, entry| entry.expires_at >= current_time);
        count_before - entries.len()
    }

    /// Retrieves an entry by ID.
    pub fn get(&self, id: &str) -> Result<Option<StoredEntry>> {
        let entries = self.entries.lock().expect("Mutex poisoned");

        Ok(entries.get(id).map(|e| StoredEntry {
            message_body: e.message_body.clone(),
            content_type: e.content_type.clone(),
            acked: e.acked,
            expires_at: e.expires_at,
        }))
    }

    /// Marks an entry as acknowledged and clears its message body.
    pub fn ack(&self, id: &str) -> Result<bool> {
        let mut entries = self.entries.lock().expect("Mutex poisoned");

        if let Some(entry) = entries.get_mut(id) {
            entry.acked = true;
            entry.message_body = None;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Deletes an entry by ID.
    #[allow(dead_code)]
    pub fn delete(&self, id: &str) -> Result<bool> {
        let mut entries = self.entries.lock().expect("Mutex poisoned");
        Ok(entries.remove(id).is_some())
    }

    /// Deletes all expired entries.
    pub fn cleanup_expired(&self) -> Result<usize> {
        let mut entries = self.entries.lock().expect("Mutex poisoned");
        Ok(Self::delete_expired(&mut entries))
    }

    /// Returns the total number of entries.
    #[allow(dead_code)]
    pub fn count(&self) -> Result<usize> {
        let entries = self.entries.lock().expect("Mutex poisoned");
        Ok(entries.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_repository() -> EntryRepository {
        EntryRepository::new(None, 100).expect("Failed to create test repository")
    }

    #[test]
    fn test_insert_and_get() {
        let repository = create_test_repository();
        let expires_at = unix_timestamp_millis() + 3_600_000;

        repository
            .insert("test-id", b"test body", Some("text/plain"), expires_at)
            .expect("Failed to insert");

        let entry = repository.get("test-id").expect("Failed to get").unwrap();
        assert_eq!(entry.message_body, Some(b"test body".to_vec()));
        assert_eq!(entry.content_type, Some("text/plain".to_string()));
        assert!(!entry.acked);
        assert_eq!(entry.expires_at, expires_at);
    }

    #[test]
    fn test_get_nonexistent() {
        let repository = create_test_repository();
        let entry = repository.get("nonexistent").expect("Failed to get");
        assert!(entry.is_none());
    }

    #[test]
    fn test_ack() {
        let repository = create_test_repository();
        let expires_at = unix_timestamp_millis() + 3_600_000;

        repository
            .insert("test-id", b"test body", Some("text/plain"), expires_at)
            .expect("Failed to insert");

        let was_acked = repository.ack("test-id").expect("Failed to ack");
        assert!(was_acked);

        let entry = repository.get("test-id").expect("Failed to get").unwrap();
        assert!(entry.acked);
        assert!(entry.message_body.is_none());
    }

    #[test]
    fn test_cleanup_expired() {
        let repository = create_test_repository();
        let past = unix_timestamp_millis() - 3_600_000;
        let future = unix_timestamp_millis() + 3_600_000;

        repository
            .insert("expired", b"old", None, past)
            .expect("Failed to insert");
        repository
            .insert("valid", b"new", None, future)
            .expect("Failed to insert");

        let deleted_count = repository.cleanup_expired().expect("Failed to cleanup");
        assert_eq!(deleted_count, 1);

        assert!(repository.get("expired").expect("Failed to get").is_none());
        assert!(repository.get("valid").expect("Failed to get").is_some());
    }

    /// Inserts `ids` in order with one millisecond between them, so their
    /// `created_at` order matches the slice order.
    fn insert_in_order(repository: &EntryRepository, ids: &[&str], expires_at: i64) {
        for id in ids {
            repository.insert(id, b"body", None, expires_at).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn test_insert_new_id_when_full_of_undelivered_messages_is_rejected() {
        let repository = EntryRepository::new(None, 3).expect("Failed to create repository");
        let expires_at = unix_timestamp_millis() + 3_600_000;
        insert_in_order(&repository, &["id1", "id2", "id3"], expires_at);

        let outcome = repository
            .insert("id4", b"body4", None, expires_at)
            .unwrap();

        assert_eq!(outcome, InsertOutcome::StoreFull);
        assert_eq!(repository.count().unwrap(), 3);
        assert!(repository.get("id4").unwrap().is_none());
        for id in ["id1", "id2", "id3"] {
            let entry = repository.get(id).unwrap().expect("entry should survive");
            assert_eq!(entry.message_body, Some(b"body".to_vec()));
        }
    }

    #[test]
    fn test_insert_when_full_deletes_expired_entry() {
        let repository = EntryRepository::new(None, 3).expect("Failed to create repository");
        let past = unix_timestamp_millis() - 3_600_000;
        let future = unix_timestamp_millis() + 3_600_000;
        insert_in_order(&repository, &["id1", "id2"], future);
        insert_in_order(&repository, &["expired"], past);

        let outcome = repository.insert("id4", b"body4", None, future).unwrap();

        assert_eq!(outcome, InsertOutcome::Stored);
        assert!(repository.get("expired").unwrap().is_none());
        for id in ["id1", "id2", "id4"] {
            assert!(repository.get(id).unwrap().is_some(), "{id} should exist");
        }
    }

    #[test]
    fn test_insert_when_full_deletes_oldest_acked_entry() {
        let repository = EntryRepository::new(None, 3).expect("Failed to create repository");
        let expires_at = unix_timestamp_millis() + 3_600_000;
        insert_in_order(
            &repository,
            &["unacked", "older-acked", "newer-acked"],
            expires_at,
        );
        repository.ack("older-acked").unwrap();
        repository.ack("newer-acked").unwrap();

        let outcome = repository
            .insert("id4", b"body4", None, expires_at)
            .unwrap();

        assert_eq!(outcome, InsertOutcome::Stored);
        assert!(repository.get("older-acked").unwrap().is_none());
        for id in ["unacked", "newer-acked", "id4"] {
            assert!(repository.get(id).unwrap().is_some(), "{id} should exist");
        }
    }

    #[test]
    fn test_insert_existing_id_when_full_replaces_without_deleting_others() {
        let repository = EntryRepository::new(None, 3).expect("Failed to create repository");
        let expires_at = unix_timestamp_millis() + 3_600_000;
        insert_in_order(&repository, &["A", "B", "C"], expires_at);

        let outcome = repository
            .insert("A", b"updated-A", None, expires_at)
            .unwrap();

        assert_eq!(outcome, InsertOutcome::Stored);
        assert_eq!(repository.count().unwrap(), 3);
        let entry_a = repository.get("A").unwrap().expect("A should exist");
        assert_eq!(entry_a.message_body, Some(b"updated-A".to_vec()));
        assert!(repository.get("B").unwrap().is_some(), "B should exist");
        assert!(repository.get("C").unwrap().is_some(), "C should exist");
    }
}
