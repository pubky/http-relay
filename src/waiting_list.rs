//! Hands messages and ACKs from the requests that send them to the requests
//! that wait for them.
//!
//! Entries live in the [`EntryRepository`]. Requests that wait for a message or
//! an ACK are in-memory oneshot channels, which can't be persisted.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use crate::client_ip::ClientNetwork;
use crate::storage::{EntryRepository, InsertOutcome, Message, StoredEntry};
use crate::waiting_clients::{WaitPermit, WaitingClients};

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
    /// Keep the permit until the wait ends: it counts the waiting request
    /// against its client's limit.
    Pending(oneshot::Receiver<T>, WaitPermit),
}

/// Limits on the requests that wait.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WaitingLimits {
    /// Maximum number of distinct IDs with waiting requests.
    pub max_ids: usize,
    /// Maximum open waiting requests of one client.
    pub max_requests_per_client: usize,
}

/// Why the waiting list couldn't serve a request.
#[derive(Debug)]
pub(crate) enum WaitingListError {
    /// Maximum waiters for this ID, or maximum number of waiting IDs, reached.
    WaiterLimitReached,
    /// The client already has as many open waiting requests as it may.
    /// Nothing was stored.
    ClientWaitLimitReached,
    /// The client already has as many unACKed messages stored as it may.
    /// Nothing was stored. One of its messages expires after `retry_after`.
    ClientLimitReached {
        retry_after: Duration,
    },
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
    /// Open waiting requests per client, so that no client can take all
    /// `max_waiting_ids`.
    waiting_clients: WaitingClients,
}

impl WaitingList {
    pub fn new(repository: EntryRepository, limits: WaitingLimits) -> Self {
        Self {
            repository,
            waiters: HashMap::new(),
            max_waiting_ids: limits.max_ids,
            last_full_map_prune: None,
            waiting_clients: WaitingClients::new(limits.max_requests_per_client),
        }
    }

    /// Stores a message from `producer` and hands it to the requests waiting
    /// for one.
    ///
    /// Requests waiting for the ACK of the replaced message are dropped. If
    /// the message can't be stored, nothing changes and the waiting requests
    /// keep waiting.
    pub fn store(
        &mut self,
        id: &str,
        message: Message,
        ttl: Duration,
        producer: ClientNetwork,
    ) -> Result<(), WaitingListError> {
        self.deliver(id, message, ttl, producer)?;
        Ok(())
    }

    /// Stores a message and waits for its ACK (link producer).
    ///
    /// Checks for waiter room before storing: a producer rejected with
    /// `WaiterLimitReached` or `ClientWaitLimitReached` must be able to retry
    /// without its first message having been delivered already.
    #[cfg(feature = "link-compat")]
    pub fn store_and_subscribe_ack(
        &mut self,
        id: &str,
        message: Message,
        ttl: Duration,
        producer: ClientNetwork,
    ) -> Result<Delivery<()>, WaitingListError> {
        let permit = self.start_waiting(producer)?;
        self.ensure_waiter_capacity(id)?;
        let acked = self.deliver(id, message, ttl, producer)?;
        if acked {
            return Ok(Delivery::Ready(()));
        }
        // Can't fail with `WaiterLimitReached` after the check above: `deliver`
        // only removes waiters, and it removed all waiters for `id`.
        self.add_ack_waiter(id, permit)
    }

    /// Returns the stored message, or waits for the next one (inbox consumer).
    pub fn get_or_subscribe(
        &mut self,
        id: &str,
        client: ClientNetwork,
    ) -> Result<Delivery<Message>, WaitingListError> {
        self.receive(id, false, client)
    }

    /// Returns the stored message, or waits for the next one, and ACKs it
    /// in the same step (link consumer), so it can't ACK a newer message.
    #[cfg(feature = "link-compat")]
    pub fn take_or_subscribe(
        &mut self,
        id: &str,
        client: ClientNetwork,
    ) -> Result<Delivery<Message>, WaitingListError> {
        self.receive(id, true, client)
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
    pub fn subscribe_ack(
        &mut self,
        id: &str,
        client: ClientNetwork,
    ) -> Result<Option<Delivery<()>>, WaitingListError> {
        match self.repository.get(id)? {
            None => Ok(None),
            Some(StoredEntry::Acked) => Ok(Some(Delivery::Ready(()))),
            Some(StoredEntry::Unacked(_)) => {
                let permit = self.start_waiting(client)?;
                self.add_ack_waiter(id, permit).map(Some)
            }
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
        producer: ClientNetwork,
    ) -> Result<bool, WaitingListError> {
        match self.repository.insert(id, &message, ttl, producer)? {
            InsertOutcome::Stored { removed_ids } => {
                // Entries removed to make room are expired or ACKed. ACK
                // waiters of an expired entry would otherwise wait in vain.
                for removed_id in &removed_ids {
                    self.take_ack_waiters(removed_id);
                }
            }
            InsertOutcome::ClientLimitReached { retry_after } => {
                return Err(WaitingListError::ClientLimitReached { retry_after })
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
            // A link consumer already has the message, so the delivery
            // succeeded. Failing it would make the producer retry and send the
            // message twice. The cost: the entry stays unACKed until it
            // expires, and the next consumer gets it again.
            if let Err(error) = self.repository.ack(id) {
                tracing::error!(?error, "Failed to store the ACK of a delivered message");
            }
        }
        Ok(acked)
    }

    /// Returns the stored message at once, also to a client at its limit of
    /// waiting requests, or waits for the next one.
    fn receive(
        &mut self,
        id: &str,
        ack_on_receive: bool,
        client: ClientNetwork,
    ) -> Result<Delivery<Message>, WaitingListError> {
        if let Some(StoredEntry::Unacked(message)) = self.repository.get(id)? {
            if ack_on_receive {
                self.ack(id)?;
            }
            return Ok(Delivery::Ready(message));
        }

        let permit = self.start_waiting(client)?;
        let (sender, receiver) = oneshot::channel();
        let waiter = MessageWaiter {
            sender,
            acks_on_delivery: ack_on_receive,
        };
        if self.waiters_for(id)?.messages.add(waiter) {
            Ok(Delivery::Pending(receiver, permit))
        } else {
            Err(WaitingListError::WaiterLimitReached)
        }
    }

    fn add_ack_waiter(
        &mut self,
        id: &str,
        permit: WaitPermit,
    ) -> Result<Delivery<()>, WaitingListError> {
        let (sender, receiver) = oneshot::channel();
        if self.waiters_for(id)?.acks.add(sender) {
            Ok(Delivery::Pending(receiver, permit))
        } else {
            Err(WaitingListError::WaiterLimitReached)
        }
    }

    /// Counts a waiting request from `client`, or fails if the client already
    /// has as many open waiting requests as it may.
    fn start_waiting(&self, client: ClientNetwork) -> Result<WaitPermit, WaitingListError> {
        self.waiting_clients
            .try_start(client)
            .ok_or(WaitingListError::ClientWaitLimitReached)
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
    use crate::storage::{test_client, test_repository, test_repository_with_capacity, Capacity};

    const TTL: Duration = Duration::from_secs(60);

    fn alice() -> ClientNetwork {
        test_client("198.51.100.1")
    }

    fn bob() -> ClientNetwork {
        test_client("198.51.100.2")
    }

    fn unlimited_clients(max_ids: usize) -> WaitingLimits {
        WaitingLimits {
            max_ids,
            max_requests_per_client: usize::MAX,
        }
    }

    fn list(max_ids: usize) -> WaitingList {
        WaitingList::new(test_repository(100), unlimited_clients(max_ids))
    }

    fn message(body: &'static str) -> Message {
        Message {
            body: Bytes::from_static(body.as_bytes()),
            content_type: Some("text/plain".to_string()),
        }
    }

    fn pending<T>(delivery: Delivery<T>) -> oneshot::Receiver<T> {
        match delivery {
            Delivery::Pending(receiver, _permit) => receiver,
            Delivery::Ready(_) => panic!("expected to wait"),
        }
    }

    fn ready<T>(delivery: Delivery<T>) -> T {
        match delivery {
            Delivery::Ready(value) => value,
            Delivery::Pending(..) => panic!("expected a value now"),
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
        list.store("id", message("hello"), TTL, alice()).unwrap();

        let delivery = list.get_or_subscribe("id", alice()).unwrap();

        assert_eq!(ready(delivery), message("hello"));
    }

    #[tokio::test]
    async fn waiting_consumer_receives_next_message() {
        let mut list = list(10);
        let receiver = pending(list.get_or_subscribe("id", alice()).unwrap());

        list.store("id", message("hello"), TTL, alice()).unwrap();

        assert_eq!(receiver.await.unwrap(), message("hello"));
    }

    #[test]
    fn acked_message_is_not_returned_again() {
        let mut list = list(10);
        list.store("id", message("hello"), TTL, alice()).unwrap();
        list.ack("id").unwrap();

        assert!(matches!(
            list.get_or_subscribe("id", alice()).unwrap(),
            Delivery::Pending(..)
        ));
    }

    // ========== ACKs ==========

    #[test]
    fn ack_state_follows_the_entry() {
        let mut list = list(10);
        assert_eq!(list.is_acked("id").unwrap(), None);
        assert!(!list.ack("id").unwrap());

        list.store("id", message("hello"), TTL, alice()).unwrap();
        assert_eq!(list.is_acked("id").unwrap(), Some(false));

        assert!(list.ack("id").unwrap());
        assert_eq!(list.is_acked("id").unwrap(), Some(true));
    }

    #[tokio::test]
    async fn ack_notifies_ack_waiters() {
        let mut list = list(10);
        list.store("id", message("hello"), TTL, alice()).unwrap();
        let receiver = pending(list.subscribe_ack("id", alice()).unwrap().unwrap());

        list.ack("id").unwrap();

        assert!(receiver.await.is_ok());
    }

    #[test]
    fn subscribe_ack_depends_on_the_entry() {
        let mut list = list(10);
        assert!(list.subscribe_ack("id", alice()).unwrap().is_none());

        list.store("id", message("hello"), TTL, alice()).unwrap();
        list.ack("id").unwrap();

        ready(list.subscribe_ack("id", alice()).unwrap().unwrap());
    }

    #[tokio::test]
    async fn new_message_drops_ack_waiters_of_the_replaced_one() {
        let mut list = list(10);
        list.store("id", message("first"), TTL, alice()).unwrap();
        let receiver = pending(list.subscribe_ack("id", alice()).unwrap().unwrap());

        list.store("id", message("second"), TTL, alice()).unwrap();

        assert!(is_closed(receiver));
    }

    // ========== Full store ==========

    #[tokio::test]
    async fn full_store_rejects_message_and_keeps_waiters_waiting() {
        let mut list = WaitingList::new(test_repository(1), unlimited_clients(10));
        list.store("unacked", message("first"), TTL, alice())
            .unwrap();
        let mut receiver = pending(list.get_or_subscribe("waiting", alice()).unwrap());

        let result = list.store("waiting", message("second"), TTL, alice());

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
        let mut list = WaitingList::new(test_repository(1), unlimited_clients(10));
        list.store(
            "expires",
            message("hello"),
            Duration::from_millis(100),
            alice(),
        )
        .unwrap();
        let receiver = pending(list.subscribe_ack("expires", alice()).unwrap().unwrap());
        tokio::time::sleep(Duration::from_millis(150)).await;

        list.store("new", message("hello"), TTL, alice()).unwrap();

        assert!(is_closed(receiver));
    }

    // ========== Link: take_or_subscribe and store_and_subscribe_ack ==========

    #[cfg(feature = "link-compat")]
    #[test]
    fn take_acks_a_stored_message() {
        let mut list = list(10);
        list.store("id", message("hello"), TTL, alice()).unwrap();

        assert_eq!(
            ready(list.take_or_subscribe("id", alice()).unwrap()),
            message("hello")
        );
        assert_eq!(list.is_acked("id").unwrap(), Some(true));
    }

    #[cfg(feature = "link-compat")]
    #[tokio::test]
    async fn producer_is_acked_by_a_waiting_taker() {
        let mut list = list(10);
        let consumer = pending(list.take_or_subscribe("id", alice()).unwrap());

        let producer = list
            .store_and_subscribe_ack("id", message("hello"), TTL, alice())
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
        let consumer = pending(list.take_or_subscribe("id", alice()).unwrap());
        list.store("id", message("first"), TTL, alice()).unwrap();
        assert_eq!(consumer.await.unwrap(), message("first"));

        let producer = list
            .store_and_subscribe_ack("id", message("second"), TTL, alice())
            .unwrap();

        pending(producer);
    }

    #[cfg(feature = "link-compat")]
    #[tokio::test]
    async fn inbox_consumer_does_not_ack() {
        let mut list = list(10);
        let consumer = pending(list.get_or_subscribe("id", alice()).unwrap());

        let producer = list
            .store_and_subscribe_ack("id", message("hello"), TTL, alice())
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
        let _waiting = list.get_or_subscribe("occupied", alice()).unwrap();

        let result = list.store_and_subscribe_ack("new", message("rejected"), TTL, alice());

        assert!(is_limit(result));
        assert_eq!(list.is_acked("new").unwrap(), None);
    }

    #[cfg(feature = "link-compat")]
    #[tokio::test]
    async fn store_and_subscribe_ack_for_waiting_id_at_limit_succeeds() {
        let mut list = list(1);
        let consumer = pending(list.get_or_subscribe("id", alice()).unwrap());

        let result = list.store_and_subscribe_ack("id", message("hello"), TTL, alice());

        assert!(result.is_ok());
        assert_eq!(consumer.await.unwrap(), message("hello"));
    }

    #[cfg(feature = "link-compat")]
    #[test]
    fn store_and_subscribe_ack_replaces_ack_waiters_at_per_id_limit() {
        let mut list = list(10);
        list.store("id", message("first"), TTL, alice()).unwrap();
        let receivers: Vec<_> = (0..MAX_WAITERS_PER_ENTRY)
            .map(|_| pending(list.subscribe_ack("id", alice()).unwrap().unwrap()))
            .collect();

        let result = list.store_and_subscribe_ack("id", message("second"), TTL, alice());

        assert!(result.is_ok());
        for receiver in receivers {
            assert!(is_closed(receiver));
        }
    }

    #[cfg(feature = "link-compat")]
    #[test]
    fn store_and_subscribe_ack_in_full_store_does_not_wait() {
        let mut list = WaitingList::new(test_repository(1), unlimited_clients(1));
        list.store("unacked", message("first"), TTL, alice())
            .unwrap();

        let result = list.store_and_subscribe_ack("new", message("rejected"), TTL, alice());

        assert!(matches!(result, Err(WaitingListError::StoreFull)));
        // The rejected producer holds no waiting-ID slot.
        assert!(list.get_or_subscribe("other", alice()).is_ok());
    }

    // ========== Waiter limits ==========

    #[test]
    fn message_waiters_per_id_are_limited() {
        let mut list = list(10);
        let _receivers: Vec<_> = (0..MAX_WAITERS_PER_ENTRY)
            .map(|_| list.get_or_subscribe("id", alice()).unwrap())
            .collect();

        assert!(is_limit(list.get_or_subscribe("id", alice())));
    }

    #[test]
    fn ack_waiters_per_id_are_limited() {
        let mut list = list(10);
        list.store("id", message("hello"), TTL, alice()).unwrap();
        let _receivers: Vec<_> = (0..MAX_WAITERS_PER_ENTRY)
            .map(|_| list.subscribe_ack("id", alice()).unwrap())
            .collect();

        assert!(is_limit(list.subscribe_ack("id", alice())));
    }

    #[test]
    fn closed_waiters_do_not_count_toward_the_per_id_limit() {
        // Regression test: a reconnecting consumer must not be blocked by its own
        // disconnected requests until the next cleanup.
        let mut list = list(10);
        list.store("acked", message("hello"), TTL, alice()).unwrap();
        for _ in 0..MAX_WAITERS_PER_ENTRY {
            drop(list.get_or_subscribe("waiting", alice()).unwrap());
            drop(list.subscribe_ack("acked", alice()).unwrap());
        }

        assert!(list.get_or_subscribe("waiting", alice()).is_ok());
        assert!(list.subscribe_ack("acked", alice()).is_ok());
    }

    #[test]
    fn new_id_beyond_waiting_id_limit_is_rejected() {
        // Regression test: unique IDs must not grow the waiter map without bound.
        let mut list = list(2);
        let _waiting_a = list.get_or_subscribe("a", alice()).unwrap();
        let _waiting_b = list.get_or_subscribe("b", alice()).unwrap();

        assert!(is_limit(list.get_or_subscribe("c", alice())));
    }

    #[test]
    fn existing_id_accepts_waiters_at_waiting_id_limit() {
        let mut list = list(2);
        let _waiting_a = list.get_or_subscribe("a", alice()).unwrap();
        let _waiting_b = list.get_or_subscribe("b", alice()).unwrap();

        assert!(list.get_or_subscribe("a", alice()).is_ok());
    }

    #[test]
    fn new_id_reuses_slot_of_disconnected_waiters() {
        let mut list = list(2);
        let _waiting_a = list.get_or_subscribe("a", alice()).unwrap();
        drop(list.get_or_subscribe("b", alice()).unwrap());

        assert!(list.get_or_subscribe("c", alice()).is_ok());
    }

    #[test]
    fn full_map_is_pruned_at_most_once_per_interval() {
        // Regression test: a flood of new IDs at a full map must not run a full
        // prune per request while holding the global lock.
        let mut list = list(2);
        let _waiting_a = list.get_or_subscribe("a", alice()).unwrap();
        drop(list.get_or_subscribe("b", alice()).unwrap());
        drop(list.get_or_subscribe("c", alice()).expect("prune frees b"));

        assert!(is_limit(list.get_or_subscribe("d", alice())));
    }

    // ========== Waiting requests per client ==========

    fn list_with_client_limit(max_requests_per_client: usize) -> WaitingList {
        let limits = WaitingLimits {
            max_ids: 100,
            max_requests_per_client,
        };
        WaitingList::new(test_repository(100), limits)
    }

    fn is_client_limit<T>(result: Result<T, WaitingListError>) -> bool {
        matches!(result, Err(WaitingListError::ClientWaitLimitReached))
    }

    #[test]
    fn message_waiters_per_client_are_limited() {
        let mut list = list_with_client_limit(2);
        let _first = list.get_or_subscribe("a", alice()).unwrap();
        let _second = list.get_or_subscribe("b", alice()).unwrap();

        assert!(is_client_limit(list.get_or_subscribe("c", alice())));
        assert!(list.get_or_subscribe("c", bob()).is_ok());
    }

    #[test]
    fn ack_waiters_count_toward_the_client_limit() {
        let mut list = list_with_client_limit(1);
        list.store("a", message("hello"), TTL, bob()).unwrap();
        let _waiting = list.get_or_subscribe("b", alice()).unwrap();

        assert!(is_client_limit(list.subscribe_ack("a", alice())));
    }

    #[cfg(feature = "link-compat")]
    #[test]
    fn link_consumers_count_toward_the_client_limit() {
        let mut list = list_with_client_limit(1);
        let _waiting = list.get_or_subscribe("a", alice()).unwrap();

        assert!(is_client_limit(list.take_or_subscribe("b", alice())));
    }

    #[cfg(feature = "link-compat")]
    #[test]
    fn link_producer_over_the_client_limit_does_not_store() {
        let mut list = list_with_client_limit(1);
        let _waiting = list.get_or_subscribe("a", alice()).unwrap();

        let result = list.store_and_subscribe_ack("b", message("hello"), TTL, alice());

        assert!(is_client_limit(result));
        assert_eq!(list.is_acked("b").unwrap(), None);
    }

    #[test]
    fn stored_message_is_returned_to_a_client_at_its_limit() {
        let mut list = list_with_client_limit(1);
        list.store("stored", message("hello"), TTL, bob()).unwrap();
        let _waiting = list.get_or_subscribe("a", alice()).unwrap();

        let delivery = list.get_or_subscribe("stored", alice()).unwrap();

        assert_eq!(ready(delivery), message("hello"));
    }

    #[test]
    fn ended_wait_frees_its_slot() {
        // A client that disconnects and reconnects must not be blocked by its
        // own earlier request, even before the waiter map is pruned.
        let mut list = list_with_client_limit(1);
        drop(list.get_or_subscribe("a", alice()).unwrap());

        assert!(list.get_or_subscribe("a", alice()).is_ok());
    }

    #[test]
    fn waiter_rejected_per_id_frees_its_slot() {
        let mut list = list_with_client_limit(MAX_WAITERS_PER_ENTRY + 1);
        let _waiting: Vec<_> = (0..MAX_WAITERS_PER_ENTRY)
            .map(|_| list.get_or_subscribe("id", alice()).unwrap())
            .collect();
        assert!(is_limit(list.get_or_subscribe("id", alice())));

        assert!(list.get_or_subscribe("other", alice()).is_ok());
    }

    #[test]
    fn stored_messages_and_waiting_requests_are_counted_separately() {
        let repository = test_repository_with_capacity(Capacity {
            max_entries: 100,
            max_entries_per_client: 1,
        });
        let limits = WaitingLimits {
            max_ids: 100,
            max_requests_per_client: 1,
        };
        let mut list = WaitingList::new(repository, limits);
        list.store("stored", message("hello"), TTL, alice())
            .unwrap();

        let _waiting = list.get_or_subscribe("a", alice()).unwrap();
        assert!(matches!(
            list.store("other", message("hello"), TTL, alice()),
            Err(WaitingListError::ClientLimitReached { .. })
        ));
    }

    // ========== remove_expired ==========

    #[tokio::test]
    async fn remove_expired_keeps_waiters_without_entry() {
        // Regression test: consumer arrives before producer, cleanup must not drop the waiter
        let mut list = list(10);
        let receiver = pending(list.get_or_subscribe("id", alice()).unwrap());

        list.remove_expired().unwrap();
        list.store("id", message("delayed"), TTL, alice()).unwrap();

        assert_eq!(receiver.await.unwrap(), message("delayed"));
    }

    #[tokio::test]
    async fn remove_expired_drops_ack_waiters_of_expired_entries() {
        let mut list = list(10);
        list.store(
            "expires",
            message("hello"),
            Duration::from_millis(200),
            alice(),
        )
        .unwrap();
        list.store("stays", message("hello"), TTL, alice()).unwrap();
        let expiring = pending(list.subscribe_ack("expires", alice()).unwrap().unwrap());
        let staying = pending(list.subscribe_ack("stays", alice()).unwrap().unwrap());
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
        list.store("id", message("first"), Duration::from_millis(200), alice())
            .unwrap();
        list.ack("id").unwrap();
        let receiver = pending(list.get_or_subscribe("id", alice()).unwrap());
        tokio::time::sleep(Duration::from_millis(250)).await;

        assert_eq!(list.remove_expired().unwrap(), 1);
        list.store("id", message("second"), TTL, alice()).unwrap();

        assert_eq!(receiver.await.unwrap(), message("second"));
    }

    #[test]
    fn remove_expired_frees_ids_without_open_waiters() {
        let mut list = list(1);
        drop(list.get_or_subscribe("a", alice()).unwrap());
        drop(list.get_or_subscribe("b", alice()).expect("prune frees a"));
        assert!(is_limit(list.get_or_subscribe("c", alice())));

        list.remove_expired().unwrap();

        assert!(list.get_or_subscribe("c", alice()).is_ok());
    }
}
