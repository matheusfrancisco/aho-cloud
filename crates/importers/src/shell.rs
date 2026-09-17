//! Command lines as the files write them (`web: gunicorn app:app -b :8000`) → argv.

/// Splits a simple command line into words (quotes and backslashes as a POSIX shell does).
/// `None` when the line needs a shell: variables, operators, redirections, globs, substitutions,
/// comments, or a leading `NAME=value` assignment.
pub(crate) fn split(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = line.trim().chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => cur.push(c),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '$' | '`' => return None,
                        '\\' => match chars.next()? {
                            c @ ('"' | '\\') => cur.push(c),
                            c => {
                                cur.push('\\');
                                cur.push(c);
                            }
                        },
                        c => cur.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                cur.push(chars.next()?);
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            '$' | '`' | '|' | '&' | ';' | '<' | '>' | '(' | ')' | '*' | '?' | '[' | ']' | '{' | '}' => return None,
            '~' | '#' if !in_word => return None,
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    let assigns = words.first().is_some_and(|w| w.split_once('=').is_some_and(|(k, _)| is_name(k)));
    (!words.is_empty() && !assigns).then_some(words)
}

fn is_name(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|f| f.is_ascii_alphabetic() || f == '_') && c.all(|x| x.is_ascii_alphanumeric() || x == '_')
}

/// argv for a command line: its words when simple, else `["/bin/sh", "-c", line]`.
pub(crate) fn argv(line: &str) -> Vec<String> {
    split(line).unwrap_or_else(|| vec!["/bin/sh".into(), "-c".into(), line.trim().to_owned()])
}

/// A literal port the command binds: `--port N`, `--port=N`, `-p N`, `--bind host:N`,
/// `-b host:N`, `--listen :N`, or a leading `PORT=N`.
pub(crate) fn port(line: &str) -> Option<u16> {
    let words: Vec<&str> = line.split_whitespace().map(|w| w.trim_matches(|c| c == '"' || c == '\'')).collect();
    for (i, w) in words.iter().enumerate() {
        let next = words.get(i + 1).copied();
        let found = match *w {
            "--port" | "-p" | "-P" => next.and_then(number),
            "--bind" | "-b" | "--listen" | "-l" | "--host" => next.and_then(after_colon),
            _ => {
                if let Some(v) = w.strip_prefix("--port=") {
                    number(v)
                } else if let Some(v) = w.strip_prefix("--bind=").or_else(|| w.strip_prefix("--listen=")) {
                    after_colon(v)
                } else if let Some(v) = w.strip_prefix("PORT=") {
                    number(v)
                } else {
                    None
                }
            }
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

fn number(s: &str) -> Option<u16> {
    s.parse().ok().filter(|p| *p > 0)
}

fn after_colon(s: &str) -> Option<u16> {
    s.rsplit_once(':').and_then(|(_, p)| number(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_lines_split_into_words() {
        assert_eq!(split("node  server.js"), Some(vec!["node".into(), "server.js".into()]));
        assert_eq!(
            split(r#"python -m "my app" 'a b' c\ d"#),
            Some(vec!["python".into(), "-m".into(), "my app".into(), "a b".into(), "c d".into()])
        );
    }

    #[test]
    fn shell_features_need_a_shell() {
        for line in [
            "gunicorn -b :$PORT app:app",
            "a && b",
            "cmd > log",
            "echo `x`",
            "run *.py",
            "FOO=1 node a.js",
            "~/bin/x",
            "a # c",
            "\"unterminated",
        ] {
            assert_eq!(split(line), None, "{line}");
        }
        assert_eq!(argv("a && b"), vec!["/bin/sh", "-c", "a && b"]);
    }

    #[test]
    fn literal_ports_only() {
        assert_eq!(port("uvicorn main:app --host 0.0.0.0 --port 8000"), Some(8000));
        assert_eq!(port("next start -p 3000"), Some(3000));
        assert_eq!(port("gunicorn -b 0.0.0.0:5000 app:app"), Some(5000));
        assert_eq!(port("serve --listen=:9090"), Some(9090));
        assert_eq!(port("PORT=4000 node a.js"), Some(4000));
        assert_eq!(port("gunicorn -b :$PORT app:app"), None);
        assert_eq!(port("node a.js"), None);
    }
}
