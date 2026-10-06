//! Tool implementations, grouped by the layer they mostly read (knowledge.md "Tools").

mod appmap;
mod errors;
mod explain;
mod files;
mod glossary;
mod history;
mod memory;
mod symbols;

use mage_core::ports::knowledge::{KnowledgeError, PageReq, Paged, SymbolHit};
use serde_json::{Value, json};

use super::envelope::{StoreKey, ToolError};

/// One page of a store-paged list resumed at `key`: the store page at `key.after` minus the
/// `key.skip` items already returned. Returns the items and the store's `next`.
pub(crate) async fn store_page<T, F, Fut>(key: &StoreKey, limit: u32, fetch: F) -> Result<(Vec<T>, Option<String>), ToolError>
where
    F: FnOnce(PageReq) -> Fut,
    Fut: Future<Output = Result<Paged<T>, KnowledgeError>>,
{
    let page = fetch(PageReq { limit: limit.saturating_add(key.skip).max(1), after: key.after.clone() }).await?;
    let items = page.items.into_iter().skip(key.skip as usize).collect();
    Ok((items, page.next))
}

/// Keyset after returning `kept` of the `fetched` items of a page started at `key`.
pub(crate) fn resume_key(key: &StoreKey, kept: usize, fetched: usize, store_next: Option<String>) -> Option<StoreKey> {
    if kept < fetched {
        Some(StoreKey { after: key.after.clone(), skip: key.skip + u32::try_from(kept).unwrap_or(u32::MAX) })
    } else {
        store_next.map(|after| StoreKey { after: Some(after), skip: 0 })
    }
}

/// `{"name", "kind"}` of a symbol.
pub(crate) fn symbol_brief(hit: &SymbolHit) -> Value {
    json!({ "name": hit.def.name, "kind": hit.def.kind })
}

/// RFC 3339 text of a timestamp (commit and resolution times in tool output).
pub(crate) fn rfc3339(at: time::OffsetDateTime) -> String {
    at.format(&time::format_description::well_known::Rfc3339).unwrap_or_else(|_| at.unix_timestamp().to_string())
}
