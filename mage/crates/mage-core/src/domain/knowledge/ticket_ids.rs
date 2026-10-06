//! Ticket identifier scanner (knowledge.md "Layer 6: History"): finds tracker identifiers such as
//! `ENG-123` in commit subjects/bodies, PR titles/bodies, and branch names for `ticket_links`.
//!
//! An identifier is `[A-Z][A-Z0-9]{1,9}-[0-9]{1,6}` bounded on both sides by a non-alphanumeric
//! char or the text edge. "Alphanumeric" is ASCII: identifiers glued to non-Latin text
//! (`修复ENG-123`) are still found. Every candidate is returned: non-ticket tokens such as `UTF-8`
//! or `SHA-256` never equal a real identifier at query time, so nothing is filtered here.

/// Identifiers in free text (commit subjects and bodies, PR titles and bodies): uppercase only, so
/// `eng-123` in prose is not a candidate. First-appearance order, deduped.
pub fn ticket_ids_in_text(text: &str) -> Vec<String> {
    scan(text, false)
}

/// Identifiers in a branch name, case-insensitive and returned uppercased
/// (`eng-123-fix-login` → `ENG-123`). First-appearance order, deduped.
pub fn ticket_ids_in_branch(branch: &str) -> Vec<String> {
    scan(branch, true)
}

const KEY_LEN: std::ops::RangeInclusive<usize> = 2..=10;
const NUMBER_LEN: std::ops::RangeInclusive<usize> = 1..=6;

fn scan(text: &str, any_case: bool) -> Vec<String> {
    let bytes = text.as_bytes();
    let run_end = |from: usize| {
        let mut end = from;
        while end < bytes.len() && bytes[end].is_ascii_alphanumeric() {
            end += 1;
        }
        end
    };
    let mut found: Vec<String> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        // Maximal alphanumeric run: the team key candidate, bounded on the left.
        let start = i;
        let key_end = run_end(start);
        i = key_end;
        if !is_key(&bytes[start..key_end], any_case) || bytes.get(key_end) != Some(&b'-') {
            continue;
        }
        let number_end = run_end(key_end + 1);
        let number = &bytes[key_end + 1..number_end];
        if !NUMBER_LEN.contains(&number.len()) || !number.iter().all(u8::is_ascii_digit) {
            continue;
        }
        // ASCII-only range, so the slice is on char boundaries.
        let id = text[start..number_end].to_ascii_uppercase();
        if !found.contains(&id) {
            found.push(id);
        }
        i = number_end;
    }
    found
}

fn is_key(run: &[u8], any_case: bool) -> bool {
    let Some((&first, rest)) = run.split_first() else {
        return false;
    };
    if !KEY_LEN.contains(&run.len()) {
        return false;
    }
    if any_case {
        // The run is maximal alphanumeric, so only the first char needs checking.
        first.is_ascii_alphabetic()
    } else {
        first.is_ascii_uppercase() && rest.iter().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_identifiers_in_subjects_bodies_titles_and_urls() {
        let cases: [(&str, &[&str]); 9] = [
            ("ENG-123: fix login redirect", &["ENG-123"]),
            ("Fix login redirect (ENG-123) (#456)", &["ENG-123"]),
            ("[OPS-7] Retry connection on timeout", &["OPS-7"]),
            ("Fixes ENG-1, refs OPS-42 and ENG-1.\n\nCloses E2E-5", &["ENG-1", "OPS-42", "E2E-5"]),
            ("See https://linear.app/acme/issue/ENG-123/fix-login", &["ENG-123"]),
            ("Merge pull request #12 from acme/eng-123-fix-login", &[]),
            ("feature_ENG-9 and ENG-123-456", &["ENG-9", "ENG-123"]),
            ("修复ENG-123（登录）", &["ENG-123"]),
            ("no ticket here: eng-123, Eng-124", &[]),
        ];
        for (text, expected) in cases {
            assert_eq!(ticket_ids_in_text(text), expected, "{text}");
        }
    }

    #[test]
    fn branch_names_are_case_insensitive_and_uppercased() {
        let cases: [(&str, &[&str]); 6] = [
            ("eng-123-fix-login", &["ENG-123"]),
            ("acme/eng-123-fix-login", &["ENG-123"]),
            ("feature/ENG-77_retry", &["ENG-77"]),
            ("chico/eng-12-and-eng-13", &["ENG-12", "ENG-13"]),
            ("Eng-5/eng-5-again", &["ENG-5"]),
            ("fix-login-flow", &[]),
        ];
        for (branch, expected) in cases {
            assert_eq!(ticket_ids_in_branch(branch), expected, "{branch}");
        }
    }

    #[test]
    fn boundaries_and_length_limits() {
        let cases: [(&str, &[&str]); 13] = [
            ("ENG-123456", &["ENG-123456"]),
            ("ENG-1234567", &[]),
            ("ABCDEFGHIJ-1", &["ABCDEFGHIJ-1"]),
            ("ABCDEFGHIJK-1", &[]),
            ("E-1", &[]),
            ("2FA-1", &[]),
            ("ENG-123abc", &[]),
            ("xENG-123", &[]),
            ("ENG--123", &[]),
            ("ENG-", &[]),
            ("-123", &[]),
            ("ENG-0", &["ENG-0"]),
            ("", &[]),
        ];
        for (text, expected) in cases {
            assert_eq!(ticket_ids_in_text(text), expected, "{text}");
        }
        // Case folding only widens the key alphabet; boundaries still apply.
        assert_eq!(ticket_ids_in_branch("xeng-123"), ["XENG-123"]);
        assert_eq!(ticket_ids_in_branch("eng-123abc"), Vec::<String>::new());
    }

    /// Documented: tokens shaped like identifiers are stored and filtered at query time.
    #[test]
    fn identifier_shaped_tokens_are_not_filtered() {
        assert_eq!(ticket_ids_in_text("decode as UTF-8 and hash with SHA-256"), ["UTF-8", "SHA-256"]);
    }
}
