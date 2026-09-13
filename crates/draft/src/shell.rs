//! Just enough POSIX shell lexing to find the simple commands in a `bash` tool call: words with
//! quotes resolved, leading assignments split off, redirections dropped, and whether the command
//! was sent to the background with `&`.

/// One simple command of a command line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Segment {
    /// `NAME=value` words before the command, in order.
    pub assigns: Vec<(String, String)>,
    /// The command and its arguments, wrappers (`nohup`, `env`, `exec`, …) removed.
    pub argv: Vec<String>,
    /// Ended by a single `&`.
    pub background: bool,
}

const MAX_DEPTH: u32 = 3;

/// Every simple command in `line`, recursing into `sh -c '…'`.
pub(crate) fn segments(line: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    collect(line, 0, false, &mut out);
    out
}

fn collect(line: &str, depth: u32, background: bool, out: &mut Vec<Segment>) {
    for (words, bg) in lex(line) {
        let seg = shape(words, bg || background);
        if let Some(script) = inline_script(&seg.argv)
            && depth < MAX_DEPTH
        {
            collect(script, depth + 1, seg.background, out);
            continue;
        }
        if !seg.argv.is_empty() {
            out.push(seg);
        }
    }
}

/// `sh -c SCRIPT`, `bash -lc SCRIPT`, …: the script.
fn inline_script(argv: &[String]) -> Option<&str> {
    let prog = basename(argv.first()?);
    if !matches!(prog, "sh" | "bash" | "zsh" | "dash") {
        return None;
    }
    let flag = argv.get(1)?;
    (flag.starts_with('-') && !flag.starts_with("--") && flag.ends_with('c')).then(|| argv.get(2).map(String::as_str))?
}

pub(crate) fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn is_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && (b[0].is_ascii_alphabetic() || b[0] == b'_') && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

fn split_assign(word: &str) -> Option<(String, String)> {
    let (k, v) = word.split_once('=')?;
    is_name(k).then(|| (k.to_owned(), v.to_owned()))
}

/// Splits off assignments and wrapper commands.
fn shape(words: Vec<String>, background: bool) -> Segment {
    let mut seg = Segment { background, ..Segment::default() };
    let mut it = words.into_iter().peekable();
    while let Some(w) = it.peek() {
        if let Some(kv) = split_assign(w) {
            seg.assigns.push(kv);
            it.next();
            continue;
        }
        match w.as_str() {
            "nohup" | "setsid" | "exec" | "time" | "command" => {
                it.next();
            }
            "env" | "sudo" => {
                it.next();
                while it.peek().is_some_and(|w| w.starts_with('-')) {
                    it.next();
                }
            }
            _ => break,
        }
    }
    seg.argv = it.collect();
    seg
}

/// Words per simple command, with the background flag of its terminator.
fn lex(src: &str) -> Vec<(Vec<String>, bool)> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut cur: Option<String> = None;
    let mut skip_word = false;
    let mut i = 0;

    fn end_word(cur: &mut Option<String>, words: &mut Vec<String>, skip: &mut bool) {
        if let Some(w) = cur.take() {
            if *skip {
                *skip = false;
            } else {
                words.push(w);
            }
        }
    }

    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' => {
                end_word(&mut cur, &mut words, &mut skip_word);
                i += 1;
            }
            '\\' => {
                match chars.get(i + 1) {
                    Some('\n') => {}
                    Some(&n) => cur.get_or_insert_with(String::new).push(n),
                    None => {}
                }
                i += 2;
            }
            '\'' => {
                let w = cur.get_or_insert_with(String::new);
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    w.push(chars[i]);
                    i += 1;
                }
                i += 1;
            }
            '"' => {
                let w = cur.get_or_insert_with(String::new);
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && matches!(chars.get(i + 1), Some('"' | '\\' | '$' | '`')) {
                        i += 1;
                    }
                    w.push(chars[i]);
                    i += 1;
                }
                i += 1;
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                let w = cur.get_or_insert_with(String::new);
                w.push('$');
                i += 1;
                let mut depth = 0;
                while i < chars.len() {
                    match chars[i] {
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        _ => {}
                    }
                    w.push(chars[i]);
                    i += 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
            '`' => {
                let w = cur.get_or_insert_with(String::new);
                w.push('`');
                i += 1;
                while i < chars.len() && chars[i] != '`' {
                    w.push(chars[i]);
                    i += 1;
                }
                w.push('`');
                i += 1;
            }
            '#' if cur.is_none() => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '>' | '<' => {
                // `2>`, `>>`, `>&1`, `<`: drop the fd digits and the target word.
                if cur.as_deref().is_some_and(|w| !w.is_empty() && w.bytes().all(|b| b.is_ascii_digit())) {
                    cur = None;
                } else {
                    end_word(&mut cur, &mut words, &mut skip_word);
                }
                i += 1;
                if matches!(chars.get(i), Some('>' | '&')) {
                    i += 1;
                }
                skip_word = true;
            }
            '&' if chars.get(i + 1) == Some(&'>') => {
                end_word(&mut cur, &mut words, &mut skip_word);
                i += 2;
                if chars.get(i) == Some(&'>') {
                    i += 1;
                }
                skip_word = true;
            }
            ';' | '\n' | '|' | '&' | '(' | ')' => {
                end_word(&mut cur, &mut words, &mut skip_word);
                let double = chars.get(i + 1) == Some(&c) && matches!(c, '&' | '|' | ';');
                let background = c == '&' && !double;
                if !words.is_empty() {
                    out.push((std::mem::take(&mut words), background));
                }
                skip_word = false;
                i += if double { 2 } else { 1 };
            }
            _ => {
                cur.get_or_insert_with(String::new).push(c);
                i += 1;
            }
        }
    }
    end_word(&mut cur, &mut words, &mut skip_word);
    if !words.is_empty() {
        out.push((words, false));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &Segment) -> Vec<&str> {
        s.argv.iter().map(String::as_str).collect()
    }

    #[test]
    fn splits_sequences_pipes_and_background() {
        let segs = segments("cd app && npm ci; npm run build | tee log.txt && nohup node server.js > out.log 2>&1 &");
        let got: Vec<(Vec<&str>, bool)> = segs.iter().map(|s| (argv(s), s.background)).collect();
        assert_eq!(
            got,
            vec![
                (vec!["cd", "app"], false),
                (vec!["npm", "ci"], false),
                (vec!["npm", "run", "build"], false),
                (vec!["tee", "log.txt"], false),
                (vec!["node", "server.js"], true),
            ]
        );
    }

    #[test]
    fn resolves_quotes_and_splits_assignments() {
        let segs = segments(r#"PORT=8080 API_KEY='a b' env -i node "my server.js" --flag="x y""#);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].assigns, vec![("PORT".into(), "8080".into()), ("API_KEY".into(), "a b".into())]);
        assert_eq!(argv(&segs[0]), vec!["node", "my server.js", "--flag=x y"]);
    }

    #[test]
    fn recurses_into_inline_shell_scripts() {
        let segs = segments("bash -lc 'docker build -t app . && echo done'");
        assert_eq!(segs.iter().map(argv).collect::<Vec<_>>(), vec![vec!["docker", "build", "-t", "app", "."], vec!["echo", "done"]]);
    }

    #[test]
    fn drops_redirections_and_comments() {
        let segs = segments("go build -o bin/app ./cmd/app 2>errors.txt # build it\n./bin/app &> run.log");
        assert_eq!(segs.iter().map(argv).collect::<Vec<_>>(), vec![vec!["go", "build", "-o", "bin/app", "./cmd/app"], vec!["./bin/app"]]);
    }
}
