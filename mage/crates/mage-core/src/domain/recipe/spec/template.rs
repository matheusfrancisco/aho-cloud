//! Recipe template syntax (executor.md "Template variables"): `${name}` is a variable, `$${` is a
//! literal `${`, any other `$` is literal. Expansion is `Env::render` (runner); the domain only
//! tokenizes, so validation and rendering share one grammar.

/// A piece of a template string, borrowed from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplatePart<'a> {
    /// Text copied verbatim (an escaped `$${` yields `Literal("${")`).
    Literal(&'a str),
    /// The name between `${` and the next `}`.
    Var(&'a str),
}

/// A `${` without a closing `}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("unterminated `${{` at byte {at}")]
pub struct UnterminatedTemplate {
    /// Byte offset of the `$`.
    pub at: usize,
}

/// Tokenizes `template` left to right. After an [`UnterminatedTemplate`] the iterator ends.
pub fn template_parts(template: &str) -> TemplateParts<'_> {
    TemplateParts { rest: template, offset: 0 }
}

#[derive(Debug, Clone)]
pub struct TemplateParts<'a> {
    rest: &'a str,
    offset: usize,
}

impl<'a> TemplateParts<'a> {
    fn advance(&mut self, n: usize) {
        self.rest = &self.rest[n..];
        self.offset += n;
    }
}

impl<'a> Iterator for TemplateParts<'a> {
    type Item = Result<TemplatePart<'a>, UnterminatedTemplate>;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = self.rest;
        if rest.is_empty() {
            return None;
        }
        if rest.starts_with("$${") {
            self.advance(3);
            return Some(Ok(TemplatePart::Literal(&rest[1..3])));
        }
        if let Some(body) = rest.strip_prefix("${") {
            return Some(match body.find('}') {
                Some(end) => {
                    self.advance(end + 3);
                    Ok(TemplatePart::Var(&body[..end]))
                }
                None => {
                    let at = self.offset;
                    self.rest = "";
                    Err(UnterminatedTemplate { at })
                }
            });
        }
        let end = rest
            .match_indices('$')
            .map(|(i, _)| i)
            .find(|&i| i > 0 && (rest[i..].starts_with("${") || rest[i..].starts_with("$${")))
            .unwrap_or(rest.len());
        self.advance(end);
        Some(Ok(TemplatePart::Literal(&rest[..end])))
    }
}

/// Variable names in `template`, ignoring an unterminated tail.
pub(super) fn vars(template: &str) -> impl Iterator<Item = &str> {
    template_parts(template).filter_map(|part| match part {
        Ok(TemplatePart::Var(name)) => Some(name),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use TemplatePart::{Literal, Var};

    fn parts(s: &str) -> Vec<Result<TemplatePart<'_>, UnterminatedTemplate>> {
        template_parts(s).collect()
    }

    #[test]
    fn variables_escapes_and_plain_dollars() {
        assert_eq!(
            parts("a${port.web}b$${HOME}$PATH $$"),
            vec![
                Ok(Literal("a")),
                Ok(Var("port.web")),
                Ok(Literal("b")),
                Ok(Literal("${")),
                Ok(Literal("HOME}$PATH $$"))
            ]
        );
        assert_eq!(parts("${ws}${scratch}"), vec![Ok(Var("ws")), Ok(Var("scratch"))]);
        assert_eq!(parts("${}"), vec![Ok(Var(""))]);
        assert!(parts("").is_empty());
    }

    #[test]
    fn escape_is_matched_before_a_variable() {
        // `$$${x}`: a literal `$`, then the escape `$${`, then text.
        assert_eq!(parts("$$${x}"), vec![Ok(Literal("$")), Ok(Literal("${")), Ok(Literal("x}"))]);
        assert_eq!(vars("$${scip.out}").count(), 0);
    }

    #[test]
    fn unterminated_variable_reports_offset_and_stops() {
        assert_eq!(parts("ab${port.web"), vec![Ok(Literal("ab")), Err(UnterminatedTemplate { at: 2 })]);
        assert_eq!(vars("${a} ${b").collect::<Vec<_>>(), vec!["a"]);
    }
}
