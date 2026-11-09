//! Hands messages and ACKs from the requests that send them to the requests
//! that wait for them.
//!
//! Entries live in the [`EntryRepository`]. Requests that wait for a message or
//! an ACK are in-memory oneshot channels, which can't be persisted.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use crate::storage::{EntryRepository, InsertOutcome, Message, StoredEntry};

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

/// A value that is either available now or arrives through a channel later.
///
/// A channel waiting for an ACK closes without a value if the entry expires or
/// a new message replaces it. A channel waiting for a message
/// stays open until the next message arrives.
pub(crate) enum Delivery<T> {
    Ready(T),
    Pending(oneshot::Receiver<T>),
}

/// Why the waiting list couldn't serve a request.
#[derive(Debug)]
pub(crate) enum WaitingListError {
    /// Maximum waiters for this ID, or maximum number of waiting IDs, reached.
    WaiterLimitReached,
    /// The store has no room for a new ID. Nothing was stored.
    StoreFull,
    Storage(anyhow::Error),
}

impl From<anyhow::Error> for WaitingListError {
    fn from(error: anyhow::Error) -> Self {
        Self::Storage(error)
    }
}

trait Waiter {
    /// Whether the waiting request is gone (client disconnected or timed out).
    fn is_closed(&self) -> bool;
}

impl Waiter for oneshot::Sender<()> {
    fn is_closed(&self) -> bool {
        oneshot::Sender::is_closed(self)
    }
}

struct MessageWaiter {
    sender: oneshot::Sender<Message>,
    /// Receiving the message ACKs it (link consumers).
    acks_on_delivery: bool,
}

impl Waiter for MessageWaiter {
    fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }
}

/// Waiters of one kind for one ID, at most [`MAX_WAITERS_PER_ENTRY`].
struct Slots<W>(Vec<W>);

impl<W> Default for Slots<W> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<W: Waiter> Slots<W> {
    /// Adds a waiter. Returns false if all slots are taken.
    ///
    /// Closed waiters are dropped first, so disconnected clients don't use up
    /// the slots of a consumer that reconnects.
    fn add(&mut self, waiter: W) -> bool {
        self.remove_closed();
        if self.0.len() >= MAX_WAITERS_PER_ENTRY {
            return false;
        }
        self.0.push(waiter);
        true
    }

    fn remove_closed(&mut self) {
        self.0.retain(|waiter| !waiter.is_closed());
    }

    fn take(&mut self) -> Vec<W> {
        std::mem::take(&mut self.0)
    }
}

#[derive(Default)]
struct Waiters {
    /// Wait for the next message: the entry is missing or ACKed.
    messages: Slots<MessageWaiter>,
    /// Wait for the ACK of the stored message.
    acks: Slots<oneshot::Sender<()>>,
}

impl Waiters {
    fn is_empty(&self) -> bool {
        self.messages.0.is_empty() && self.acks.0.is_empty()
    }

    fn remove_closed(&mut self) {
        self.messages.remove_closed();
        self.acks.remove_closed();
    }
}

/// Stored entries plus the requests waiting on them.
///
/// Supports two patterns:
/// 1. **Inbox**: producer stores, consumer polls or long-polls for the message,
///    then ACKs it; the producer can wait for the ACK.
/// 2. **Link**: producer stores and waits for the ACK; consumer receiving the
///    message ACKs it.
pub(crate) struct WaitingList {
    repository: EntryRepository,
    waiters: HashMap<String, Waiters>,
    /// Maximum number of distinct IDs in `waiters`.
    ///
    /// Consumers can wait for IDs that have no stored entry yet, so `waiters`
    /// isn't bounded by the repository.
    max_waiting_ids: usize,
    /// When a full waiter map was last pruned (see [`FULL_MAP_PRUNE_INTERVAL`]).
    last_full_map_prune: Option<Instant>,
}

impl WaitingList {
    pub fn new(repository: EntryRepository, max_waiting_ids: usize) -> Self {
        Self {
            repository,
            waiters: HashMap::new(),
            max_waiting_ids,
            last_full_map_prune: None,
        }
    }

    /// Stores a message and hands it to the requests waiting for one.
    ///
    /// Requests waiting for the ACK of the replaced message are dropped. If
    /// the store is full, nothing changes and the waiting requests keep
    /// waiting.
    pub fn store(
        &mut self,
        id: &str,
        message: Message,
        ttl: Duration,
    ) -> Result<(), WaitingListError> {
        self.deliver(id, message, ttl)?;
        Ok(())
    }

    /// Stores a message and waits for its ACK (link producer).
    ///
    /// Checks for waiter room before storing: a producer rejected with
    /// `WaiterLimitReached` must be able to retry without its first message
    /// having been delivered already.
    #[cfg(feature = "link-compat")]
    pub fn store_and_subscribe_ack(
        &mut self,
        id: &str,
        message: Message,
        ttl: Duration,
    ) -> Result<Delivery<()>, WaitingListError> {
        self.ensure_waiter_capacity(id)?;
        let acked = self.deliver(id, message, ttl)?;
        if acked {
            return Ok(Delivery::Ready(()));
        }
        self.add_ack_waiter(id)
    }

    /// Returns the stored message, or waits for the next one (inbox consumer).
    pub fn get_or_subscribe(&mut self, id: &str) -> Result<Delivery<Message>, WaitingListError> {
        self.receive(id, false)
    }

    /// Returns the stored message, or waits for the next one, and ACKs it
    /// in the same step (link consumer), so it can't ACK a newer message.
    #[cfg(feature = "link-compat")]
    pub fn take_or_subscribe(&mut self, id: &str) -> Result<Delivery<Message>, WaitingListError> {
        self.receive(id, true)
    }

    /// ACKs the entry and notifies the requests waiting for the ACK.
    ///
    /// Returns false if there is no live entry.
    pub fn ack(&mut self, id: &str) -> anyhow::Result<bool> {
        if !self.repository.ack(id)? {
            return Ok(false);
        }
        for waiter in self.take_ack_waiters(id) {
            let _ = waiter.send(());
        }
        Ok(true)
    }

    /// Returns whether the entry is ACKed, or `None` if there is no live entry.
    pub fn is_acked(&self, id: &str) -> anyhow::Result<Option<bool>> {
        Ok(self
            .repository
            .get(id)?
            .map(|entry| entry == StoredEntry::Acked))
    }

    /// Waits for the entry's ACK, or returns `None` if there is no live entry.
    pub fn subscribe_ack(&mut self, id: &str) -> Result<Option<Delivery<()>>, WaitingListError> {
        match self.repository.get(id)? {
            None => Ok(None),
            Some(StoredEntry::Acked) => Ok(Some(Delivery::Ready(()))),
            Some(StoredEntry::Unacked(_)) => self.add_ack_waiter(id).map(Some),
        }
    }

    /// Removes expired entries and the waiters that can no longer be served.
    ///
    /// Drops closed waiters, the ACK waiters of the deleted entries, and IDs
    /// left without waiters. Message waiters stay: they wait for the next
    /// message, which can still arrive.
    ///
    /// Returns the number of expired entries deleted.
    pub fn remove_expired(&mut self) -> anyhow::Result<usize> {
        self.remove_closed_waiters();
        let expired_ids = self.repository.remove_expired()?;
        for id in &expired_ids {
            self.take_ack_waiters(id);
        }
        Ok(expired_ids.len())
    }

    /// Stores a message and hands it to the requests waiting for one.
    ///
    /// Returns whether a link consumer received it, which ACKs it.
    fn deliver(
        &mut self,
        id: &str,
        message: Message,
        ttl: Duration,
    ) -> Result<bool, WaitingListError> {
        match self.repository.insert(id, &message, ttl)? {
            InsertOutcome::Stored { removed_ids } => {
                // Entries removed to make room are expired or ACKed. ACK
                // waiters of an expired entry would otherwise wait in vain.
                for removed_id in &removed_ids {
                    self.take_ack_waiters(removed_id);
                }
            }
            InsertOutcome::StoreFull => return Err(WaitingListError::StoreFull),
        }

        // Both kinds of waiters are done: ACK waiters waited for the replaced message.
        let Some(mut waiters) = self.waiters.remove(id) else {
            return Ok(false);
        };
        let mut acked = false;
        for waiter in waiters.messages.take() {
            if waiter.sender.send(message.clone()).is_ok() && waiter.acks_on_delivery {
                acked = true;
            }
        }
        if acked {
            self.repository.ack(id)?;
        }
        Ok(acked)
    }

    fn receive(
        &mut self,
        id: &str,
        ack_on_receive: bool,
    ) -> Result<Delivery<Message>, WaitingListError> {
        if let Some(StoredEntry::Unacked(message)) = self.repository.get(id)? {
            if ack_on_receive {
                self.ack(id)?;
            }
            return Ok(Delivery::Ready(message));
        }

        let (sender, receiver) = oneshot::channel();
        let waiter = MessageWaiter {
            sender,
            acks_on_delivery: ack_on_receive,
        };
        if self.waiters_for(id)?.messages.add(waiter) {
            Ok(Delivery::Pending(receiver))
        } else {
            Err(WaitingListError::WaiterLimitReached)
        }
    }

    fn add_ack_waiter(&mut self, id: &str) -> Result<Delivery<()>, WaitingListError> {
        let (sender, receiver) = oneshot::channel();
        if self.waiters_for(id)?.acks.add(sender) {
            Ok(Delivery::Pending(receiver))
        } else {
            Err(WaitingListError::WaiterLimitReached)
        }
    }

    /// Removes and returns the ACK waiters for `id`, and `id` itself if no
    /// message waiters remain.
    fn take_ack_waiters(&mut self, id: &str) -> Vec<oneshot::Sender<()>> {
        let Some(waiters) = self.waiters.get_mut(id) else {
            return Vec::new();
        };
        let ack_waiters = waiters.acks.take();
        if waiters.is_empty() {
            self.waiters.remove(id);
        }
        ack_waiters
    }

    /// Checks that a waiter can be added for `id`, without adding `id`.
    ///
    /// Fails if `id` is new and `max_waiting_ids` IDs already have live waiters.
    fn ensure_waiter_capacity(&mut self, id: &str) -> Result<(), WaitingListError> {
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
            Err(WaitingListError::WaiterLimitReached)
        }
    }

    /// Returns the waiters for `id`, adding an empty entry if the ID is new.
    fn waiters_for(&mut self, id: &str) -> Result<&mut Waiters, WaitingListError> {
        self.ensure_waiter_capacity(id)?;
        Ok(self.waiters.entry(id.to_string()).or_default())
    }

    fn remove_closed_waiters(&mut self) {
        self.waiters.retain(|_, waiters| {
            waiters.remove_closed();
            !waiters.is_empty()
        });
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::storage::test_repository;

    const TTL: Duration = Duration::from_secs(60);

    fn list(max_ids: usize) -> WaitingList {
        WaitingList::new(test_repository(100), max_ids)
    }

    fn message(body: &'static str) -> Message {
        Message {
            body: Bytes::from_static(body.as_bytes()),
            content_type: Some("text/plain".to_string()),
        }
    }

    fn pending<T>(delivery: Delivery<T>) -> oneshot::Receiver<T> {
        match delivery {
            Delivery::Pending(receiver) => receiver,
            Delivery::Ready(_) => panic!("expected to wait"),
        }
    }

    fn ready<T>(delivery: Delivery<T>) -> T {
        match delivery {
            Delivery::Ready(value) => value,
            Delivery::Pending(_) => panic!("expected a value now"),
        }
    }

    /// Whether the channel closed without a value. Doesn't wait, so a test
    /// fails instead of hanging if the channel stays open.
    fn is_closed<T>(mut receiver: oneshot::Receiver<T>) -> bool {
        matches!(
            receiver.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        )
    }

    fn is_limit<T>(result: Result<T, WaitingListError>) -> bool {
        matches!(result, Err(WaitingListError::WaiterLimitReached))
    }

    // ========== Messages ==========

    #[test]
    fn stored_message_is_returned_at_once() {
        let mut list = list(10);
        list.store("id", message("hello"), TTL).unwrap();

        let delivery = list.get_or_subscribe("id").unwrap();

        assert_eq!(ready(delivery), message("hello"));
    }

    #[tokio::test]
    async fn waiting_consumer_receives_next_message() {
        let mut list = list(10);
        let receiver = pending(list.get_or_subscribe("id").unwrap());

        list.store("id", message("hello"), TTL).unwrap();

        assert_eq!(receiver.await.unwrap(), message("hello"));
    }

    #[test]
    fn acked_message_is_not_returned_again() {
        let mut list = list(10);
        list.store("id", message("hello"), TTL).unwrap();
        list.ack("id").unwrap();

        assert!(matches!(
            list.get_or_subscribe("id").unwrap(),
            Delivery::Pending(_)
        ));
    }

    // ========== ACKs ==========

    #[test]
    fn ack_state_follows_the_entry() {
        let mut list = list(10);
        assert_eq!(list.is_acked("id").unwrap(), None);
        assert!(!list.ack("id").unwrap());

        list.store("id", message("hello"), TTL).unwrap();
        assert_eq!(list.is_acked("id").unwrap(), Some(false));

        assert!(list.ack("id").unwrap());
        assert_eq!(list.is_acked("id").unwrap(), Some(true));
    }

    #[tokio::test]
    async fn ack_notifies_ack_waiters() {
        let mut list = list(10);
        list.store("id", message("hello"), TTL).unwrap();
        let receiver = pending(list.subscribe_ack("id").unwrap().unwrap());

        list.ack("id").unwrap();

        assert!(receiver.await.is_ok());
    }

    #[test]
    fn subscribe_ack_depends_on_the_entry() {
        let mut list = list(10);
        assert!(list.subscribe_ack("id").unwrap().is_none());

        list.store("id", message("hello"), TTL).unwrap();
        list.ack("id").unwrap();

        ready(list.subscribe_ack("id").unwrap().unwrap());
    }

    #[tokio::test]
    async fn new_message_drops_ack_waiters_of_the_replaced_one() {
        let mut list = list(10);
        list.store("id", message("first"), TTL).unwrap();
        let receiver = pending(list.subscribe_ack("id").unwrap().unwrap());

        list.store("id", message("second"), TTL).unwrap();

        assert!(is_closed(receiver));
    }

    // ========== Full store ==========

    #[tokio::test]
    async fn full_store_rejects_message_and_keeps_waiters_waiting() {
        let mut list = WaitingList::new(test_repository(1), 10);
        list.store("unacked", message("first"), TTL).unwrap();
        let mut receiver = pending(list.get_or_subscribe("waiting").unwrap());

        let result = list.store("waiting", message("second"), TTL);

        assert!(matches!(result, Err(WaitingListError::StoreFull)));
        assert!(matches!(
            receiver.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn removing_an_expired_entry_to_make_room_drops_its_ack_waiters() {
        // Regression test: ACK waiters of an entry deleted to make room waited
        // until their timeout instead of failing at once.
        let mut list = WaitingList::new(test_repository(1), 10);
        list.store("expires", message("hello"), Duration::from_millis(100))
            .unwrap();
        let receiver = pending(list.subscribe_ack("expires").unwrap().unwrap());
        tokio::time::sleep(Duration::from_millis(150)).await;

        list.store("new", message("hello"), TTL).unwrap();

        assert!(is_closed(receiver));
    }

    // ========== Link: take_or_subscribe and store_and_subscribe_ack ==========

    #[cfg(feature = "link-compat")]
    #[test]
    fn take_acks_a_stored_message() {
        let mut list = list(10);
        list.store("id", message("hello"), TTL).unwrap();

        assert_eq!(
            ready(list.take_or_subscribe("id").unwrap()),
            message("hello")
        );
        assert_eq!(list.is_acked("id").unwrap(), Some(true));
    }

    #[cfg(feature = "link-compat")]
    #[tokio::test]
    async fn producer_is_acked_by_a_waiting_taker() {
        let mut list = list(10);
        let consumer = pending(list.take_or_subscribe("id").unwrap());

        let producer = list
            .store_and_subscribe_ack("id", message("hello"), TTL)
            .unwrap();

        ready(producer);
        assert_eq!(consumer.await.unwrap(), message("hello"));
    }

    #[cfg(feature = "link-compat")]
    #[tokio::test]
    async fn taker_acks_only_the_message_it_received() {
        // Regression test: a link consumer that acked after a second lock
        // could ACK a message stored in between.
        let mut list = list(10);
        let consumer = pending(list.take_or_subscribe("id").unwrap());
        list.store("id", message("first"), TTL).unwrap();
        assert_eq!(consumer.await.unwrap(), message("first"));

        let producer = list
            .store_and_subscribe_ack("id", message("second"), TTL)
            .unwrap();

        pending(producer);
    }

    #[cfg(feature = "link-compat")]
    #[tokio::test]
    async fn inbox_consumer_does_not_ack() {
        let mut list = list(10);
        let consumer = pending(list.get_or_subscribe("id").unwrap());

        let producer = list
            .store_and_subscribe_ack("id", message("hello"), TTL)
            .unwrap();

        pending(producer);
        assert_eq!(consumer.await.unwrap(), message("hello"));
    }

    #[cfg(feature = "link-compat")]
    #[test]
    fn store_and_subscribe_ack_at_waiting_id_limit_does_not_store() {
        // Regression test: a producer rejected for lack of waiter room must not
        // leave its message behind for a consumer to fetch.
        let mut list = list(1);
        let _waiting = list.get_or_subscribe("occupied").unwrap();

        let result = list.store_and_subscribe_ack("new", message("rejected"), TTL);

        assert!(is_limit(result));
        assert_eq!(list.is_acked("new").unwrap(), None);
    }

    #[cfg(feature = "link-compat")]
    #[tokio::test]
    async fn store_and_subscribe_ack_for_waiting_id_at_limit_succeeds() {
        let mut list = list(1);
        let consumer = pending(list.get_or_subscribe("id").unwrap());

        let result = list.store_and_subscribe_ack("id", message("hello"), TTL);

        assert!(result.is_ok());
        assert_eq!(consumer.await.unwrap(), message("hello"));
    }

    #[cfg(feature = "link-compat")]
    #[test]
    fn store_and_subscribe_ack_replaces_ack_waiters_at_per_id_limit() {
        let mut list = list(10);
        list.store("id", message("first"), TTL).unwrap();
        let receivers: Vec<_> = (0..MAX_WAITERS_PER_ENTRY)
            .map(|_| pending(list.subscribe_ack("id").unwrap().unwrap()))
            .collect();

        let result = list.store_and_subscribe_ack("id", message("second"), TTL);

        assert!(result.is_ok());
        for receiver in receivers {
            assert!(is_closed(receiver));
        }
    }

    #[cfg(feature = "link-compat")]
    #[test]
    fn store_and_subscribe_ack_in_full_store_does_not_wait() {
        let mut list = WaitingList::new(test_repository(1), 1);
        list.store("unacked", message("first"), TTL).unwrap();

        let result = list.store_and_subscribe_ack("new", message("rejected"), TTL);

        assert!(matches!(result, Err(WaitingListError::StoreFull)));
        // The rejected producer holds no waiting-ID slot.
        assert!(list.get_or_subscribe("other").is_ok());
    }

    // ========== Waiter limits ==========

    #[test]
    fn message_waiters_per_id_are_limited() {
        let mut list = list(10);
        let _receivers: Vec<_> = (0..MAX_WAITERS_PER_ENTRY)
            .map(|_| list.get_or_subscribe("id").unwrap())
            .collect();

        assert!(is_limit(list.get_or_subscribe("id")));
    }

    #[test]
    fn ack_waiters_per_id_are_limited() {
        let mut list = list(10);
        list.store("id", message("hello"), TTL).unwrap();
        let _receivers: Vec<_> = (0..MAX_WAITERS_PER_ENTRY)
            .map(|_| list.subscribe_ack("id").unwrap())
            .collect();

        assert!(is_limit(list.subscribe_ack("id")));
    }

    #[test]
    fn closed_waiters_do_not_count_toward_the_per_id_limit() {
        // Regression test: a reconnecting consumer must not be blocked by its own
        // disconnected requests until the next cleanup.
        let mut list = list(10);
        list.store("acked", message("hello"), TTL).unwrap();
        for _ in 0..MAX_WAITERS_PER_ENTRY {
            drop(list.get_or_subscribe("waiting").unwrap());
            drop(list.subscribe_ack("acked").unwrap());
        }

        assert!(list.get_or_subscribe("waiting").is_ok());
        assert!(list.subscribe_ack("acked").is_ok());
    }

    #[test]
    fn new_id_beyond_waiting_id_limit_is_rejected() {
        // Regression test: unique IDs must not grow the waiter map without bound.
        let mut list = list(2);
        let _waiting_a = list.get_or_subscribe("a").unwrap();
        let _waiting_b = list.get_or_subscribe("b").unwrap();

        assert!(is_limit(list.get_or_subscribe("c")));
    }

    #[test]
    fn existing_id_accepts_waiters_at_waiting_id_limit() {
        let mut list = list(2);
        let _waiting_a = list.get_or_subscribe("a").unwrap();
        let _waiting_b = list.get_or_subscribe("b").unwrap();

        assert!(list.get_or_subscribe("a").is_ok());
    }

    #[test]
    fn new_id_reuses_slot_of_disconnected_waiters() {
        let mut list = list(2);
        let _waiting_a = list.get_or_subscribe("a").unwrap();
        drop(list.get_or_subscribe("b").unwrap());

        assert!(list.get_or_subscribe("c").is_ok());
    }

    #[test]
    fn full_map_is_pruned_at_most_once_per_interval() {
        // Regression test: a flood of new IDs at a full map must not run a full
        // prune per request while holding the global lock.
        let mut list = list(2);
        let _waiting_a = list.get_or_subscribe("a").unwrap();
        drop(list.get_or_subscribe("b").unwrap());
        drop(list.get_or_subscribe("c").expect("prune frees b"));

        assert!(is_limit(list.get_or_subscribe("d")));
    }

    // ========== remove_expired ==========

    #[tokio::test]
    async fn remove_expired_keeps_waiters_without_entry() {
        // Regression test: consumer arrives before producer, cleanup must not drop the waiter
        let mut list = list(10);
        let receiver = pending(list.get_or_subscribe("id").unwrap());

        list.remove_expired().unwrap();
        list.store("id", message("delayed"), TTL).unwrap();

        assert_eq!(receiver.await.unwrap(), message("delayed"));
    }

    #[tokio::test]
    async fn remove_expired_drops_ack_waiters_of_expired_entries() {
        let mut list = list(10);
        list.store("expires", message("hello"), Duration::from_millis(200))
            .unwrap();
        list.store("stays", message("hello"), TTL).unwrap();
        let expiring = pending(list.subscribe_ack("expires").unwrap().unwrap());
        let staying = pending(list.subscribe_ack("stays").unwrap().unwrap());
        tokio::time::sleep(Duration::from_millis(250)).await;

        assert_eq!(list.remove_expired().unwrap(), 1);

        assert!(is_closed(expiring));
        list.ack("stays").unwrap();
        assert!(staying.await.is_ok());
    }

    #[tokio::test]
    async fn remove_expired_keeps_waiters_for_the_next_message() {
        // Regression test: a consumer waiting after an ACK got 404 when the
        // ACKed entry expired, instead of the next message.
        let mut list = list(10);
        list.store("id", message("first"), Duration::from_millis(200))
            .unwrap();
        list.ack("id").unwrap();
        let receiver = pending(list.get_or_subscribe("id").unwrap());
        tokio::time::sleep(Duration::from_millis(250)).await;

        assert_eq!(list.remove_expired().unwrap(), 1);
        list.store("id", message("second"), TTL).unwrap();

        assert_eq!(receiver.await.unwrap(), message("second"));
    }

    #[test]
    fn remove_expired_frees_ids_without_open_waiters() {
        let mut list = list(1);
        drop(list.get_or_subscribe("a").unwrap());
        drop(list.get_or_subscribe("b").expect("prune frees a"));
        assert!(is_limit(list.get_or_subscribe("c")));

        list.remove_expired().unwrap();

        assert!(list.get_or_subscribe("c").is_ok());
    }
}
