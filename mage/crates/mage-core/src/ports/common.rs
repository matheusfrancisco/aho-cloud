//! Common port types (storage.md "Common types"): keyset pagination and optimistic concurrency.

use serde::{Deserialize, Serialize};

/// Opaque keyset token. Adapters encode it; callers only pass it back unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Cursor(pub String);

/// One page request. `limit` is clamped to `1..=200` by every adapter ([`Self::effective_limit`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRequest {
    pub after: Option<Cursor>,
    pub limit: u16,
}

impl PageRequest {
    pub const MAX_LIMIT: u16 = 200;

    /// First page of at most `limit` items.
    pub fn first(limit: u16) -> Self {
        Self { after: None, limit }
    }

    /// The page after `cursor`, with the same limit.
    pub fn after(&self, cursor: Cursor) -> Self {
        Self { after: Some(cursor), limit: self.limit }
    }

    /// `limit` clamped to `1..=MAX_LIMIT`.
    pub fn effective_limit(&self) -> usize {
        usize::from(self.limit.clamp(1, Self::MAX_LIMIT))
    }
}

/// One page of results; `next` is `None` on the last page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<Cursor>,
}

/// A value with its row revision, for compare-and-set writes (`expected_rev`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Versioned<T> {
    pub value: T,
    pub rev: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_is_clamped_to_one_through_two_hundred() {
        assert_eq!(PageRequest::first(0).effective_limit(), 1);
        assert_eq!(PageRequest::first(50).effective_limit(), 50);
        assert_eq!(PageRequest::first(u16::MAX).effective_limit(), 200);
        let next = PageRequest::first(7).after(Cursor("c".into()));
        assert_eq!(next, PageRequest { after: Some(Cursor("c".into())), limit: 7 });
    }
}
