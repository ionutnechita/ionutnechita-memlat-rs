// A minimal JSON writer for --json, so the report needs no dependency
// beyond libc. Values are built as a tree and written in one go.

use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i128),
    Num(f64), // NaN and infinities are written as null
    Str(String),
    Arr(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

// {"key": value, ...} in the given order
pub fn obj<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Obj(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

pub fn arr<T: Into<Value>>(items: impl IntoIterator<Item = T>) -> Value {
    Value::Arr(items.into_iter().map(Into::into).collect())
}

impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Bool(b)
    }
}

macro_rules! from_int {
    ($($t:ty),*) => {$(
        impl From<$t> for Value {
            fn from(n: $t) -> Value {
                Value::Int(n as i128)
            }
        }
    )*};
}
from_int!(u8, u32, u64, usize, i32, i64);

impl From<f64> for Value {
    fn from(x: f64) -> Value {
        Value::Num(x)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::Str(s.to_string())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::Str(s)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(o: Option<T>) -> Value {
        o.map_or(Value::Null, Into::into)
    }
}

fn string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

impl Value {
    // indented by two spaces per level; short arrays of scalars on one line
    pub fn write(&self, out: &mut String, depth: usize) {
        let pad = |out: &mut String, d: usize| out.extend(std::iter::repeat_n(' ', 2 * d));
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Value::Num(x) if x.is_finite() => {
                // Rust's shortest round-trip form; integral floats as "3.0"
                let _ = write!(out, "{x:?}");
            }
            Value::Num(_) => out.push_str("null"),
            Value::Str(s) => string(out, s),
            Value::Arr(a) if a.is_empty() => out.push_str("[]"),
            Value::Arr(a) if a.iter().all(|v| !matches!(v, Value::Arr(_) | Value::Obj(_))) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    v.write(out, depth);
                }
                out.push(']');
            }
            Value::Arr(a) => {
                out.push_str("[\n");
                for (i, v) in a.iter().enumerate() {
                    pad(out, depth + 1);
                    v.write(out, depth + 1);
                    out.push_str(if i + 1 < a.len() { ",\n" } else { "\n" });
                }
                pad(out, depth);
                out.push(']');
            }
            Value::Obj(o) if o.is_empty() => out.push_str("{}"),
            Value::Obj(o) => {
                out.push_str("{\n");
                for (i, (k, v)) in o.iter().enumerate() {
                    pad(out, depth + 1);
                    string(out, k);
                    out.push_str(": ");
                    v.write(out, depth + 1);
                    out.push_str(if i + 1 < o.len() { ",\n" } else { "\n" });
                }
                pad(out, depth);
                out.push('}');
            }
        }
    }

    pub fn to_json(&self) -> String {
        let mut s = String::new();
        self.write(&mut s, 0);
        s.push('\n');
        s
    }
}

// writes `v` to `path`, or to stdout for "-"
pub fn save(path: &str, v: &Value) -> std::io::Result<()> {
    let s = v.to_json();
    if path == "-" {
        use std::io::Write as _;
        let mut out = std::io::stdout().lock();
        out.write_all(s.as_bytes())?;
        out.flush()
    } else {
        std::fs::write(path, s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars() {
        assert_eq!(Value::Null.to_json(), "null\n");
        assert_eq!(Value::from(true).to_json(), "true\n");
        assert_eq!(Value::from(u64::MAX).to_json(), "18446744073709551615\n");
        assert_eq!(Value::from(-3i64).to_json(), "-3\n");
        assert_eq!(Value::from(1.5).to_json(), "1.5\n");
        assert_eq!(Value::from(2.0).to_json(), "2.0\n");
        assert_eq!(Value::from(f64::NAN).to_json(), "null\n");
        assert_eq!(Value::from(f64::INFINITY).to_json(), "null\n");
        assert_eq!(Value::from(None::<u64>).to_json(), "null\n");
        assert_eq!(Value::from(Some(7u32)).to_json(), "7\n");
    }

    #[test]
    fn escaping() {
        let v = Value::from("a\"b\\c\nd\te\u{1}é");
        assert_eq!(v.to_json(), "\"a\\\"b\\\\c\\nd\\te\\u0001é\"\n");
    }

    #[test]
    fn nesting() {
        let v = obj([
            ("cpus", arr([2u64, 40])),
            ("empty", arr(Vec::<u64>::new())),
            ("runs", Value::Arr(vec![obj([("cpu", 2u64.into()), ("max_us", 4.5.into())])])),
            ("none", Value::Obj(vec![])),
        ]);
        let want = "{\n  \"cpus\": [2, 40],\n  \"empty\": [],\n  \"runs\": [\n    {\n      \"cpu\": 2,\n      \
                    \"max_us\": 4.5\n    }\n  ],\n  \"none\": {}\n}\n";
        assert_eq!(v.to_json(), want);
    }
}
