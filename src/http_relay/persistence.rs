//! SQLite-based persistence for relay entries.
//!
//! Provides durable storage with automatic LRU eviction when the entry count
//! exceeds the configured maximum. Supports both file-based and in-memory SQLite.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Default maximum number of entries before LRU eviction kicks in.
#[allow(dead_code)]
pub const DEFAULT_MAX_ENTRIES: usize = 10_000;

/// A stored entry retrieved from the database.
pub struct StoredEntry {
    /// The message body (None if entry was acked and body cleared).
    pub message_body: Option<Vec<u8>>,
    /// The content type of the message.
    pub content_type: Option<String>,
    /// Whether the entry has been acknowledged.
    pub acked: bool,
    /// Unix timestamp when the entry expires.
    pub expires_at: i64,
}

/// Repository for persisting relay entries to SQLite.
///
/// Thread-safe via internal Mutex. Uses WAL mode for better concurrency.
pub struct EntryRepository {
    connection: Mutex<Connection>,
    max_entries: usize,
}

impl EntryRepository {
    /// Creates a new repository.
    ///
    /// # Arguments
    /// * `path` - File path for SQLite database, or None for in-memory database
    /// * `max_entries` - Maximum entries before LRU eviction (oldest by created_at)
    ///
    /// # Errors
    /// Returns error if database connection or schema creation fails.
    pub fn new(path: Option<&Path>, max_entries: usize) -> Result<Self> {
        let connection = match path {
            Some(path) => Connection::open(path).context("Failed to open SQLite database file")?,
            None => {
                Connection::open_in_memory().context("Failed to open in-memory SQLite database")?
            }
        };

        // Enable WAL mode for better concurrency (only for file-based databases)
        if path.is_some() {
            connection
                .execute_batch("PRAGMA journal_mode=WAL;")
                .context("Failed to enable WAL mode")?;
        }

        Self::create_schema(&connection)?;

        Ok(Self {
            connection: Mutex::new(connection),
            max_entries,
        })
    }

    /// Creates the database schema if it doesn't exist.
    fn create_schema(connection: &Connection) -> Result<()> {
        connection
            .execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS entries (
                    id TEXT PRIMARY KEY,
                    message_body BLOB,
                    content_type TEXT,
                    acked INTEGER DEFAULT 0,
                    expires_at INTEGER NOT NULL,
                    created_at INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_expires_at ON entries(expires_at);
                CREATE INDEX IF NOT EXISTS idx_created_at ON entries(created_at);
                "#,
            )
            .context("Failed to create database schema")?;
        Ok(())
    }

    /// Returns the current Unix timestamp in milliseconds.
    fn current_timestamp_millis() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System time before Unix epoch")
            .as_millis() as i64
    }

    /// Inserts or replaces an entry in the database.
    ///
    /// If the entry count exceeds `max_entries`, the oldest entry (by created_at)
    /// is deleted first to make room (LRU eviction).
    ///
    /// # Arguments
    /// * `id` - Unique identifier for the entry
    /// * `body` - Message body bytes
    /// * `content_type` - Optional content type header
    /// * `expires_at` - Unix timestamp when entry expires
    pub fn insert(
        &self,
        id: &str,
        body: &[u8],
        content_type: Option<&str>,
        expires_at: i64,
    ) -> Result<()> {
        let connection = self.connection.lock().expect("Mutex poisoned");
        let created_at = Self::current_timestamp_millis();

        // Check if we need to evict oldest entry (LRU eviction for disk overflow protection)
        let count: usize = connection
            .query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
            .context("Failed to count entries")?;

        if count >= self.max_entries {
            // Find the oldest entry's ID before evicting
            let oldest_id: Option<String> = connection
                .query_row(
                    "SELECT id FROM entries ORDER BY created_at ASC LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .ok();

            // Only evict if the oldest entry is NOT the one we're about to update.
            // If we're updating the oldest entry, INSERT OR REPLACE will handle it
            // without needing eviction (no net change in entry count).
            if let Some(ref oldest) = oldest_id {
                if oldest != id {
                    connection
                        .execute("DELETE FROM entries WHERE id = ?1", [oldest])
                        .context("Failed to delete oldest entry for LRU eviction")?;
                }
            }
        }

        // Always reset acked=0: a new message requires a new acknowledgment from the consumer.
        // Previous ACKs were for previous messages and don't carry over to new content.
        connection
            .execute(
                "INSERT OR REPLACE INTO entries (id, message_body, content_type, acked, expires_at, created_at) VALUES (?1, ?2, ?3, 0, ?4, ?5)",
                rusqlite::params![id, body, content_type, expires_at, created_at],
            )
            .context("Failed to insert entry")?;

        Ok(())
    }

    /// Retrieves an entry by ID.
    ///
    /// Returns None if the entry doesn't exist.
    pub fn get(&self, id: &str) -> Result<Option<StoredEntry>> {
        let connection = self.connection.lock().expect("Mutex poisoned");

        let result = connection.query_row(
            "SELECT message_body, content_type, acked, expires_at FROM entries WHERE id = ?1",
            [id],
            |row| {
                Ok(StoredEntry {
                    message_body: row.get(0)?,
                    content_type: row.get(1)?,
                    acked: row.get::<_, i64>(2)? != 0,
                    expires_at: row.get(3)?,
                })
            },
        );

        match result {
            Ok(entry) => Ok(Some(entry)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error).context("Failed to get entry"),
        }
    }

    /// Marks an entry as acknowledged and clears its message body.
    ///
    /// Returns true if an entry was updated, false if the entry didn't exist.
    pub fn ack(&self, id: &str) -> Result<bool> {
        let connection = self.connection.lock().expect("Mutex poisoned");

        let rows_affected = connection
            .execute(
                "UPDATE entries SET acked = 1, message_body = NULL WHERE id = ?1",
                [id],
            )
            .context("Failed to acknowledge entry")?;

        Ok(rows_affected > 0)
    }

    /// Deletes an entry by ID.
    ///
    /// Returns true if an entry was deleted, false if the entry didn't exist.
    #[allow(dead_code)]
    pub fn delete(&self, id: &str) -> Result<bool> {
        let connection = self.connection.lock().expect("Mutex poisoned");

        let rows_affected = connection
            .execute("DELETE FROM entries WHERE id = ?1", [id])
            .context("Failed to delete entry")?;

        Ok(rows_affected > 0)
    }

    /// Deletes all expired entries.
    ///
    /// Returns the number of entries deleted.
    pub fn cleanup_expired(&self) -> Result<usize> {
        let connection = self.connection.lock().expect("Mutex poisoned");
        let current_time = Self::current_timestamp_millis();

        let rows_deleted = connection
            .execute("DELETE FROM entries WHERE expires_at < ?1", [current_time])
            .context("Failed to cleanup expired entries")?;

        Ok(rows_deleted)
    }

    /// Returns the total number of entries in the database.
    #[allow(dead_code)]
    pub fn count(&self) -> Result<usize> {
        let connection = self.connection.lock().expect("Mutex poisoned");

        let count: usize = connection
            .query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
            .context("Failed to count entries")?;

        Ok(count)
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
        let expires_at = EntryRepository::current_timestamp_millis() + 3_600_000;

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
        let expires_at = EntryRepository::current_timestamp_millis() + 3_600_000;

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
    fn test_ack_nonexistent() {
        let repository = create_test_repository();

        let was_acked = repository.ack("nonexistent").expect("Failed to ack");
        assert!(!was_acked);
    }

    #[test]
    fn test_delete() {
        let repository = create_test_repository();
        let expires_at = EntryRepository::current_timestamp_millis() + 3_600_000;

        repository
            .insert("test-id", b"test body", None, expires_at)
            .expect("Failed to insert");

        let was_deleted = repository.delete("test-id").expect("Failed to delete");
        assert!(was_deleted);

        let entry = repository.get("test-id").expect("Failed to get");
        assert!(entry.is_none());
    }

    #[test]
    fn test_delete_nonexistent() {
        let repository = create_test_repository();

        let was_deleted = repository.delete("nonexistent").expect("Failed to delete");
        assert!(!was_deleted);
    }

    #[test]
    fn test_cleanup_expired() {
        let repository = create_test_repository();
        let past = EntryRepository::current_timestamp_millis() - 3_600_000;
        let future = EntryRepository::current_timestamp_millis() + 3_600_000;

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

    #[test]
    fn test_count() {
        let repository = create_test_repository();
        let expires_at = EntryRepository::current_timestamp_millis() + 3_600_000;

        assert_eq!(repository.count().expect("Failed to count"), 0);

        repository
            .insert("id1", b"body1", None, expires_at)
            .expect("Failed to insert");
        assert_eq!(repository.count().expect("Failed to count"), 1);

        repository
            .insert("id2", b"body2", None, expires_at)
            .expect("Failed to insert");
        assert_eq!(repository.count().expect("Failed to count"), 2);
    }

    #[test]
    fn test_lru_eviction() {
        let repository = EntryRepository::new(None, 3).expect("Failed to create repository");
        let expires_at = EntryRepository::current_timestamp_millis() + 3_600_000;

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

        // Insert fourth entry, should evict id1 (oldest)
        repository
            .insert("id4", b"body4", None, expires_at)
            .unwrap();

        assert_eq!(repository.count().unwrap(), 3);
        assert!(repository.get("id1").unwrap().is_none());
        assert!(repository.get("id2").unwrap().is_some());
        assert!(repository.get("id3").unwrap().is_some());
        assert!(repository.get("id4").unwrap().is_some());
    }

    #[test]
    fn test_insert_or_replace() {
        let repository = create_test_repository();
        let expires_at = EntryRepository::current_timestamp_millis() + 3_600_000;

        repository
            .insert("test-id", b"original", Some("text/plain"), expires_at)
            .expect("Failed to insert");

        repository
            .insert("test-id", b"replaced", Some("application/json"), expires_at)
            .expect("Failed to replace");

        let entry = repository.get("test-id").expect("Failed to get").unwrap();
        assert_eq!(entry.message_body, Some(b"replaced".to_vec()));
        assert_eq!(entry.content_type, Some("application/json".to_string()));
        assert_eq!(repository.count().unwrap(), 1);
    }

    #[test]
    fn test_lru_eviction_does_not_evict_updated_entry() {
        // Regression test: updating the oldest entry at capacity should NOT evict it
        let repository = EntryRepository::new(None, 3).expect("Failed to create repository");
        let expires_at = EntryRepository::current_timestamp_millis() + 3_600_000;

        // Fill repository to capacity: A is oldest, then B, then C
        repository
            .insert("A", b"original-A", None, expires_at)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1)); // Ensure distinct timestamps
        repository.insert("B", b"body-B", None, expires_at).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1));
        repository.insert("C", b"body-C", None, expires_at).unwrap();

        assert_eq!(repository.count().unwrap(), 3);

        // Update entry A (the oldest) with new data - should NOT evict A
        repository
            .insert("A", b"updated-A", None, expires_at)
            .unwrap();

        // All three entries should still exist
        assert_eq!(repository.count().unwrap(), 3);

        let entry_a = repository
            .get("A")
            .expect("Failed to get A")
            .expect("A should exist");
        assert_eq!(entry_a.message_body, Some(b"updated-A".to_vec()));

        assert!(
            repository.get("B").unwrap().is_some(),
            "B should still exist"
        );
        assert!(
            repository.get("C").unwrap().is_some(),
            "C should still exist"
        );
    }
}
