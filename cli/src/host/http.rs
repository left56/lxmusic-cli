use anyhow::{anyhow, bail, Result};
use reqwest::blocking::Client;
use reqwest::redirect::Policy;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

pub const SCRIPT_BODY_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

/// 音源脚本用的客户端：不自动解压，也不跟随重定向。
/// Node `http.request` 把 302 和 gzip 原样交给脚本。
pub fn script_client() -> &'static Client {
    static CELL: std::sync::OnceLock<Client> = std::sync::OnceLock::new();
    CELL.get_or_init(|| {
        Client::builder()
            .redirect(Policy::custom(|attempt| attempt.stop()))
            .gzip(false)
            .build()
            .expect("script http client")
    })
}

pub fn browser_client() -> &'static Client {
    static CELL: std::sync::OnceLock<Client> = std::sync::OnceLock::new();
    CELL.get_or_init(|| {
        Client::builder()
            .timeout(Duration::from_secs(30))
            .gzip(true)
            .build()
            .expect("browser http client")
    })
}

pub fn script_request(
    method: &str,
    url: &str,
    headers: &HashMap<String, String>,
    body: Option<&[u8]>,
    timeout: Duration,
) -> Result<HttpResponse> {
    let method = method.trim().to_ascii_uppercase();
    let method = if method.is_empty() { "GET" } else { method.as_str() };
    let mut req = script_client()
        .request(
            method.parse().map_err(|_| anyhow!("unsupported method {method}"))?,
            url,
        )
        .timeout(timeout);
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    if let Some(body) = body {
        req = req.body(body.to_vec());
    }
    let resp = req.send().map_err(|e| anyhow!("{e}"))?;
    let status = resp.status().as_u16();
    let mut headers_out = HashMap::new();
    for (k, v) in resp.headers() {
        if let Ok(val) = v.to_str() {
            headers_out
                .entry(k.as_str().to_string())
                .and_modify(|cur: &mut String| {
                    cur.push_str(", ");
                    cur.push_str(val);
                })
                .or_insert_with(|| val.to_string());
        }
    }
    let body = read_limited(resp, SCRIPT_BODY_LIMIT)?;
    Ok(HttpResponse {
        status,
        headers: headers_out,
        body,
    })
}

pub fn browser_get(
    url: &str,
    headers: &[(&str, &str)],
    timeout: Duration,
    limit: usize,
) -> Result<HttpResponse> {
    let mut req = browser_client()
        .get(url)
        .timeout(timeout)
        .header("User-Agent", USER_AGENT);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req.send().map_err(|e| anyhow!("{e}"))?;
    let status = resp.status().as_u16();
    let headers_out = header_map(resp.headers());
    if !(200..300).contains(&status) {
        let body = read_limited(resp, 2048).unwrap_or_default();
        let snippet: String = String::from_utf8_lossy(&body).chars().take(180).collect();
        bail!("HTTP {status} {snippet}");
    }
    let body = read_limited(resp, limit)?;
    Ok(HttpResponse {
        status,
        headers: headers_out,
        body,
    })
}

pub const AUDIO_LIMIT: u64 = 300 * 1024 * 1024;

/// 跟随重定向并解压 gzip，把有限的正文读进内存。歌词和封面用这个，不走音频那条 300MB 的路。
pub fn browser_bytes(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    timeout: Duration,
    limit: usize,
) -> Result<Vec<u8>> {
    let method = method.trim();
    let method = if method.is_empty() { "GET" } else { method };
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| anyhow!("unsupported method {method}"))?;
    let mut req = browser_client().request(method, url).timeout(timeout);
    let has_ua = headers
        .iter()
        .any(|(key, _)| key.eq_ignore_ascii_case("user-agent"));
    if !has_ua {
        req = req.header("User-Agent", USER_AGENT);
    }
    for (key, value) in headers {
        req = req.header(*key, *value);
    }
    if let Some(body) = body {
        req = req.body(body.to_vec());
    }
    let resp = req.send().map_err(|err| anyhow!("{err}"))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        bail!("HTTP {status}");
    }
    read_limited(resp, limit)
}

/// 跟随重定向并解压 gzip，把正文写到 `dest`。超过 `limit` 时删除半成品。
pub fn browser_download(url: &str, dest: &Path, timeout: Duration, limit: u64) -> Result<u64> {
    use std::io::{Read, Write};
    let mut resp = browser_client()
        .get(url)
        .timeout(timeout)
        .header("User-Agent", USER_AGENT)
        .send()
        .map_err(|e| anyhow!("{e}"))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        bail!("HTTP {status}");
    }
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut file = std::fs::File::create(dest)?;
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = match resp.read(&mut buf) {
            Ok(n) => n,
            Err(err) => {
                drop(file);
                let _ = std::fs::remove_file(dest);
                return Err(err.into());
            }
        };
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > limit {
            drop(file);
            let _ = std::fs::remove_file(dest);
            bail!("音频超过 {}MB 上限", limit / 1024 / 1024);
        }
        if let Err(err) = file.write_all(&buf[..n]) {
            drop(file);
            let _ = std::fs::remove_file(dest);
            return Err(err.into());
        }
    }
    file.flush()?;
    Ok(total)
}

fn header_map(headers: &reqwest::header::HeaderMap) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (k, v) in headers {
        if let Ok(val) = v.to_str() {
            out.entry(k.as_str().to_string())
                .and_modify(|cur: &mut String| {
                    cur.push_str(", ");
                    cur.push_str(val);
                })
                .or_insert_with(|| val.to_string());
        }
    }
    out
}

pub fn read_limited(mut resp: reqwest::blocking::Response, limit: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    let mut buf = [0u8; 32 * 1024];
    loop {
        let n = resp.read(&mut buf).map_err(|e| anyhow!("{e}"))?;
        if n == 0 {
            break;
        }
        if out.len() + n > limit {
            anyhow::bail!("响应体过大");
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}
