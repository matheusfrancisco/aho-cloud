//! Keyset paging for the fakes: items carry an ordered key; the cursor is the encoded key of the
//! last item of a page, and the next page starts strictly after it.

use crate::ports::common::Cursor;

/// `items` with their keys, in any order: sorts by key, keeps those after `after`, takes `limit`
/// and returns the encoded key of the last one when more remain.
pub(crate) fn keyset_page<K: Ord, T>(mut items: Vec<(K, T)>, after: Option<&K>, limit: usize, encode: impl Fn(&K) -> String) -> (Vec<T>, Option<Cursor>) {
    items.sort_by(|a, b| a.0.cmp(&b.0));
    let mut rest: Vec<(K, T)> = match after {
        Some(after) => items.into_iter().filter(|(key, _)| key > after).collect(),
        None => items,
    };
    let more = rest.len() > limit;
    rest.truncate(limit);
    let next = if more { rest.last().map(|(key, _)| Cursor(encode(key))) } else { None };
    (rest.into_iter().map(|(_, item)| item).collect(), next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_concatenate_to_the_sorted_list() {
        let items: Vec<(u32, u32)> = [5, 1, 4, 2, 3].into_iter().map(|n| (n, n * 10)).collect();
        let (first, next) = keyset_page(items.clone(), None, 2, |k| k.to_string());
        assert_eq!((first, next.clone()), (vec![10, 20], Some(Cursor("2".into()))));
        let after: u32 = next.map(|c| c.0.parse().unwrap()).unwrap();
        let (second, next) = keyset_page(items.clone(), Some(&after), 2, |k| k.to_string());
        assert_eq!((second, next), (vec![30, 40], Some(Cursor("4".into()))));
        let (last, next) = keyset_page(items, Some(&4), 2, |k| k.to_string());
        assert_eq!((last, next), (vec![50], None));
    }
}
