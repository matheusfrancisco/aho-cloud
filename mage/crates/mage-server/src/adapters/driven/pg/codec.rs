//! Column codecs shared by the server pg adapters: fixed-size `bytea`, integer widths, `inet` as
//! text, closed enums as text, keyset cursors. A stored value that does not decode is
//! `DbError::Invalid` (the data does not match the code).

use std::net::IpAddr;

use mage_core::domain::CommitSha;
use mage_core::ports::common::{Cursor, Page, PageRequest};
use mage_infra::pg::DbError;
use uuid::Uuid;

/// `bytea` of exactly `N` bytes.
pub(crate) fn fixed<const N: usize>(bytes: Vec<u8>, column: &str) -> Result<[u8; N], DbError> {
    let len = bytes.len();
    bytes.try_into().map_err(|_| DbError::Invalid(format!("{column}: expected {N} bytes, got {len}")))
}

pub(crate) fn sha(bytes: Vec<u8>, column: &str) -> Result<CommitSha, DbError> {
    fixed::<20>(bytes, column).map(CommitSha::from_bytes)
}

pub(crate) fn opt_sha(bytes: Option<Vec<u8>>, column: &str) -> Result<Option<CommitSha>, DbError> {
    bytes.map(|b| sha(b, column)).transpose()
}

/// Unsigned value to a signed column.
pub(crate) fn to_i64(value: u64, column: &str) -> Result<i64, DbError> {
    i64::try_from(value).map_err(|_| DbError::Invalid(format!("{column}: {value} out of range")))
}

/// Signed column to an unsigned field (`u8`, `u16`, `u32`, `u64`).
pub(crate) fn to_unsigned<T: TryFrom<i64>>(value: i64, column: &str) -> Result<T, DbError> {
    T::try_from(value).map_err(|_| DbError::Invalid(format!("{column}: stored value {value} out of range")))
}

/// `inet` is bound as text (`$n::text::inet`) and read back with `host(col)`.
pub(crate) fn ip_text(ip: Option<IpAddr>) -> Option<String> {
    ip.map(|ip| ip.to_string())
}

pub(crate) fn parse_ip(text: Option<String>, column: &str) -> Result<Option<IpAddr>, DbError> {
    text.map(|t| t.parse().map_err(|_| DbError::Invalid(format!("{column}: bad address {t:?}")))).transpose()
}

/// A closed enum stored as text.
pub(crate) fn parse_enum<T>(value: &str, column: &str, parse: impl FnOnce(&str) -> Option<T>) -> Result<T, DbError> {
    parse(value).ok_or_else(|| DbError::Invalid(format!("{column}: unknown value {value:?}")))
}

/// `LIMIT` for a keyset page: one extra row tells whether another page exists.
pub(crate) fn page_limit(page: &PageRequest) -> i64 {
    page.effective_limit() as i64 + 1
}

/// Cut the lookahead row and build the cursor from the last returned item.
pub(crate) fn finish_page<T>(mut items: Vec<T>, page: &PageRequest, cursor: impl Fn(&T) -> String) -> Page<T> {
    let limit = page.effective_limit();
    let more = items.len() > limit;
    items.truncate(limit);
    let next = if more { items.last().map(|last| Cursor(cursor(last))) } else { None };
    Page { items, next }
}

/// Cursor holding one uuid (keyset on an id column).
pub(crate) fn uuid_cursor(cursor: Option<&Cursor>) -> Result<Option<Uuid>, DbError> {
    cursor.map(|c| Uuid::try_parse(&c.0).map_err(|_| DbError::Invalid(format!("bad cursor {:?}", c.0)))).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_cut_the_lookahead_row() {
        let page = PageRequest::first(2);
        assert_eq!(page_limit(&page), 3);
        let full = finish_page(vec![1, 2, 3], &page, |n| n.to_string());
        assert_eq!((full.items, full.next), (vec![1, 2], Some(Cursor("2".into()))));
        let last = finish_page(vec![4], &page, |n| n.to_string());
        assert_eq!((last.items, last.next), (vec![4], None));
        assert!(fixed::<4>(vec![1, 2, 3], "x").is_err());
        assert!(uuid_cursor(Some(&Cursor("nope".into()))).is_err());
    }
}
