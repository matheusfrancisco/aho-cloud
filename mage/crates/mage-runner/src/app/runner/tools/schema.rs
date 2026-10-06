//! Validation of `submit_*` arguments against the checked-in JSON Schemas
//! (`mage_core::wire::mcp::SUBMIT_*_SCHEMA`, draft 2020-12), so a harness that skips client-side
//! validation gets the same answer as one that validates.
//!
//! Supports exactly the keywords those schemas use: `type`, `properties`, `required`,
//! `additionalProperties: false`, `minLength`/`maxLength` (Unicode scalar values),
//! `minItems`/`maxItems`, `items`, `minimum`/`maximum`, `enum`, `const`, `$ref` into `#/$defs`,
//! `allOf`, and `if`/`then`/`else`. Annotations (`title`, `description`, `$id`, `$schema`) are
//! ignored. An unsupported keyword is a programming error caught by the tests below, which run
//! every checked-in schema through [`Schema::compile`].

use serde_json::{Map, Value};

/// One violation, located by a JSON path into the arguments (e.g. `causes[0].evidence[1].path`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaViolation {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for SchemaViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() { write!(f, "{}", self.message) } else { write!(f, "{}: {}", self.path, self.message) }
    }
}

/// A parsed checked-in schema.
#[derive(Debug, Clone)]
pub struct Schema {
    root: Value,
}

const KNOWN_KEYWORDS: &[&str] = &[
    "$schema",
    "$id",
    "$defs",
    "$ref",
    "title",
    "description",
    "type",
    "properties",
    "required",
    "additionalProperties",
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "items",
    "minimum",
    "maximum",
    "enum",
    "const",
    "allOf",
    "if",
    "then",
    "else",
];

impl Schema {
    /// Parses `text` and rejects keywords this validator does not implement.
    pub fn compile(text: &str) -> Result<Schema, String> {
        let root: Value = serde_json::from_str(text).map_err(|e| format!("schema is not JSON: {e}"))?;
        check_keywords(&root, "")?;
        Ok(Schema { root })
    }

    /// The schema as served in `inputSchema`.
    pub fn as_object(&self) -> Map<String, Value> {
        match &self.root {
            Value::Object(map) => map.clone(),
            _ => Map::new(),
        }
    }

    /// Every violation of `value`, in document order.
    pub fn validate(&self, value: &Value) -> Vec<SchemaViolation> {
        let mut out = Vec::new();
        self.check(&self.root, value, &mut String::new(), &mut out);
        out
    }

    fn resolve<'a>(&'a self, reference: &str) -> Option<&'a Value> {
        let name = reference.strip_prefix("#/$defs/")?;
        self.root.get("$defs")?.get(name)
    }

    fn check(&self, schema: &Value, value: &Value, path: &mut String, out: &mut Vec<SchemaViolation>) {
        let Value::Object(s) = schema else { return };
        let fail = |out: &mut Vec<SchemaViolation>, path: &str, message: String| {
            out.push(SchemaViolation { path: path.to_owned(), message });
        };
        if let Some(Value::String(reference)) = s.get("$ref") {
            match self.resolve(reference) {
                Some(target) => self.check(target, value, path, out),
                None => fail(out, path, format!("unresolvable schema reference {reference}")),
            }
        }
        if let Some(Value::String(ty)) = s.get("type")
            && !type_matches(ty, value)
        {
            fail(out, path, format!("expected {ty}, got {}", type_name(value)));
            return;
        }
        if let Some(Value::Array(allowed)) = s.get("enum")
            && !allowed.contains(value)
        {
            let list: Vec<String> = allowed.iter().map(Value::to_string).collect();
            fail(out, path, format!("must be one of {}", list.join(", ")));
        }
        if let Some(expected) = s.get("const")
            && expected != value
        {
            fail(out, path, format!("must be {expected}"));
        }
        match value {
            Value::String(text) => {
                let chars = text.chars().count() as u64;
                if let Some(min) = s.get("minLength").and_then(Value::as_u64)
                    && chars < min
                {
                    fail(out, path, format!("must be at least {min} characters"));
                }
                if let Some(max) = s.get("maxLength").and_then(Value::as_u64)
                    && chars > max
                {
                    fail(out, path, format!("must be at most {max} characters (got {chars})"));
                }
            }
            Value::Number(n) => {
                if let (Some(min), Some(v)) = (s.get("minimum").and_then(Value::as_f64), n.as_f64())
                    && v < min
                {
                    fail(out, path, format!("must be ≥ {min}"));
                }
                if let (Some(max), Some(v)) = (s.get("maximum").and_then(Value::as_f64), n.as_f64())
                    && v > max
                {
                    fail(out, path, format!("must be ≤ {max}"));
                }
            }
            Value::Array(items) => {
                let len = items.len() as u64;
                if let Some(min) = s.get("minItems").and_then(Value::as_u64)
                    && len < min
                {
                    fail(out, path, format!("must have at least {min} items"));
                }
                if let Some(max) = s.get("maxItems").and_then(Value::as_u64)
                    && len > max
                {
                    fail(out, path, format!("must have at most {max} items (got {len})"));
                }
                if let Some(item_schema) = s.get("items") {
                    for (i, item) in items.iter().enumerate() {
                        let len = path.len();
                        path.push_str(&format!("[{i}]"));
                        self.check(item_schema, item, path, out);
                        path.truncate(len);
                    }
                }
            }
            Value::Object(fields) => {
                if let Some(Value::Array(required)) = s.get("required") {
                    for name in required.iter().filter_map(Value::as_str) {
                        if !fields.contains_key(name) {
                            fail(out, &join(path, name), "is required".to_owned());
                        }
                    }
                }
                let props = s.get("properties").and_then(Value::as_object);
                for (name, field) in fields {
                    match props.and_then(|p| p.get(name)) {
                        Some(field_schema) => {
                            let len = path.len();
                            *path = join(path, name);
                            self.check(field_schema, field, path, out);
                            path.truncate(len);
                        }
                        None if s.get("additionalProperties") == Some(&Value::Bool(false)) => {
                            fail(out, &join(path, name), "is not an allowed field".to_owned());
                        }
                        None => {}
                    }
                }
            }
            Value::Null | Value::Bool(_) => {}
        }
        if let Some(Value::Array(all)) = s.get("allOf") {
            for sub in all {
                self.check(sub, value, path, out);
            }
        }
        if let Some(condition) = s.get("if") {
            let mut probe = Vec::new();
            self.check(condition, value, &mut path.clone(), &mut probe);
            let branch = if probe.is_empty() { s.get("then") } else { s.get("else") };
            if let Some(branch) = branch {
                self.check(branch, value, path, out);
            }
        }
    }
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() { name.to_owned() } else { format!("{path}.{name}") }
}

fn type_matches(ty: &str, value: &Value) -> bool {
    match ty {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some() || value.as_f64().is_some_and(|f| f.fract() == 0.0),
        _ => false,
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn check_keywords(schema: &Value, at: &str) -> Result<(), String> {
    let Value::Object(map) = schema else { return Ok(()) };
    for (key, sub) in map {
        if !KNOWN_KEYWORDS.contains(&key.as_str()) {
            return Err(format!("unsupported schema keyword {key:?} at {at:?}"));
        }
        match key.as_str() {
            "properties" | "$defs" => {
                if let Value::Object(children) = sub {
                    for (name, child) in children {
                        check_keywords(child, &format!("{at}/{key}/{name}"))?;
                    }
                }
            }
            "items" | "if" | "then" | "else" => check_keywords(sub, &format!("{at}/{key}"))?,
            "allOf" => {
                if let Value::Array(items) = sub {
                    for (i, item) in items.iter().enumerate() {
                        check_keywords(item, &format!("{at}/allOf/{i}"))?;
                    }
                }
            }
            "type" if !matches!(sub, Value::String(_)) => return Err(format!("only single-type schemas are supported at {at:?}")),
            "additionalProperties" if *sub != Value::Bool(false) => {
                return Err(format!("only additionalProperties: false is supported at {at:?}"));
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use mage_core::wire::mcp::{SUBMIT_DIAGNOSIS_SCHEMA, SUBMIT_FIX_SCHEMA, SUBMIT_RECIPE_SCHEMA, SUBMIT_TRIAGE_SCHEMA};
    use serde_json::json;

    use super::*;

    /// Sorted: field order follows the JSON map's iteration order.
    fn paths(v: &[SchemaViolation]) -> Vec<&str> {
        let mut p: Vec<&str> = v.iter().map(|x| x.path.as_str()).collect();
        p.sort_unstable();
        p
    }

    #[test]
    fn every_checked_in_schema_uses_only_supported_keywords() {
        for text in [SUBMIT_TRIAGE_SCHEMA, SUBMIT_RECIPE_SCHEMA, SUBMIT_DIAGNOSIS_SCHEMA, SUBMIT_FIX_SCHEMA] {
            Schema::compile(text).unwrap();
        }
        assert!(Schema::compile(r#"{"type": "object", "patternProperties": {}}"#).is_err());
    }

    fn citation() -> Value {
        json!({"path": "src/a.ts", "start_line": 3, "end_line": 9, "quote": "x", "note": "why"})
    }

    fn cause() -> Value {
        json!({
            "title": "t", "explanation": "e", "evidence": [citation()], "repro_steps": [],
            "confirm_if": "c", "rule_out_if": "r", "confidence": 0.5, "method": "ui"
        })
    }

    #[test]
    fn triage_accepts_a_valid_submission() {
        let s = Schema::compile(SUBMIT_TRIAGE_SCHEMA).unwrap();
        let ok = json!({"summary": "s", "causes": [cause()], "missing_info": []});
        assert_eq!(s.validate(&ok), vec![]);
    }

    #[test]
    fn triage_rejections_are_located() {
        let s = Schema::compile(SUBMIT_TRIAGE_SCHEMA).unwrap();
        let mut bad_cause = cause();
        bad_cause["confidence"] = json!(1.5);
        bad_cause["method"] = json!("telepathy");
        bad_cause["evidence"] = json!([{"path": "", "start_line": 0, "end_line": 2, "extra": true}]);
        let bad = json!({"summary": "", "causes": [bad_cause], "missing_info": ["x".repeat(301)], "bonus": 1});
        let v = s.validate(&bad);
        assert_eq!(
            paths(&v),
            [
                "bonus",
                "causes[0].confidence",
                "causes[0].evidence[0].extra",
                "causes[0].evidence[0].path",
                "causes[0].evidence[0].start_line",
                "causes[0].method",
                "missing_info[0]",
                "summary",
            ],
            "{v:?}"
        );
        // Missing required fields, wrong types.
        let v = s.validate(&json!({"summary": 3, "causes": {}}));
        assert_eq!(paths(&v), ["causes", "missing_info", "summary"]);
        assert!(v.iter().any(|x| x.path == "summary" && x.message.contains("expected string")));
    }

    #[test]
    fn triage_if_then_needs_missing_info_without_causes() {
        let s = Schema::compile(SUBMIT_TRIAGE_SCHEMA).unwrap();
        let v = s.validate(&json!({"summary": "s", "causes": [], "missing_info": []}));
        assert_eq!(paths(&v), ["missing_info"]);
        assert!(v[0].message.contains("at least 1"));
        assert!(s.validate(&json!({"summary": "s", "causes": [], "missing_info": ["which org?"]})).is_empty());
    }

    #[test]
    fn diagnosis_conditional_requirements() {
        let s = Schema::compile(SUBMIT_DIAGNOSIS_SCHEMA).unwrap();
        let v = s.validate(&json!({"verdict": "inconclusive", "summary": "s"}));
        assert_eq!(paths(&v), ["reason"]);
        let v = s.validate(&json!({"verdict": "reproduced", "summary": "s"}));
        assert_eq!(paths(&v), ["script", "signature"]);
        let ok = json!({"verdict": "reproduced", "summary": "s", "script": "e2e/r.spec.ts",
                        "signature": {"kind": "http_status", "value": "500", "matcher": "exact"}});
        assert!(s.validate(&ok).is_empty());
    }

    #[test]
    fn length_counts_unicode_scalars() {
        let s = Schema::compile(SUBMIT_FIX_SCHEMA).unwrap();
        assert!(s.validate(&json!({"summary": "é".repeat(8192)})).is_empty());
        assert_eq!(paths(&s.validate(&json!({"summary": "é".repeat(8193)}))), ["summary"]);
    }
}
