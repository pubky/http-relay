//! Shared types for relay storage implementations.

/// A stored entry retrieved from the repository.
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

/// Outcome of storing a message when storage itself did not fail.
///
/// Both storage backends return this so callers can tell a full store apart
/// from an operational error and answer with `503 Server at capacity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    /// The message was stored (as a new entry or replacing an existing one).
    Stored,
    /// The message was rejected: every entry still holds an undelivered,
    /// unexpired message, so there was no room for a new ID.
    StoreFull,
}
