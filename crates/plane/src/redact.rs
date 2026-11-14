//! Secret scanning and redaction for event payloads before they are stored.

use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

static PATTERNS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    let p = |name, re: &str| (name, Regex::new(re).expect("redaction regex"));
    vec![
        p("private_key", r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----"),
        p("aws_access_key", r"\b(AKIA|ASIA)[0-9A-Z]{16}\b"),
        p("github_token", r"\b(gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{40,})\b"),
        p("slack_token", r"\bxox[abposr]-[A-Za-z0-9-]{10,}\b"),
        p("openai_key", r"\bsk-(proj-|ant-)?[A-Za-z0-9_\-]{20,}\b"),
        p("factory_token", r"\bf(rt|mk)_[A-Za-z0-9_\-]{20,}\b"),
        p("jwt", r"\beyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\b"),
        p("bearer", r"(?i)\b(bearer|basic)\s+[A-Za-z0-9._~+/=\-]{16,}"),
        p("assignment", r#"(?i)\b(password|passwd|secret|api[_-]?key|token)\s*[:=]\s*["']?[^\s"',;]{6,}"#),
        p("azure_conn", r"(?i)AccountKey=[A-Za-z0-9+/=]{20,}"),
    ]
});

/// Redact known secret patterns in a string. Returns the new string and the
/// names of the patterns that matched.
pub fn redact_str(s: &str) -> (String, Vec<&'static str>) {
    let mut out = s.to_string();
    let mut hits = vec![];
    for (name, re) in PATTERNS.iter() {
        if re.is_match(&out) {
            hits.push(*name);
            out = re.replace_all(&out, format!("[REDACTED:{name}]").as_str()).into_owned();
        }
    }
    (out, hits)
}

/// Redact every string inside a JSON value, in place. Returns pattern names hit.
pub fn redact_json(v: &mut Value) -> Vec<&'static str> {
    let mut hits = vec![];
    walk(v, &mut hits);
    hits.sort();
    hits.dedup();
    hits
}

fn walk(v: &mut Value, hits: &mut Vec<&'static str>) {
    match v {
        Value::String(s) => {
            let (r, h) = redact_str(s);
            if !h.is_empty() {
                *s = r;
                hits.extend(h);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| walk(x, hits)),
        Value::Object(m) => m.values_mut().for_each(|x| walk(x, hits)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redacts_common_secrets() {
        let mut v = serde_json::json!({
            "a": "token ghp_abcdefghijklmnopqrstuvwxyz0123456789 here",
            "b": ["AKIAABCDEFGHIJKLMNOP", "password=hunter22"],
            "c": "Authorization: Bearer abcdefghijklmnopqrstuvwxyz",
            "d": "nothing secret here",
            "e": "frt_abcdefghijklmnopqrstuvwxyz123456",
        });
        let hits = redact_json(&mut v);
        let s = v.to_string();
        assert!(!s.contains("ghp_abc"));
        assert!(!s.contains("AKIAABCD"));
        assert!(!s.contains("hunter22"));
        assert!(!s.contains("abcdefghijklmnopqrstuvwxyz123456"));
        assert!(s.contains("nothing secret here"));
        assert!(hits.contains(&"github_token"));
        assert!(hits.contains(&"aws_access_key"));
    }
}
