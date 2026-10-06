//! Observation extraction for `FailureSignature` comparison (executor.md "Observation extraction").
//! Pure: the app layer maps a `BrowserOutcome` / `ExecOutcome` onto [`ScriptRun`] and passes the
//! service log lines appended after the pre-run cursors.
//!
//! Output: observations of the expected signature's `kind`, `matcher = Exact`, `value` = observed
//! text (≤ 1 KiB). Candidates the expected signature matches come first, then the rest in source
//! order; duplicates are dropped and the list is capped at [`MAX_OBSERVATIONS`]. The server
//! re-evaluates with the same `FailureSignature::matches`.

use std::borrow::Cow;

use mage_core::domain::signature::SIGNATURE_VALUE_MAX_BYTES;
use mage_core::domain::verify::MAX_OBSERVATIONS;
use mage_core::domain::{FailureSignature, Matcher, SignatureKind};

/// How the repro script ran, reduced to what observation needs.
#[derive(Debug, Clone, Copy)]
pub enum ScriptRun<'a> {
    Playwright(PlaywrightRun<'a>),
    Shell(ShellRun<'a>),
}

#[derive(Debug, Clone, Copy)]
pub struct PlaywrightRun<'a> {
    /// The invocation or a test timed out: nothing is observed.
    pub timed_out: bool,
    /// Each failing result's `error.message` and `errors[].message`, raw.
    pub failure_messages: &'a [&'a str],
    /// HAR statuses ≥ 400 plus `mage.http_status` annotations.
    pub http_statuses: &'a [u16],
    /// stdout/stderr text of the Playwright process.
    pub output: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub struct ShellRun<'a> {
    pub exit: ShellExit,
    pub stdout: &'a str,
    pub stderr: &'a str,
}

/// Exit of a shell script, from `ExecOutcome.exit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellExit {
    Code(i32),
    Signal(i32),
    TimedOut,
    /// Destroyed, quota, or cancelled: infrastructure, never an observation.
    Killed,
}

/// Observations of `expected.kind` in `run` plus `service_lines`.
pub fn observe(expected: &FailureSignature, run: &ScriptRun<'_>, service_lines: &[String]) -> Vec<FailureSignature> {
    let mut candidates: Vec<String> = Vec::new();
    match (expected.kind, run) {
        (SignatureKind::AssertionText, ScriptRun::Playwright(pw)) if !pw.timed_out => {
            candidates.extend(pw.failure_messages.iter().map(|m| collapse_whitespace(&strip_ansi(m))));
        }
        (SignatureKind::HttpStatus, ScriptRun::Playwright(pw)) if !pw.timed_out => {
            candidates.extend(pw.http_statuses.iter().map(u16::to_string));
        }
        (SignatureKind::HttpStatus, ScriptRun::Shell(sh)) if sh.exit != ShellExit::TimedOut => {
            candidates.extend(sh.stdout.lines().filter_map(http_status_line).map(str::to_owned));
        }
        (SignatureKind::ErrorString, ScriptRun::Playwright(pw)) if !pw.timed_out => {
            for message in pw.failure_messages {
                candidates.extend(text_lines(message));
            }
            candidates.extend(text_lines(pw.output));
            for line in service_lines {
                candidates.extend(text_lines(line));
            }
        }
        (SignatureKind::ErrorString, ScriptRun::Shell(sh)) if sh.exit != ShellExit::TimedOut => {
            candidates.extend(text_lines(sh.stdout));
            candidates.extend(text_lines(sh.stderr));
            for line in service_lines {
                candidates.extend(text_lines(line));
            }
        }
        (SignatureKind::ExitCode, ScriptRun::Shell(sh)) => match sh.exit {
            ShellExit::Code(code) => candidates.push(code.to_string()),
            ShellExit::Signal(sig) => candidates.push(format!("signal:{sig}")),
            ShellExit::TimedOut => candidates.push("timeout".to_owned()),
            ShellExit::Killed => {}
        },
        // assertion_text with Shell and exit_code with Playwright are rejected at submit; a timed
        // out script observes nothing.
        _ => {}
    }
    rank(expected, candidates)
}

/// Matching candidates first (stable), then the rest; deduplicated by value, capped at 16.
fn rank(expected: &FailureSignature, candidates: Vec<String>) -> Vec<FailureSignature> {
    let mut seen = std::collections::HashSet::new();
    let observed: Vec<FailureSignature> = candidates
        .into_iter()
        .map(|value| cap_value(value))
        .filter(|value| !value.is_empty())
        .filter(|value| seen.insert(value.clone()))
        .map(|value| FailureSignature { kind: expected.kind, value, matcher: Matcher::Exact })
        .collect();
    let (mut matching, rest): (Vec<_>, Vec<_>) = observed.into_iter().partition(|o| expected.matches(o));
    matching.extend(rest);
    matching.truncate(MAX_OBSERVATIONS);
    matching
}

/// `^MAGE_HTTP_STATUS=(\d{3})$` (a trailing `\r` is tolerated).
fn http_status_line(line: &str) -> Option<&str> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let digits = line.strip_prefix("MAGE_HTTP_STATUS=")?;
    (digits.len() == 3 && digits.bytes().all(|b| b.is_ascii_digit())).then_some(digits)
}

/// Non-empty lines of `text`, ANSI-stripped and trimmed.
fn text_lines(text: &str) -> impl Iterator<Item = String> + '_ {
    text.lines().map(|line| strip_ansi(line).trim().to_owned()).filter(|line| !line.is_empty())
}

/// Truncates to at most 1 KiB on a char boundary.
fn cap_value(mut value: String) -> String {
    if value.len() > SIGNATURE_VALUE_MAX_BYTES {
        let mut end = SIGNATURE_VALUE_MAX_BYTES;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
    }
    value
}

/// Removes ANSI escape sequences: CSI (`ESC [ … final`, the `\x1b\[[0-9;?]*[A-Za-z]` family and
/// every other CSI form), OSC (`ESC ] … BEL` / `ESC ] … ESC \`), and two-byte escapes.
pub fn strip_ansi(text: &str) -> Cow<'_, str> {
    if !text.contains('\u{1b}') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                // Parameter and intermediate bytes, then one final byte in 0x40..=0x7e.
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // Charset designation (`ESC ( B`, …) carries one more byte.
            Some('(' | ')' | '*' | '+') => {
                chars.next();
                chars.next();
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    Cow::Owned(out)
}

/// Collapses every whitespace run to one space and trims.
pub fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(kind: SignatureKind, matcher: Matcher, value: &str) -> FailureSignature {
        FailureSignature { kind, value: value.into(), matcher }
    }

    fn values(obs: &[FailureSignature]) -> Vec<&str> {
        obs.iter().map(|o| o.value.as_str()).collect()
    }

    fn pw<'a>(messages: &'a [&'a str], statuses: &'a [u16], output: &'a str) -> ScriptRun<'a> {
        ScriptRun::Playwright(PlaywrightRun { timed_out: false, failure_messages: messages, http_statuses: statuses, output })
    }

    fn sh<'a>(exit: ShellExit, stdout: &'a str, stderr: &'a str) -> ScriptRun<'a> {
        ScriptRun::Shell(ShellRun { exit, stdout, stderr })
    }

    #[test]
    fn assertion_text_playwright_is_ansi_stripped_and_collapsed() {
        let expected = sig(SignatureKind::AssertionText, Matcher::Contains, "Expected: 42");
        let msg = "\u{1b}[2mexpect(\u{1b}[22m\u{1b}[31mreceived\u{1b}[39m).toBe(expected)\n\nExpected: 42\nReceived: 41";
        let obs = observe(&expected, &pw(&[msg], &[], ""), &[]);
        assert_eq!(values(&obs), ["expect(received).toBe(expected) Expected: 42 Received: 41"]);
        assert!(obs.iter().all(|o| o.kind == SignatureKind::AssertionText && o.matcher == Matcher::Exact));
        assert!(expected.matches(&obs[0]));
    }

    #[test]
    fn assertion_text_and_exit_code_need_their_script_kind() {
        let assertion = sig(SignatureKind::AssertionText, Matcher::Contains, "x");
        assert!(observe(&assertion, &sh(ShellExit::Code(1), "x", "x"), &[]).is_empty());
        let exit = sig(SignatureKind::ExitCode, Matcher::Exact, "1");
        assert!(observe(&exit, &pw(&["x"], &[500], "x"), &[]).is_empty());
    }

    #[test]
    fn http_status_per_script_kind() {
        let expected = sig(SignatureKind::HttpStatus, Matcher::Exact, "500");
        let obs = observe(&expected, &pw(&[], &[404, 500], ""), &[]);
        assert_eq!(values(&obs), ["500", "404"], "matching first");

        let stdout = "start\nMAGE_HTTP_STATUS=500\r\nMAGE_HTTP_STATUS=5000\n MAGE_HTTP_STATUS=401\nMAGE_HTTP_STATUS=422\n";
        let obs = observe(&expected, &sh(ShellExit::Code(1), stdout, "MAGE_HTTP_STATUS=503"), &[]);
        assert_eq!(values(&obs), ["500", "422"], "only exact stdout lines count");
    }

    #[test]
    fn error_string_sources_in_order() {
        let expected = sig(SignatureKind::ErrorString, Matcher::Contains, "customer is null");
        let service = vec!["[api] TypeError: customer is null".to_owned(), "".to_owned()];
        let obs = observe(&expected, &pw(&["Error: boom\n  at x.ts:3"], &[], "Running 1 test\n  1 failed"), &service);
        assert_eq!(values(&obs), ["[api] TypeError: customer is null", "Error: boom", "at x.ts:3", "Running 1 test", "1 failed"]);

        let obs = observe(&expected, &sh(ShellExit::Code(1), "out line\n", "\u{1b}[31merr line\u{1b}[0m\n"), &[]);
        assert_eq!(values(&obs), ["out line", "err line"]);
    }

    #[test]
    fn exit_code_forms() {
        let expected = sig(SignatureKind::ExitCode, Matcher::Exact, "1");
        assert_eq!(values(&observe(&expected, &sh(ShellExit::Code(1), "", ""), &[])), ["1"]);
        assert_eq!(values(&observe(&expected, &sh(ShellExit::Signal(9), "", ""), &[])), ["signal:9"]);
        assert_eq!(values(&observe(&expected, &sh(ShellExit::TimedOut, "", ""), &[])), ["timeout"]);
        assert!(observe(&expected, &sh(ShellExit::Killed, "", ""), &[]).is_empty());
    }

    #[test]
    fn timed_out_script_observes_nothing() {
        let expected = sig(SignatureKind::ErrorString, Matcher::Contains, "boom");
        let run = ScriptRun::Playwright(PlaywrightRun { timed_out: true, failure_messages: &["boom"], http_statuses: &[500], output: "boom" });
        assert!(observe(&expected, &run, &["boom".to_owned()]).is_empty());
        assert!(observe(&expected, &sh(ShellExit::TimedOut, "boom", "boom"), &[]).is_empty());
    }

    #[test]
    fn dedup_cap_and_matching_first() {
        let expected = sig(SignatureKind::ErrorString, Matcher::Regex, "^fatal");
        let mut lines: Vec<String> = (0..40).map(|i| format!("line {i}")).collect();
        lines.push("line 3".into());
        lines.push("fatal: disk".into());
        let obs = observe(&expected, &sh(ShellExit::Code(1), "", ""), &lines);
        assert_eq!(obs.len(), MAX_OBSERVATIONS);
        assert_eq!(obs[0].value, "fatal: disk");
        assert_eq!(obs[1].value, "line 0");
        let mut unique = values(&obs);
        unique.dedup();
        assert_eq!(unique.len(), obs.len());
    }

    #[test]
    fn values_are_capped_at_one_kib_on_a_char_boundary() {
        let expected = sig(SignatureKind::ErrorString, Matcher::Contains, "é");
        let long = "é".repeat(700);
        let obs = observe(&expected, &sh(ShellExit::Code(1), &long, ""), &[]);
        assert_eq!(obs.len(), 1);
        assert!(obs[0].value.len() <= SIGNATURE_VALUE_MAX_BYTES);
        assert_eq!(obs[0].value.len(), 1024);
    }

    #[test]
    fn strip_ansi_forms() {
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("\u{1b}[1;31mred\u{1b}[0m"), "red");
        assert_eq!(strip_ansi("\u{1b}[?25lhide"), "hide");
        assert_eq!(strip_ansi("\u{1b}]8;;http://x\u{7}link\u{1b}]8;;\u{1b}\\"), "link");
        assert_eq!(strip_ansi("a\u{1b}(Bb"), "ab");
    }
}
