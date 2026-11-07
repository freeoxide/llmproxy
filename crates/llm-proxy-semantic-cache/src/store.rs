//! The in-process store behind the semantic cache: per-caller buckets of
//! bounded entries with TTL expiry. Vectors and responses live only
//! here, for the configured TTL, and nowhere else.

use std::collections::HashMap;

use llm_proxy_protocol::core::CoreResponse;
use std::time::Duration;

use time::OffsetDateTime;

use crate::embedder::cosine_similarity;

/// One stored response with the embedding of the prompt that produced it.
pub(crate) struct Entry {
    pub(crate) embedding: Vec<f32>,
    pub(crate) response: CoreResponse,
    pub(crate) stored_at: OffsetDateTime,
}

/// Bucket key: caller, provider, model — the caller scopes everything.
type BucketKey = (String, String, String);

#[derive(Default)]
pub(crate) struct Store {
    buckets: HashMap<BucketKey, Vec<Entry>>,
}

impl Store {
    /// Drops entries older than `ttl`.
    pub(crate) fn expire(&mut self, now: OffsetDateTime, ttl: Duration) {
        for entries in self.buckets.values_mut() {
            entries.retain(|entry| now - entry.stored_at < ttl);
        }
        self.buckets.retain(|_, entries| !entries.is_empty());
    }

    /// The closest entry at or above `threshold`, if any.
    pub(crate) fn nearest(
        &self,
        caller: &str,
        provider: &str,
        model: &str,
        query: &[f32],
        threshold: f32,
    ) -> Option<&Entry> {
        let entries =
            self.buckets
                .get(&(caller.to_owned(), provider.to_owned(), model.to_owned()))?;
        entries
            .iter()
            .map(|entry| (cosine_similarity(query, &entry.embedding), entry))
            .filter(|(similarity, _)| *similarity >= threshold)
            .max_by(|(left, _), (right, _)| {
                left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(_, entry)| entry)
    }

    /// Inserts an entry, bounding its bucket to `max_entries` by evicting
    /// the oldest.
    pub(crate) fn insert(
        &mut self,
        caller: &str,
        provider: &str,
        model: &str,
        entry: Entry,
        max_entries: usize,
    ) {
        let bucket = self
            .buckets
            .entry((caller.to_owned(), provider.to_owned(), model.to_owned()))
            .or_default();
        bucket.push(entry);
        let overflow = bucket.len().saturating_sub(max_entries.max(1));
        if overflow > 0 {
            bucket.drain(..overflow);
        }
    }
}
