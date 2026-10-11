//! Agent configuration JSON must have an unambiguous object-key mapping.
//! Preserve serde_json syntax/number/depth validation, but reject duplicates
//! recursively before adapters or selective restoration can discard user data.
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Number, Value};
use std::fmt;

struct UniqueValue(Value);
struct UniqueVisitor;
impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(UniqueVisitor).map(Self)
    }
}
impl<'de> Visitor<'de> for UniqueVisitor {
    type Value = Value;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON with unique object keys")
    }
    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }
    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }
    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }
    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("nonfinite JSON number"))
    }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }
    fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(UniqueValue(value)) = seq.next_element()? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut values = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.contains_key(&key) {
                // Do not put the key or its values (potential credentials) in errors.
                return Err(de::Error::custom(
                    "duplicate key in agent JSON configuration",
                ));
            }
            let UniqueValue(value) = map.next_value()?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}
pub(super) fn from_str(text: &str) -> serde_json::Result<Value> {
    from_slice(text.as_bytes())
}
pub(super) fn from_slice(bytes: &[u8]) -> serde_json::Result<Value> {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let UniqueValue(value) = UniqueValue::deserialize(&mut decoder)?;
    decoder.end()?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_keys_are_rejected_recursively_after_escape_decoding() {
        for text in [
            r#"{"model":1,"model":2}"#,
            r#"{"model":1,"\u006dodel":2}"#,
            r#"{"foreign":{"key":1,"key":2}}"#,
            r#"{"foreign":[{"key":1,"key":2}]}"#,
            r#"{"credential-private":1,"credential-private":2}"#,
        ] {
            let error = from_str(text).unwrap_err().to_string();
            assert!(error.contains("duplicate key"));
            assert!(!error.contains("credential-private"));
        }
    }
    #[test]
    fn unique_json_keeps_normal_values_and_validation_bounds() {
        let text = r#"{"first":{"key":1},"second":{"key":2},"values":[null,true,false,-9223372036854775808,18446744073709551615,1.25,"中文\n"]}"#;
        assert_eq!(
            from_str(text).unwrap(),
            serde_json::from_str::<Value>(text).unwrap()
        );
        assert_eq!(
            from_str(&format!("\u{feff}{text}")).unwrap(),
            from_str(text).unwrap()
        );
        for text in [
            r#"{"x":1} {"x":2}"#,
            r#"{"x":1,}"#,
            r#"{"x":1e999}"#,
            r#"{"x":NaN}"#,
        ] {
            assert!(from_str(text).is_err());
        }
        let deeply_nested = format!("{}0{}", "[".repeat(130), "]".repeat(130));
        assert!(from_str(&deeply_nested).is_err());
    }
}
