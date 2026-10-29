//! Coordination layer for pub-sub message delivery with persistent storage.
//!
//! Combines SQLite persistence (via EntryRepository) with in-memory waiter channels.
//! Entries are persisted; oneshot channels for waiting requests cannot be persisted.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::oneshot;

use super::persistence::EntryRepository;
use super::unix_timestamp_millis;

/// Bounds concurrent long-poll connections per ID and waiter kind (ACK or message).
///
/// Together with the limit on waiting IDs (see [`WaitingList`]), memory is bounded:
/// with the default 10k IDs, worst case is 10k × 2 × 10 = ~200k oneshot channels.
const MAX_WAITERS_PER_ENTRY: usize = 10;

/// Minimum time between prunes of a full waiter map.
///
/// A prune scans every waiting ID while the caller holds the lock that every
/// request needs. Without this interval, a flood of requests for new IDs at a
/// full map would run one full scan per request. The cost: right after a prune,
/// disconnected clients can keep new IDs out for up to this long.
const FULL_MAP_PRUNE_INTERVAL: Duration = Duration::from_secs(1);

/// Payload stored in the waiting list, cloned to multiple subscribers on delivery.
///
/// Uses `Bytes` for zero-copy cloning when fanning out to concurrent waiters.
#[derive(Clone, Debug)]
pub struct Message {
    /// The message body bytes.
    pub body: Bytes,
    /// Optional MIME content type (e.g., "application/json").
    pub content_type: Option<String>,
}

/// In-memory waiters for a single entry.
/// These cannot be persisted (oneshot channels are runtime-specific).
struct Waiters {
    /// Subscribers waiting to be notified when ACK occurs.
    ack_waiters: Vec<oneshot::Sender<()>>,
    /// Subscribers waiting to receive a message when it arrives.
    message_waiters: Vec<oneshot::Sender<Message>>,
}

impl Waiters {
    fn new() -> Self {
        Self {
            ack_waiters: Vec::new(),
            message_waiters: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.ack_waiters.is_empty() && self.message_waiters.is_empty()
    }

    /// Adds an ACK waiter. Returns false if limit reached.
    ///
    /// Closed senders are dropped first, so disconnected clients don't use up
    /// the slots of a consumer that reconnects.
    fn add_ack_waiter(&mut self, sender: oneshot::Sender<()>) -> bool {
        self.ack_waiters.retain(|sender| !sender.is_closed());
        if self.ack_waiters.len() >= MAX_WAITERS_PER_ENTRY {
            return false;
        }
        self.ack_waiters.push(sender);
        true
    }

    /// Adds a message waiter. Returns false if limit reached.
    ///
    /// Closed senders are dropped first, so disconnected clients don't use up
    /// the slots of a consumer that reconnects.
    fn add_message_waiter(&mut self, sender: oneshot::Sender<Message>) -> bool {
        self.message_waiters.retain(|sender| !sender.is_closed());
        if self.message_waiters.len() >= MAX_WAITERS_PER_ENTRY {
            return false;
        }
        self.message_waiters.push(sender);
        true
    }

    /// Drops senders whose receivers are gone (client disconnected or timed out).
    fn remove_closed_senders(&mut self) {
        self.ack_waiters.retain(|sender| !sender.is_closed());
        self.message_waiters.retain(|sender| !sender.is_closed());
    }

    /// Notifies all message waiters and clears the list.
    fn notify_message_waiters(&mut self, message: &Message) {
        for waiter in self.ack_waiters.drain(..) {
            // Drop stale ack waiters when message is overwritten
            drop(waiter);
        }
        for waiter in self.message_waiters.drain(..) {
            let _ = waiter.send(message.clone());
        }
    }

    /// Notifies all ACK waiters and clears the list.
    fn notify_ack_waiters(&mut self) {
        for waiter in self.ack_waiters.drain(..) {
            let _ = waiter.send(());
        }
    }
}

/// Coordination layer for pub-sub message delivery.
///
/// Combines persistent storage (EntryRepository) with in-memory waiter channels.
/// Supports two patterns:
/// 1. **Inbox**: producer stores, consumer polls or long-polls for message
/// 2. **Link**: producer stores, waits for ACK; consumer fetches and ACKs
///
/// Memory is bounded by the repository's `max_entries` and by the waiter limits.
pub struct WaitingList {
    repository: EntryRepository,
    waiters: HashMap<String, Waiters>,
    /// Maximum number of distinct IDs in `waiters`.
    ///
    /// Consumers can wait for IDs that have no stored entry yet, so `waiters`
    /// isn't bounded by the repository. Reusing the repository's `max_entries`
    /// keeps a single capacity setting: the relay serves as many waiting IDs
    /// as it stores entries.
    max_waiting_ids: usize,
    /// When a full waiter map was last pruned (see [`FULL_MAP_PRUNE_INTERVAL`]).
    last_full_map_prune: Option<Instant>,
}

/// Error returned when subscribing to a waiting list entry fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscribeError {
    /// Maximum waiters for this ID, or maximum number of waiting IDs, reached.
    WaiterLimitReached,
    /// Entry not found or expired.
    NotFound,
}

/// Error returned by [`WaitingList::store_and_subscribe_ack`].
#[derive(Debug)]
pub enum StoreAndSubscribeError {
    /// Maximum waiters for this ID, or maximum number of waiting IDs, reached.
    /// Nothing was stored.
    WaiterLimitReached,
    /// Entry expired right after being stored.
    NotFound,
    /// Persisting the message failed.
    Storage(anyhow::Error),
}

impl From<SubscribeError> for StoreAndSubscribeError {
    fn from(error: SubscribeError) -> Self {
        match error {
            SubscribeError::WaiterLimitReached => Self::WaiterLimitReached,
            SubscribeError::NotFound => Self::NotFound,
        }
    }
}

/// Result of get_or_subscribe operation.
pub enum GetOrSubscribeResult {
    /// Message was immediately available.
    Message(Message),
    /// No message yet - receiver will fire when message arrives.
    Waiting(oneshot::Receiver<Message>),
}

impl WaitingList {
    /// Creates a WaitingList with the given repository.
    ///
    /// The number of waiting IDs is limited to the repository's `max_entries`.
    pub fn new(repository: EntryRepository) -> Self {
        let max_waiting_ids = repository.max_entries();
        Self {
            repository,
            waiters: HashMap::new(),
            max_waiting_ids,
            last_full_map_prune: None,
        }
    }

    /// Stores a message, immediately notifying any long-polling subscribers.
    ///
    /// On overwrite: message_waiters receive the new message; stale ack_waiters
    /// (from previous message) are dropped (their receivers get `RecvError`).
    ///
    /// Returns an error if persistence fails. Waiters are notified regardless
    /// of persistence success (message is delivered but may not survive restart).
    pub fn store(&mut self, id: String, message: Message, ttl: Duration) -> anyhow::Result<()> {
        let expires_at = unix_timestamp_millis() + ttl.as_millis() as i64;

        // Notify any waiting subscribers before persisting
        if let Some(waiters) = self.waiters.get_mut(&id) {
            waiters.notify_message_waiters(&message);
            if waiters.is_empty() {
                self.waiters.remove(&id);
            }
        }

        // Persist to SQLite
        self.repository.insert(
            &id,
            &message.body,
            message.content_type.as_deref(),
            expires_at,
        )
    }

    /// Marks entry as acknowledged, clearing message payload and notifying waiters.
    ///
    /// Returns false if entry missing or expired (caller should return 404).
    pub fn ack(&mut self, id: &str) -> bool {
        // Check if entry exists and is not expired
        let entry = match self.repository.get(id) {
            Ok(Some(entry)) if !Self::is_expired(entry.expires_at) => entry,
            _ => return false,
        };

        // Already acked? Return true but don't notify again
        if entry.acked {
            return true;
        }

        // Persist the ACK
        if let Err(e) = self.repository.ack(id) {
            tracing::error!(?e, id, "Failed to ack entry in repository");
            return false;
        }

        // Notify waiters
        if let Some(waiters) = self.waiters.get_mut(id) {
            waiters.notify_ack_waiters();
            if waiters.is_empty() {
                self.waiters.remove(id);
            }
        }

        true
    }

    /// Returns whether an entry has been acknowledged.
    ///
    /// Returns `None` if entry doesn't exist or is expired, `Some(bool)` otherwise.
    pub fn is_acked(&self, id: &str) -> Option<bool> {
        match self.repository.get(id) {
            Ok(Some(entry)) if !Self::is_expired(entry.expires_at) => Some(entry.acked),
            _ => None,
        }
    }

    /// Subscribes to receive notification when this entry is ACKed.
    ///
    /// Returns `Err(SubscribeError::NotFound)` if entry missing/expired,
    /// `Err(SubscribeError::WaiterLimitReached)` if waiter limit reached.
    pub fn subscribe_ack(&mut self, id: &str) -> Result<oneshot::Receiver<()>, SubscribeError> {
        // Check if entry exists and is not expired
        let entry = match self.repository.get(id) {
            Ok(Some(entry)) if !Self::is_expired(entry.expires_at) => entry,
            _ => return Err(SubscribeError::NotFound),
        };

        // If already acked, create a channel and immediately notify
        if entry.acked {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(());
            return Ok(rx);
        }

        let (tx, rx) = oneshot::channel();
        let waiters = self.waiters_for(id)?;
        if waiters.add_ack_waiter(tx) {
            Ok(rx)
        } else {
            Err(SubscribeError::WaiterLimitReached)
        }
    }

    /// Stores a message and subscribes its producer for the ACK (link pattern).
    ///
    /// Checks for waiter room before storing: a producer rejected with
    /// `WaiterLimitReached` must be able to retry without its first message
    /// having been delivered already.
    pub fn store_and_subscribe_ack(
        &mut self,
        id: String,
        message: Message,
        ttl: Duration,
    ) -> Result<oneshot::Receiver<()>, StoreAndSubscribeError> {
        self.ensure_waiter_capacity(&id)?;
        self.store(id.clone(), message, ttl)
            .map_err(StoreAndSubscribeError::Storage)?;
        // Storing drops the ID's old ACK waiters, so the per-ID limit can't fail here.
        Ok(self.subscribe_ack(&id)?)
    }

    /// Atomically checks for message and subscribes if not present.
    ///
    /// Prevents TOCTOU race: without this, a separate "check then subscribe" would
    /// miss messages arriving between the two operations.
    ///
    /// Creates a waiter entry if no message exists, allowing waiters to register
    /// before any message is stored (consumer arrives before producer).
    pub fn get_or_subscribe(&mut self, id: &str) -> Result<GetOrSubscribeResult, SubscribeError> {
        // Check if entry exists with a message
        if let Ok(Some(entry)) = self.repository.get(id) {
            if !Self::is_expired(entry.expires_at) {
                if let Some(body) = entry.message_body {
                    return Ok(GetOrSubscribeResult::Message(Message {
                        body: Bytes::from(body),
                        content_type: entry.content_type,
                    }));
                }
            }
        }

        // No message - subscribe for notification
        let (tx, rx) = oneshot::channel();
        let waiters = self.waiters_for(id)?;
        if waiters.add_message_waiter(tx) {
            Ok(GetOrSubscribeResult::Waiting(rx))
        } else {
            Err(SubscribeError::WaiterLimitReached)
        }
    }

    /// Checks that a waiter can be added for `id`.
    ///
    /// Returns `Err(SubscribeError::WaiterLimitReached)` if `id` is new and
    /// `max_waiting_ids` IDs already have live waiters. Doesn't add `id`, so
    /// callers can check before a side effect that must not happen when the
    /// following subscribe fails (see [`Self::store_and_subscribe_ack`]).
    fn ensure_waiter_capacity(&mut self, id: &str) -> Result<(), SubscribeError> {
        if self.waiters.contains_key(id) || self.waiters.len() < self.max_waiting_ids {
            return Ok(());
        }

        // Disconnected clients leave IDs behind until the next cleanup. Pruning
        // them here means only open connections can keep new IDs out.
        let prune_due = self
            .last_full_map_prune
            .is_none_or(|last_prune| last_prune.elapsed() >= FULL_MAP_PRUNE_INTERVAL);
        if prune_due {
            self.remove_closed_waiters();
            self.last_full_map_prune = Some(Instant::now());
        }

        if self.waiters.len() < self.max_waiting_ids {
            Ok(())
        } else {
            Err(SubscribeError::WaiterLimitReached)
        }
    }

    /// Returns the waiters for `id`, adding an empty entry if the ID is new.
    ///
    /// Returns `Err(SubscribeError::WaiterLimitReached)` if there is no room
    /// for `id` (see [`Self::ensure_waiter_capacity`]).
    fn waiters_for(&mut self, id: &str) -> Result<&mut Waiters, SubscribeError> {
        self.ensure_waiter_capacity(id)?;
        Ok(self
            .waiters
            .entry(id.to_string())
            .or_insert_with(Waiters::new))
    }

    /// Removes expired entries and the waiters that can no longer be served.
    ///
    /// Drops the waiters of the deleted entries, closed senders, and IDs left
    /// without waiters. Waiters for IDs that have no entry yet are kept
    /// (consumer arrives before producer).
    ///
    /// Returns the number of expired entries deleted.
    pub fn cleanup_expired(&mut self) -> usize {
        let expired_ids = match self.repository.cleanup_expired() {
            Ok(ids) => ids,
            Err(e) => {
                tracing::error!(?e, "Failed to cleanup expired entries");
                Vec::new()
            }
        };

        for id in &expired_ids {
            self.waiters.remove(id);
        }
        self.remove_closed_waiters();

        expired_ids.len()
    }

    /// Drops closed senders and removes IDs left without waiters.
    fn remove_closed_waiters(&mut self) {
        self.waiters.retain(|_, waiters| {
            waiters.remove_closed_senders();
            !waiters.is_empty()
        });
    }

    /// Checks if an entry has expired based on its expires_at timestamp (in milliseconds).
    fn is_expired(expires_at: i64) -> bool {
        unix_timestamp_millis() >= expires_at
    }
}

#[cfg(test)]
impl WaitingList {
    /// Creates a WaitingList with an in-memory repository for testing.
    pub fn new_in_memory(max_entries: usize) -> Self {
        let repository =
            EntryRepository::new(None, max_entries).expect("Failed to create in-memory repository");
        Self::new(repository)
    }

    /// Returns the number of entries in the repository.
    pub fn len(&self) -> usize {
        self.repository.count().unwrap_or(0)
    }

    /// Returns true if the repository is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::sleep;

    fn make_message(body: &str) -> Message {
        Message {
            body: Bytes::from(body.to_string()),
            content_type: Some("text/plain".to_string()),
        }
    }

    fn create_test_list() -> WaitingList {
        WaitingList::new_in_memory(100)
    }

    // ========== cleanup_expired tests ==========

    #[tokio::test]
    async fn cleanup_expired_removes_expired_entries() {
        let mut list = create_test_list();
        let short_ttl = Duration::from_millis(10);
        let long_ttl = Duration::from_secs(60);

        list.store("expires-soon".to_string(), make_message("a"), short_ttl)
            .unwrap();
        list.store("stays-alive".to_string(), make_message("b"), long_ttl)
            .unwrap();

        assert_eq!(list.len(), 2);

        sleep(Duration::from_millis(20)).await;

        let removed = list.cleanup_expired();

        assert_eq!(removed, 1);
        assert_eq!(list.len(), 1);
    }

    #[tokio::test]
    async fn cleanup_expired_returns_zero_when_nothing_expired() {
        let mut list = create_test_list();
        let ttl = Duration::from_secs(60);

        list.store("a".to_string(), make_message("a"), ttl).unwrap();
        list.store("b".to_string(), make_message("b"), ttl).unwrap();

        let removed = list.cleanup_expired();

        assert_eq!(removed, 0);
        assert_eq!(list.len(), 2);
    }

    // ========== Message overwrite tests ==========

    #[tokio::test]
    async fn store_notifies_message_waiters_on_overwrite() {
        let mut list = create_test_list();
        let ttl = Duration::from_secs(60);

        // Subscribe for message
        let result = list.get_or_subscribe("id1").expect("should succeed");
        let rx = match result {
            GetOrSubscribeResult::Waiting(rx) => rx,
            GetOrSubscribeResult::Message(_) => panic!("expected waiting"),
        };

        // Store message - should notify the waiter
        list.store("id1".to_string(), make_message("overwrite"), ttl)
            .unwrap();

        let received = rx.await.expect("should receive message");
        assert_eq!(received.body, Bytes::from("overwrite"));
    }

    #[tokio::test]
    async fn store_drops_ack_waiters_on_overwrite() {
        let mut list = create_test_list();
        let ttl = Duration::from_secs(60);

        list.store("id1".to_string(), make_message("first"), ttl)
            .unwrap();
        let ack_rx = list.subscribe_ack("id1").expect("should subscribe");

        // Overwrite the entry
        list.store("id1".to_string(), make_message("second"), ttl)
            .unwrap();

        // Old ack waiter should be dropped (receive error)
        let result = ack_rx.await;
        assert!(result.is_err(), "old ack waiter should be dropped");
    }

    // ========== Waiter limit tests ==========

    #[test]
    fn subscribe_ack_returns_limit_error() {
        let mut list = create_test_list();
        let ttl = Duration::from_secs(60);

        list.store("id1".to_string(), make_message("test"), ttl)
            .unwrap();

        // Keep the receivers alive: closed senders don't count toward the limit
        let _receivers: Vec<_> = (0..MAX_WAITERS_PER_ENTRY)
            .map(|_| list.subscribe_ack("id1").expect("should subscribe"))
            .collect();

        let result = list.subscribe_ack("id1");
        assert!(
            matches!(result, Err(SubscribeError::WaiterLimitReached)),
            "expected WaiterLimitReached error"
        );
    }

    #[test]
    fn get_or_subscribe_returns_limit_error() {
        let mut list = create_test_list();

        // Subscribe multiple times (no message stored yet), keeping the receivers alive
        let _receivers: Vec<_> = (0..MAX_WAITERS_PER_ENTRY)
            .map(|_| list.get_or_subscribe("id1").expect("should succeed"))
            .collect();

        let result = list.get_or_subscribe("id1");
        assert!(
            matches!(result, Err(SubscribeError::WaiterLimitReached)),
            "expected WaiterLimitReached error"
        );
    }

    #[test]
    fn closed_message_waiters_do_not_count_toward_limit() {
        // Regression test: a reconnecting consumer must not be blocked by its own
        // disconnected requests until the next cleanup.
        let mut list = create_test_list();

        for _ in 0..MAX_WAITERS_PER_ENTRY {
            let result = list.get_or_subscribe("id1").expect("should succeed");
            drop(result); // Client disconnected: receiver dropped, sender closed
        }

        assert!(list.get_or_subscribe("id1").is_ok());
    }

    #[test]
    fn closed_ack_waiters_do_not_count_toward_limit() {
        let mut list = create_test_list();
        let ttl = Duration::from_secs(60);
        list.store("id1".to_string(), make_message("test"), ttl)
            .unwrap();

        for _ in 0..MAX_WAITERS_PER_ENTRY {
            let rx = list.subscribe_ack("id1").expect("should subscribe");
            drop(rx);
        }

        assert!(list.subscribe_ack("id1").is_ok());
    }

    #[test]
    fn new_id_beyond_waiting_id_limit_is_rejected() {
        // Regression test: unique IDs must not grow the waiter map without bound.
        let mut list = WaitingList::new_in_memory(2);
        let _waiting_a = list.get_or_subscribe("a").expect("should succeed");
        let _waiting_b = list.get_or_subscribe("b").expect("should succeed");

        let result = list.get_or_subscribe("c");

        assert!(
            matches!(result, Err(SubscribeError::WaiterLimitReached)),
            "expected WaiterLimitReached error"
        );
        assert!(!list.waiters.contains_key("c"));
    }

    #[test]
    fn existing_id_accepts_waiters_at_waiting_id_limit() {
        let mut list = WaitingList::new_in_memory(2);
        let _waiting_a = list.get_or_subscribe("a").expect("should succeed");
        let _waiting_b = list.get_or_subscribe("b").expect("should succeed");

        let result = list.get_or_subscribe("a");

        assert!(result.is_ok(), "existing ID should accept another waiter");
    }

    #[test]
    fn new_id_reuses_slot_of_disconnected_waiters() {
        let mut list = WaitingList::new_in_memory(2);
        let _waiting_a = list.get_or_subscribe("a").expect("should succeed");
        let disconnected_b = list.get_or_subscribe("b").expect("should succeed");
        drop(disconnected_b);

        let result = list.get_or_subscribe("c");

        assert!(result.is_ok(), "closed waiters should not keep new IDs out");
        assert!(!list.waiters.contains_key("b"));
    }

    #[test]
    fn full_map_is_pruned_at_most_once_per_interval() {
        // Regression test: a flood of new IDs at a full map must not run a full
        // prune per request while holding the global lock.
        let mut list = WaitingList::new_in_memory(2);
        let _waiting_a = list.get_or_subscribe("a").expect("should succeed");
        drop(list.get_or_subscribe("b").expect("should succeed"));
        let disconnected_c = list.get_or_subscribe("c").expect("prune frees b");
        drop(disconnected_c);

        let result = list.get_or_subscribe("d");

        assert!(
            matches!(result, Err(SubscribeError::WaiterLimitReached)),
            "no second prune within FULL_MAP_PRUNE_INTERVAL"
        );
        assert!(list.waiters.contains_key("c"), "closed ID not pruned yet");
    }

    #[test]
    fn ensure_waiter_capacity_does_not_add_id() {
        let mut list = WaitingList::new_in_memory(1);

        assert!(list.ensure_waiter_capacity("a").is_ok());

        assert!(list.waiters.is_empty());
    }

    // ========== store_and_subscribe_ack tests ==========

    #[test]
    fn store_and_subscribe_ack_at_waiting_id_limit_does_not_store() {
        // Regression test: a producer rejected for lack of waiter room must not
        // leave its message behind for a consumer to fetch.
        let mut list = WaitingList::new_in_memory(1);
        let _waiting = list.get_or_subscribe("occupied").expect("should succeed");

        let result = list.store_and_subscribe_ack(
            "new".to_string(),
            make_message("rejected"),
            Duration::from_secs(60),
        );

        assert!(
            matches!(result, Err(StoreAndSubscribeError::WaiterLimitReached)),
            "expected WaiterLimitReached error"
        );
        assert_eq!(list.is_acked("new"), None, "message must not be stored");
    }

    #[tokio::test]
    async fn store_and_subscribe_ack_for_waiting_id_at_limit_succeeds() {
        let mut list = WaitingList::new_in_memory(1);
        let consumer = match list.get_or_subscribe("id1").expect("should succeed") {
            GetOrSubscribeResult::Waiting(rx) => rx,
            GetOrSubscribeResult::Message(_) => panic!("expected waiting"),
        };

        let result = list.store_and_subscribe_ack(
            "id1".to_string(),
            make_message("hello"),
            Duration::from_secs(60),
        );

        assert!(result.is_ok(), "existing ID should accept the producer");
        let received = consumer.await.expect("consumer should receive message");
        assert_eq!(received.body, Bytes::from("hello"));
    }

    #[test]
    fn store_and_subscribe_ack_replaces_ack_waiters_at_per_id_limit() {
        let mut list = create_test_list();
        let ttl = Duration::from_secs(60);
        list.store("id1".to_string(), make_message("first"), ttl)
            .unwrap();
        let _receivers: Vec<_> = (0..MAX_WAITERS_PER_ENTRY)
            .map(|_| list.subscribe_ack("id1").expect("should subscribe"))
            .collect();

        let result = list.store_and_subscribe_ack("id1".to_string(), make_message("second"), ttl);

        assert!(result.is_ok(), "storing drops the old ACK waiters");
    }

    // ========== State transition tests ==========

    #[test]
    fn is_acked_false_before_ack() {
        let mut list = create_test_list();
        let ttl = Duration::from_secs(60);

        list.store("id1".to_string(), make_message("test"), ttl)
            .unwrap();

        assert_eq!(list.is_acked("id1"), Some(false));
    }

    #[test]
    fn is_acked_true_after_ack() {
        let mut list = create_test_list();
        let ttl = Duration::from_secs(60);

        list.store("id1".to_string(), make_message("test"), ttl)
            .unwrap();
        let ack_result = list.ack("id1");

        assert!(ack_result, "ack should succeed");
        assert_eq!(list.is_acked("id1"), Some(true));
    }

    #[tokio::test]
    async fn ack_notifies_waiters() {
        let mut list = create_test_list();
        let ttl = Duration::from_secs(60);

        list.store("id1".to_string(), make_message("test"), ttl)
            .unwrap();
        let rx = list.subscribe_ack("id1").expect("should subscribe");

        list.ack("id1");

        let result = rx.await;
        assert!(result.is_ok(), "ack waiter should receive notification");
    }

    #[tokio::test]
    async fn ack_fails_for_expired_entry() {
        let mut list = create_test_list();
        let short_ttl = Duration::from_millis(10);

        list.store("id1".to_string(), make_message("test"), short_ttl)
            .unwrap();

        sleep(Duration::from_millis(20)).await;

        assert!(!list.ack("id1"), "ack should fail for expired entry");
        assert_eq!(
            list.is_acked("id1"),
            None,
            "is_acked should be None for expired"
        );
    }

    #[test]
    fn ack_fails_for_nonexistent_entry() {
        let mut list = create_test_list();

        assert!(!list.ack("nonexistent"));
        assert_eq!(list.is_acked("nonexistent"), None);
    }

    // ========== get_or_subscribe atomicity tests ==========

    #[test]
    fn get_or_subscribe_returns_existing_message() {
        let mut list = create_test_list();
        let ttl = Duration::from_secs(60);

        list.store("id1".to_string(), make_message("existing"), ttl)
            .unwrap();

        let result = list.get_or_subscribe("id1").expect("should succeed");

        match result {
            GetOrSubscribeResult::Message(msg) => {
                assert_eq!(msg.body, Bytes::from("existing"));
            }
            GetOrSubscribeResult::Waiting(_) => {
                panic!("should return message, not waiting");
            }
        }
    }

    #[test]
    fn get_or_subscribe_returns_receiver_when_no_entry() {
        let mut list = create_test_list();

        let result = list.get_or_subscribe("id1").expect("should succeed");

        match result {
            GetOrSubscribeResult::Message(_) => {
                panic!("should return waiting, not message");
            }
            GetOrSubscribeResult::Waiting(_) => {
                // Correct - waiter registered
                assert!(list.waiters.contains_key("id1"));
            }
        }
    }

    #[tokio::test]
    async fn get_or_subscribe_receiver_gets_message_when_stored() {
        let mut list = create_test_list();
        let ttl = Duration::from_secs(60);

        let result = list.get_or_subscribe("id1").expect("should succeed");
        let rx = match result {
            GetOrSubscribeResult::Waiting(rx) => rx,
            GetOrSubscribeResult::Message(_) => panic!("should be waiting"),
        };

        list.store("id1".to_string(), make_message("arrived"), ttl)
            .unwrap();

        let msg = rx.await.expect("should receive message");
        assert_eq!(msg.body, Bytes::from("arrived"));
    }

    #[tokio::test]
    async fn get_or_subscribe_ignores_expired_message() {
        let mut list = create_test_list();
        let short_ttl = Duration::from_millis(10);

        list.store("id1".to_string(), make_message("expired"), short_ttl)
            .unwrap();

        sleep(Duration::from_millis(20)).await;

        let result = list.get_or_subscribe("id1").expect("should succeed");

        match result {
            GetOrSubscribeResult::Message(_) => {
                panic!("should not return expired message");
            }
            GetOrSubscribeResult::Waiting(_) => {
                // Correct behavior
            }
        }
    }

    // ========== Cleanup behavior tests ==========

    #[tokio::test]
    async fn cleanup_does_not_remove_waiters_without_entry() {
        // Regression test: consumer arrives before producer, cleanup must not evict waiter
        let mut list = create_test_list();
        let ttl = Duration::from_secs(60);

        // Consumer subscribes - no entry in DB yet
        let result = list
            .get_or_subscribe("consumer-first")
            .expect("should succeed");
        let rx = match result {
            GetOrSubscribeResult::Waiting(rx) => rx,
            GetOrSubscribeResult::Message(_) => panic!("should be waiting"),
        };

        // Cleanup runs - must NOT remove the waiter
        list.cleanup_expired();

        // Producer sends message - waiter should still receive it
        list.store("consumer-first".to_string(), make_message("delayed"), ttl)
            .unwrap();

        let msg = rx
            .await
            .expect("waiter should not have been removed by cleanup");
        assert_eq!(msg.body, Bytes::from("delayed"));
    }

    #[tokio::test]
    async fn cleanup_removes_waiters_for_expired_entries() {
        let mut list = create_test_list();
        let short_ttl = Duration::from_millis(10);

        // Store message, subscribe for ack
        list.store("will-expire".to_string(), make_message("test"), short_ttl)
            .unwrap();
        let ack_rx = list.subscribe_ack("will-expire").expect("should subscribe");

        // Wait for expiry
        sleep(Duration::from_millis(20)).await;

        // Cleanup should remove the expired entry AND its waiters
        list.cleanup_expired();

        // Ack waiter should be dropped (sender removed)
        assert!(
            ack_rx.await.is_err(),
            "waiter should be removed for expired entry"
        );
    }

    #[tokio::test]
    async fn cleanup_removes_closed_senders() {
        let mut list = create_test_list();

        // Subscribe but immediately drop the receiver (simulates timeout)
        let result = list
            .get_or_subscribe("dropped-receiver")
            .expect("should succeed");
        match result {
            GetOrSubscribeResult::Waiting(rx) => drop(rx), // Receiver dropped
            GetOrSubscribeResult::Message(_) => panic!("should be waiting"),
        };

        assert!(list.waiters.contains_key("dropped-receiver"));

        // Cleanup should remove closed senders
        list.cleanup_expired();

        // Waiter entry should be removed (no live senders)
        assert!(!list.waiters.contains_key("dropped-receiver"));
    }
}
