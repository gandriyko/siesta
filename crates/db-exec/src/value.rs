//! The one value type shared by the table, CLI and popup.
//!
//! Postgres values are decoded from the server's *text* representation: simple-query results
//! (`raw_sql`) are text already, and the filtered path casts every output column to `::text`
//! (see `wrap`). So one small text parser covers every type, and anything unknown becomes
//! `Other` with the server's own text; nothing can fail to decode.
//! MySQL values are decoded by column type through sqlx (text and binary protocol both work).

use serde_json::json;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    /// Exact text (numeric / decimal), never rounded through f64.
    Decimal(String),
    Float(f64),
    Text(String),
    Json(serde_json::Value),
    Bytes(Vec<u8>),
    /// Date, time, timestamp: the server's text (PG) or chrono's ISO-like text (MySQL).
    DateTime(String),
    /// Anything else (enum, interval, inet, PostGIS…): the server's text.
    Other { type_name: String, text: String },
}

impl Value {
    /// JSON for CLI output and "row as JSON". Decimal and DateTime are strings (exact),
    /// non-finite floats are strings, bytes are a lowercase hex string without prefix.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Null => serde_json::Value::Null,
            Value::Bool(b) => json!(b),
            Value::Int(i) => json!(i),
            Value::Float(f) if f.is_finite() => json!(f),
            Value::Float(f) => json!(f.to_string()),
            Value::Decimal(s) | Value::Text(s) | Value::DateTime(s) => json!(s),
            Value::Json(j) => j.clone(),
            Value::Bytes(b) => json!(hex(b)),
            Value::Other { text, .. } => json!(text),
        }
    }

    /// Plain text as a table cell would show it.
    pub fn display(&self) -> String {
        match self {
            Value::Null => "NULL".into(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Decimal(s) | Value::Text(s) | Value::DateTime(s) => s.clone(),
            Value::Json(j) => j.to_string(),
            Value::Bytes(b) => format!("\\x{}", hex(b)),
            Value::Other { text, .. } => text.clone(),
        }
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// A MySQL / MariaDB text value. MariaDB has no real JSON type (`JSON` is `LONGTEXT` with a
/// `json_valid` check), so its documents arrive as text. Heuristic: text whose trimmed content starts
/// with `{` or `[` and parses as JSON becomes [`Value::Json`]; anything else stays [`Value::Text`].
/// (A `VARCHAR` that merely holds JSON-looking text is converted too; that is the price.)
pub(crate) fn mysql_text(s: String) -> Value {
    let t = s.trim_start();
    if t.starts_with(['{', '[']) && let Ok(j) = serde_json::from_str(t) {
        return Value::Json(j);
    }
    Value::Text(s)
}

/// Normalizes a sqlx Postgres type name: lowercase; arrays as `elem[]`.
pub(crate) fn pg_type_name(sqlx_name: &str) -> String {
    let n = sqlx_name.to_lowercase();
    match n.strip_prefix('_') {
        Some(elem) => format!("{elem}[]"),
        None => n,
    }
}

/// `ty` is a normalized type name (see [`pg_type_name`]); `s` the server's text.
pub(crate) fn from_pg_text(ty: &str, s: &str) -> Value {
    let other = || Value::Other { type_name: ty.to_string(), text: s.to_string() };
    if let Some(elem) = ty.strip_suffix("[]") {
        return match parse_array(s) {
            Some(node) => Value::Json(node.into_json(elem)),
            None => other(),
        };
    }
    match ty {
        "bool" => Value::Bool(s == "t" || s == "true"),
        "int2" | "int4" | "int8" | "oid" => s.parse().map(Value::Int).unwrap_or_else(|_| other()),
        "float4" | "float8" => s.parse().map(Value::Float).unwrap_or_else(|_| other()),
        "numeric" => Value::Decimal(s.to_string()),
        "text" | "varchar" | "bpchar" | "char" | "\"char\"" | "name" | "uuid" | "citext" => Value::Text(s.to_string()),
        "json" | "jsonb" => serde_json::from_str(s).map(Value::Json).unwrap_or_else(|_| Value::Text(s.to_string())),
        "bytea" => s.strip_prefix("\\x").and_then(unhex).map(Value::Bytes).unwrap_or_else(other),
        "date" | "time" | "timetz" | "timestamp" | "timestamptz" => Value::DateTime(s.to_string()),
        _ => other(),
    }
}

/// Postgres array text: `{1,2,NULL,"a b",{3,4}}`.
#[derive(Debug, PartialEq)]
pub(crate) enum Node {
    Null,
    Str(String),
    Arr(Vec<Node>),
}

impl Node {
    fn into_json(self, elem: &str) -> serde_json::Value {
        match self {
            Node::Null => serde_json::Value::Null,
            Node::Str(s) => from_pg_text(elem, &s).to_json(),
            Node::Arr(v) => serde_json::Value::Array(v.into_iter().map(|n| n.into_json(elem)).collect()),
        }
    }
}

pub(crate) fn parse_array(s: &str) -> Option<Node> {
    // Skip an explicit bounds prefix: `[0:2]={…}`.
    let s = if s.starts_with('[') { s.split_once('=')?.1 } else { s };
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let node = parse_node(&chars, &mut i)?;
    (i == chars.len()).then_some(node)
}

fn parse_node(c: &[char], i: &mut usize) -> Option<Node> {
    if *c.get(*i)? != '{' {
        return None;
    }
    *i += 1;
    let mut items = Vec::new();
    if c.get(*i) == Some(&'}') {
        *i += 1;
        return Some(Node::Arr(items));
    }
    loop {
        match c.get(*i)? {
            '{' => items.push(parse_node(c, i)?),
            '"' => {
                *i += 1;
                let mut s = String::new();
                loop {
                    match c.get(*i)? {
                        '\\' => {
                            *i += 1;
                            s.push(*c.get(*i)?);
                        }
                        '"' => break,
                        ch => s.push(*ch),
                    }
                    *i += 1;
                }
                *i += 1;
                items.push(Node::Str(s));
            }
            _ => {
                let start = *i;
                while !matches!(c.get(*i)?, ',' | '}') {
                    *i += 1;
                }
                let s: String = c[start..*i].iter().collect();
                items.push(if s == "NULL" { Node::Null } else { Node::Str(s) });
            }
        }
        match c.get(*i)? {
            ',' => *i += 1,
            '}' => {
                *i += 1;
                return Some(Node::Arr(items));
            }
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars() {
        assert_eq!(from_pg_text("bool", "t"), Value::Bool(true));
        assert_eq!(from_pg_text("int8", "-5"), Value::Int(-5));
        assert_eq!(from_pg_text("numeric", "1.50"), Value::Decimal("1.50".into()));
        assert_eq!(from_pg_text("float8", "1.5"), Value::Float(1.5));
        assert!(matches!(from_pg_text("float8", "NaN"), Value::Float(f) if f.is_nan()));
        assert_eq!(from_pg_text("jsonb", r#"{"a": 1}"#), Value::Json(json!({"a": 1})));
        assert_eq!(from_pg_text("bytea", "\\xdead"), Value::Bytes(vec![0xde, 0xad]));
        assert_eq!(from_pg_text("timestamptz", "2026-01-01 00:00:00+00"), Value::DateTime("2026-01-01 00:00:00+00".into()));
        assert_eq!(from_pg_text("inet", "10.0.0.1/8"), Value::Other { type_name: "inet".into(), text: "10.0.0.1/8".into() });
        assert!(matches!(from_pg_text("int8", "zz"), Value::Other { .. }));
    }

    #[test]
    fn arrays() {
        assert_eq!(from_pg_text("int4[]", "{1,2,NULL}"), Value::Json(json!([1, 2, null])));
        assert_eq!(from_pg_text("text[]", r#"{a,"b, \"c\"","NULL"}"#), Value::Json(json!(["a", "b, \"c\"", "NULL"])));
        assert_eq!(from_pg_text("int4[]", "{{1,2},{3,4}}"), Value::Json(json!([[1, 2], [3, 4]])));
        assert_eq!(from_pg_text("int4[]", "{}"), Value::Json(json!([])));
        assert_eq!(from_pg_text("bool[]", "{t,f}"), Value::Json(json!([true, false])));
        assert_eq!(from_pg_text("mood[]", "{happy,sad}"), Value::Json(json!(["happy", "sad"])));
        assert_eq!(from_pg_text("int4[]", "[0:1]={5,6}"), Value::Json(json!([5, 6])));
        assert!(matches!(from_pg_text("int4[]", "garbage"), Value::Other { .. }));
    }

    #[test]
    fn mysql_json_heuristic() {
        assert_eq!(mysql_text(" {\"a\": [1]}\n".into()), Value::Json(json!({"a": [1]})));
        assert_eq!(mysql_text("[1, 2]".into()), Value::Json(json!([1, 2])));
        for t in ["plain", "{not json", "[1,", "123", "\"s\"", ""] {
            assert_eq!(mysql_text(t.into()), Value::Text(t.into()));
        }
    }

    #[test]
    fn names() {
        assert_eq!(pg_type_name("INT8"), "int8");
        assert_eq!(pg_type_name("_mood"), "mood[]");
        assert_eq!(pg_type_name("INT4[]"), "int4[]");
    }

    #[test]
    fn to_json() {
        assert_eq!(Value::Bytes(vec![1, 255]).to_json(), json!("01ff"));
        assert_eq!(Value::Decimal("1.10".into()).to_json(), json!("1.10"));
        assert_eq!(Value::Float(f64::NAN).to_json(), json!("NaN"));
        assert_eq!(Value::Null.to_json(), serde_json::Value::Null);
        assert_eq!(Value::Json(json!([1])).to_json(), json!([1]));
    }
}
