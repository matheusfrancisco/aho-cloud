//! Dockerfile: the final stage's `EXPOSE`, `CMD`/`ENTRYPOINT` and `HEALTHCHECK`.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::json;

use crate::{Fact, Findings, health_from_argv, read};

const NAME: &str = "Dockerfile";

pub(crate) fn import(root: &Path, rel: &str) -> Option<Findings> {
    let text = match read(root, rel)? {
        Ok(t) => t,
        Err(e) => return Some(Findings::failed(NAME, e)),
    };
    let mut f = Findings::new(NAME);
    let Some(stage) = final_stage(&lex(&text)) else {
        f.problems.push(format!("{rel}: no FROM instruction"));
        return Some(f);
    };
    f.build = Some(Fact::new(
        json!({"kind": "dockerfile", "path": rel, "context": "."}),
        rel,
        stage.from_line,
        format!("{rel} builds the image (final stage: FROM {})", stage.image),
    ));
    let exposed = stage.expose.first().copied();
    let p = f.primary_mut();
    if let Some((port, line)) = exposed {
        p.port = Some(Fact::new(json!(port), rel, line, format!("EXPOSE {port}")));
        p.expose = Some(Fact::new(json!("public"), rel, line, "the exposed port serves HTTP"));
    }
    if let Some((argv, line, note)) = stage.command() {
        p.command = Some(Fact::new(json!(argv), rel, line, note));
    }
    if let Some((Some(form), line)) = &stage.health {
        let argv = form.argv(&stage.shell);
        p.health = Some(Fact::new(health_from_argv(&argv, exposed.map(|e| e.0)), rel, *line, "HEALTHCHECK"));
    }
    if p == &Default::default() {
        f.primary = None;
    }
    Some(f)
}

/// One instruction, continuation lines joined; `line` is where it starts.
#[derive(Debug, PartialEq)]
struct Instr {
    line: usize,
    keyword: String,
    args: String,
}

fn lex(text: &str) -> Vec<Instr> {
    let mut escape = '\\';
    let mut out = Vec::new();
    let mut directives = true;
    let mut cur: Option<(usize, String)> = None;
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let n = i + 1;
        let t = raw.trim();
        if let Some((marker, strip)) = heredocs.first() {
            let body = if *strip { raw.trim_start_matches('\t') } else { raw };
            if body == marker {
                heredocs.remove(0);
            }
            continue;
        }
        if directives {
            if let Some((k, v)) = t.strip_prefix('#').and_then(|d| d.split_once('=')) {
                let key = k.trim().to_ascii_lowercase();
                if key == "escape" {
                    escape = v.trim().chars().next().unwrap_or('\\');
                    continue;
                }
                if key == "syntax" || key == "check" {
                    continue;
                }
            }
            directives = false;
        }
        let continuing = cur.is_some();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let (content, more) = match t.strip_suffix(escape) {
            Some(c) => (c, true),
            None => (t, false),
        };
        let (start, mut buf) = cur.take().unwrap_or((n, String::new()));
        if continuing {
            buf.push(' ');
        }
        buf.push_str(content.trim_end());
        if more {
            cur = Some((start, buf));
            continue;
        }
        let (keyword, args) = buf.split_once(char::is_whitespace).unwrap_or((&buf, ""));
        heredocs.extend(heredoc_markers(args));
        out.push(Instr { line: start, keyword: keyword.to_ascii_uppercase(), args: args.trim().to_owned() });
    }
    if let Some((start, buf)) = cur {
        let (keyword, args) = buf.split_once(char::is_whitespace).unwrap_or((&buf, ""));
        out.push(Instr { line: start, keyword: keyword.to_ascii_uppercase(), args: args.trim().to_owned() });
    }
    out
}

/// `<<EOF`, `<<-EOF`, `<<"EOF"` in an instruction's arguments: (marker, strip leading tabs).
fn heredoc_markers(args: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut rest = args;
    while let Some(i) = rest.find("<<") {
        rest = &rest[i + 2..];
        let strip = rest.starts_with('-');
        let r = rest.trim_start_matches('-').trim_start_matches(['"', '\'']);
        let marker: String = r.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        if !marker.is_empty() {
            out.push((marker, strip));
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
enum Form {
    Exec(Vec<String>),
    Shell(String),
}

impl Form {
    fn parse(args: &str) -> Self {
        let a = args.trim();
        if a.starts_with('[')
            && let Ok(v) = serde_json::from_str::<Vec<String>>(a)
        {
            return Form::Exec(v);
        }
        Form::Shell(a.to_owned())
    }

    fn argv(&self, shell: &[String]) -> Vec<String> {
        match self {
            Form::Exec(v) => v.clone(),
            Form::Shell(s) => shell.iter().cloned().chain([s.clone()]).collect(),
        }
    }
}

#[derive(Clone, Debug)]
struct Stage {
    name: Option<String>,
    image: String,
    from_line: usize,
    vars: BTreeMap<String, String>,
    shell: Vec<String>,
    expose: Vec<(u16, usize)>,
    cmd: Option<(Form, usize)>,
    /// CMD was set in this stage (an inherited CMD is cleared by a new ENTRYPOINT).
    cmd_here: bool,
    entrypoint: Option<(Form, usize)>,
    /// `None` form = `HEALTHCHECK NONE`.
    health: Option<(Option<Form>, usize)>,
}

impl Stage {
    fn fresh(image: String, name: Option<String>, line: usize) -> Self {
        Self {
            name,
            image,
            from_line: line,
            vars: BTreeMap::new(),
            shell: vec!["/bin/sh".into(), "-c".into()],
            expose: Vec::new(),
            cmd: None,
            cmd_here: false,
            entrypoint: None,
            health: None,
        }
    }

    /// The argv the image runs by default (ENTRYPOINT + CMD, as Docker combines them).
    fn command(&self) -> Option<(Vec<String>, usize, String)> {
        let lines = |a: usize, b: Option<usize>| b.map_or(a, |b| a.min(b));
        match (&self.entrypoint, &self.cmd) {
            (Some((Form::Exec(e), el)), cmd) => {
                let mut argv = e.clone();
                if let Some((c, _)) = cmd {
                    argv.extend(c.argv(&self.shell));
                }
                Some((argv, lines(*el, cmd.as_ref().map(|c| c.1)), "ENTRYPOINT + CMD".into()))
            }
            (Some((shell @ Form::Shell(_), el)), _) => Some((shell.argv(&self.shell), *el, "ENTRYPOINT (shell form)".into())),
            (None, Some((c, cl))) => Some((c.argv(&self.shell), *cl, "CMD".into())),
            (None, None) => None,
        }
    }

    fn apply(&mut self, ins: &Instr) {
        match ins.keyword.as_str() {
            "ENV" => {
                for (k, v) in assignments(&ins.args, true) {
                    let v = self.substitute(&v);
                    self.vars.insert(k, v);
                }
            }
            "ARG" => {
                for (k, v) in assignments(&ins.args, false) {
                    let v = self.substitute(&v);
                    self.vars.entry(k).or_insert(v);
                }
            }
            "EXPOSE" => {
                for tok in ins.args.split_whitespace() {
                    let tok = self.substitute(tok);
                    let (spec, proto) = tok.split_once('/').unwrap_or((&tok, "tcp"));
                    let first = spec.split('-').next().unwrap_or(spec);
                    if proto.eq_ignore_ascii_case("tcp")
                        && let Ok(port) = first.parse::<u16>()
                        && port > 0
                    {
                        self.expose.push((port, ins.line));
                    }
                }
            }
            "CMD" => {
                self.cmd = Some((Form::parse(&ins.args), ins.line));
                self.cmd_here = true;
            }
            "ENTRYPOINT" => {
                self.entrypoint = Some((Form::parse(&ins.args), ins.line));
                if !self.cmd_here {
                    self.cmd = None;
                }
            }
            "SHELL" => {
                if let Form::Exec(v) = Form::parse(&ins.args)
                    && !v.is_empty()
                {
                    self.shell = v;
                }
            }
            "HEALTHCHECK" => self.health = parse_healthcheck(&ins.args).map(|f| (f, ins.line)),
            _ => {}
        }
    }

    /// `$VAR`, `${VAR}`, `${VAR:-default}` / `${VAR-default}` from ENV/ARG seen so far.
    fn substitute(&self, s: &str) -> String {
        let mut out = String::new();
        let mut rest = s;
        while let Some(i) = rest.find('$') {
            out.push_str(&rest[..i]);
            rest = &rest[i + 1..];
            if let Some(body) = rest.strip_prefix('{') {
                let Some(end) = body.find('}') else {
                    out.push('$');
                    continue;
                };
                let expr = &body[..end];
                rest = &body[end + 1..];
                let (name, default) = match expr.split_once(":-").or_else(|| expr.split_once('-')) {
                    Some((n, d)) => (n, Some(d)),
                    None => (expr, None),
                };
                match (self.vars.get(name), default) {
                    (Some(v), _) if !v.is_empty() => out.push_str(v),
                    (_, Some(d)) => out.push_str(d),
                    (Some(v), None) => out.push_str(v),
                    (None, None) => {}
                }
            } else {
                let len = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(rest.len());
                if len == 0 {
                    out.push('$');
                    continue;
                }
                if let Some(v) = self.vars.get(&rest[..len]) {
                    out.push_str(v);
                }
                rest = &rest[len..];
            }
        }
        out.push_str(rest);
        out
    }
}

/// `HEALTHCHECK [--opt=v …] CMD <form>` → `Some(Some(form))`; `HEALTHCHECK NONE` → `Some(None)`.
fn parse_healthcheck(args: &str) -> Option<Option<Form>> {
    let mut rest = args.trim_start();
    while rest.starts_with("--") {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        rest = rest[end..].trim_start();
    }
    let (word, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    if word.eq_ignore_ascii_case("NONE") {
        return Some(None);
    }
    word.eq_ignore_ascii_case("CMD").then(|| Some(Form::parse(tail)))
}

/// `ENV A=1 B="two words"` / legacy `ENV A value with spaces`; `ARG NAME[=default]`.
fn assignments(args: &str, legacy_allowed: bool) -> Vec<(String, String)> {
    let words = words(args);
    match words.first() {
        Some(first) if legacy_allowed && !first.contains('=') => {
            let value = args.trim_start()[first.len()..].trim().to_owned();
            vec![(first.clone(), value)]
        }
        _ => words
            .into_iter()
            .map(|w| match w.split_once('=') {
                Some((k, v)) => (k.to_owned(), v.to_owned()),
                None => (w, String::new()),
            })
            .collect(),
    }
}

/// Whitespace-separated words with double and single quotes removed.
fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote = None;
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    in_word = true;
                }
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

/// The last stage, with `FROM <earlier stage>` inheriting that stage's settings.
fn final_stage(instrs: &[Instr]) -> Option<Stage> {
    let mut stages: Vec<Stage> = Vec::new();
    for ins in instrs {
        if ins.keyword == "FROM" {
            let toks: Vec<&str> = ins.args.split_whitespace().filter(|t| !t.starts_with("--")).collect();
            let image = toks.first().copied().unwrap_or_default().to_owned();
            let name = match toks.as_slice() {
                [_, kw, name, ..] if kw.eq_ignore_ascii_case("as") => Some(name.to_ascii_lowercase()),
                _ => None,
            };
            let parent = stages.iter().rev().find(|s| s.name.as_deref() == Some(image.to_ascii_lowercase().as_str()));
            let stage = match parent {
                Some(p) => Stage { name, image: image.clone(), from_line: ins.line, cmd_here: false, ..p.clone() },
                None => Stage::fresh(image, name, ins.line),
            };
            stages.push(stage);
        } else if let Some(s) = stages.last_mut() {
            s.apply(ins);
        }
    }
    stages.pop()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage(text: &str) -> Stage {
        final_stage(&lex(text)).unwrap()
    }

    #[test]
    fn continuations_comments_and_heredocs() {
        let text =
            "# syntax=docker/dockerfile:1\nFROM alpine\nRUN apk add \\\n  # a comment\n  curl\nRUN <<EOF\nEXPOSE 9999\nEOF\nEXPOSE 80\n";
        let ins = lex(text);
        assert_eq!(ins.iter().map(|i| i.keyword.as_str()).collect::<Vec<_>>(), ["FROM", "RUN", "RUN", "EXPOSE"]);
        assert_eq!(ins[1], Instr { line: 3, keyword: "RUN".into(), args: "apk add curl".into() });
        assert_eq!(stage(text).expose, vec![(80, 9)]);
    }

    #[test]
    fn escape_directive_changes_the_continuation_character() {
        let ins = lex("# escape=`\nFROM windows\nCMD a `\n  b\n");
        assert_eq!(ins[1].args, "a b");
    }

    #[test]
    fn entrypoint_and_cmd_combine_like_docker() {
        let s = stage("FROM x\nENTRYPOINT [\"tini\", \"--\"]\nCMD [\"node\", \"a.js\"]\n");
        assert_eq!(s.command().unwrap().0, ["tini", "--", "node", "a.js"]);
        let s = stage("FROM x\nCMD node a.js\n");
        assert_eq!(s.command().unwrap().0, ["/bin/sh", "-c", "node a.js"]);
        let s = stage("FROM x\nSHELL [\"/bin/bash\", \"-lc\"]\nENTRYPOINT exec app\nCMD [\"ignored\"]\n");
        assert_eq!(s.command().unwrap().0, ["/bin/bash", "-lc", "exec app"]);
    }

    #[test]
    fn a_new_entrypoint_clears_an_inherited_cmd() {
        let s = stage("FROM x AS base\nCMD [\"old\"]\nFROM base\nENTRYPOINT [\"new\"]\n");
        assert_eq!(s.command().unwrap().0, ["new"]);
    }

    #[test]
    fn variables_resolve_in_expose() {
        let s = stage("FROM x\nARG BASE=7000\nENV PORT=${PORT:-8080} OTHER=$BASE\nEXPOSE $PORT ${OTHER} 53/udp 9000-9005\n");
        assert_eq!(s.expose.iter().map(|e| e.0).collect::<Vec<_>>(), [8080, 7000, 9000]);
    }

    #[test]
    fn healthcheck_forms() {
        assert_eq!(parse_healthcheck("NONE"), Some(None));
        assert_eq!(parse_healthcheck("--interval=5s --retries=3 CMD [\"pg_isready\"]"), Some(Some(Form::Exec(vec!["pg_isready".into()]))));
        assert_eq!(parse_healthcheck("--interval=5s"), None);
    }
}
