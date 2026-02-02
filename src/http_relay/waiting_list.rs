use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::time::Instant;

use axum::body::Bytes;
use lru::LruCache;
use tokio::sync::oneshot;

/// A message containing body and optional content type.
#[derive(Clone, Debug)]
pub struct Message {
    pub body: Bytes,
    pub content_type: Option<String>,
}

/// A cached value with its expiration time.
pub struct CachedValue {
    /// The cached payload.
    pub body: Bytes,
    /// The content type of the cached payload.
    pub content_type: Option<String>,
    /// When this cached value expires.
    pub expires_at: Instant,
}

impl CachedValue {
    /// Creates a new cached value that expires after the given duration.
    pub fn new(body: Bytes, content_type: Option<String>, ttl: std::time::Duration) -> Self {
        Self {
            body,
            content_type,
            expires_at: Instant::now() + ttl,
        }
    }

    /// Returns true if this cached value has expired.
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

/// A list of waiting producers and consumers, plus cached values.
pub struct WaitingList {
    pending_producers: HashMap<String, WaitingProducer>,
    pending_consumers: HashMap<String, WaitingConsumer>,
    cache: LruCache<String, CachedValue>,
    max_pending: usize,
}

impl Default for WaitingList {
    fn default() -> Self {
        Self::new(10_000, 10_000)
    }
}

/// Error returned when a limit is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitError {
    /// Maximum pending requests reached.
    PendingLimitReached,
}

impl WaitingList {
    /// Creates a new WaitingList with the specified limits.
    pub fn new(max_pending: usize, max_cache: usize) -> Self {
        // LruCache requires NonZeroUsize, use 1 as minimum if 0 is passed
        let cache_cap = NonZeroUsize::new(max_cache).unwrap_or(NonZeroUsize::MIN);
        Self {
            pending_producers: HashMap::new(),
            pending_consumers: HashMap::new(),
            cache: LruCache::new(cache_cap),
            max_pending,
        }
    }

    /// Returns the current count of pending requests (producers + consumers).
    pub fn pending_count(&self) -> usize {
        self.pending_producers.len() + self.pending_consumers.len()
    }

    pub fn remove_producer(&mut self, id: &str) -> Option<WaitingProducer> {
        self.pending_producers.remove(id)
    }

    pub fn insert_producer(
        &mut self,
        id: &str,
        body: Bytes,
        content_type: Option<String>,
    ) -> Result<oneshot::Receiver<()>, LimitError> {
        if self.pending_count() >= self.max_pending {
            return Err(LimitError::PendingLimitReached);
        }
        let (producer, completion_receiver) = WaitingProducer::new(body, content_type);
        self.pending_producers.insert(id.to_string(), producer);
        Ok(completion_receiver)
    }

    pub fn remove_consumer(&mut self, id: &str) -> Option<WaitingConsumer> {
        self.pending_consumers.remove(id)
    }

    pub fn insert_consumer(&mut self, id: &str) -> Result<oneshot::Receiver<Message>, LimitError> {
        if self.pending_count() >= self.max_pending {
            return Err(LimitError::PendingLimitReached);
        }
        let (consumer, message_receiver) = WaitingConsumer::new();
        self.pending_consumers.insert(id.to_string(), consumer);
        Ok(message_receiver)
    }

    /// Gets a cached value if it exists and hasn't expired.
    /// Accessing a value promotes it in the LRU cache.
    pub fn get_cached(&mut self, id: &str) -> Option<Message> {
        self.cache.get(id).and_then(|cached| {
            if cached.is_expired() {
                None
            } else {
                Some(Message {
                    body: cached.body.clone(),
                    content_type: cached.content_type.clone(),
                })
            }
        })
    }

    /// Inserts a value into the cache with the given TTL.
    /// If the cache is full, automatically evicts the least recently used entry.
    pub fn insert_cached(
        &mut self,
        id: &str,
        body: Bytes,
        content_type: Option<String>,
        ttl: std::time::Duration,
    ) {
        self.cache
            .push(id.to_string(), CachedValue::new(body, content_type, ttl));
    }

    /// Removes a value from the cache and returns it if it existed.
    pub fn remove_cached(&mut self, id: &str) -> Option<CachedValue> {
        self.cache.pop(id)
    }

    /// Removes expired entries from the cache. Returns the number removed.
    pub fn cleanup_expired_cache(&mut self) -> usize {
        // Collect expired keys first to avoid borrow issues
        let expired_keys: Vec<String> = self
            .cache
            .iter()
            .filter(|(_, v)| v.is_expired())
            .map(|(k, _)| k.clone())
            .collect();

        let count = expired_keys.len();
        for key in expired_keys {
            self.cache.pop(&key);
        }
        count
    }
}

#[cfg(test)]
impl WaitingList {
    pub fn is_empty(&self) -> bool {
        self.pending_producers.is_empty() && self.pending_consumers.is_empty()
    }

    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }
}

/// A producer that is waiting for a consumer to request data.
pub struct WaitingProducer {
    /// The payload of the producer.
    pub body: Bytes,
    /// The content type of the payload.
    pub content_type: Option<String>,
    /// The sender to notify the producer that the request has been resolved.
    pub completion: oneshot::Sender<()>,
}

impl WaitingProducer {
    fn new(body: Bytes, content_type: Option<String>) -> (Self, oneshot::Receiver<()>) {
        let (completion_sender, completion_receiver) = oneshot::channel();
        (
            Self {
                body,
                content_type,
                completion: completion_sender,
            },
            completion_receiver,
        )
    }
}

/// A consumer that is waiting for a producer to send data.
pub struct WaitingConsumer {
    /// The sender to notify the consumer that the request has been resolved.
    pub message_sender: oneshot::Sender<Message>,
}

impl WaitingConsumer {
    fn new() -> (Self, oneshot::Receiver<Message>) {
        let (message_sender, message_receiver) = oneshot::channel();
        (Self { message_sender }, message_receiver)
    }
}
