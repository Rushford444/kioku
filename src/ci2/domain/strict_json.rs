//! Bounded JSON ingestion: duplicate keys and invalid values are never silently normalized.
use super::primitives::{ContractError, Validate};
use serde_json::Value;
use std::collections::BTreeMap;

const MAX_REQUEST_INPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_WIRE_DEPTH: usize = 128;

struct StrictNode;
impl<'de> serde::de::DeserializeSeed<'de> for StrictNode {
    type Value = Value;
    fn deserialize<D>(self, deserializer: D) -> Result<Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictNode)
    }
}
impl<'de> serde::de::Visitor<'de> for StrictNode {
    type Value = Value;
    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON object with unique keys and bounded nested values")
    }
    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: serde::de::MapAccess<'de>,
    {
        let mut fields = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if fields.contains_key(&key) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate JSON field {key}"
                )));
            }
            if key.chars().any(char::is_control) {
                return Err(serde::de::Error::custom("control characters in field name"));
            }
            fields.insert(key, map.next_value_seed(StrictNode)?);
        }
        Ok(Value::Object(fields))
    }
    fn visit_seq<S>(self, mut sequence: S) -> Result<Self::Value, S::Error>
    where
        S: serde::de::SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(StrictNode)? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(value))
    }
    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }
    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }
    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("invalid JSON number"))
    }
    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Value::String(value.into()))
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }
}

fn visit<'a>(_value: &'a Value, stack: &mut Vec<(usize, &'a Value)>) -> Result<(), ContractError> {
    while let Some((depth, value)) = stack.pop() {
        if depth > MAX_WIRE_DEPTH {
            return Err(ContractError::Configuration(
                "wire depth bound exceeded".into(),
            ));
        }
        match value {
            Value::Array(items) => stack.extend(items.iter().map(|item| (depth + 1, item))),
            Value::Object(fields) => {
                let mut names = std::collections::BTreeSet::new();
                for (key, child) in fields {
                    if !names.insert(key) {
                        return Err(ContractError::Configuration(format!(
                            "duplicate JSON field {key}"
                        )));
                    }
                    if key.chars().any(char::is_control) {
                        return Err(ContractError::Configuration(
                            "control chars in object name".into(),
                        ));
                    }
                    stack.push((depth + 1, child));
                }
            }
            Value::Number(number) => {
                if let Some(magnitude) = number.as_u64() {
                    super::primitives::check_range(magnitude)?;
                } else if !number.is_i64() {
                    return Err(ContractError::Unsupported("non-integer wire number".into()));
                } else {
                    return Err(ContractError::Unsupported(
                        "negative or out-of-range wire number".into(),
                    ));
                }
            }
            Value::Null | Value::Bool(_) | Value::String(_) => {}
        }
    }
    Ok(())
}

/// Reads a bounded, closed record then validates the cross-field contract.
///
/// # Errors
/// Size, parser, duplicate field, unknown field, integer range or semantic
/// violations are all returned; no fallback is made to an unqualified profile.
pub fn decode_bounded<T: serde::de::DeserializeOwned + Validate>(
    bytes: &[u8],
) -> Result<T, ContractError> {
    if bytes.len() > MAX_REQUEST_INPUT_BYTES {
        return Err(ContractError::Unsupported(
            "local request exceeds 4 MiB".into(),
        ));
    }
    if bytes.contains(&0) {
        return Err(ContractError::Configuration(
            "NUL outside valid encoded text".into(),
        ));
    }
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let value = serde::Deserializer::deserialize_map(&mut decoder, StrictNode)
        .map_err(|error| ContractError::Configuration(format!("invalid JSON wire: {error}")))?;
    decoder
        .end()
        .map_err(|error| ContractError::Configuration(format!("trailing bytes: {error}")))?;
    let mut stack = vec![(0, &value)];
    visit(&value, &mut stack)?;
    let object = serde_json::from_value(value).map_err(|error| {
        ContractError::Configuration(format!("record failed typed decoding: {error}"))
    })?;
    let typed: T = object;
    typed.validate()?;
    Ok(typed)
}

/// Stable canonical request digest uses sorted object names recursively.
/// This local encoding does not advertise arbitrary JSON numeric JCS behavior.
///
/// # Errors
/// Invalid range/wire shapes are rejected before any field is used.
pub fn digest_request_document(
    value: &serde_json::Value,
) -> Result<super::primitives::Digest, ContractError> {
    let mut pending = vec![(0, value)];
    visit(value, &mut pending)?;
    #[allow(clippy::items_after_statements)]
    fn canonical(value: &Value) -> Result<String, ContractError> {
        Ok(match value {
            Value::Null => "null".into(),
            Value::Bool(bool) => bool.to_string(),
            Value::String(s) => serde_json::to_string(s)
                .map_err(|error| ContractError::Configuration(error.to_string()))?,
            Value::Number(n) => n.to_string(),
            Value::Array(items) => {
                let mut result = Vec::with_capacity(items.len());
                for item in items {
                    result.push(canonical(item)?);
                }
                format!("[{}]", result.join(","))
            }
            Value::Object(fields) => {
                let sorted: BTreeMap<_, _> = fields.iter().collect();
                let mut result = Vec::with_capacity(sorted.len());
                for (key, field) in sorted {
                    result.push(format!(
                        "{}:{}",
                        serde_json::to_string(key)
                            .map_err(|error| ContractError::Configuration(error.to_string()))?,
                        canonical(field)?
                    ));
                }
                format!("{{{}}}", result.join(","))
            }
        })
    }
    let canonical = canonical(value)?;
    super::primitives::exact_digest("kioku/ci2/local-request/key/v1", canonical.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Document {
        value: Value,
    }
    impl Validate for Document {
        fn validate(&self) -> Result<(), ContractError> {
            Ok(())
        }
    }

    #[test]
    fn nested_object_duplicate_is_rejected_before_value_normalization() {
        let error = decode_bounded::<Document>(br#"{"value":{"field":1,"field":2}}"#).unwrap_err();
        assert!(
            matches!(error, ContractError::Configuration(ref detail) if detail.contains("duplicate JSON field field"))
        );
    }

    #[test]
    fn duplicate_inside_array_object_is_rejected_before_value_normalization() {
        let error =
            decode_bounded::<Document>(br#"{"value":[{"field":1,"field":2}]}"#).unwrap_err();
        assert!(
            matches!(error, ContractError::Configuration(ref detail) if detail.contains("duplicate JSON field field"))
        );
    }

    #[test]
    fn field_names_may_repeat_in_distinct_objects() {
        let expected = serde_json::json!({
            "value": {
                "value": "nested",
                "siblings": [{"value": "first"}, {"value": "second"}]
            }
        });
        let decoded = decode_bounded::<Document>(&serde_json::to_vec(&expected).unwrap()).unwrap();
        assert_eq!(decoded.value, expected["value"]);
    }
}
