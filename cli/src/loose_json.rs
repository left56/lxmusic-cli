//! kw 等接口的单引号伪 JSON，以及 JSONP / `try{var jsondata=...}` 包装。

use anyhow::{anyhow, Result};
use serde_json::Value;

pub fn parse_loose_json(raw: &str) -> Result<Value> {
    let mut text = raw.trim().to_string();
    if text.is_empty() {
        return Err(anyhow!("空响应"));
    }
    if let Some(inner) = unwrap_jsonp(&text) {
        text = inner;
    }
    if let Some(inner) = unwrap_kw_try(&text) {
        text = inner;
    }
    if let Ok(v) = serde_json::from_str::<Value>(&text) {
        return Ok(v);
    }
    let normalized = text.replace('\'', "\"");
    serde_json::from_str(&normalized).map_err(|err| anyhow!("响应不是合法 JSON: {err}"))
}

fn unwrap_jsonp(text: &str) -> Option<String> {
    let t = text.trim();
    let bytes = t.as_bytes();
    if bytes.is_empty() || !is_ident_start(bytes[0]) {
        return None;
    }
    let mut i = 1;
    while i < bytes.len() && is_ident_cont(bytes[i]) {
        i += 1;
    }
    let rest = t[i..].trim_start();
    let rest = rest.strip_prefix('(')?;
    let rest = rest.trim_end().trim_end_matches(';').trim_end();
    let inner = rest.strip_suffix(')')?.trim();
    if inner.is_empty() {
        return None;
    }
    Some(inner.to_string())
}

fn unwrap_kw_try(text: &str) -> Option<String> {
    let t = text.trim();
    let lower = t.to_ascii_lowercase();
    if !lower.starts_with("try") {
        return None;
    }
    let after = t[3..].trim_start();
    let after = after.strip_prefix('{')?.trim_start();
    let lower_after = after.to_ascii_lowercase();
    let after = if lower_after.starts_with("var ") {
        after[4..].trim_start()
    } else {
        return None;
    };
    let eq = after.find('=')?;
    let mut body = after[eq + 1..].trim();
    if let Some(idx) = body.to_ascii_lowercase().rfind("catch") {
        body = body[..idx].trim();
        body = body.trim_end_matches('}').trim();
    }
    body = body.trim_end_matches(';').trim();
    if body.is_empty() {
        None
    } else {
        Some(body.to_string())
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$'
}

fn is_ident_cont(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_quotes_and_jsonp_and_kw_wrapper() {
        let v = parse_loose_json("{'a':'b','n':1}").unwrap();
        assert_eq!(v["a"], "b");
        assert_eq!(v["n"], 1);

        let v = parse_loose_json("callback({'ok':true});").unwrap();
        assert_eq!(v["ok"], true);

        let v = parse_loose_json("try{var jsondata={'HIT':'1','abslist':[]};}catch(e){}").unwrap();
        assert_eq!(v["HIT"], "1");
        assert!(v["abslist"].as_array().unwrap().is_empty());
    }

    #[test]
    fn plain_json_still_works() {
        let v = parse_loose_json(r#"{"songs":[]}"#).unwrap();
        assert!(v["songs"].as_array().unwrap().is_empty());
    }
}
