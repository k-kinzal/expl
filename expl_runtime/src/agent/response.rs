//! Recovering a JSON response from free-form agent text.
//!
//! Agents are asked for a JSON object, but text-only CLIs may wrap it in
//! prose or a Markdown fence. [`extract_json_object`] finds the **last**
//! complete JSON object in the text — the final answer wins over examples
//! quoted earlier.

use serde_json::Value;

/// The last complete JSON object embedded in `text`.
#[must_use]
pub fn extract_json_object(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(trimmed) {
        return Some(value);
    }
    let mut found = None;
    let mut skip_until = 0;
    for (index, ch) in text.char_indices() {
        if ch != '{' || index < skip_until {
            continue;
        }
        let mut stream = serde_json::Deserializer::from_str(&text[index..]).into_iter::<Value>();
        if let Some(Ok(value @ Value::Object(_))) = stream.next() {
            skip_until = index + stream.byte_offset();
            found = Some(value);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_object() {
        assert_eq!(extract_json_object(r#" {"a":1} "#), Some(json!({"a":1})));
    }

    #[test]
    fn fenced_object_after_prose() {
        let text = "Here you go:\n```json\n{\"a\": {\"b\": [1, 2]}}\n```\nDone.";
        assert_eq!(extract_json_object(text), Some(json!({"a": {"b": [1, 2]}})));
    }

    #[test]
    fn last_object_wins() {
        let text = r#"example {"a":1} final {"a":2}"#;
        assert_eq!(extract_json_object(text), Some(json!({"a":2})));
    }

    #[test]
    fn no_object() {
        assert_eq!(extract_json_object("nothing here [1,2]"), None);
    }
}
