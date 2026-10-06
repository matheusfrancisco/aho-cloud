//! `Parser::catalog`: i18n message catalogs. JSON / YAML / EDN through their tree-sitter
//! grammars (nested maps flattened into dotted keys, string leaves only), `.properties` and
//! gettext `.po` by line parsers. Messages are kept raw: placeholders untouched, only the
//! format's own quoting / escapes decoded.

use mage_core::domain::RepoPath;
use mage_core::ports::knowledge::{BlobInput, Catalog, CatalogEntry, CatalogFormat};
use tree_sitter::Node;

use super::clojure::{sym_text, values};
use super::literals::str_value;
use super::registry::Registry;
use super::text;
use crate::ports::parser::ParseError;

/// Directory names under which path segments / file stems name a locale.
const LOCALE_DIRS: &[&str] = &["locales", "locale", "i18n", "l10n", "translations", "lang", "langs", "languages", "messages"];

pub(super) fn catalog(registry: &Registry, blob: &BlobInput) -> Result<Option<Catalog>, ParseError> {
    let Some(ext) = blob.path.extension().map(str::to_ascii_lowercase) else { return Ok(None) };
    let format = match ext.as_str() {
        "json" => CatalogFormat::Json,
        "yml" | "yaml" => CatalogFormat::Yaml,
        "properties" => CatalogFormat::Properties,
        "po" => CatalogFormat::Po,
        "edn" => CatalogFormat::Edn,
        _ => return Ok(None),
    };
    let path_locale = path_locale(&blob.path, format);
    let entries = match format {
        CatalogFormat::Properties => properties(&String::from_utf8_lossy(&blob.bytes), path_locale),
        CatalogFormat::Po => po(&String::from_utf8_lossy(&blob.bytes), path_locale),
        CatalogFormat::Json | CatalogFormat::Yaml | CatalogFormat::Edn => {
            let language = match format {
                CatalogFormat::Json => &registry.json,
                CatalogFormat::Yaml => &registry.yaml,
                _ => registry.clojure(),
            };
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(language).map_err(|e| ParseError::Unavailable(format!("catalog grammar: {e}")))?;
            let tree = parser.parse(&blob.bytes, None).ok_or_else(|| ParseError::Invalid("catalog parse failed".into()))?;
            let root = tree.root_node();
            let value = match format {
                CatalogFormat::Json => json_root(root, &blob.bytes),
                CatalogFormat::Yaml => yaml_root(root, &blob.bytes),
                _ => edn_root(root, &blob.bytes),
            };
            let Some(Value::Map(map)) = value else { return Ok(None) };
            flatten_root(map, path_locale)
        }
    };
    Ok(Some(Catalog { format, entries }))
}

// ---------------------------------------------------------------------------------------------
// Locale from path

fn is_locale(s: &str, strict: bool) -> bool {
    let mut parts = s.split(['-', '_']);
    let Some(lang) = parts.next() else { return false };
    let lang_ok = if strict { lang.len() == 2 } else { (2..=3).contains(&lang.len()) };
    if !lang_ok || !lang.bytes().all(|b| b.is_ascii_lowercase()) {
        return false;
    }
    let mut count = 0;
    for part in parts {
        count += 1;
        if count > 2 || !(2..=4).contains(&part.len()) || !part.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return false;
        }
    }
    true
}

/// `locales/en/…`, `i18n/fr.json`, `en.yml`, `translation.de.json`, `messages_de.properties`.
fn path_locale(path: &RepoPath, format: CatalogFormat) -> Option<String> {
    let segments: Vec<&str> = path.segments().collect();
    let (file, dirs) = segments.split_last()?;
    let root = dirs.iter().position(|d| LOCALE_DIRS.contains(&d.to_ascii_lowercase().as_str()));
    if let Some(dir) = root.and_then(|r| dirs.get(r + 1)).filter(|d| is_locale(d, false)) {
        return Some((*dir).to_string());
    }
    let stem = file.rsplit_once('.').map_or(*file, |(s, _)| s);
    let first = stem.split('.').next().unwrap_or(stem);
    if is_locale(first, root.is_none()) {
        return Some(first.to_string());
    }
    if let Some((_, last)) = stem.rsplit_once('.') {
        if is_locale(last, true) {
            return Some(last.to_string());
        }
    }
    if format == CatalogFormat::Properties {
        // ResourceBundle: base_lang[_COUNTRY]
        let tokens: Vec<&str> = stem.split('_').collect();
        if tokens.len() >= 3 {
            let pair = format!("{}_{}", tokens[tokens.len() - 2], tokens[tokens.len() - 1]);
            if is_locale(&pair, true) {
                return Some(pair);
            }
        }
        if tokens.len() >= 2 && is_locale(tokens[tokens.len() - 1], true) {
            return Some(tokens[tokens.len() - 1].to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Tree catalogs

enum Value {
    Map(Vec<(String, Value)>),
    Str(String, u32),
    Other,
}

/// Flattens nested maps; a root whose keys are all locales (Rails `en:`) sets each entry's
/// locale and drops that key.
fn flatten_root(map: Vec<(String, Value)>, path_locale: Option<String>) -> Vec<CatalogEntry> {
    let mut out = Vec::new();
    let locale_root = !map.is_empty() && map.iter().all(|(k, v)| is_locale(k, true) && matches!(v, Value::Map(_)));
    if locale_root {
        for (locale, value) in map {
            if let Value::Map(inner) = value {
                flatten(inner, "", &Some(locale), &mut out);
            }
        }
    } else {
        flatten(map, "", &path_locale, &mut out);
    }
    out
}

fn flatten(map: Vec<(String, Value)>, prefix: &str, locale: &Option<String>, out: &mut Vec<CatalogEntry>) {
    for (key, value) in map {
        let full = if prefix.is_empty() { key } else { format!("{prefix}.{key}") };
        match value {
            Value::Map(inner) => flatten(inner, &full, locale, out),
            Value::Str(message, line) => out.push(CatalogEntry { key: full, locale: locale.clone(), message, line }),
            Value::Other => {}
        }
    }
}

fn json_root(root: Node<'_>, src: &[u8]) -> Option<Value> {
    text::named_children(root).into_iter().find(|n| n.kind() == "object").map(|o| json_value(o, src))
}

fn json_value(node: Node<'_>, src: &[u8]) -> Value {
    match node.kind() {
        "object" => Value::Map(
            text::named_children(node)
                .into_iter()
                .filter(|p| p.kind() == "pair")
                .filter_map(|p| Some((json_string(p.child_by_field_name("key")?, src), json_value(p.child_by_field_name("value")?, src))))
                .collect(),
        ),
        "string" => Value::Str(json_string(node, src), text::start_line(node)),
        _ => Value::Other,
    }
}

fn json_string(node: Node<'_>, src: &[u8]) -> String {
    let mut out = String::new();
    for c in text::named_children(node) {
        match c.kind() {
            "string_content" => out.push_str(text::text(c, src)),
            "escape_sequence" => text::unescape_into(text::text(c, src), &mut out),
            _ => {}
        }
    }
    out
}

fn yaml_root(root: Node<'_>, src: &[u8]) -> Option<Value> {
    let document = text::named_children(root).into_iter().find(|n| n.kind() == "document")?;
    let body = text::named_children(document).into_iter().find(|n| matches!(n.kind(), "block_node" | "flow_node"))?;
    Some(yaml_value(body, src))
}

/// Content child of a `block_node` / `flow_node` (anchors and tags skipped).
fn yaml_content(node: Node<'_>) -> Option<Node<'_>> {
    if !matches!(node.kind(), "block_node" | "flow_node") {
        return Some(node);
    }
    text::named_children(node).into_iter().find(|c| !matches!(c.kind(), "anchor" | "tag"))
}

fn yaml_value(node: Node<'_>, src: &[u8]) -> Value {
    let Some(content) = yaml_content(node) else { return Value::Other };
    match content.kind() {
        "block_mapping" => Value::Map(
            text::named_children(content)
                .into_iter()
                .filter(|p| p.kind() == "block_mapping_pair")
                .filter_map(|p| yaml_pair(p, src))
                .collect(),
        ),
        "flow_mapping" => Value::Map(
            text::named_children(content)
                .into_iter()
                .filter(|p| p.kind() == "flow_pair")
                .filter_map(|p| yaml_pair(p, src))
                .collect(),
        ),
        "plain_scalar" | "double_quote_scalar" | "single_quote_scalar" | "block_scalar" => match yaml_scalar(content, src) {
            Some(s) => Value::Str(s, text::start_line(content)),
            None => Value::Other,
        },
        _ => Value::Other,
    }
}

fn yaml_pair(pair: Node<'_>, src: &[u8]) -> Option<(String, Value)> {
    let key = pair.child_by_field_name("key").and_then(yaml_content)?;
    let key = match key.kind() {
        "plain_scalar" => text::text(key, src).trim().to_string(),
        _ => yaml_scalar(key, src)?,
    };
    let value = pair.child_by_field_name("value").map_or(Value::Other, |v| yaml_value(v, src));
    Some((key, value))
}

/// String value of a scalar; `None` for non-string plain scalars (numbers, booleans, null).
fn yaml_scalar(node: Node<'_>, src: &[u8]) -> Option<String> {
    let raw = text::text(node, src);
    match node.kind() {
        "plain_scalar" => {
            let inner = text::named_children(node).into_iter().next()?;
            (inner.kind() == "string_scalar").then(|| raw.lines().map(str::trim).collect::<Vec<_>>().join(" "))
        }
        "double_quote_scalar" => Some(text::unescape(text::unquote(raw))),
        "single_quote_scalar" => Some(text::unquote(raw).replace("''", "'")),
        "block_scalar" => Some(block_scalar(raw)),
        _ => None,
    }
}

/// `|` keeps line breaks, `>` folds them into spaces; indentation stripped, trailing breaks dropped.
fn block_scalar(raw: &str) -> String {
    let mut lines = raw.lines();
    let header = lines.next().unwrap_or("");
    let folded = header.trim_start().starts_with('>');
    let body: Vec<&str> = lines.collect();
    let indent = body.iter().filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
    let stripped: Vec<&str> = body.iter().map(|l| l.get(indent..).unwrap_or("").trim_end()).collect();
    let joined = if folded { stripped.join(" ") } else { stripped.join("\n") };
    joined.trim_end().to_string()
}

fn edn_root(root: Node<'_>, src: &[u8]) -> Option<Value> {
    text::named_children(root).into_iter().find(|n| n.kind() == "map_lit").map(|m| edn_value(m, src))
}

fn edn_value(node: Node<'_>, src: &[u8]) -> Value {
    match node.kind() {
        "map_lit" => {
            let vals = values(node);
            Value::Map(vals.chunks(2).filter_map(|pair| match pair {
                [k, v] => Some((edn_key(*k, src), edn_value(*v, src))),
                _ => None,
            }).collect())
        }
        "str_lit" => Value::Str(str_value(node, src), text::start_line(node)),
        _ => Value::Other,
    }
}

/// Keyword keys without the colon (`:errors/x` → `errors/x`), string keys unquoted.
fn edn_key(node: Node<'_>, src: &[u8]) -> String {
    match node.kind() {
        "kwd_lit" => text::text(node, src).trim_start_matches(':').to_string(),
        "str_lit" => str_value(node, src),
        "sym_lit" => sym_text(node, src),
        _ => text::squash(text::text(node, src)),
    }
}

// ---------------------------------------------------------------------------------------------
// .properties

fn properties(content: &str, locale: Option<String>) -> Vec<CatalogEntry> {
    let mut out = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let start_line = u32::try_from(i + 1).unwrap_or(u32::MAX);
        let first = lines[i].trim_start();
        i += 1;
        if first.is_empty() || first.starts_with('#') || first.starts_with('!') {
            continue;
        }
        // Logical line: a trailing odd run of backslashes continues on the next line.
        let mut logical = String::from(first);
        while ends_with_continuation(&logical) && i < lines.len() {
            logical.pop();
            logical.push_str(lines[i].trim_start());
            i += 1;
        }
        let (key, value) = split_property(&logical);
        if key.is_empty() {
            continue;
        }
        out.push(CatalogEntry { key: properties_unescape(&key), locale: locale.clone(), message: properties_unescape(&value), line: start_line });
    }
    out
}

fn ends_with_continuation(s: &str) -> bool {
    s.bytes().rev().take_while(|b| *b == b'\\').count() % 2 == 1
}

/// Key up to the first unescaped `=`, `:` or whitespace; value after the separator.
fn split_property(line: &str) -> (String, String) {
    let mut key_end = line.len();
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '=' | ':' => {
                key_end = i;
                break;
            }
            c if c.is_whitespace() => {
                key_end = i;
                break;
            }
            _ => {}
        }
    }
    let key = line[..key_end].to_string();
    let mut rest = line[key_end..].trim_start();
    if let Some(r) = rest.strip_prefix(['=', ':']) {
        rest = r.trim_start();
    }
    (key, rest.to_string())
}

fn properties_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('f') => out.push('\u{0c}'),
            Some('u') => {
                let hex: String = (0..4).filter_map(|_| chars.next_if(char::is_ascii_hexdigit)).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(ch) if hex.len() == 4 => out.push(ch),
                    _ => {
                        out.push_str("\\u");
                        out.push_str(&hex);
                    }
                }
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// gettext .po

#[derive(Default)]
struct PoEntry {
    msgctxt: Option<String>,
    msgid: Option<String>,
    msgid_line: u32,
    msgstr: Option<String>,
    msgstr_line: u32,
    /// Field continuation lines append to: 0 ctxt, 1 id, 2 id_plural, 3 str (first form), 4 ignored.
    field: u8,
}

fn po(content: &str, path_locale: Option<String>) -> Vec<CatalogEntry> {
    let mut out: Vec<CatalogEntry> = Vec::new();
    let mut header_locale: Option<String> = None;
    let mut cur = PoEntry::default();
    for (i, raw) in content.lines().enumerate() {
        let line_no = u32::try_from(i + 1).unwrap_or(u32::MAX);
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            if cur.msgstr.is_some() {
                flush_po(std::mem::take(&mut cur), &mut out, &mut header_locale);
            }
            continue;
        }
        if line.starts_with('"') {
            let s = po_string(line);
            let target = match cur.field {
                0 => cur.msgctxt.as_mut(),
                1 => cur.msgid.as_mut(),
                3 => cur.msgstr.as_mut(),
                _ => None,
            };
            if let Some(t) = target {
                t.push_str(&s);
            }
            continue;
        }
        let (keyword, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let value = po_string(rest.trim());
        match keyword {
            "msgctxt" | "msgid" if cur.msgstr.is_some() => {
                flush_po(std::mem::take(&mut cur), &mut out, &mut header_locale);
                po_start(&mut cur, keyword, value, line_no);
            }
            "msgctxt" | "msgid" => po_start(&mut cur, keyword, value, line_no),
            "msgid_plural" => cur.field = 2,
            "msgstr" | "msgstr[0]" => {
                cur.msgstr = Some(value);
                cur.msgstr_line = line_no;
                cur.field = 3;
            }
            k if k.starts_with("msgstr[") => cur.field = 4,
            _ => {}
        }
    }
    flush_po(cur, &mut out, &mut header_locale);
    let locale = header_locale.or(path_locale);
    for e in &mut out {
        e.locale = locale.clone();
    }
    out
}

fn po_start(cur: &mut PoEntry, keyword: &str, value: String, line: u32) {
    if keyword == "msgctxt" {
        cur.msgctxt = Some(value);
        cur.field = 0;
    } else {
        cur.msgid = Some(value);
        cur.msgid_line = line;
        cur.field = 1;
    }
}

fn flush_po(entry: PoEntry, out: &mut Vec<CatalogEntry>, header_locale: &mut Option<String>) {
    let Some(msgid) = entry.msgid else { return };
    let msgstr = entry.msgstr.unwrap_or_default();
    if msgid.is_empty() {
        // Header entry: `Language: fr\n`.
        if entry.msgctxt.is_none() {
            *header_locale = msgstr
                .lines()
                .find_map(|l| l.strip_prefix("Language:"))
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty());
        }
        return;
    }
    let (message, line) = if msgstr.is_empty() { (msgid.clone(), entry.msgid_line) } else { (msgstr, entry.msgstr_line) };
    out.push(CatalogEntry { key: msgid, locale: None, message, line });
}

/// Decoded content of a `"…"` PO string.
fn po_string(s: &str) -> String {
    let inner = s.strip_prefix('"').and_then(|r| r.strip_suffix('"')).unwrap_or(s);
    text::unescape(inner)
}

#[cfg(test)]
mod tests {
    use mage_core::domain::{BlobOid, Language};

    use super::*;

    fn run(path: &str, src: &str) -> Option<Catalog> {
        let registry = Registry::new().unwrap();
        let blob = BlobInput { oid: BlobOid::from_bytes([0; 20]), path: RepoPath::new(path).unwrap(), lang: Language::Other, bytes: src.as_bytes().to_vec() };
        catalog(&registry, &blob).unwrap()
    }

    fn entries(c: &Catalog) -> Vec<(&str, Option<&str>, &str, u32)> {
        c.entries.iter().map(|e| (e.key.as_str(), e.locale.as_deref(), e.message.as_str(), e.line)).collect()
    }

    #[test]
    fn json_nested_objects_flatten_to_dotted_keys() {
        let src = "{\n  \"errors\": {\n    \"not_found\": \"Connection {{name}} not found\",\n    \"count\": 3,\n    \"quote\": \"say \\\"hi\\\"\"\n  },\n  \"title\": \"Home\"\n}\n";
        let c = run("web/public/locales/pt-BR/common.json", src).unwrap();
        assert_eq!(c.format, CatalogFormat::Json);
        assert_eq!(
            entries(&c),
            vec![
                ("errors.not_found", Some("pt-BR"), "Connection {{name}} not found", 3),
                ("errors.quote", Some("pt-BR"), "say \"hi\"", 5),
                ("title", Some("pt-BR"), "Home", 7),
            ]
        );
    }

    #[test]
    fn json_locale_from_i18n_file_stem() {
        let c = run("src/i18n/fr.json", "{\"a\": \"b\"}").unwrap();
        assert_eq!(entries(&c), vec![("a", Some("fr"), "b", 1)]);
    }

    #[test]
    fn json_array_root_is_not_a_catalog() {
        assert!(run("locales/en.json", "[\"a\"]").is_none());
    }

    #[test]
    fn yaml_rails_locale_root() {
        let src = "en:\n  errors:\n    messages:\n      blank: \"can't be blank\"\n      taken: 'has already been %{attribute}'\n      long: |\n        line one\n        line two\n  hello: Hello world\n  count: 3\n";
        let c = run("config/locales/devise.yml", src).unwrap();
        assert_eq!(c.format, CatalogFormat::Yaml);
        assert_eq!(
            entries(&c),
            vec![
                ("errors.messages.blank", Some("en"), "can't be blank", 4),
                ("errors.messages.taken", Some("en"), "has already been %{attribute}", 5),
                ("errors.messages.long", Some("en"), "line one\nline two", 6),
                ("hello", Some("en"), "Hello world", 9),
            ]
        );
    }

    #[test]
    fn yaml_without_locale_root_uses_path() {
        let c = run("config/locales/de.yml", "greeting: Hallo\n").unwrap();
        assert_eq!(entries(&c), vec![("greeting", Some("de"), "Hallo", 1)]);
    }

    #[test]
    fn properties_lines_continuations_and_comments() {
        let src = "# comment\n! also comment\nerror.notFound=Resource {0} not found\nerror.multi = first \\\n    second\nkey\\:with\\=sep : value\\tTab\n\nempty=\n";
        let c = run("src/main/resources/messages_de.properties", src).unwrap();
        assert_eq!(c.format, CatalogFormat::Properties);
        assert_eq!(
            entries(&c),
            vec![
                ("error.notFound", Some("de"), "Resource {0} not found", 3),
                ("error.multi", Some("de"), "first second", 4),
                ("key:with=sep", Some("de"), "value\tTab", 6),
                ("empty", Some("de"), "", 8),
            ]
        );
    }

    #[test]
    fn properties_country_suffix() {
        let c = run("messages_pt_BR.properties", "a=b\n").unwrap();
        assert_eq!(entries(&c), vec![("a", Some("pt_BR"), "b", 1)]);
    }

    #[test]
    fn po_pairs_multiline_and_header_language() {
        let src = "msgid \"\"\nmsgstr \"\"\n\"Language: fr\\n\"\n\"Content-Type: text/plain\\n\"\n\n#: src/a.py:10\nmsgid \"Connection %s not found\"\nmsgstr \"Connexion %s \"\n\"introuvable\"\n\nmsgid \"Untranslated\"\nmsgstr \"\"\n\nmsgctxt \"menu\"\nmsgid \"File\"\nmsgid_plural \"Files\"\nmsgstr[0] \"Fichier\"\nmsgstr[1] \"Fichiers\"\n";
        let c = run("locale/de/LC_MESSAGES/django.po", src).unwrap();
        assert_eq!(c.format, CatalogFormat::Po);
        assert_eq!(
            entries(&c),
            vec![
                ("Connection %s not found", Some("fr"), "Connexion %s introuvable", 8),
                ("Untranslated", Some("fr"), "Untranslated", 11),
                ("File", Some("fr"), "Fichier", 17),
            ]
        );
    }

    #[test]
    fn po_locale_from_path_without_header() {
        let c = run("locale/de/LC_MESSAGES/app.po", "msgid \"Hi\"\nmsgstr \"Hallo\"\n").unwrap();
        assert_eq!(entries(&c), vec![("Hi", Some("de"), "Hallo", 2)]);
    }

    #[test]
    fn edn_keyword_maps_flatten_with_locale_root() {
        let src = "{:en {:errors {:not-found \"Connection %1 not found\"\n               :code 404}\n      :title \"Home\"}\n :fr {:title \"Accueil\"}}\n";
        let c = run("resources/i18n/dict.edn", src).unwrap();
        assert_eq!(c.format, CatalogFormat::Edn);
        assert_eq!(
            entries(&c),
            vec![
                ("errors.not-found", Some("en"), "Connection %1 not found", 1),
                ("title", Some("en"), "Home", 3),
                ("title", Some("fr"), "Accueil", 4),
            ]
        );
    }

    #[test]
    fn non_catalog_extension_is_none() {
        assert!(run("src/app.ts", "export const a = 1;").is_none());
        assert!(run("README", "x").is_none());
    }
}
