//! In-memory backend for relay entries. Entries are lost on restart.

use std::{collections::HashMap, time::Duration};

use anyhow::Result;

use super::{expires_at, now_millis, Capacity, InsertOutcome, Message, StoredEntry};
use crate::client_ip::ClientNetwork;

struct Entry {
    /// `None` once the entry is ACKed.
    message: Option<Message>,
    /// The client that stored the message.
    owner: ClientNetwork,
    expires_at: i64,
    /// Orders entries by when they were stored, to delete the oldest ACKed
    /// entry first when making room.
    sequence: u64,
}

impl Entry {
    fn is_live(&self) -> bool {
        self.expires_at > now_millis()
    }
}

/// Entries in a `HashMap`.
pub(crate) struct EntryRepository {
    entries: HashMap<String, Entry>,
    capacity: Capacity,
    next_sequence: u64,
}

impl EntryRepository {
    pub fn new(capacity: Capacity) -> Self {
        Self {
            entries: HashMap::new(),
            capacity,
            next_sequence: 0,
        }
    }

    /// Stores `message` for `id` with `owner` as its owner, replacing any
    /// previous entry and its ACK.
    ///
    /// The owner must be below its limit, and a new ID in a full store first
    /// needs room; see the module docs of [`super`].
    pub fn insert(
        &mut self,
        id: &str,
        message: &Message,
        ttl: Duration,
        owner: ClientNetwork,
    ) -> Result<InsertOutcome> {
        if self.count_other_unacked_entries(owner, id) >= self.capacity.max_entries_per_client {
            return Ok(InsertOutcome::ClientLimitReached);
        }

        let mut removed_ids = Vec::new();
        if !self.entries.contains_key(id) {
            match self.make_room() {
                Some(ids) => removed_ids = ids,
                None => return Ok(InsertOutcome::StoreFull),
            }
        }

        self.entries.insert(
            id.to_string(),
            Entry {
                message: Some(message.clone()),
                owner,
                expires_at: expires_at(ttl),
                sequence: self.next_sequence,
            },
        );
        self.next_sequence += 1;
        Ok(InsertOutcome::Stored { removed_ids })
    }

    /// Counts the live unACKed entries that `owner` stored, except the one
    /// for `id`, which an insert for `id` replaces.
    fn count_other_unacked_entries(&self, owner: ClientNetwork, id: &str) -> usize {
        let now = now_millis();
        self.entries
            .iter()
            .filter(|(entry_id, entry)| {
                entry.owner == owner
                    && entry.message.is_some()
                    && entry.expires_at > now
                    && entry_id.as_str() != id
            })
            .count()
    }

    /// Deletes expired entries, then ACKed ones stored longest ago, until there is room for
    /// one more. Returns their IDs, or `None` without deleting anything if
    /// that isn't enough.
    fn make_room(&mut self) -> Option<Vec<String>> {
        let excess = (self.entries.len() + 1).saturating_sub(self.capacity.max_entries);
        if excess == 0 {
            return Some(Vec::new());
        }
        let removable = self
            .entries
            .values()
            .filter(|entry| !entry.is_live() || entry.message.is_none())
            .count();
        if removable < excess {
            return None;
        }

        let mut removed_ids = self.remove_expired_ids();
        let mut acked: Vec<(u64, String)> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.message.is_none())
            .map(|(id, entry)| (entry.sequence, id.clone()))
            .collect();
        acked.sort_unstable();
        for (_, id) in acked
            .into_iter()
            .take(excess.saturating_sub(removed_ids.len()))
        {
            self.entries.remove(&id);
            removed_ids.push(id);
        }
        Some(removed_ids)
    }

    /// Returns the live entry for `id`.
    pub fn get(&self, id: &str) -> Result<Option<StoredEntry>> {
        Ok(self
            .entries
            .get(id)
            .filter(|entry| entry.is_live())
            .map(|entry| match &entry.message {
                Some(message) => StoredEntry::Unacked(message.clone()),
                None => StoredEntry::Acked,
            }))
    }

    /// ACKs the live entry for `id` and deletes its body.
    ///
    /// Returns false if there is no live entry.
    pub fn ack(&mut self, id: &str) -> Result<bool> {
        match self.entries.get_mut(id).filter(|entry| entry.is_live()) {
            Some(entry) => {
                entry.message = None;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Deletes all expired entries and returns their IDs.
    pub fn remove_expired(&mut self) -> Result<Vec<String>> {
        Ok(self.remove_expired_ids())
    }

    fn remove_expired_ids(&mut self) -> Vec<String> {
        let expired_ids: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, entry)| !entry.is_live())
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired_ids {
            self.entries.remove(id);
        }
        expired_ids
    }
}
