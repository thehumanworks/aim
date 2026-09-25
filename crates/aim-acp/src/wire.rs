//! Tolerant field access on raw ACP JSON: a missing or mistyped field reads as absent instead of
//! failing the whole message (the ACP schema's own `DefaultOnError` stance, applied field by
//! field).

use std::collections::BTreeMap;

use serde_json::Value;

/// `value[key]` as a string.
pub fn str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// `value[key]` as an owned string.
pub fn string(value: &Value, key: &str) -> Option<String> {
    str(value, key).map(str::to_owned)
}

/// `value[key]` as a bool, or `false`.
pub fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// `value[key]` as an unsigned integer.
pub fn u64(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

/// `value[key]` as an array (empty when absent or not an array).
pub fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value.get(key).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// `value[key]` as a list of strings, skipping non-strings.
pub fn strings(value: &Value, key: &str) -> Vec<String> {
    array(value, key).iter().filter_map(Value::as_str).map(str::to_owned).collect()
}

/// An object of string values as a map, skipping non-strings.
pub fn string_map(value: Option<&Value>) -> BTreeMap<String, String> {
    value
        .and_then(Value::as_object)
        .map(|object| object.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))).collect())
        .unwrap_or_default()
}

/// Whether `value[key]` is present and not `null`/`false`.
pub fn present(value: &Value, key: &str) -> bool {
    !matches!(value.get(key), None | Some(Value::Null | Value::Bool(false)))
}
