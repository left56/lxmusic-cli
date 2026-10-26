//! 四个平台的搜索。取链不在这里，只把曲目信息交给后面的 QuickJS。

use crate::host::browser_get;
use crate::loose_json::parse_loose_json;
use crate::store::{Hit, Store};
use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Duration;

pub const PLATFORMS: [&str; 4] = ["wy", "kw", "kg", "tx"];
const PAGE_SIZE: u32 = 30;
const SEARCH_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct PlatformError {
    pub platform: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchOutput {
    pub hits: Vec<Hit>,
    pub errors: Vec<PlatformError>,
    pub warnings: Vec<String>,
}

#[derive(Clone)]
struct Draft {
    platform: String,
    external_id: String,
    title: String,
    artist: String,
    album: String,
    duration: i64,
    qualitys: Vec<String>,
    music_info: Value,
}

struct Channel {
    url: String,
    headers: &'static [(&'static str, &'static str)],
}

pub fn normalize_platforms(input: &[String]) -> Result<Vec<String>> {
    if input.iter().all(|s| s.trim().is_empty()) {
        return Ok(PLATFORMS.iter().map(|s| (*s).to_string()).collect());
    }
    let mut out = Vec::new();
    for raw in input {
        for part in raw.split(|c: char| c == ',' || c.is_whitespace()) {
            let platform = part.trim().to_ascii_lowercase();
            if platform.is_empty() {
                continue;
            }
            if !PLATFORMS.contains(&platform.as_str()) {
                bail!("不支持的平台 {platform}。可选：wy、kw、kg、tx");
            }
            if !out.contains(&platform) {
                out.push(platform);
            }
        }
    }
    if out.is_empty() {
        bail!("请指定平台：wy、kw、kg、tx");
    }
    Ok(out)
}

pub fn platform_label(platform: &str) -> &'static str {
    match platform {
        "wy" => "网易云",
        "kw" => "酷我",
        "kg" => "酷狗",
        "tx" => "QQ音乐",
        _ => "未知",
    }
}

pub fn search(
    store: &Store,
    keyword: &str,
    platforms: &[String],
    page: u32,
    progress: bool,
) -> Result<SearchOutput> {
    let keyword = keyword.trim();
    if keyword.is_empty() {
        bail!("请输入关键词");
    }
    if page == 0 {
        bail!("页码从 1 开始");
    }
    let platforms = normalize_platforms(platforms)?;
    let mut hits = Vec::new();
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for platform in platforms {
        if progress {
            eprintln!("搜索 {}…", platform_label(&platform));
        }
        match search_platform(&platform, keyword, page) {
            Ok(mut drafts) => {
                if platform == "tx" && !drafts.is_empty() {
                    if let Err(err) = enrich_tx(&mut drafts) {
                        warnings.push(format!("QQ 音质信息没有补全：{err}"));
                    }
                }
                for draft in drafts {
                    let hit = draft_to_hit(draft);
                    store.upsert_hit(&hit)?;
                    hits.push(hit);
                }
            }
            Err(err) => errors.push(PlatformError {
                platform: platform.clone(),
                message: err.to_string(),
            }),
        }
    }
    Ok(SearchOutput {
        hits,
        errors,
        warnings,
    })
}

fn search_platform(platform: &str, keyword: &str, page: u32) -> Result<Vec<Draft>> {
    let mut errors = Vec::new();
    for channel in channels(platform, keyword, page) {
        match browser_get(&channel.url, channel.headers, Duration::from_secs(12), SEARCH_LIMIT) {
            Err(err) => errors.push(err.to_string()),
            Ok(resp) => match parse_body(&resp.body) {
                Err(err) => errors.push(err.to_string()),
                Ok(value) => {
                    if let Some(message) = detect_error(platform, &value) {
                        errors.push(message);
                        continue;
                    }
                    return Ok(parse_tracks(platform, &value));
                }
            },
        }
    }
    Err(anyhow!(
        "{}",
        if errors.is_empty() {
            "没有可用搜索通道".to_string()
        } else {
            errors.join(" | ")
        }
    ))
}

fn channels(platform: &str, keyword: &str, page: u32) -> Vec<Channel> {
    let offset = (page - 1) * PAGE_SIZE;
    match platform {
        "wy" => vec![
            Channel {
                url: format!(
                    "https://music.163.com/api/cloudsearch/pc?s={}&type=1&limit={PAGE_SIZE}&offset={offset}",
                    encode_component(keyword)
                ),
                headers: &[("Referer", "https://music.163.com/")],
            },
            Channel {
                url: format!(
                    "https://music.163.com/api/search/get/web?s={}&type=1&limit={PAGE_SIZE}&offset={offset}",
                    encode_component(keyword)
                ),
                headers: &[("Referer", "https://music.163.com/")],
            },
        ],
        "kw" => vec![Channel {
            url: format!(
                "https://search.kuwo.cn/r.s?all={}&ft=music&client=kt&pn={}&rn={PAGE_SIZE}&rformat=json&encoding=utf8",
                encode_component(keyword),
                page - 1
            ),
            headers: &[],
        }],
        "kg" => vec![
            Channel {
                url: format!(
                    "https://mobileservice.kugou.com/api/v3/search/song?format=json&keyword={}&page={page}&pagesize={PAGE_SIZE}&showtype=1",
                    encode_component(keyword)
                ),
                headers: &[("Referer", "https://www.kugou.com/")],
            },
            Channel {
                url: format!(
                    "http://mobilecdn.kugou.com/api/v3/search/song?format=json&keyword={}&page={page}&pagesize={PAGE_SIZE}&showtype=1&version=9108",
                    encode_component(keyword)
                ),
                headers: &[("Referer", "https://www.kugou.com/")],
            },
        ],
        "tx" => vec![
            Channel {
                url: format!(
                    "https://c.y.qq.com/soso/fcgi-bin/search_for_qq_cp?w={}&p={page}&n={PAGE_SIZE}&format=json",
                    encode_component(keyword)
                ),
                headers: &[("Referer", "https://y.qq.com/")],
            },
            Channel {
                url: format!(
                    "https://c.y.qq.com/soso/fcgi-bin/client_search_cp?w={}&p={page}&n={PAGE_SIZE}&format=json",
                    encode_component(keyword)
                ),
                headers: &[("Referer", "https://y.qq.com/")],
            },
        ],
        _ => Vec::new(),
    }
}

fn enrich_tx(drafts: &mut [Draft]) -> Result<()> {
    let mids: Vec<String> = drafts.iter().map(|d| d.external_id.clone()).filter(|id| !id.is_empty()).collect();
    if mids.is_empty() {
        return Ok(());
    }
    let mut files = std::collections::HashMap::<String, Value>::new();
    for chunk in mids.chunks(50) {
        let url = tx_trackinfo_url(chunk);
        let resp = browser_get(&url, &[("Referer", "https://y.qq.com/")], Duration::from_secs(8), SEARCH_LIMIT)?;
        let value = parse_body(&resp.body)?;
        if let Some(code) = value.pointer("/req_1/code").and_then(|v| v.as_i64()) {
            if code != 0 {
                bail!("trackinfo code {code}");
            }
        }
        let tracks = value.pointer("/req_1/data/tracks").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        for track in tracks {
            let Some(mid) = track.get("mid").and_then(|v| v.as_str()) else {
                continue;
            };
            if let Some(file) = track.get("file").cloned() {
                files.insert(mid.to_string(), file);
            }
        }
    }
    if files.is_empty() {
        bail!("没有返回曲目文件信息");
    }
    for draft in drafts.iter_mut() {
        if let Some(file) = files.get(&draft.external_id) {
            apply_tx_file(draft, file);
        }
    }
    Ok(())
}

fn tx_trackinfo_url(mids: &[String]) -> String {
    let payload = json!({
        "comm": { "ct": 24, "cv": 0 },
        "req_1": {
            "module": "music.trackInfo.UniformRuleCtrl",
            "method": "CgiGetTrackInfo",
            "param": {
                "ids": vec![0; mids.len()],
                "types": vec![0; mids.len()],
                "mids": mids,
            }
        }
    });
    format!(
        "https://u.y.qq.com/cgi-bin/musicu.fcg?format=json&data={}",
        encode_component(&payload.to_string())
    )
}

fn apply_tx_file(draft: &mut Draft, file: &Value) {
    if let Some(mid) = file.get("media_mid").and_then(|v| v.as_str()) {
        if !mid.is_empty() {
            draft.music_info["strMediaMid"] = json!(mid);
            draft.music_info["media_mid"] = json!(mid);
        }
    }
    let qualitys = tx_qualitys(file);
    draft.qualitys.clone_from(&qualitys);
    draft.music_info["qualitys"] = json!(qualitys);
    if positive(file.get("size_flac")) || positive(file.get("size_hires")) || positive(file.get("size_ape")) {
        draft.music_info["hasLossless"] = json!(true);
    }
}

fn tx_qualitys(file: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if positive(file.get("size_128mp3")) {
        out.push("128k".into());
    }
    if positive(file.get("size_320mp3")) {
        out.push("320k".into());
    }
    if positive(file.get("size_flac")) || positive(file.get("size_ape")) {
        out.push("flac".into());
    }
    if positive(file.get("size_hires")) {
        out.push("flac24bit".into());
    }
    if out.is_empty() {
        vec!["128k".into(), "320k".into()]
    } else {
        out
    }
}

fn parse_tracks(platform: &str, value: &Value) -> Vec<Draft> {
    let list = match platform {
        "wy" => value.pointer("/result/songs").and_then(|v| v.as_array()),
        "kw" => value.get("abslist").and_then(|v| v.as_array()),
        "kg" => value.pointer("/data/info").and_then(|v| v.as_array()),
        "tx" => value.pointer("/data/song/list").and_then(|v| v.as_array()),
        _ => None,
    };
    list.map(|songs| {
        songs
            .iter()
            .filter_map(|song| map_track(platform, song))
            .collect()
    })
    .unwrap_or_default()
}

fn map_track(platform: &str, song: &Value) -> Option<Draft> {
    let draft = match platform {
        "wy" => map_wy(song)?,
        "kw" => map_kw(song)?,
        "kg" => map_kg(song)?,
        "tx" => map_tx(song)?,
        _ => return None,
    };
    if draft.external_id.is_empty() {
        None
    } else {
        Some(finish(draft))
    }
}

fn map_wy(song: &Value) -> Option<Draft> {
    let id = id_string(song.get("id"))?;
    let album = song.get("al").or_else(|| song.get("album")).cloned().unwrap_or(Value::Null);
    let artist = artists_join(song.get("ar").or_else(|| song.get("artists")));
    let title = text(song.get("name"), "未知");
    let album_name = text(album.get("name"), "");
    let cover = json_opt(album.get("picUrl").and_then(|v| v.as_str()));
    let ms = num(song.get("dt").or_else(|| song.get("duration")));
    Some(base(
        "wy",
        id.clone(),
        title.clone(),
        artist.clone(),
        album_name.clone(),
        secs_from_ms(ms),
        vec!["128k".into(), "320k".into(), "flac".into()],
        json!({
            "name": title,
            "singer": artist,
            "albumName": album_name,
            "songmid": &id,
            "hash": &id,
            "source": "wy",
            "img": cover,
            "interval": interval_from_ms(ms),
        }),
    ))
}

fn map_kw(song: &Value) -> Option<Draft> {
    let raw_id = text(song.get("MUSICRID").or_else(|| song.get("DC_TARGETID")), "");
    let id = raw_id.replace("MUSIC_", "");
    if id.is_empty() {
        return None;
    }
    let title = decode_component(&text(song.get("NAME").or_else(|| song.get("SONGNAME")), ""));
    let artist = decode_component(&text(song.get("ARTIST"), ""));
    let album = decode_component(&text(song.get("ALBUM"), ""));
    let duration = num(song.get("DURATION"));
    let pic = song.get("web_albumpic_short").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
    let cover = json_opt(pic.map(|p| format!("https://img2.kuwo.cn/star/albumcover/{p}")).as_deref());
    Some(base(
        "kw",
        id.clone(),
        if title.is_empty() { "未知".into() } else { title.clone() },
        artist.clone(),
        album.clone(),
        duration,
        vec!["128k".into(), "320k".into()],
        json!({
            "name": title,
            "singer": artist,
            "albumName": album,
            "songmid": &id,
            "hash": &id,
            "source": "kw",
            "img": cover,
            "interval": interval_from_sec(duration),
        }),
    ))
}

fn map_kg(song: &Value) -> Option<Draft> {
    let hash = id_string(song.get("hash"))
        .or_else(|| id_string(song.get("sqhash")))
        .or_else(|| id_string(song.get("320hash")))
        .unwrap_or_default();
    if hash.is_empty() {
        return None;
    }
    let title = text(song.get("songname").or_else(|| song.get("songname_original")), "未知");
    let artist = text(song.get("singername"), "未知");
    let album = text(song.get("album_name"), "");
    let duration = num(song.get("duration"));
    let cover = json_opt(
        song.pointer("/trans_param/union_cover")
            .and_then(|v| v.as_str())
            .map(|s| s.replace("{size}", "240"))
            .as_deref(),
    );
    Some(base(
        "kg",
        hash.clone(),
        title.clone(),
        artist.clone(),
        album.clone(),
        duration,
        vec!["128k".into(), "320k".into()],
        json!({
            "name": title,
            "singer": artist,
            "albumName": album,
            "hash": &hash,
            "songmid": &hash,
            "source": "kg",
            "img": cover,
            "interval": interval_from_sec(duration),
        }),
    ))
}

fn map_tx(song: &Value) -> Option<Draft> {
    let mid = id_string(song.get("songmid")).or_else(|| id_string(song.get("mid")))?;
    let title = text(song.get("songname").or_else(|| song.get("name")), "未知");
    let artist = artists_join(song.get("singer"));
    let album = text(song.get("albumname"), "");
    let duration = num(song.get("interval"));
    let cover = json_opt(
        song.get("albummid")
            .and_then(|v| v.as_str())
            .map(|id| format!("https://y.qq.com/music/photo_new/T002R300x300M000{id}.jpg"))
            .as_deref(),
    );
    let media = song
        .get("strMediaMid")
        .or_else(|| song.get("media_mid"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let songid = song.get("songid").or_else(|| song.get("id")).cloned().unwrap_or(Value::Null);
    Some(base(
        "tx",
        mid.clone(),
        title.clone(),
        artist.clone(),
        album.clone(),
        duration,
        vec!["128k".into(), "320k".into()],
        json!({
            "name": title,
            "singer": artist,
            "albumName": album,
            "songmid": &mid,
            "hash": &mid,
            "songid": songid,
            "strMediaMid": media,
            "source": "tx",
            "img": cover,
            "interval": interval_from_sec(duration),
        }),
    ))
}

fn json_opt(value: Option<&str>) -> Value {
    match value.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Value::String(s.to_string()),
        None => Value::Null,
    }
}

fn base(
    platform: &str,
    external_id: String,
    title: String,
    artist: String,
    album: String,
    duration: i64,
    qualitys: Vec<String>,
    music_info: Value,
) -> Draft {
    Draft {
        platform: platform.into(),
        external_id,
        title,
        artist,
        album,
        duration,
        qualitys,
        music_info,
    }
}

fn finish(mut draft: Draft) -> Draft {
    draft.artist = clean_artist(&draft.artist);
    let cleaned = draft
        .music_info
        .get("singer")
        .and_then(|v| v.as_str())
        .map(clean_artist);
    if let Some(cleaned) = cleaned {
        draft.music_info["singer"] = Value::String(cleaned);
    } else {
        draft.music_info["singer"] = Value::String(draft.artist.clone());
    }
    draft
}

fn draft_to_hit(draft: Draft) -> Hit {
    Hit {
        id: hit_id(&draft.platform, &draft.external_id),
        platform: draft.platform,
        external_id: draft.external_id,
        title: draft.title,
        artist: draft.artist,
        album: draft.album,
        duration: draft.duration.max(0),
        qualitys: draft.qualitys,
        music_info: draft.music_info,
    }
}

pub fn hit_id(platform: &str, external_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(platform.as_bytes());
    hasher.update([0xff]);
    hasher.update(external_id.as_bytes());
    hex::encode(&hasher.finalize()[..4])
}

fn detect_error(platform: &str, value: &Value) -> Option<String> {
    match platform {
        "wy" => {
            let code = value.get("code")?;
            (num(Some(code)) != 200).then(|| format!("code {}", num(Some(code))))
        }
        "tx" => {
            let code = value.get("code")?;
            (num(Some(code)) != 0).then(|| format!("code {}", num(Some(code))))
        }
        "kg" => kg_error(value),
        "kw" => kw_error(value),
        _ => None,
    }
}

fn kg_error(value: &Value) -> Option<String> {
    if !value.is_object() {
        return None;
    }
    if nonzero(value.get("error_code")) {
        return Some(format!(
            "{} (error_code={})",
            text(value.get("error_msg").or_else(|| value.get("error")), "upstream error"),
            num(value.get("error_code"))
        ));
    }
    if nonzero(value.get("errcode")) {
        return Some(format!(
            "{} (errcode={})",
            text(value.get("error").or_else(|| value.get("error_msg")), "upstream error"),
            num(value.get("errcode"))
        ));
    }
    if value.get("status").is_some() && num(value.get("status")) != 1 {
        return Some(format!("upstream status={}", num(value.get("status"))));
    }
    None
}

fn kw_error(value: &Value) -> Option<String> {
    if !value.is_object() {
        return None;
    }
    if value.get("success").and_then(|v| v.as_bool()) == Some(false) {
        return Some(text(value.get("message").or_else(|| value.get("msg")), "request rejected"));
    }
    if nonzero(value.get("error_code")) {
        return Some(format!(
            "{} (error_code={})",
            text(value.get("error_msg").or_else(|| value.get("error")), "upstream error"),
            num(value.get("error_code"))
        ));
    }
    if value.get("code").is_some() && num(value.get("code")) >= 400 {
        return Some(format!("code {}", num(value.get("code"))));
    }
    if nonzero(value.get("errcode")) {
        return Some(format!(
            "{} (errcode={})",
            text(value.get("error"), "upstream error"),
            num(value.get("errcode"))
        ));
    }
    None
}

fn parse_body(bytes: &[u8]) -> Result<Value> {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim().trim_start_matches('\u{feff}');
    parse_loose_json(text)
}

fn artists_join(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return "未知".into();
    };
    match value {
        Value::String(s) => clean_artist(s),
        Value::Array(items) => {
            let names: Vec<String> = items
                .iter()
                .filter_map(|item| {
                    item.as_str()
                        .map(|s| s.to_string())
                        .or_else(|| item.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()))
                })
                .filter(|s| !s.is_empty())
                .collect();
            clean_artist(&names.join(" / "))
        }
        other => clean_artist(&other.to_string()),
    }
}

fn clean_artist(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    if s.is_empty() {
        return "未知".into();
    }
    if let Some(idx) = dash_slash_index(&s) {
        s.truncate(idx);
        s = s.trim().to_string();
    }
    while s.ends_with([' ', '-', '/', '\t']) {
        s.pop();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut collapsed = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '/' {
            while collapsed.ends_with(char::is_whitespace) {
                collapsed.pop();
            }
            collapsed.push_str(" / ");
            i += 1;
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            continue;
        }
        collapsed.push(chars[i]);
        i += 1;
    }
    let collapsed = collapsed.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        "未知".into()
    } else {
        collapsed
    }
}

fn dash_slash_index(s: &str) -> Option<usize> {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    for i in 0..chars.len() {
        let mut j = i;
        while j < chars.len() && chars[j].1.is_whitespace() {
            j += 1;
        }
        if j >= chars.len() || chars[j].1 != '-' {
            continue;
        }
        j += 1;
        while j < chars.len() && chars[j].1.is_whitespace() {
            j += 1;
        }
        if j >= chars.len() || chars[j].1 != '/' {
            continue;
        }
        j += 1;
        while j < chars.len() && chars[j].1.is_whitespace() {
            j += 1;
        }
        if j < chars.len() {
            return Some(chars[i].0);
        }
    }
    None
}

fn text(value: Option<&Value>, default: &str) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => default.to_string(),
    }
}

fn id_string(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(s) = value.as_str() {
        let s = s.trim();
        return (!s.is_empty()).then(|| s.to_string());
    }
    if let Some(i) = value.as_i64() {
        return Some(i.to_string());
    }
    if let Some(u) = value.as_u64() {
        return Some(u.to_string());
    }
    if let Some(f) = value.as_f64() {
        if f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
            return Some((f as i64).to_string());
        }
    }
    None
}

fn num(value: Option<&Value>) -> i64 {
    let Some(value) = value else {
        return 0;
    };
    if let Some(i) = value.as_i64() {
        return i;
    }
    if let Some(u) = value.as_u64() {
        return u as i64;
    }
    if let Some(f) = value.as_f64() {
        return f.round() as i64;
    }
    value.as_str().and_then(|s| s.parse::<f64>().ok()).map(|f| f.round() as i64).unwrap_or(0)
}

fn positive(value: Option<&Value>) -> bool {
    num(value) > 0
}

fn nonzero(value: Option<&Value>) -> bool {
    value.is_some() && num(value) != 0
}

fn secs_from_ms(ms: i64) -> i64 {
    (ms as f64 / 1000.0).round() as i64
}

fn interval_from_ms(ms: i64) -> String {
    interval_from_sec(secs_from_ms(ms))
}

fn interval_from_sec(sec: i64) -> String {
    let sec = sec.max(0);
    format!("{}:{:02}", sec / 60, sec % 60)
}

fn encode_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn decode_component(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_urls_match_the_search_order() {
        let wy = channels("wy", "晴天", 2);
        assert!(wy[0].url.contains("/api/cloudsearch/pc"));
        assert!(wy[0].url.contains("offset=30"));
        assert!(wy[1].url.contains("/api/search/get/web"));

        let kw = channels("kw", "晴天", 2);
        assert!(kw[0].url.starts_with("https://search.kuwo.cn/r.s?"));
        assert!(kw[0].url.contains("pn=1"));
        assert!(kw[0].url.contains("rformat=json"));

        let kg = channels("kg", "晴天", 1);
        assert!(kg[0].url.starts_with("https://mobileservice.kugou.com/api/v3/search/song"));
        assert!(kg[1].url.starts_with("http://mobilecdn.kugou.com/api/v3/search/song"));
        assert!(kg[1].url.contains("version=9108"));

        let tx = channels("tx", "晴天", 1);
        assert!(tx[0].url.contains("/soso/fcgi-bin/search_for_qq_cp"));
        assert!(tx[1].url.contains("/soso/fcgi-bin/client_search_cp"));

        let info = tx_trackinfo_url(&["0039MnYb0qxYhV".into()]);
        assert!(info.contains("https://u.y.qq.com/cgi-bin/musicu.fcg"));
        assert!(info.contains("CgiGetTrackInfo"));
    }

    #[test]
    fn maps_wy_kw_kg_and_tx_shapes() {
        let wy = parse_tracks(
            "wy",
            &serde_json::from_str(
                r#"{"code":200,"result":{"songs":[{"id":1,"name":"晴天","ar":[{"name":"周杰伦- / A-LNK"}],"al":{"name":"叶惠美"},"dt":269000}]}}"#,
            )
            .unwrap(),
        );
        assert_eq!(wy[0].title, "晴天");
        assert_eq!(wy[0].artist, "周杰伦");
        assert_eq!(wy[0].duration, 269);
        assert_eq!(wy[0].music_info["interval"], "4:29");
        assert_eq!(wy[0].music_info["songmid"], "1");

        let raw = parse_loose_json(
            "try{var jsondata={'abslist':[{'MUSICRID':'MUSIC_9','NAME':'%E6%99%B4%E5%A4%A9','ARTIST':'%E5%91%A8%E6%9D%B0%E4%BC%A6','ALBUM':'','DURATION':'269'}]};}catch(e){}",
        )
        .unwrap();
        let kw = parse_tracks("kw", &raw);
        assert_eq!(kw[0].external_id, "9");
        assert_eq!(kw[0].title, "晴天");
        assert_eq!(kw[0].artist, "周杰伦");

        let kg = parse_tracks(
            "kg",
            &serde_json::from_str(
                r#"{"status":1,"data":{"info":[{"hash":"ABC","songname":"晴天","singername":"周杰伦","duration":269,"trans_param":{"union_cover":"http://img/{size}/a.jpg"}}]}}"#,
            )
            .unwrap(),
        );
        assert_eq!(kg[0].music_info["hash"], "ABC");
        assert_eq!(kg[0].music_info["img"], "http://img/240/a.jpg");
        assert!(kg_error(&serde_json::json!({"status": 0, "error_code": 20006})).is_some());
        assert!(kw_error(&serde_json::json!({"HIT": "0", "abslist": []})).is_none());

        let mut tx = parse_tracks(
            "tx",
            &serde_json::from_str(
                r#"{"code":0,"data":{"song":{"list":[{"songmid":"003","songname":"晴天","singer":[{"name":"周杰伦"}],"albumname":"叶惠美","interval":269}]}}}"#,
            )
            .unwrap(),
        );
        apply_tx_file(
            &mut tx[0],
            &serde_json::json!({"media_mid":"MED","size_128mp3":1,"size_320mp3":1,"size_flac":9,"size_hires":0}),
        );
        assert_eq!(tx[0].music_info["strMediaMid"], "MED");
        assert_eq!(tx[0].music_info["media_mid"], "MED");
        assert_eq!(tx[0].qualitys, vec!["128k", "320k", "flac"]);
        assert_eq!(tx[0].music_info["hasLossless"], true);
        assert_eq!(hit_id("wy", "1"), hit_id("wy", "1"));
        assert_ne!(hit_id("wy", "1"), hit_id("kw", "1"));
    }
}
