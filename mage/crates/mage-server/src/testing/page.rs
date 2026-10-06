//! Keyset paging for the fakes: items carry an ordered key; the cursor is the encoded key of the
//! last item of a page and the next page starts strictly after it.

use mage_core::ports::common::{Cursor, Page, PageRequest};
use uuid::Uuid;

/// `items` with their keys, in any order: sorted by key, those after `after`, at most the page's
/// limit, with a cursor when more remain.
pub(crate) fn keyset_page<K: Ord, T>(mut items: Vec<(K, T)>, after: Option<&K>, page: &PageRequest, encode: impl Fn(&K) -> String) -> Page<T> {
    items.sort_by(|a, b| a.0.cmp(&b.0));
    let mut rest: Vec<(K, T)> = match after {
        Some(after) => items.into_iter().filter(|(key, _)| key > after).collect(),
        None => items,
    };
    let limit = page.effective_limit();
    let more = rest.len() > limit;
    rest.truncate(limit);
    let next = if more { rest.last().map(|(key, _)| Cursor(encode(key))) } else { None };
    Page { items: rest.into_iter().map(|(_, item)| item).collect(), next }
}

/// Decodes a uuid cursor (the pg adapters use the same encoding).
pub(crate) fn uuid_after(page: &PageRequest) -> Result<Option<Uuid>, String> {
    page.after.as_ref().map(|c| Uuid::try_parse(&c.0).map_err(|_| format!("bad cursor {:?}", c.0))).transpose()
}
