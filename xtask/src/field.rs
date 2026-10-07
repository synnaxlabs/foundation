//! Reads fields of JSON from `cargo` and `gh`. A missing field is an error that names
//! it.

use serde_json::Value;

/// The string field `key` of `value`.
pub(crate) fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value[key].as_str().ok_or_else(|| missing(key, "string"))
}

/// The array field `key` of `value`.
pub(crate) fn list<'a>(value: &'a Value, key: &str) -> Result<&'a [Value], String> {
    value[key]
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| missing(key, "array"))
}

/// The boolean field `key` of `value`.
pub(crate) fn flag(value: &Value, key: &str) -> Result<bool, String> {
    value[key].as_bool().ok_or_else(|| missing(key, "boolean"))
}

fn missing(key: &str, kind: &str) -> String {
    format!("JSON has no {kind} field `{key}`")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn value() -> Value {
        json!({ "name": "raft", "kind": ["test"], "test": false })
    }

    #[test]
    fn reads_present_fields() {
        assert_eq!(text(&value(), "name"), Ok("raft"));
        assert_eq!(list(&value(), "kind"), Ok(&[json!("test")][..]));
        assert_eq!(flag(&value(), "test"), Ok(false));
    }

    #[test]
    fn names_a_missing_or_mistyped_field() {
        assert_eq!(
            text(&value(), "kind").unwrap_err(),
            "JSON has no string field `kind`"
        );
        assert_eq!(
            list(&value(), "id").unwrap_err(),
            "JSON has no array field `id`"
        );
        assert_eq!(
            flag(&value(), "name").unwrap_err(),
            "JSON has no boolean field `name`"
        );
    }
}
