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
