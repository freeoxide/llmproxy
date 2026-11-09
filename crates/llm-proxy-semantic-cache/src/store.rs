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

    /// Inserts an entry, bounding the caller's total across all of its
    /// `(provider, model)` buckets to `max_entries` by evicting the
    /// caller's oldest entries wherever they sit.
    pub(crate) fn insert(
        &mut self,
        caller: &str,
        provider: &str,
        model: &str,
        entry: Entry,
        max_entries: usize,
    ) {
        let cap = max_entries.max(1);
        self.buckets
            .entry((caller.to_owned(), provider.to_owned(), model.to_owned()))
            .or_default()
            .push(entry);
        // The bound is per caller, not per bucket (docs/data-class-policy.md):
        // a caller naming many model strings must not grow the cache
        // without limit. Only this caller's buckets are counted and evicted.
        while self.caller_len(caller) > cap {
            let Some(oldest) = self
                .buckets
                .iter_mut()
                .filter(|((bucket_caller, _, _), entries)| {
                    bucket_caller == caller && !entries.is_empty()
                })
                // Buckets keep insertion order, so each bucket's oldest
                // entry is its first; both sides of the compare are
                // `Some` because of the filter above.
                .min_by(|(_, left), (_, right)| {
                    left.first()
                        .map(|entry| entry.stored_at)
                        .cmp(&right.first().map(|entry| entry.stored_at))
                })
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(bucket) = self.buckets.get_mut(&oldest) {
                bucket.drain(..1);
            }
            if self
                .buckets
                .get(&oldest)
                .is_some_and(std::vec::Vec::is_empty)
            {
                self.buckets.remove(&oldest);
            }
        }
    }

    /// How many entries one caller holds across all of its buckets.
    fn caller_len(&self, caller: &str) -> usize {
        self.buckets
            .iter()
            .filter(|((bucket_caller, _, _), _)| bucket_caller == caller)
            .map(|(_, entries)| entries.len())
            .sum()
    }
}
