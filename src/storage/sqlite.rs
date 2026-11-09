//! SQLite backend for relay entries.
//!
//! # Deleted message bodies
//!
//! ACKs and cleanup only remove bodies from the live table. By default, SQLite
//! marks the freed space as reusable and leaves the old bytes in place, so
//! `PRAGMA secure_delete=ON` overwrites them with zeros. This is best-effort
//! hygiene, not a guarantee: older page versions stay in the `-wal` file until
//! SQLite overwrites it, and copies outside SQLite's control (filesystem
//! blocks, SSD wear leveling, backups) remain possible. Clients that need
//! confidentiality must encrypt message bodies before sending them.
//!
//! # Entry owners
//!
//! The `owner` column holds a keyed hash of the owner's client network, not
//! the network itself, so client IP addresses never reach the disk. The key is
//! random and lives only in memory. After a restart, the hashes of entries
//! stored before it match no client, so those entries count against no
//! client's limit until they expire.

use std::{
    hash::{BuildHasher, RandomState},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result};
use bytes::Bytes;
use rusqlite::{Connection, OptionalExtension};

use super::{expires_at, now_millis, Capacity, InsertOutcome, Message, OwnedEntries, StoredEntry};
use crate::client_ip::ClientNetwork;

/// Entries in a file-based or in-memory SQLite database.
pub(crate) struct EntryRepository {
    connection: Connection,
    capacity: Capacity,
    /// Hashes owners for the `owner` column; see "Entry owners" in the module docs.
    owner_hasher: RandomState,
}

impl EntryRepository {
    /// Opens the database at `path`, or an in-memory database if `path` is
    /// `None`, and creates or upgrades the schema if needed.
    pub fn open(path: Option<&Path>, capacity: Capacity) -> Result<Self> {
        let connection = match path {
            Some(path) => Connection::open(path).context("Failed to open SQLite database file")?,
            None => {
                Connection::open_in_memory().context("Failed to open in-memory SQLite database")?
            }
        };

        // Per-connection setting; see "Deleted message bodies" in the module docs.
        connection
            .execute_batch("PRAGMA secure_delete=ON;")
            .context("Failed to enable secure delete")?;

        // In-memory databases don't support WAL.
        if path.is_some() {
            connection
                .execute_batch("PRAGMA journal_mode=WAL;")
                .context("Failed to enable WAL mode")?;
        }

        connection
            .execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS entries (
                    id TEXT PRIMARY KEY,
                    message_body BLOB,
                    content_type TEXT,
                    -- Same as `message_body IS NULL`; kept so that existing
                    -- database files keep working.
                    acked INTEGER DEFAULT 0,
                    expires_at INTEGER NOT NULL,
                    created_at INTEGER NOT NULL,
                    owner INTEGER
                );
                CREATE INDEX IF NOT EXISTS idx_expires_at ON entries(expires_at);
                CREATE INDEX IF NOT EXISTS idx_created_at ON entries(created_at);
                "#,
            )
            .context("Failed to create database schema")?;
        add_owner_column_if_missing(&connection)?;

        Ok(Self {
            connection,
            capacity,
            owner_hasher: RandomState::new(),
        })
    }

    /// Stores `message` for `id` with `owner` as its owner, replacing any
    /// previous entry and its ACK.
    ///
    /// The owner must be below its limit, and a new ID in a full store first
    /// needs room; see the module docs of [`super`]. Runs in one transaction:
    /// if it fails, no entry was deleted to make room.
    pub fn insert(
        &mut self,
        id: &str,
        message: &Message,
        ttl: Duration,
        owner: ClientNetwork,
    ) -> Result<InsertOutcome> {
        // SQLite integers are signed; the cast keeps all 64 bits of the hash.
        let owner_hash = self.owner_hasher.hash_one(owner) as i64;
        let transaction = self
            .connection
            .transaction()
            .context("Failed to begin insert transaction")?;

        let owned = transaction
            .query_row(
                "SELECT COUNT(*), MIN(expires_at) FROM entries WHERE owner = ?1 AND id != ?2 AND acked = 0 AND expires_at > ?3",
                rusqlite::params![owner_hash, id, now_millis()],
                |row| {
                    Ok(OwnedEntries {
                        count: row.get(0)?,
                        first_expires_at: row.get(1)?,
                    })
                },
            )
            .context("Failed to count the owner's entries")?;
        if owned.count >= self.capacity.max_entries_per_client {
            return Ok(owned.limit_reached());
        }

        let exists: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM entries WHERE id = ?1)",
                [id],
                |row| row.get(0),
            )
            .context("Failed to check for existing entry")?;
        let removed_ids = if exists {
            Vec::new()
        } else {
            match make_room(&transaction, self.capacity.max_entries)? {
                Some(removed_ids) => removed_ids,
                // Dropping the transaction rolls it back; nothing was changed.
                None => return Ok(InsertOutcome::StoreFull),
            }
        };

        transaction
            .execute(
                "INSERT OR REPLACE INTO entries (id, message_body, content_type, acked, expires_at, created_at, owner) VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6)",
                rusqlite::params![
                    id,
                    message.body.as_ref(),
                    message.content_type,
                    expires_at(ttl),
                    now_millis(),
                    owner_hash
                ],
            )
            .context("Failed to insert entry")?;
        transaction
            .commit()
            .context("Failed to commit insert transaction")?;

        Ok(InsertOutcome::Stored { removed_ids })
    }

    /// Returns the live entry for `id`.
    pub fn get(&self, id: &str) -> Result<Option<StoredEntry>> {
        self.connection
            .query_row(
                "SELECT message_body, content_type, acked FROM entries WHERE id = ?1 AND expires_at > ?2",
                rusqlite::params![id, now_millis()],
                |row| {
                    let body: Option<Vec<u8>> = row.get(0)?;
                    let acked: bool = row.get(2)?;
                    Ok(match body {
                        Some(body) if !acked => StoredEntry::Unacked(Message {
                            body: Bytes::from(body),
                            content_type: row.get(1)?,
                        }),
                        _ => StoredEntry::Acked,
                    })
                },
            )
            .optional()
            .context("Failed to get entry")
    }

    /// ACKs the live entry for `id` and deletes its body.
    ///
    /// Returns false if there is no live entry.
    pub fn ack(&mut self, id: &str) -> Result<bool> {
        let rows_affected = self
            .connection
            .execute(
                "UPDATE entries SET acked = 1, message_body = NULL WHERE id = ?1 AND expires_at > ?2",
                rusqlite::params![id, now_millis()],
            )
            .context("Failed to acknowledge entry")?;

        Ok(rows_affected > 0)
    }

    /// Deletes all expired entries and returns their IDs.
    pub fn remove_expired(&mut self) -> Result<Vec<String>> {
        delete_expired(&self.connection)
    }
}

/// Adds the `owner` column to a database file written before entries had
/// owners, and indexes it. Existing entries get no owner.
fn add_owner_column_if_missing(connection: &Connection) -> Result<()> {
    let has_owner: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('entries') WHERE name = 'owner')",
            [],
            |row| row.get(0),
        )
        .context("Failed to read the database schema")?;
    if !has_owner {
        connection
            .execute_batch("ALTER TABLE entries ADD COLUMN owner INTEGER;")
            .context("Failed to add the owner column")?;
    }
    connection
        .execute_batch("CREATE INDEX IF NOT EXISTS idx_owner ON entries(owner);")
        .context("Failed to index the owner column")
}

/// Deletes expired entries, then ACKed ones stored longest ago, until there is
/// room for one more. Returns their IDs, or `None` without deleting anything if
/// that isn't enough.
fn make_room(connection: &Connection, max_entries: usize) -> Result<Option<Vec<String>>> {
    let count: usize = connection
        .query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
        .context("Failed to count entries")?;
    // More than one entry is over the limit if `max_entries` was lowered
    // since the database file was written.
    let excess = (count + 1).saturating_sub(max_entries);
    if excess == 0 {
        return Ok(Some(Vec::new()));
    }

    // Only a full store pays for this scan: `acked` has no index.
    let removable: usize = connection
        .query_row(
            "SELECT COUNT(*) FROM entries WHERE expires_at <= ?1 OR acked = 1",
            [now_millis()],
            |row| row.get(0),
        )
        .context("Failed to count removable entries")?;
    if removable < excess {
        return Ok(None);
    }

    let mut removed_ids = delete_expired(connection)?;
    let acked_to_remove = excess.saturating_sub(removed_ids.len());
    if acked_to_remove > 0 {
        // `INSERT OR REPLACE` gives a replaced row a new rowid, so rowid
        // orders entries stored in the same millisecond.
        let mut statement = connection
            .prepare(
                "DELETE FROM entries WHERE id IN (
                    SELECT id FROM entries WHERE acked = 1 ORDER BY created_at, rowid LIMIT ?1
                ) RETURNING id",
            )
            .context("Failed to prepare deletion of ACKed entries")?;
        let acked_ids = statement
            .query_map([acked_to_remove], |row| row.get(0))
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<String>>>())
            .context("Failed to delete ACKed entries")?;
        removed_ids.extend(acked_ids);
    }
    Ok(Some(removed_ids))
}

/// Deletes all expired entries and returns their IDs.
fn delete_expired(connection: &Connection) -> Result<Vec<String>> {
    let mut statement = connection
        .prepare("DELETE FROM entries WHERE expires_at <= ?1 RETURNING id")
        .context("Failed to prepare cleanup of expired entries")?;
    statement
        .query_map([now_millis()], |row| row.get(0))
        .and_then(|rows| rows.collect::<rusqlite::Result<Vec<String>>>())
        .context("Failed to cleanup expired entries")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_client;

    const TTL: Duration = Duration::from_secs(60);

    /// Holds `max_entries` entries, with no limit per client.
    fn capacity(max_entries: usize) -> Capacity {
        Capacity {
            max_entries,
            max_entries_per_client: usize::MAX,
        }
    }

    fn alice() -> ClientNetwork {
        test_client("198.51.100.1")
    }

    #[test]
    fn test_secure_delete_is_enabled() {
        // Regression test for https://github.com/pubky/http-relay/issues/18
        let repository = EntryRepository::open(None, capacity(10)).unwrap();

        let secure_delete: i64 = repository
            .connection
            .query_row("PRAGMA secure_delete", [], |row| row.get(0))
            .unwrap();

        assert_eq!(secure_delete, 1);
    }

    fn message() -> Message {
        Message {
            body: Bytes::from_static(b"body"),
            content_type: None,
        }
    }

    #[test]
    fn failed_insert_keeps_the_entries_it_would_remove() {
        let mut repository = EntryRepository::open(None, capacity(1)).unwrap();
        repository
            .insert("acked", &message(), TTL, alice())
            .unwrap();
        repository.ack("acked").unwrap();
        // Makes the insert fail after `make_room` deleted "acked".
        repository
            .connection
            .execute_batch(
                "CREATE TRIGGER fail_insert BEFORE INSERT ON entries
                 WHEN NEW.id = 'fails' BEGIN SELECT RAISE(ABORT, 'insert failed'); END;",
            )
            .unwrap();

        let result = repository.insert("fails", &message(), TTL, alice());

        assert!(result.is_err());
        assert_eq!(repository.get("acked").unwrap(), Some(StoredEntry::Acked));
    }

    fn temp_directory(name: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "http-relay-test-{name}-{}-{}",
            std::process::id(),
            now_millis()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn entries_survive_reopening_the_database_file() {
        let directory = temp_directory("reopen");
        let path = directory.join("relay.db");
        let message = message();

        EntryRepository::open(Some(&path), capacity(10))
            .unwrap()
            .insert("id", &message, TTL, alice())
            .unwrap();
        let reopened = EntryRepository::open(Some(&path), capacity(10)).unwrap();

        assert_eq!(
            reopened.get("id").unwrap(),
            Some(StoredEntry::Unacked(message))
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn insert_over_a_lowered_limit_removes_enough_acked_entries() {
        let directory = temp_directory("lowered-limit");
        let path = directory.join("relay.db");
        let mut repository = EntryRepository::open(Some(&path), capacity(4)).unwrap();
        for id in ["acked1", "acked2", "acked3", "unacked"] {
            repository.insert(id, &message(), TTL, alice()).unwrap();
        }
        for id in ["acked1", "acked2", "acked3"] {
            repository.ack(id).unwrap();
        }
        drop(repository);
        let mut reopened = EntryRepository::open(Some(&path), capacity(2)).unwrap();

        let outcome = reopened.insert("new", &message(), TTL, alice()).unwrap();

        let InsertOutcome::Stored { mut removed_ids } = outcome else {
            panic!("expected the message to be stored");
        };
        removed_ids.sort();
        assert_eq!(removed_ids, ["acked1", "acked2", "acked3"]);
        assert!(reopened.get("unacked").unwrap().is_some());
        assert!(reopened.get("new").unwrap().is_some());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn owner_column_holds_no_client_address() {
        let mut repository = EntryRepository::open(None, capacity(10)).unwrap();

        repository.insert("id", &message(), TTL, alice()).unwrap();

        let owner_type: String = repository
            .connection
            .query_row("SELECT typeof(owner) FROM entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(owner_type, "integer");
    }

    #[test]
    fn database_file_without_owners_is_upgraded() {
        let directory = temp_directory("without-owners");
        let path = directory.join("relay.db");
        // The schema before entries had owners.
        Connection::open(&path)
            .unwrap()
            .execute_batch(&format!(
                "CREATE TABLE entries (
                    id TEXT PRIMARY KEY,
                    message_body BLOB,
                    content_type TEXT,
                    acked INTEGER DEFAULT 0,
                    expires_at INTEGER NOT NULL,
                    created_at INTEGER NOT NULL
                );
                INSERT INTO entries (id, message_body, acked, expires_at, created_at)
                    VALUES ('old', x'00', 0, {}, 0);",
                expires_at(TTL)
            ))
            .unwrap();
        let limit_of_one = Capacity {
            max_entries: 10,
            max_entries_per_client: 1,
        };

        let mut repository = EntryRepository::open(Some(&path), limit_of_one).unwrap();
        let outcome = repository.insert("new", &message(), TTL, alice()).unwrap();

        // The old entry counts against no client's limit.
        assert_eq!(
            outcome,
            InsertOutcome::Stored {
                removed_ids: vec![]
            }
        );
        assert!(repository.get("old").unwrap().is_some());
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
