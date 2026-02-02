use std::collections::HashMap;
use std::time::Instant;

use axum::body::Bytes;
use tokio::sync::oneshot;

/// A message containing body and optional content type.
#[derive(Clone)]
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
#[derive(Default)]
pub struct WaitingList {
    pending_producers: HashMap<String, WaitingProducer>,
    pending_consumers: HashMap<String, WaitingConsumer>,
    cache: HashMap<String, CachedValue>,
}

impl WaitingList {
    pub fn remove_producer(&mut self, id: &str) -> Option<WaitingProducer> {
        self.pending_producers.remove(id)
    }

    pub fn insert_producer(
        &mut self,
        id: &str,
        body: Bytes,
        content_type: Option<String>,
    ) -> oneshot::Receiver<()> {
        let (producer, completion_receiver) = WaitingProducer::new(body, content_type);
        self.pending_producers.insert(id.to_string(), producer);
        completion_receiver
    }

    pub fn remove_consumer(&mut self, id: &str) -> Option<WaitingConsumer> {
        self.pending_consumers.remove(id)
    }

    pub fn insert_consumer(&mut self, id: &str) -> oneshot::Receiver<Message> {
        let (consumer, message_receiver) = WaitingConsumer::new();
        self.pending_consumers.insert(id.to_string(), consumer);
        message_receiver
    }

    /// Gets a cached value if it exists and hasn't expired.
    pub fn get_cached(&self, id: &str) -> Option<Message> {
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
    pub fn insert_cached(
        &mut self,
        id: &str,
        body: Bytes,
        content_type: Option<String>,
        ttl: std::time::Duration,
    ) {
        self.cache
            .insert(id.to_string(), CachedValue::new(body, content_type, ttl));
    }

    /// Removes a value from the cache and returns it if it existed.
    pub fn remove_cached(&mut self, id: &str) -> Option<CachedValue> {
        self.cache.remove(id)
    }

    /// Removes expired entries from the cache. Returns the number removed.
    pub fn cleanup_expired_cache(&mut self) -> usize {
        let before = self.cache.len();
        self.cache.retain(|_, v| !v.is_expired());
        before - self.cache.len()
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
