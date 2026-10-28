//! SQLite-based persistence for relay entries.
//!
//! Provides durable storage bounded by a maximum entry count. When the store is
//! full, only entries that no longer hold a message (expired or acknowledged)
//! are deleted to make room; otherwise new IDs are rejected. Supports both
//! file-based and in-memory SQLite.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

use super::types::{InsertOutcome, StoredEntry};
use super::unix_timestamp_millis;

/// Default maximum number of entries the store holds.
#[allow(dead_code)]
pub const DEFAULT_MAX_ENTRIES: usize = 10_000;

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
    /// * `max_entries` - Maximum number of stored entries (see [`Self::insert`])
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

    /// Inserts or replaces an entry in the database.
    ///
    /// Replacing an existing ID always succeeds. A new ID needs a free slot;
    /// when the store holds `max_entries`, room is made by deleting, in order:
    /// 1. expired entries,
    /// 2. the oldest acknowledged entries (they no longer hold a message).
    ///
    /// Entries with an undelivered, unexpired message are never deleted. If no
    /// room can be made, nothing is written and [`InsertOutcome::StoreFull`]
    /// is returned.
    ///
    /// # Arguments
    /// * `id` - Unique identifier for the entry
    /// * `body` - Message body bytes
    /// * `content_type` - Optional content type header
    /// * `expires_at` - Unix timestamp when entry expires
    ///
    /// # Errors
    /// Returns error if a database operation fails.
    pub fn insert(
        &self,
        id: &str,
        body: &[u8],
        content_type: Option<&str>,
        expires_at: i64,
    ) -> Result<InsertOutcome> {
        let connection = self.connection.lock().expect("Mutex poisoned");
        let created_at = unix_timestamp_millis();

        let is_new_id = !Self::contains(&connection, id)?;
        if is_new_id && !self.make_room(&connection)? {
            return Ok(InsertOutcome::StoreFull);
        }

        // Always reset acked=0: a new message requires a new acknowledgment from the consumer.
        // Previous ACKs were for previous messages and don't carry over to new content.
        connection
            .execute(
                "INSERT OR REPLACE INTO entries (id, message_body, content_type, acked, expires_at, created_at) VALUES (?1, ?2, ?3, 0, ?4, ?5)",
                rusqlite::params![id, body, content_type, expires_at, created_at],
            )
            .context("Failed to insert entry")?;

        Ok(InsertOutcome::Stored)
    }

    fn contains(connection: &Connection, id: &str) -> Result<bool> {
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM entries WHERE id = ?1)",
                [id],
                |row| row.get(0),
            )
            .context("Failed to check for existing entry")
    }

    /// Frees one slot if the store is full, deleting only entries that no
    /// longer hold a message. Returns whether a slot is available.
    fn make_room(&self, connection: &Connection) -> Result<bool> {
        let mut count = Self::count_entries(connection)?;
        if count < self.max_entries {
            return Ok(true);
        }

        count -= Self::delete_expired(connection)?;
        if count < self.max_entries {
            return Ok(true);
        }

        // More than one entry may be over the limit if `max_entries` was
        // lowered since the database file was written.
        let entries_over_limit = count + 1 - self.max_entries;
        count -= connection
            .execute(
                "DELETE FROM entries WHERE id IN (
                    SELECT id FROM entries WHERE acked = 1 ORDER BY created_at ASC LIMIT ?1
                )",
                [entries_over_limit],
            )
            .context("Failed to delete acknowledged entries")?;
        Ok(count < self.max_entries)
    }

    fn count_entries(connection: &Connection) -> Result<usize> {
        connection
            .query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
            .context("Failed to count entries")
    }

    fn delete_expired(connection: &Connection) -> Result<usize> {
        connection
            .execute(
                "DELETE FROM entries WHERE expires_at < ?1",
                [unix_timestamp_millis()],
            )
            .context("Failed to delete expired entries")
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
        Self::delete_expired(&connection)
    }

    /// Returns the total number of entries in the database.
    #[allow(dead_code)]
    pub fn count(&self) -> Result<usize> {
        let connection = self.connection.lock().expect("Mutex poisoned");
        Self::count_entries(&connection)
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
    fn test_ack_nonexistent() {
        let repository = create_test_repository();

        let was_acked = repository.ack("nonexistent").expect("Failed to ack");
        assert!(!was_acked);
    }

    #[test]
    fn test_delete() {
        let repository = create_test_repository();
        let expires_at = unix_timestamp_millis() + 3_600_000;

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

    #[test]
    fn test_count() {
        let repository = create_test_repository();
        let expires_at = unix_timestamp_millis() + 3_600_000;

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
    fn test_insert_or_replace() {
        let repository = create_test_repository();
        let expires_at = unix_timestamp_millis() + 3_600_000;

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
