//! In-memory storage for relay entries (when `persist` feature is disabled).
//!
//! Provides a HashMap-based storage with LRU eviction. Data is lost on restart.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Mutex;

use super::types::StoredEntry;
use super::unix_timestamp_millis;

/// Default maximum number of entries before LRU eviction kicks in.
#[allow(dead_code)]
pub const DEFAULT_MAX_ENTRIES: usize = 10_000;

/// Internal entry with creation timestamp for LRU eviction.
struct Entry {
    message_body: Option<Vec<u8>>,
    content_type: Option<String>,
    acked: bool,
    expires_at: i64,
    created_at: i64,
}

/// In-memory repository for relay entries.
///
/// Thread-safe via internal Mutex. Uses LRU eviction based on created_at.
pub struct EntryRepository {
    entries: Mutex<HashMap<String, Entry>>,
    max_entries: usize,
}

impl EntryRepository {
    /// Creates a new in-memory repository.
    ///
    /// # Arguments
    /// * `_path` - Ignored (for API compatibility with SQLite version)
    /// * `max_entries` - Maximum entries before LRU eviction
    pub fn new(_path: Option<&std::path::Path>, max_entries: usize) -> Result<Self> {
        Ok(Self {
            entries: Mutex::new(HashMap::new()),
            max_entries,
        })
    }

    /// Inserts or replaces an entry.
    ///
    /// If the entry count exceeds `max_entries`, the oldest entry (by created_at)
    /// is deleted first (LRU eviction).
    pub fn insert(
        &self,
        id: &str,
        body: &[u8],
        content_type: Option<&str>,
        expires_at: i64,
    ) -> Result<()> {
        let mut entries = self.entries.lock().expect("Mutex poisoned");
        let created_at = unix_timestamp_millis();

        // LRU eviction if at capacity and not updating existing entry
        if entries.len() >= self.max_entries && !entries.contains_key(id) {
            if let Some(oldest_id) = entries
                .iter()
                .min_by_key(|(_, e)| e.created_at)
                .map(|(k, _)| k.clone())
            {
                entries.remove(&oldest_id);
            }
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

        Ok(())
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
        let current_time = unix_timestamp_millis();

        let expired: Vec<String> = entries
            .iter()
            .filter(|(_, e)| e.expires_at < current_time)
            .map(|(k, _)| k.clone())
            .collect();

        let count = expired.len();
        for id in expired {
            entries.remove(&id);
        }

        Ok(count)
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
    fn test_lru_eviction() {
        let repository = EntryRepository::new(None, 3).expect("Failed to create repository");
        let expires_at = unix_timestamp_millis() + 3_600_000;

        repository
            .insert("id1", b"body1", None, expires_at)
            .unwrap();
        repository
            .insert("id2", b"body2", None, expires_at)
            .unwrap();
        repository
            .insert("id3", b"body3", None, expires_at)
            .unwrap();

        assert_eq!(repository.count().unwrap(), 3);

        // Insert fourth entry, should evict oldest
        repository
            .insert("id4", b"body4", None, expires_at)
            .unwrap();

        assert_eq!(repository.count().unwrap(), 3);
        assert!(repository.get("id4").unwrap().is_some());
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
}
