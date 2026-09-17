//! YAML as a tree whose nodes remember their line, so evidence can cite `file:<path>:<line>`.
//! Anchors, aliases and `<<` merge keys are resolved.

use std::collections::HashMap;

use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser};
use yaml_rust2::scanner::Marker;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Yaml {
    Scalar(String),
    Seq(Vec<Node>),
    Map(Vec<(Node, Node)>),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Node {
    pub(crate) v: Yaml,
    /// 1-based.
    pub(crate) line: usize,
}

impl Node {
    pub(crate) fn get(&self, key: &str) -> Option<&Node> {
        match &self.v {
            Yaml::Map(entries) => entries.iter().find(|(k, _)| k.str() == Some(key)).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The key node of `key`, for evidence that should point at the key's line.
    pub(crate) fn key(&self, key: &str) -> Option<&Node> {
        match &self.v {
            Yaml::Map(entries) => entries.iter().find(|(k, _)| k.str() == Some(key)).map(|(k, _)| k),
            _ => None,
        }
    }

    pub(crate) fn str(&self) -> Option<&str> {
        match &self.v {
            Yaml::Scalar(s) if !is_null(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn seq(&self) -> &[Node] {
        match &self.v {
            Yaml::Seq(items) => items,
            _ => &[],
        }
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = (&Node, &Node)> {
        let items: &[(Node, Node)] = match &self.v {
            Yaml::Map(entries) => entries,
            _ => &[],
        };
        items.iter().map(|(k, v)| (k, v))
    }

    pub(crate) fn is_map(&self) -> bool {
        matches!(self.v, Yaml::Map(_))
    }

    pub(crate) fn bool(&self) -> Option<bool> {
        match self.str()? {
            "true" | "True" | "TRUE" | "yes" | "on" => Some(true),
            "false" | "False" | "FALSE" | "no" | "off" => Some(false),
            _ => None,
        }
    }
}

fn is_null(s: &str) -> bool {
    matches!(s, "" | "~" | "null" | "Null" | "NULL")
}

/// Parses the first document.
pub(crate) fn parse(text: &str) -> Result<Node, String> {
    let mut b = Builder::default();
    Parser::new_from_str(text).load(&mut b, false).map_err(|e| e.to_string())?;
    if let Some(e) = b.error {
        return Err(e);
    }
    Ok(b.root.unwrap_or(Node { v: Yaml::Map(Vec::new()), line: 1 }))
}

enum Frame {
    Seq(Vec<Node>, usize, usize),
    Map(Vec<(Node, Node)>, Option<Node>, usize, usize),
}

#[derive(Default)]
struct Builder {
    stack: Vec<Frame>,
    anchors: HashMap<usize, Node>,
    root: Option<Node>,
    error: Option<String>,
}

impl Builder {
    fn push(&mut self, node: Node, anchor: usize) {
        if anchor > 0 {
            self.anchors.insert(anchor, node.clone());
        }
        match self.stack.last_mut() {
            None => {
                if self.root.is_none() {
                    self.root = Some(node);
                }
            }
            Some(Frame::Seq(items, ..)) => items.push(node),
            Some(Frame::Map(entries, pending, ..)) => match pending.take() {
                None => *pending = Some(node),
                Some(key) => entries.push((key, node)),
            },
        }
    }
}

impl MarkedEventReceiver for Builder {
    fn on_event(&mut self, ev: Event, mark: Marker) {
        let line = mark.line().max(1);
        match ev {
            Event::Scalar(s, _, anchor, _) => self.push(Node { v: Yaml::Scalar(s), line }, anchor),
            Event::Alias(id) => match self.anchors.get(&id).cloned() {
                Some(n) => self.push(n, 0),
                None => self.error = Some(format!("line {line}: unknown alias")),
            },
            Event::SequenceStart(anchor, _) => self.stack.push(Frame::Seq(Vec::new(), line, anchor)),
            Event::MappingStart(anchor, _) => self.stack.push(Frame::Map(Vec::new(), None, line, anchor)),
            Event::SequenceEnd => {
                if let Some(Frame::Seq(items, line, anchor)) = self.stack.pop() {
                    self.push(Node { v: Yaml::Seq(items), line }, anchor);
                }
            }
            Event::MappingEnd => {
                if let Some(Frame::Map(entries, _, line, anchor)) = self.stack.pop() {
                    self.push(Node { v: Yaml::Map(resolve_merges(entries)), line }, anchor);
                }
            }
            _ => {}
        }
    }
}

/// `<<: *base` / `<<: [*a, *b]`: merged keys fill in what the map does not set itself.
fn resolve_merges(entries: Vec<(Node, Node)>) -> Vec<(Node, Node)> {
    let (merges, mut own): (Vec<_>, Vec<_>) = entries.into_iter().partition(|(k, _)| k.str() == Some("<<"));
    for (_, source) in merges {
        let sources: Vec<Node> = match source.v {
            Yaml::Seq(items) => items,
            other => vec![Node { v: other, line: source.line }],
        };
        for s in sources {
            if let Yaml::Map(inherited) = s.v {
                for (k, v) in inherited {
                    if !own.iter().any(|(ok, _)| ok.str() == k.str()) {
                        own.push((k, v));
                    }
                }
            }
        }
    }
    own
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodes_carry_their_lines() {
        let doc = parse("a: 1\nlist:\n  - x\n  - y: 2\n").unwrap();
        assert_eq!(doc.get("a").unwrap().line, 1);
        let list = doc.get("list").unwrap().seq();
        assert_eq!((list[0].str(), list[0].line), (Some("x"), 3));
        assert_eq!(list[1].get("y").unwrap().line, 4);
        assert_eq!(doc.key("list").unwrap().line, 2);
    }

    #[test]
    fn anchors_and_merge_keys_resolve() {
        let doc = parse("base: &b\n  image: x\n  port: 1\nsvc:\n  <<: *b\n  port: 2\n").unwrap();
        let svc = doc.get("svc").unwrap();
        assert_eq!(svc.get("image").and_then(Node::str), Some("x"));
        assert_eq!(svc.get("port").and_then(Node::str), Some("2"));
    }

    #[test]
    fn nulls_are_not_strings() {
        let doc = parse("a: ~\nb:\nc: ''\n").unwrap();
        assert!(["a", "b", "c"].iter().all(|k| doc.get(k).unwrap().str().is_none()));
    }

    #[test]
    fn syntax_errors_are_reported() {
        assert!(parse("a: [1, 2\n").is_err());
    }
}
