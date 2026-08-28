//! Canonical JSON: object keys sorted by code point, no insignificant whitespace. Independent of
//! whether `serde_json/preserve_order` is enabled anywhere in the build.

use serde::Serialize;
use serde_json::Value;

/// Canonical bytes of any serializable value.
pub fn to_bytes<T: Serialize + ?Sized>(value: &T) -> Vec<u8> {
    let tree = serde_json::to_value(value).unwrap_or(Value::Null);
    let mut out = Vec::new();
    write(&tree, &mut out);
    out
}

/// `blake3:<hex>` over [`to_bytes`].
pub fn hash<T: Serialize + ?Sized>(value: &T) -> String {
    format!("blake3:{}", blake3::hash(&to_bytes(value)).to_hex())
}

fn write(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push(b'{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_str(key, out);
                out.push(b':');
                if let Some(v) = map.get(*key) {
                    write(v, out);
                }
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write(item, out);
            }
            out.push(b']');
        }
        Value::String(s) => write_str(s, out),
        other => out.extend_from_slice(other.to_string().as_bytes()),
    }
}

fn write_str(s: &str, out: &mut Vec<u8>) {
    out.extend_from_slice(Value::String(s.to_owned()).to_string().as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn key_order_does_not_change_the_hash() {
        let a: Value = serde_json::from_str(r#"{"b":1,"a":{"y":[1,2],"x":"s"}}"#).unwrap_or_default();
        let b: Value = serde_json::from_str(r#"{"a":{"x":"s","y":[1,2]},"b":1}"#).unwrap_or_default();
        assert_eq!(hash(&a), hash(&b));
        assert_eq!(to_bytes(&a), br#"{"a":{"x":"s","y":[1,2]},"b":1}"#.to_vec());
    }

    #[test]
    fn array_order_changes_the_hash() {
        assert_ne!(hash(&json!([1, 2])), hash(&json!([2, 1])));
    }

    #[test]
    fn strings_are_escaped() {
        assert_eq!(to_bytes(&json!({"k": "a\"b\n"})), br#"{"k":"a\"b\n"}"#.to_vec());
    }
}
