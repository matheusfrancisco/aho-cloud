//! `Link` header pagination (`<url>; rel="next", <url>; rel="last"`).

/// The `rel="next"` target of a `Link` header value.
pub(crate) fn next_link(header: &str) -> Option<String> {
    let mut rest = header;
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        let close = after.find('>')?;
        let target = &after[..close];
        let tail = &after[close + 1..];
        // Parameters run up to the next entry; they never contain '<'.
        let params_end = tail.find('<').unwrap_or(tail.len());
        let params = &tail[..params_end];
        if params.split(';').any(is_rel_next) {
            return Some(target.to_owned());
        }
        rest = &tail[params_end..];
    }
    None
}

fn is_rel_next(param: &str) -> bool {
    let Some((name, value)) = param.split_once('=') else { return false };
    if !name.trim().eq_ignore_ascii_case("rel") {
        return false;
    }
    let value = value.trim().trim_end_matches(',').trim().trim_matches('"');
    value.split_ascii_whitespace().any(|rel| rel.eq_ignore_ascii_case("next"))
}

/// The `page` query parameter of a pagination URL.
pub(crate) fn page_number(url: &str) -> Option<u32> {
    let parsed = url::Url::parse(url).ok()?;
    parsed.query_pairs().find(|(key, _)| key == "page").and_then(|(_, value)| value.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_next_among_other_relations() {
        let header = r#"<https://api.github.com/repositories/1/pulls?page=1>; rel="prev", <https://api.github.com/repositories/1/pulls?page=3>; rel="next", <https://api.github.com/repositories/1/pulls?page=9>; rel="last""#;
        assert_eq!(next_link(header).as_deref(), Some("https://api.github.com/repositories/1/pulls?page=3"));
    }

    #[test]
    fn handles_unquoted_multi_value_rel_and_commas_in_urls() {
        assert_eq!(next_link("<https://x/a?q=a,b&page=2>; rel=next").as_deref(), Some("https://x/a?q=a,b&page=2"));
        assert_eq!(next_link(r#"<https://x/b>; rel="next last""#).as_deref(), Some("https://x/b"));
    }

    #[test]
    fn no_next_on_the_last_page_or_garbage() {
        assert_eq!(next_link(r#"<https://x/a?page=1>; rel="first", <https://x/a?page=1>; rel="prev""#), None);
        assert_eq!(next_link(""), None);
        assert_eq!(next_link("<https://x/a; rel=next"), None);
        assert_eq!(next_link(r#"<https://x/a>; title="next""#), None);
    }

    #[test]
    fn page_number_reads_the_page_parameter() {
        assert_eq!(page_number("https://api.github.com/repos/o/r/pulls?state=all&page=4&per_page=2"), Some(4));
        assert_eq!(page_number("https://api.github.com/repos/o/r/pulls?per_page=2"), None);
        assert_eq!(page_number("/relative?page=2"), None);
    }
}
