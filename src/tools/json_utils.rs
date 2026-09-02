use serde_json::Value;

/// Serde helpers that accept either a JSON number or a numeric string.
/// MCP tool calls from form UIs often serialize every field as a string.
pub mod de {
    use serde::de::{self, Visitor};

    macro_rules! coerce_opt {
        ($fn_name:ident, $ty:ty, $visit_u64:expr, $visit_f64:expr) => {
            pub fn $fn_name<'de, D>(d: D) -> Result<Option<$ty>, D::Error>
            where
                D: de::Deserializer<'de>,
            {
                struct V;
                impl<'de> Visitor<'de> for V {
                    type Value = Option<$ty>;
                    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                        write!(f, "number or numeric string")
                    }
                    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                        Ok(Some($visit_u64(v)))
                    }
                    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                        Ok(Some($visit_u64(v as u64)))
                    }
                    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
                        Ok(Some($visit_f64(v)))
                    }
                    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                        if v.is_empty() { return Ok(None); }
                        v.parse::<$ty>().map(Some).map_err(E::custom)
                    }
                    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> { Ok(None) }
                    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> { Ok(None) }
                }
                d.deserialize_any(V)
            }
        };
    }

    coerce_opt!(opt_u32, u32, |v: u64| v as u32, |v: f64| v as u32);
    coerce_opt!(opt_usize, usize, |v: u64| v as usize, |v: f64| v as usize);
    coerce_opt!(opt_f32, f32, |v: u64| v as f32, |v: f64| v as f32);

    macro_rules! coerce_plain {
        ($fn_name:ident, $ty:ty, $visit_u64:expr, $visit_f64:expr) => {
            pub fn $fn_name<'de, D>(d: D) -> Result<$ty, D::Error>
            where
                D: de::Deserializer<'de>,
            {
                struct V;
                impl<'de> Visitor<'de> for V {
                    type Value = $ty;
                    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                        write!(f, "number or numeric string")
                    }
                    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                        Ok($visit_u64(v))
                    }
                    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                        Ok($visit_u64(v as u64))
                    }
                    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
                        Ok($visit_f64(v))
                    }
                    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                        v.parse::<$ty>().map_err(E::custom)
                    }
                }
                d.deserialize_any(V)
            }
        };
    }

    coerce_plain!(u32_or_str,   u32,   |v: u64| v as u32,   |v: f64| v as u32);
}

/// Tolerant extractor for LLM responses that should contain a JSON array.
///
/// Strategy (tried in order):
/// 1. Strip Markdown code fences anywhere in the response (preamble-aware).
/// 2. If result starts with `[`, return it directly.
/// 3. Try parsing as JSON; drill into common wrapper keys (`facts`, `leads`, etc.)
///    and OpenAI-style `choices[*].message.content`.
/// 4. Last resort: scan the raw response for the first `[` and extract the
///    balanced JSON array starting there (handles "Here is the JSON: [...]" prose).
pub fn extract_json_array(response: &str) -> String {
    // Remove all Markdown code fences, whether at the start or mid-response.
    fn strip_fences(s: &str) -> String {
        let mut result = String::with_capacity(s.len());
        let mut remaining = s;
        while let Some(fence_start) = remaining.find("```") {
            // Append everything before the fence
            result.push_str(&remaining[..fence_start]);
            // Skip over the fence opener line (e.g. "```json\n")
            let after_fence = &remaining[fence_start + 3..];
            let content_start = after_fence.find('\n').map(|p| p + 1).unwrap_or(0);
            let content = &after_fence[content_start..];
            // Find closing fence
            if let Some(close) = content.find("```") {
                result.push_str(&content[..close]);
                remaining = &content[close + 3..];
            } else {
                // No closing fence; treat the rest as content
                result.push_str(content);
                remaining = "";
            }
        }
        result.push_str(remaining);
        result.trim().to_string()
    }

    fn find_json_string(v: &Value) -> Option<String> {
        match v {
            Value::String(s) => {
                let s = s.trim();
                if s.starts_with('[') || s.starts_with('{') || s.starts_with("```") {
                    Some(s.to_string())
                } else {
                    None
                }
            }
            Value::Object(map) => {
                // Common wrapper keys
                for key in ["facts", "leads", "results", "items", "data"].iter() {
                    if let Some(vv) = map.get(*key) {
                        if vv.is_array() {
                            return Some(vv.to_string());
                        }
                        if let Some(s) = vv.as_str() {
                            return Some(s.to_string());
                        }
                    }
                }
                // OpenAI-style choices -> message.content or content
                if let Some(choices) = map.get("choices") {
                    if let Some(first) = choices.as_array().and_then(|a| a.get(0)) {
                        if let Some(msg) = first
                            .get("message")
                            .and_then(|m| m.get("content"))
                            .and_then(|c| c.as_str())
                        {
                            return Some(msg.to_string());
                        }
                        if let Some(content) = first.get("content").and_then(|c| c.as_str()) {
                            return Some(content.to_string());
                        }
                    }
                }
                // Recurse into object fields
                for (_k, v) in map.iter() {
                    if let Some(found) = find_json_string(v) {
                        return Some(found);
                    }
                }
                None
            }
            Value::Array(_) => Some(v.to_string()),
            _ => None,
        }
    }

    /// Extract the longest balanced JSON array starting at the first `[` in `s`.
    fn scan_for_array(s: &str) -> Option<String> {
        let start = s.find('[')?;
        let slice = &s[start..];
        // Walk characters tracking bracket depth to find the closing `]`.
        let mut depth: i32 = 0;
        let mut in_string = false;
        let mut escape_next = false;
        for (i, ch) in slice.char_indices() {
            if escape_next {
                escape_next = false;
                continue;
            }
            match ch {
                '\\' if in_string => escape_next = true,
                '"' => in_string = !in_string,
                '[' if !in_string => depth += 1,
                ']' if !in_string => {
                    depth -= 1;
                    if depth == 0 {
                        let candidate = &slice[..=i];
                        if serde_json::from_str::<Value>(candidate).is_ok() {
                            return Some(candidate.to_string());
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
        None
    }

    let stripped = strip_fences(response);

    if stripped.trim_start().starts_with('[') {
        return stripped;
    }

    if let Ok(val) = serde_json::from_str::<Value>(&stripped) {
        if val.is_array() {
            return val.to_string();
        }
        // Bare object that is itself a lead (has "title" or "observed_pattern") —
        // wrap it in an array rather than drilling into its fields and accidentally
        // returning an inner array like `affected_files`.
        if val.as_object().map_or(false, |o| {
            o.contains_key("title") || o.contains_key("observed_pattern")
        }) {
            return format!("[{}]", stripped.trim());
        }
        if let Some(found) = find_json_string(&val) {
            return extract_json_array(&found);
        }
    }

    // Last resort: scan the original response for an embedded JSON array.
    if let Some(found) = scan_for_array(response) {
        return found;
    }

    // Fallback: return stripped text (parse error will surface a useful warning upstream)
    stripped
}

/// Parse an LLM response in key-value format into a JSON object with the same
/// field names as `LlmLead`. Recognises multi-line values: continuation lines
/// (not starting with a known KEY:) are appended to the current field.
/// FILES is split on commas and newlines; leading dashes are stripped so
/// bullet-list responses work too.
pub fn parse_kv_lead(response: &str) -> serde_json::Value {
    const KEYS: &[(&str, &str)] = &[
        ("TITLE:", "title"),
        ("PATTERN:", "observed_pattern"),
        ("RATIONALE:", "rationale"),
        ("FILES:", "affected_files"),
        ("CONFIDENCE:", "confidence"),
    ];

    let mut current: Option<&str> = None;
    let mut buckets: std::collections::HashMap<&str, String> = std::collections::HashMap::new();

    for line in response.lines() {
        // Strip leading markdown noise (* # space) before checking for a key prefix.
        let trimmed = line.trim_start_matches(|c: char| c == '*' || c == '#' || c == ' ');
        let mut matched = false;
        for (prefix, field) in KEYS {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                // New key — overwrite (not append) so repeated keys take the last value.
                buckets.insert(field, rest.trim().to_string());
                current = Some(field);
                matched = true;
                break;
            }
        }
        if !matched {
            if let Some(key) = current {
                let s = line.trim();
                if !s.is_empty() {
                    let val = buckets.entry(key).or_default();
                    if !val.is_empty() {
                        val.push('\n');
                    }
                    val.push_str(s);
                }
            }
        }
    }

    let get = |k: &str| -> Option<String> {
        buckets.get(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    };

    let files: Vec<String> = get("affected_files")
        .map(|s| {
            s.split(|c| c == ',' || c == '\n')
                .map(|f| f.trim().trim_start_matches('-').trim().to_string())
                .filter(|f| !f.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let confidence: Option<f32> = get("confidence").and_then(|s| s.parse().ok());

    serde_json::json!({
        "title": get("title"),
        "observed_pattern": get("observed_pattern"),
        "rationale": get("rationale"),
        "affected_files": if files.is_empty() { serde_json::Value::Null } else { serde_json::json!(files) },
        "confidence": confidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_array() {
        let input = r#"[{"title":"X","confidence":0.8}]"#;
        let out = extract_json_array(input);
        assert!(out.starts_with('['), "expected array, got: {out}");
        serde_json::from_str::<serde_json::Value>(&out).expect("valid JSON");
    }

    #[test]
    fn fenced_json_at_start() {
        let input = "```json\n[{\"title\":\"X\"}]\n```";
        let out = extract_json_array(input);
        assert!(out.trim_start().starts_with('['), "got: {out}");
        serde_json::from_str::<serde_json::Value>(&out).expect("valid JSON");
    }

    #[test]
    fn fenced_json_with_preamble() {
        let input = "Sure, here is the JSON:\n```json\n[{\"title\":\"Y\",\"confidence\":0.7}]\n```\nLet me know if you need changes.";
        let out = extract_json_array(input);
        assert!(out.trim_start().starts_with('['), "got: {out}");
        serde_json::from_str::<serde_json::Value>(&out).expect("valid JSON");
    }

    #[test]
    fn prose_preamble_bare_array() {
        let input = "Here is the result: [{\"title\":\"Z\",\"confidence\":0.5}]";
        let out = extract_json_array(input);
        assert!(out.starts_with('['), "got: {out}");
        serde_json::from_str::<serde_json::Value>(&out).expect("valid JSON");
    }

    #[test]
    fn wrapper_key_facts() {
        let input = r#"{"facts":[{"subject":"a","relation":"must_use","object":"b","confidence":0.9}]}"#;
        let out = extract_json_array(input);
        assert!(out.starts_with('['), "got: {out}");
        serde_json::from_str::<serde_json::Value>(&out).expect("valid JSON");
    }

    #[test]
    fn kv_lead_basic() {
        let input = "TITLE: Provider Abstraction\nPATTERN: Uses a trait.\nRATIONALE: Decouples backends.\nFILES: src/llm.rs, src/embeddings.rs\nCONFIDENCE: 0.85";
        let val = parse_kv_lead(input);
        assert_eq!(val["title"], "Provider Abstraction");
        assert_eq!(val["observed_pattern"], "Uses a trait.");
        assert_eq!(val["rationale"], "Decouples backends.");
        assert_eq!(val["confidence"], 0.85_f32);
        let files = val["affected_files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], "src/llm.rs");
    }

    #[test]
    fn kv_lead_multiline_pattern() {
        let input = "TITLE: T\nPATTERN: Line one.\nLine two.\nFILES: src/a.rs\nCONFIDENCE: 0.7";
        let val = parse_kv_lead(input);
        assert!(val["observed_pattern"].as_str().unwrap().contains("Line two"));
    }

    #[test]
    fn kv_lead_bullet_files() {
        let input = "TITLE: T\nPATTERN: P\nFILES:\n- src/a.rs\n- src/b.rs\nCONFIDENCE: 0.6";
        let val = parse_kv_lead(input);
        let files = val["affected_files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[1], "src/b.rs");
    }

    #[test]
    fn fenced_bare_lead_object() {
        let input = "```json\n{\n  \"title\": \"Some Pattern\",\n  \"observed_pattern\": \"desc\",\n  \"affected_files\": [\"src/foo.rs\"],\n  \"confidence\": 0.85\n}\n```";
        let out = extract_json_array(input);
        assert!(out.trim_start().starts_with('['), "expected array wrapping bare object, got: {out}");
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&out).expect("valid JSON array");
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].get("title").is_some());
    }

    #[test]
    fn openai_choices_wrapper() {
        let input = r#"{"choices":[{"message":{"content":"[{\"title\":\"W\"}]"}}]}"#;
        let out = extract_json_array(input);
        assert!(out.starts_with('['), "got: {out}");
        serde_json::from_str::<serde_json::Value>(&out).expect("valid JSON");
    }
}
