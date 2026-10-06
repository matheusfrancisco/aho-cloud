//! Playwright `JSONReport` subset (executor.md "JSON report parsing", mirrored from Playwright's
//! `testReporter.d.ts`) and the HAR subset used for `http_statuses`. Pure: bytes in, summary out.

use serde::Deserialize;

use crate::domain::observe::strip_ansi;
use crate::ports::browser::{BrowserStatus, TestFailure};
use crate::ports::executor::Exit;

/// `error_text` cap.
pub const ERROR_TEXT_MAX_BYTES: usize = 4 * 1024;
/// Annotation type scripts use for API statuses the HAR does not see.
pub const HTTP_STATUS_ANNOTATION: &str = "mage.http_status";

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct JsonReport {
    suites: Vec<Suite>,
    errors: Vec<ReportError>,
    stats: Stats,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Suite {
    title: String,
    specs: Vec<Spec>,
    suites: Vec<Suite>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Spec {
    title: String,
    tests: Vec<Test>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Test {
    annotations: Vec<Annotation>,
    results: Vec<TestResult>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TestResult {
    status: String,
    error: Option<ReportError>,
    errors: Vec<ReportError>,
    attachments: Vec<Attachment>,
    annotations: Vec<Annotation>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ReportError {
    message: Option<String>,
    location: Option<Location>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Location {
    file: String,
    line: u32,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Attachment {
    name: String,
    path: Option<String>,
    #[serde(rename = "contentType")]
    content_type: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Annotation {
    #[serde(rename = "type")]
    kind: String,
    description: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Stats {
    pub expected: u32,
    pub unexpected: u32,
    pub flaky: u32,
    pub skipped: u32,
}

/// A recording a test result attached (video, trace, screenshot) with its path as env processes
/// see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportAttachment {
    pub name: String,
    pub path: String,
    pub content_type: String,
}

/// What `run()` needs from `report.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportSummary {
    /// Top-level `errors[].message` (config or syntax errors), ANSI-stripped.
    pub top_errors: Vec<String>,
    pub stats: Stats,
    pub any_timed_out: bool,
    pub any_interrupted: bool,
    /// One per error of a failing result (`error`, then distinct `errors[]`), in report order.
    pub failures: Vec<TestFailure>,
    /// `mage.http_status` annotation values (test and result level), in order.
    pub annotated_statuses: Vec<u16>,
    pub attachments: Vec<ReportAttachment>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("report.json is not a Playwright JSON report: {0}")]
pub struct ReportParseError(String);

pub fn parse_report(bytes: &[u8]) -> Result<ReportSummary, ReportParseError> {
    let report: JsonReport = serde_json::from_slice(bytes).map_err(|e| ReportParseError(e.to_string()))?;
    let mut summary = ReportSummary {
        top_errors: report.errors.iter().filter_map(|e| e.message.as_deref()).map(|m| strip_ansi(m).into_owned()).collect(),
        stats: report.stats,
        any_timed_out: false,
        any_interrupted: false,
        failures: Vec::new(),
        annotated_statuses: Vec::new(),
        attachments: Vec::new(),
    };
    let mut titles = Vec::new();
    for suite in &report.suites {
        walk_suite(suite, &mut titles, &mut summary);
    }
    Ok(summary)
}

fn walk_suite<'a>(suite: &'a Suite, titles: &mut Vec<&'a str>, out: &mut ReportSummary) {
    let pushed = !suite.title.is_empty();
    if pushed {
        titles.push(&suite.title);
    }
    for spec in &suite.specs {
        let mut path = titles.clone();
        path.push(&spec.title);
        let title = path.join(" › ");
        for test in &spec.tests {
            // Playwright copies test annotations into each result: count a status once per test.
            let mut statuses = Vec::new();
            collect_statuses(&test.annotations, &mut statuses);
            for result in &test.results {
                collect_statuses(&result.annotations, &mut statuses);
            }
            out.annotated_statuses.extend(statuses);
            for result in &test.results {
                match result.status.as_str() {
                    "timedOut" => out.any_timed_out = true,
                    "interrupted" => out.any_interrupted = true,
                    _ => {}
                }
                if matches!(result.status.as_str(), "failed" | "timedOut" | "interrupted") {
                    push_failures(&title, result, &mut out.failures);
                }
                for att in &result.attachments {
                    if let Some(path) = &att.path {
                        out.attachments.push(ReportAttachment {
                            name: att.name.clone(),
                            path: path.clone(),
                            content_type: att.content_type.clone(),
                        });
                    }
                }
            }
        }
    }
    for child in &suite.suites {
        walk_suite(child, titles, out);
    }
    if pushed {
        titles.pop();
    }
}

fn push_failures(title: &str, result: &TestResult, out: &mut Vec<TestFailure>) {
    let mut seen: Vec<&str> = Vec::new();
    for err in result.error.iter().chain(result.errors.iter()) {
        let Some(message) = err.message.as_deref() else { continue };
        // `errors[]` repeats `error` with a code snippet appended.
        if seen.iter().any(|s| message.starts_with(s)) {
            continue;
        }
        seen.push(message);
        out.push(TestFailure {
            title: title.to_owned(),
            message: strip_ansi(message).into_owned(),
            location: err.location.as_ref().map(|l| format!("{}:{}", l.file, l.line)),
        });
    }
    if seen.is_empty() {
        out.push(TestFailure { title: title.to_owned(), message: format!("test {}", result.status), location: None });
    }
}

/// Appends `mage.http_status` values not yet in `out`.
fn collect_statuses(annotations: &[Annotation], out: &mut Vec<u16>) {
    for a in annotations {
        if a.kind == HTTP_STATUS_ANNOTATION
            && let Some(status) = a.description.as_deref().and_then(|d| d.trim().parse::<u16>().ok())
            && (100..=599).contains(&status)
            && !out.contains(&status)
        {
            out.push(status);
        }
    }
}

/// Top-level error Playwright reports when the run selected zero tests (an empty file, or a
/// `grep` matching nothing). Pinned 1.57.0 wording.
const NO_TESTS_ERROR: &str = "Error: No tests found";
/// Top-level error prefix when `globalTimeout` elapsed ("Timed out waiting 180s for the test suite
/// to run"). Pinned 1.57.0 wording.
const GLOBAL_TIMEOUT_ERROR: &str = "Timed out waiting ";

fn is_no_tests(message: &str) -> bool {
    message.trim_start().starts_with(NO_TESTS_ERROR)
}

fn is_global_timeout(message: &str) -> bool {
    message.trim_start().starts_with(GLOBAL_TIMEOUT_ERROR)
}

/// Top-level errors that mean the run crashed (syntax or config errors), as opposed to "no
/// tests" and "global timeout", which the report also lists under `errors`.
fn crash_errors(summary: &ReportSummary) -> impl Iterator<Item = &String> {
    summary.top_errors.iter().filter(|m| !is_no_tests(m) && !is_global_timeout(m))
}

/// `BrowserStatus` per the executor.md table. `exec_timed_out` is the invocation's own timeout.
/// Playwright also reports zero selected tests and an elapsed `globalTimeout` as top-level
/// errors; those map to `NoTests` and `TimedOut`, not `Crashed`.
pub fn status_of(summary: &ReportSummary, exit: &Exit, exec_timed_out: bool) -> BrowserStatus {
    let stats = summary.stats;
    if crash_errors(summary).next().is_some() {
        BrowserStatus::Crashed { exit: exit.clone() }
    } else if exec_timed_out || summary.any_timed_out || summary.top_errors.iter().any(|m| is_global_timeout(m)) {
        BrowserStatus::TimedOut
    } else if summary.any_interrupted {
        BrowserStatus::Interrupted
    } else if stats.expected + stats.unexpected + stats.flaky == 0 {
        BrowserStatus::NoTests
    } else if stats.unexpected > 0 || stats.flaky > 0 {
        BrowserStatus::Failed
    } else {
        BrowserStatus::Passed
    }
}

/// `error_text`: the first crash error, else the first failing result's message plus
/// ` at <file>:<line>`, else the first other top-level error; ≤ 4 KiB.
pub fn error_text(summary: &ReportSummary) -> Option<String> {
    let text = if let Some(top) = crash_errors(summary).next() {
        top.clone()
    } else if let Some(first) = summary.failures.first() {
        match &first.location {
            Some(loc) => format!("{} at {loc}", first.message),
            None => first.message.clone(),
        }
    } else {
        summary.top_errors.first()?.clone()
    };
    Some(truncate_utf8(text, ERROR_TEXT_MAX_BYTES))
}

/// HAR `log.entries[].response.status` values ≥ 400, in order.
pub fn har_error_statuses(bytes: &[u8]) -> Vec<u16> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Har {
        log: Log,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Log {
        entries: Vec<Entry>,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Entry {
        response: Response,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Response {
        status: i64,
    }
    let Ok(har) = serde_json::from_slice::<Har>(bytes) else { return Vec::new() };
    har.log.entries.iter().filter_map(|e| u16::try_from(e.response.status).ok()).filter(|s| (400..=599).contains(s)).collect()
}

/// HAR statuses, then annotations; deduplicated keeping the first occurrence.
pub fn merge_statuses(har: &[u16], annotated: &[u16]) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::new();
    for s in har.iter().chain(annotated) {
        if !out.contains(s) {
            out.push(*s);
        }
    }
    out
}

pub fn truncate_utf8(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

#[cfg(test)]
mod tests {
    //! Golden reports produced by the pinned Playwright 1.57.0 (scratch generator: fixture app +
    //! one spec per case, paths rewritten to `/mage/scratch/pw/3`).

    use super::*;

    const PASSED: &str = include_str!("testdata/passed.json");
    const FAILED: &str = include_str!("testdata/failed.json");
    const FAILED_HAR: &str = include_str!("testdata/failed.har");
    const TIMED_OUT: &str = include_str!("testdata/timed_out.json");
    const GLOBAL_TIMEOUT: &str = include_str!("testdata/global_timeout.json");
    const FLAKY: &str = include_str!("testdata/flaky.json");
    const TOP_ERROR: &str = include_str!("testdata/top_level_error.json");
    const NESTED: &str = include_str!("testdata/nested_suites.json");
    const NO_TESTS: &str = include_str!("testdata/no_tests.json");
    const INTERRUPTED: &str = include_str!("testdata/interrupted.json");

    fn parse(json: &str) -> ReportSummary {
        parse_report(json.as_bytes()).unwrap()
    }

    fn status(json: &str, exit: Exit, exec_timed_out: bool) -> BrowserStatus {
        status_of(&parse(json), &exit, exec_timed_out)
    }

    #[test]
    fn status_table() {
        assert_eq!(status(PASSED, Exit::Code(0), false), BrowserStatus::Passed);
        assert_eq!(status(FAILED, Exit::Code(1), false), BrowserStatus::Failed);
        assert_eq!(status(TIMED_OUT, Exit::Code(1), false), BrowserStatus::TimedOut);
        assert_eq!(status(GLOBAL_TIMEOUT, Exit::Code(1), false), BrowserStatus::TimedOut);
        assert_eq!(status(FLAKY, Exit::Code(0), false), BrowserStatus::Failed);
        assert_eq!(status(NESTED, Exit::Code(1), false), BrowserStatus::Failed);
        assert_eq!(status(TOP_ERROR, Exit::Code(1), false), BrowserStatus::Crashed { exit: Exit::Code(1) });
        assert_eq!(status(NO_TESTS, Exit::Code(1), false), BrowserStatus::NoTests);
        assert_eq!(status(INTERRUPTED, Exit::Code(130), false), BrowserStatus::Interrupted);
        // The invocation's own timeout wins over a passing report.
        assert_eq!(status(PASSED, Exit::TimedOut, true), BrowserStatus::TimedOut);
        // A crash error wins over everything.
        assert_eq!(status(TOP_ERROR, Exit::TimedOut, true), BrowserStatus::Crashed { exit: Exit::TimedOut });
    }

    #[test]
    fn failed_report_failures_error_text_annotations_attachments() {
        let s = parse(FAILED);
        assert_eq!(s.failures.len(), 1, "errors[] repeating error with a snippet is not a second failure");
        let f = &s.failures[0];
        assert_eq!(f.title, "repro.spec.ts › export shows the customer");
        assert!(f.message.starts_with("Error: expect(locator).toHaveText(expected) failed"), "{}", f.message);
        assert!(f.message.contains("Expected: \"Ada Lovelace\""));
        assert!(!f.message.contains('\u{1b}'), "ANSI stripped");
        assert_eq!(f.location.as_deref(), Some("/mage/scratch/pw/3/tests/repro.spec.ts:6"));
        let text = error_text(&s).unwrap();
        assert!(text.starts_with("Error: expect(locator)"));
        assert!(text.ends_with(" at /mage/scratch/pw/3/tests/repro.spec.ts:6"));
        assert_eq!(s.annotated_statuses, vec![500], "test and result copies count once");
        let names: Vec<&str> = s.attachments.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["screenshot", "video", "error-context", "trace"]);
        let video = &s.attachments[1];
        assert_eq!(video.content_type, "video/webm");
        assert_eq!(video.path, "/mage/scratch/pw/3/out/repro-export-shows-the-customer-chromium/video.webm");
        assert_eq!(s.attachments[3].content_type, "application/zip");
    }

    #[test]
    fn passed_report_has_recordings_and_no_failures() {
        let s = parse(PASSED);
        assert!(s.failures.is_empty());
        assert_eq!(error_text(&s), None);
        let names: Vec<&str> = s.attachments.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["video", "trace"]);
    }

    #[test]
    fn crash_and_no_tests_texts() {
        let s = parse(TOP_ERROR);
        let text = error_text(&s).unwrap();
        assert!(text.starts_with("SyntaxError: /mage/scratch/pw/3/tests/repro.spec.ts: Unexpected token"), "{text}");
        assert!(s.failures.is_empty());
        assert_eq!(error_text(&parse(NO_TESTS)).as_deref(), Some("Error: No tests found"));
        assert!(error_text(&parse(GLOBAL_TIMEOUT)).unwrap().starts_with("Timed out waiting 4s"));
    }

    #[test]
    fn nested_suites_join_titles_and_keep_every_soft_error() {
        let s = parse(NESTED);
        let titles: Vec<&str> = s.failures.iter().map(|f| f.title.as_str()).collect();
        assert_eq!(titles, ["checkout.spec.ts › billing › invoices › totals add up"; 2]);
        assert!(s.failures[0].message.starts_with("Error: first soft assertion"));
        assert!(s.failures[1].message.starts_with("Error: second soft assertion"));
        assert_eq!(s.stats, Stats { expected: 1, unexpected: 1, flaky: 0, skipped: 1 });
    }

    #[test]
    fn timed_out_result_keeps_both_errors() {
        let s = parse(TIMED_OUT);
        assert!(s.any_timed_out);
        assert_eq!(s.failures.len(), 2);
        assert_eq!(s.failures[0].message, "Test timeout of 3000ms exceeded.");
        assert!(s.failures[1].message.starts_with("Error: page.waitForSelector: Test timeout of 3000ms exceeded.\nCall log:"));
        assert_eq!(s.failures[1].location.as_deref(), Some("/mage/scratch/pw/3/tests/repro.spec.ts:4"));
    }

    #[test]
    fn flaky_keeps_the_failing_first_try() {
        let s = parse(FLAKY);
        assert_eq!(s.failures.len(), 1);
        assert!(s.failures[0].message.starts_with("Error: first try fails"));
        assert_eq!(s.stats.flaky, 1);
    }

    #[test]
    fn interrupted_result_is_a_failure() {
        let s = parse(INTERRUPTED);
        assert!(s.any_interrupted);
        assert_eq!(s.failures[0].message, "Error: page.waitForTimeout: Test ended.");
    }

    #[test]
    fn not_a_report() {
        assert!(parse_report(b"<html>").is_err());
    }

    #[test]
    fn har_statuses_and_merge() {
        // The fixture app: `/` 200, `/missing.js` 404, `/api/customer` 500 (page fetch and the
        // script's own API call).
        let statuses = har_error_statuses(FAILED_HAR.as_bytes());
        assert_eq!(statuses, vec![500, 404, 500]);
        assert_eq!(merge_statuses(&statuses, &[422, 404]), vec![500, 404, 422]);
        assert!(har_error_statuses(b"not json").is_empty());
    }

    #[test]
    fn error_text_is_capped() {
        let mut s = parse(FAILED);
        s.failures[0].message = "é".repeat(5000);
        let text = error_text(&s).unwrap();
        assert!(text.len() <= ERROR_TEXT_MAX_BYTES);
    }
}
