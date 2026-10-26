//! 音频落下之后，旁边再写歌词和封面，歌曲信息写进音频文件的标准标签。
//! 这三样失败都不影响已经下好的音频，也不改音源状态。
//!
//! 歌词和封面先问音源的 `lyric` / `pic`（洛雪脚本约定）。
//! 音源没有歌词时，再按觅音的方式请求各平台公开接口。
//! `lx.env` 仍然是 `desktop`，这里不把它改成 `cli`。

use crate::host::{browser_bytes, SourceSession};
use crate::store::Hit;
use base64::Engine;
use flate2::read::ZlibDecoder;
use lofty::config::WriteOptions;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::probe::Probe;
use lofty::tag::Tag;
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

const TEXT_LIMIT: usize = 2 * 1024 * 1024;
const COVER_LIMIT: usize = 8 * 1024 * 1024;
const LYRIC_WAIT: Duration = Duration::from_secs(12);

#[derive(Debug, Clone, Default)]
pub struct Attached {
    pub tagged: bool,
    pub info_note: String,
    pub lyric_path: Option<String>,
    pub cover_path: Option<String>,
    pub lyric_note: String,
    pub cover_note: String,
}

pub fn attach(
    session: Option<&SourceSession>,
    hit: &Hit,
    audio: &Path,
    native_lyric: bool,
    progress: bool,
) -> Attached {
    let mut attached = Attached::default();
    let info = music_info_for_lyric(hit);

    if progress {
        eprintln!("  歌词");
    }
    let (lyric, lyric_err) = match session {
        Some(session) => script_lyric(session, &hit.platform, &info),
        None => (None, Some("音源无法再次打开".into())),
    };
    let lyric = if lyric.is_some() {
        lyric
    } else if native_lyric {
        if progress {
            eprintln!("  歌词改走平台接口");
        }
        fetch_native_lyric(&hit.platform, &info, hit.duration).or(lyric)
    } else {
        lyric
    };
    if let Some(text) = lyric.as_deref().filter(|text| usable_lyric(text)) {
        let path = sidecar(audio, "lrc");
        match std::fs::write(&path, format!("{}\n", text.trim())) {
            Ok(()) => attached.lyric_path = Some(path.display().to_string()),
            Err(err) => attached.lyric_note = missing(Some(err.to_string())),
        }
    } else {
        attached.lyric_note = missing(lyric_err);
    }

    if progress {
        eprintln!("  封面");
    }
    let mut cover_err = None;
    for url in cover_candidates(session, hit, &info) {
        match save_cover(&url, &hit.platform, audio) {
            Ok(path) => {
                attached.cover_path = Some(path.display().to_string());
                cover_err = None;
                break;
            }
            Err(err) => cover_err = Some(err),
        }
    }
    if attached.cover_path.is_none() {
        if let Some(err) = cover_err {
            attached.cover_note = missing(Some(err));
        }
    }

    if progress {
        eprintln!("  歌曲信息");
    }
    let cover_bytes = attached
        .cover_path
        .as_deref()
        .and_then(|path| std::fs::read(path).ok());
    match embed_tags(audio, hit, cover_bytes.as_deref()) {
        Ok(()) => attached.tagged = true,
        Err(err) if cover_bytes.is_some() => match embed_tags(audio, hit, None) {
            Ok(()) => {
                attached.tagged = true;
                attached.info_note = format!("封面未写入标签（{err}）");
            }
            Err(err) => attached.info_note = missing(Some(err)),
        },
        Err(err) => attached.info_note = missing(Some(err)),
    }
    attached
}

fn embed_tags(audio: &Path, hit: &Hit, cover: Option<&[u8]>) -> Result<(), String> {
    let mut tagged = Probe::open(audio)
        .map_err(|err| brief(&err.to_string()))?
        .guess_file_type()
        .map_err(|err| brief(&err.to_string()))?
        .read()
        .map_err(|err| brief(&err.to_string()))?;
    {
        let tag = tag_slot(&mut tagged)?;
        set_text(tag, hit);
        if let Some(bytes) = cover {
            if let Some(picture) = picture_from(bytes) {
                tag.set_picture(0, picture);
            }
        }
    }
    tagged
        .save_to_path(audio, WriteOptions::default())
        .map_err(|err| brief(&err.to_string()))
}

fn tag_slot(tagged: &mut lofty::file::TaggedFile) -> Result<&mut Tag, String> {
    if tagged.primary_tag().is_none() && tagged.first_tag().is_none() {
        let tag_type = tagged.primary_tag_type();
        tagged.insert_tag(Tag::new(tag_type));
    }
    if tagged.primary_tag().is_some() {
        Ok(tagged.primary_tag_mut().expect("primary tag"))
    } else {
        tagged.first_tag_mut().ok_or_else(|| "无法创建标签".into())
    }
}

fn set_text(tag: &mut Tag, hit: &Hit) {
    let title = hit.title.trim();
    let artist = hit.artist.trim();
    let album = hit.album.trim();
    if !title.is_empty() {
        tag.set_title(title.to_string());
    }
    if !artist.is_empty() {
        tag.set_artist(artist.to_string());
    }
    if !album.is_empty() {
        tag.set_album(album.to_string());
    }
}

fn picture_from(bytes: &[u8]) -> Option<Picture> {
    let mime = match image_ext(bytes)? {
        "jpg" => MimeType::Jpeg,
        "png" => MimeType::Png,
        "gif" => MimeType::Gif,
        "webp" => MimeType::Unknown("image/webp".into()),
        _ => return None,
    };
    Some(
        Picture::unchecked(bytes.to_vec())
            .pic_type(PictureType::CoverFront)
            .mime_type(mime)
            .build(),
    )
}

fn music_info_for_lyric(hit: &Hit) -> Value {
    let mut info = if hit.music_info.is_object() {
        hit.music_info.clone()
    } else {
        json!({})
    };
    if info_id(&info).is_none() && !hit.external_id.trim().is_empty() {
        info["songmid"] = json!(hit.external_id);
    }
    if info.get("name").and_then(|v| v.as_str()).unwrap_or("").trim().is_empty() {
        info["name"] = json!(hit.title);
    }
    if info.get("singer").and_then(|v| v.as_str()).unwrap_or("").trim().is_empty() {
        info["singer"] = json!(hit.artist);
    }
    info
}

fn script_lyric(session: &SourceSession, platform: &str, info: &Value) -> (Option<String>, Option<String>) {
    match session.call_action("lyric", platform, json!({ "musicInfo": info })) {
        Ok((ok, payload)) => {
            if let Some(text) = lyric_from_script(ok, &payload) {
                (Some(text), None)
            } else if !ok {
                (None, Some(brief(&payload)))
            } else {
                (None, None)
            }
        }
        Err(err) => (None, Some(brief(&err.to_string()))),
    }
}

fn cover_candidates(session: Option<&SourceSession>, hit: &Hit, info: &Value) -> Vec<String> {
    let mut urls = Vec::new();
    let from_script = session.and_then(|session| {
        match session.call_action("pic", &hit.platform, json!({ "musicInfo": info })) {
            Ok((ok, payload)) => cover_from_script(ok, &payload),
            Err(_) => None,
        }
    });
    if let Some(url) = from_script {
        urls.push(url);
    }
    if let Some(url) = cover_from_info(info).or_else(|| cover_from_info(&hit.music_info)) {
        if !urls.iter().any(|item| item == &url) {
            urls.push(url);
        }
    }
    urls
}

fn save_cover(url: &str, platform: &str, audio: &Path) -> Result<PathBuf, String> {
    let mut headers = Vec::new();
    if let Some(referer) = platform_referer(platform) {
        headers.push(("Referer", referer));
    }
    let bytes = browser_bytes("GET", url, &headers, None, LYRIC_WAIT, COVER_LIMIT)
        .map_err(|err| brief(&err.to_string()))?;
    let ext = image_ext(&bytes).ok_or_else(|| "不是图片".to_string())?;
    let path = sidecar(audio, ext);
    std::fs::write(&path, bytes).map_err(|err| err.to_string())?;
    Ok(path)
}

fn sidecar(audio: &Path, ext: &str) -> PathBuf {
    let stem = audio
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("track");
    let name = format!("{stem}.{ext}");
    match audio.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(name),
        _ => PathBuf::from(name),
    }
}

fn missing(err: Option<String>) -> String {
    match err.map(|item| item.trim().to_string()).filter(|item| !item.is_empty()) {
        Some(err) => format!("未获取（{err}）"),
        None => "未获取".into(),
    }
}

fn brief(err: &str) -> String {
    let one = err.split(['\n', '\r']).next().unwrap_or(err).trim();
    let mut out: String = one.chars().take(80).collect();
    if one.chars().count() > 80 {
        out.push('…');
    }
    out
}

pub(crate) fn lyric_from_script(ok: bool, payload: &str) -> Option<String> {
    if !ok {
        return None;
    }
    if let Some(text) = payload.strip_prefix("s:") {
        return accept_lyric(text);
    }
    if let Some(json) = payload.strip_prefix("j:") {
        let value: Value = serde_json::from_str(json).ok()?;
        return lyric_from_value(&value);
    }
    None
}

fn lyric_from_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => accept_lyric(text),
        Value::Object(_) => {
            let lyric = as_text(value.get("lyric"));
            let translated = {
                let text = as_text(value.get("tlyric"));
                if text.trim().is_empty() {
                    as_text(value.get("rlyric"))
                } else {
                    text
                }
            };
            let mut merged = merge_bilingual(&lyric, &translated);
            if merged.trim().is_empty() {
                merged = as_text(value.get("lxlyric"));
            }
            accept_lyric(&merged)
        }
        _ => None,
    }
}

fn accept_lyric(text: &str) -> Option<String> {
    let trimmed = text.trim();
    usable_lyric(trimmed).then(|| trimmed.to_string())
}

pub(crate) fn cover_from_script(ok: bool, payload: &str) -> Option<String> {
    if !ok {
        return None;
    }
    if let Some(text) = payload.strip_prefix("s:") {
        return http_url(text);
    }
    let json = payload.strip_prefix("j:")?;
    let value: Value = serde_json::from_str(json).ok()?;
    match value {
        Value::String(text) => http_url(&text),
        Value::Object(_) => ["url", "pic", "picUrl", "cover"]
            .iter()
            .find_map(|key| value.get(*key).and_then(|item| item.as_str()).and_then(http_url)),
        _ => None,
    }
}

pub(crate) fn cover_from_info(info: &Value) -> Option<String> {
    ["img", "pic", "cover", "albumPic", "picUrl", "albumPicUrl"]
        .iter()
        .find_map(|key| info.get(*key).and_then(|item| item.as_str()).and_then(http_url))
}

fn http_url(raw: &str) -> Option<String> {
    let text = raw.trim();
    if text.starts_with("http://") || text.starts_with("https://") {
        Some(text.to_string())
    } else {
        None
    }
}

fn usable_lyric(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() || text.len() > TEXT_LIMIT {
        return false;
    }
    let head: String = text.chars().take(32).collect::<String>().to_ascii_lowercase();
    if head.starts_with("http://") || head.starts_with("https://") {
        return false;
    }
    if head.starts_with("<!doctype") || head.starts_with("<html") {
        return false;
    }
    if matches!(text, "undefined" | "null" | "false" | "true" | "none") {
        return false;
    }
    text.contains('[') || text.lines().filter(|line| !line.trim().is_empty()).count() >= 2
}

fn as_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        _ => String::new(),
    }
}

/// 同一时间戳先原文、再翻译。翻译里多出来的时间戳补在后面。
pub(crate) fn merge_bilingual(original: &str, translated: &str) -> String {
    let original = original.trim();
    let translated = translated.trim();
    if original.is_empty() {
        return translated.to_string();
    }
    if translated.is_empty() {
        return original.to_string();
    }
    let mut translated_at: Vec<(String, Vec<String>)> = Vec::new();
    for (time, text) in parse_lrc(translated) {
        if time.is_empty() || text.is_empty() {
            continue;
        }
        if let Some((_, bucket)) = translated_at.iter_mut().find(|(key, _)| key == &time) {
            bucket.push(text);
        } else {
            translated_at.push((time, vec![text]));
        }
    }
    let mut used = Vec::new();
    let mut out = Vec::new();
    for (time, text) in parse_lrc(original) {
        if time.is_empty() {
            out.push(text);
            continue;
        }
        out.push(format!("[{time}]{text}"));
        if let Some((_, lines)) = translated_at.iter().find(|(key, _)| key == &time) {
            used.push(time.clone());
            for line in lines {
                if !line.is_empty() && line != &text {
                    out.push(format!("[{time}]{line}"));
                }
            }
        }
    }
    for (time, lines) in &translated_at {
        if used.iter().any(|item| item == time) {
            continue;
        }
        for line in lines {
            out.push(format!("[{time}]{line}"));
        }
    }
    out.join("\n")
}

fn parse_lrc(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let line = raw.trim().trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            if let Some((tag, body)) = rest.split_once(']') {
                if is_timestamp(tag) {
                    out.push((tag.to_string(), body.trim().to_string()));
                    continue;
                }
            }
        }
        out.push((String::new(), line.to_string()));
    }
    out
}

fn is_timestamp(tag: &str) -> bool {
    let mut parts = tag.split(':');
    let (Some(min), Some(rest)) = (parts.next(), parts.next()) else {
        return false;
    };
    if parts.next().is_some()
        || min.is_empty()
        || min.len() > 2
        || !min.chars().all(|c| c.is_ascii_digit())
    {
        return false;
    }
    let (sec, frac) = match rest.split_once('.') {
        Some((sec, frac)) => (sec, Some(frac)),
        None => (rest, None),
    };
    if sec.len() != 2 || !sec.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    match frac {
        Some(frac) => (1..=3).contains(&frac.len()) && frac.chars().all(|c| c.is_ascii_digit()),
        None => true,
    }
}

fn fetch_native_lyric(platform: &str, info: &Value, duration_sec: i64) -> Option<String> {
    let text = match platform {
        "wy" => fetch_wy(info)?,
        "kw" => fetch_kw(info)?,
        "tx" => fetch_tx(info)?,
        "kg" => fetch_kg(info, duration_sec)?,
        "mg" => fetch_mg(info)?,
        _ => return None,
    };
    accept_lyric(&text)
}

fn fetch_wy(info: &Value) -> Option<String> {
    let id = info_id(info)?;
    let url = format!(
        "https://music.163.com/api/song/lyric?id={}&lv=1&kv=1&tv=-1",
        encode_component(&id)
    );
    let body = get_json(&url, &[("Referer", "https://music.163.com/")])?;
    lyric_from_wy_body(&body)
}

pub(crate) fn lyric_from_wy_body(body: &Value) -> Option<String> {
    let lyric = body.pointer("/lrc/lyric").and_then(|v| v.as_str()).unwrap_or("");
    let translated = body
        .pointer("/tlyric/lyric")
        .and_then(|v| v.as_str())
        .filter(|text| !text.trim().is_empty())
        .or_else(|| body.pointer("/romalrc/lyric").and_then(|v| v.as_str()))
        .unwrap_or("");
    accept_lyric(&merge_bilingual(lyric, translated))
}

fn fetch_kw(info: &Value) -> Option<String> {
    let id = info_id(info)?;
    let h5 = format!(
        "https://m.kuwo.cn/newh5/singles/songinfoandlrc?musicId={}",
        encode_component(&id)
    );
    if let Some(body) = get_json(&h5, &[("Referer", "https://m.kuwo.cn/")]) {
        if let Some(text) = lyric_from_kw_body(&body) {
            return Some(text);
        }
    }
    let mobi = format!("https://mobi.kuwo.cn/mobi.s?f=web&type=lyric&musicId={id}");
    let body = get_json(&mobi, &[("Referer", "https://m.kuwo.cn/")])?;
    lyric_from_kw_body(&body)
}

fn lyric_from_kw_body(body: &Value) -> Option<String> {
    let list = body
        .pointer("/data/lrclist")
        .and_then(|v| v.as_array())
        .or_else(|| body.get("lrclist").and_then(|v| v.as_array()))?;
    lyric_from_kw_list(list)
}

pub(crate) fn lyric_from_kw_list(list: &[Value]) -> Option<String> {
    let mut lyric = Vec::new();
    let mut translated = Vec::new();
    let mut seen = Vec::new();
    for item in list {
        let time = format_kuwo_time(item.get("time").unwrap_or(&Value::Null));
        if time.is_empty() {
            continue;
        }
        let text = item.get("lineLyric").and_then(|v| v.as_str()).unwrap_or("").trim();
        let line = format!("[{time}]{text}");
        if seen.iter().any(|item| item == &time) {
            translated.push(line);
        } else {
            seen.push(time);
            lyric.push(line);
        }
    }
    accept_lyric(&merge_bilingual(&lyric.join("\n"), &translated.join("\n")))
}

fn format_kuwo_time(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        if text.contains(':') {
            return text.trim().to_string();
        }
    }
    let Some(seconds) = json_f64(value) else {
        return String::new();
    };
    if !seconds.is_finite() || seconds < 0.0 {
        return String::new();
    }
    let min = (seconds / 60.0).floor() as u64;
    let rest = seconds - (min as f64) * 60.0;
    let sec = rest.floor() as u64;
    let frac = ((rest - sec as f64) * 100.0).round() as u64;
    format!("{min:02}:{sec:02}.{frac:02}")
}

fn fetch_tx(info: &Value) -> Option<String> {
    // 旧接口是 Base64 的 LRC。QRC 密文依赖非标准 3DES，这里不解，避免把密文写成歌词。
    let mid = info
        .get("songmid")
        .or_else(|| info.get("hash"))
        .and_then(json_string)
        .filter(|id| !id.is_empty())?;
    let url = format!(
        "https://c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric_new.fcg?songmid={}&g_tk=5381&loginUin=0&hostUin=0&format=json&inCharset=utf8&outCharset=utf-8&notice=0&platform=yqq.json&needNewCode=0",
        encode_component(&mid)
    );
    let body = get_json(&url, &[("Referer", "https://y.qq.com/portal/player.html")])?;
    lyric_from_tx_legacy(&body)
}

pub(crate) fn lyric_from_tx_legacy(body: &Value) -> Option<String> {
    let code_ok = json_i64(body.get("code")).unwrap_or(-1) == 0
        || json_i64(body.get("retcode")).unwrap_or(-1) == 0;
    if !code_ok {
        return None;
    }
    let lyric = body.get("lyric").and_then(|v| v.as_str()).map(b64_utf8).unwrap_or_default();
    let translated = body.get("trans").and_then(|v| v.as_str()).map(b64_utf8).unwrap_or_default();
    accept_lyric(&merge_bilingual(&lyric, &translated))
}

fn fetch_kg(info: &Value, duration_sec: i64) -> Option<String> {
    let hash = info.get("hash").or_else(|| info.get("songmid")).and_then(json_string).unwrap_or_default();
    let name = info
        .get("name")
        .or_else(|| info.get("songname"))
        .or_else(|| info.get("title"))
        .and_then(json_string)
        .unwrap_or_default();
    if hash.is_empty() && name.is_empty() {
        return None;
    }
    let headers = [
        ("KG-RC", "1"),
        ("KG-THash", "expand_search_manager.cpp:852736169:451"),
        ("User-Agent", "KuGou2012-9020-ExpandSearchManager"),
    ];
    let search = format!(
        "http://lyrics.kugou.com/search?ver=1&man=yes&client=pc&keyword={}&hash={}&timelength={}&lrctxt=1",
        encode_component(if name.is_empty() { &hash } else { &name }),
        encode_component(&hash),
        lyric_time_ms(info, duration_sec)
    );
    let searched = get_json(&search, &headers)?;
    let cand = searched.pointer("/candidates/0")?;
    let id = json_string(cand.get("id")?)?;
    let accesskey = json_string(cand.get("accesskey")?)?;
    let krc = json_i64(cand.get("krctype")).unwrap_or(0) == 1 && json_i64(cand.get("contenttype")).unwrap_or(0) != 1;
    let fmt = if krc { "krc" } else { "lrc" };
    let download = format!(
        "http://lyrics.kugou.com/download?ver=1&client=pc&id={}&accesskey={}&fmt={fmt}&charset=utf8",
        encode_component(&id),
        encode_component(&accesskey)
    );
    let body = get_json(&download, &headers)?;
    let content = body.get("content").and_then(|v| v.as_str())?;
    let fmt = body.get("fmt").and_then(|v| v.as_str()).unwrap_or(fmt);
    if fmt == "krc" {
        decode_krc_base64(content)
    } else {
        accept_lyric(&b64_utf8(content))
    }
}

fn fetch_mg(info: &Value) -> Option<String> {
    let headers = [("Referer", "https://music.migu.cn/")];
    let lyric = info
        .get("lrcUrl")
        .or_else(|| info.get("lrc_url"))
        .and_then(|v| v.as_str())
        .and_then(http_url)
        .and_then(|url| get_text(&url, &headers))
        .unwrap_or_default();
    let translated = info
        .get("trcUrl")
        .or_else(|| info.get("trc_url"))
        .and_then(|v| v.as_str())
        .and_then(http_url)
        .and_then(|url| get_text(&url, &headers))
        .unwrap_or_default();
    accept_lyric(&merge_bilingual(&lyric, &translated))
}

pub(crate) fn decode_krc_base64(content: &str) -> Option<String> {
    let raw = decode_b64(content)?;
    if raw.len() <= 4 {
        return None;
    }
    let mut buf = raw[4..].to_vec();
    for (i, byte) in buf.iter_mut().enumerate() {
        *byte ^= KRC_KEY[i % KRC_KEY.len()];
    }
    let text = inflate_zlib(&buf)?;
    let text = text.replace('\r', "");
    let text = strip_krc_id(&text);
    let (text, language) = split_language(&text);
    let (roman, translated) = language_rows(&language);
    let mut lyric = Vec::new();
    let mut tlines = Vec::new();
    let mut rlines = Vec::new();
    let mut index = 0usize;
    for line in text.split('\n') {
        let Some((ms, body)) = parse_krc_line(line) else {
            continue;
        };
        let tag = ms_to_lrc_tag(ms);
        lyric.push(format!("[{tag}]{}", percent_decode(&body)));
        if let Some(row) = translated.get(index) {
            tlines.push(format!("[{tag}]{}", percent_decode(row)));
        }
        if let Some(row) = roman.get(index) {
            rlines.push(format!("[{tag}]{}", percent_decode(row)));
        }
        index += 1;
    }
    let extra = if tlines.is_empty() {
        rlines.join("\n")
    } else {
        tlines.join("\n")
    };
    accept_lyric(&merge_bilingual(&lyric.join("\n"), &extra))
}

const KRC_KEY: [u8; 16] = [
    0x40, 0x47, 0x61, 0x77, 0x5e, 0x32, 0x74, 0x47, 0x51, 0x36, 0x31, 0x2d, 0xce, 0xd2, 0x6e, 0x69,
];

fn parse_krc_line(line: &str) -> Option<(u64, String)> {
    let rest = line.trim().strip_prefix('[')?;
    let (times, body) = rest.split_once(']')?;
    let (start, _) = times.split_once(',')?;
    let ms = start.parse().ok()?;
    Some((ms, strip_krc_words(body)))
}

fn strip_krc_words(body: &str) -> String {
    let chars: Vec<char> = body.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<' {
            if let Some(end) = chars[i + 1..].iter().position(|ch| *ch == '>') {
                let inside: String = chars[i + 1..i + 1 + end].iter().collect();
                if is_krc_tag(&inside) {
                    i += end + 2;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn is_krc_tag(text: &str) -> bool {
    let parts: Vec<&str> = text.split(',').collect();
    (parts.len() == 2 || parts.len() == 3)
        && parts.iter().all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

fn ms_to_lrc_tag(ms: u64) -> String {
    let min = ms / 60_000;
    let sec = (ms % 60_000) / 1000;
    let frac = ms % 1000;
    format!("{min:02}:{sec:02}.{frac:03}")
}

fn strip_krc_id(text: &str) -> String {
    let Some(start) = text.find("[id:$") else {
        return text.to_string();
    };
    let Some(end) = text[start..].find('\n') else {
        return text.to_string();
    };
    text[start + end + 1..].to_string()
}

fn split_language(text: &str) -> (String, String) {
    let Some(start) = text.find("[language:") else {
        return (text.to_string(), String::new());
    };
    let after = &text[start + "[language:".len()..];
    let Some(end) = after.find(']') else {
        return (text.to_string(), String::new());
    };
    let token = &after[..end];
    if token.is_empty()
        || !token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '=' | '/' | '+' | '\\' | '_' | '-'))
    {
        return (text.to_string(), String::new());
    }
    let mut remove_end = start + "[language:".len() + end + 1;
    if text.as_bytes().get(remove_end) == Some(&b'\n') {
        remove_end += 1;
    }
    let mut cleaned = text.to_string();
    cleaned.replace_range(start..remove_end, "");
    (cleaned, token.to_string())
}

fn language_rows(token: &str) -> (Vec<String>, Vec<String>) {
    if token.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let Some(raw) = decode_b64(token) else {
        return (Vec::new(), Vec::new());
    };
    let Ok(text) = String::from_utf8(raw) else {
        return (Vec::new(), Vec::new());
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return (Vec::new(), Vec::new());
    };
    let mut roman = Vec::new();
    let mut translated = Vec::new();
    let Some(content) = value.get("content").and_then(|v| v.as_array()) else {
        return (roman, translated);
    };
    for item in content {
        let rows = lyric_rows(item.get("lyricContent"));
        match json_i64(item.get("type")).unwrap_or(-1) {
            0 => roman = rows,
            1 => translated = rows,
            _ => {}
        }
    }
    (roman, translated)
}

fn lyric_rows(value: Option<&Value>) -> Vec<String> {
    let Some(rows) = value.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    rows.iter()
        .map(|row| {
            row.as_array()
                .map(|parts| parts.iter().filter_map(|part| part.as_str()).collect::<String>())
                .unwrap_or_default()
        })
        .collect()
}

fn inflate_zlib(data: &[u8]) -> Option<String> {
    let mut decoder = ZlibDecoder::new(data);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match decoder.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if out.len() + n > TEXT_LIMIT {
                    return None;
                }
                out.extend_from_slice(&buf[..n]);
            }
            Err(_) => return None,
        }
    }
    String::from_utf8(out).ok()
}

fn get_json(url: &str, headers: &[(&str, &str)]) -> Option<Value> {
    let bytes = get_bytes(url, headers)?;
    parse_json_loose(&bytes)
}

fn get_text(url: &str, headers: &[(&str, &str)]) -> Option<String> {
    let bytes = get_bytes(url, headers)?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn get_bytes(url: &str, headers: &[(&str, &str)]) -> Option<Vec<u8>> {
    browser_bytes("GET", url, headers, None, LYRIC_WAIT, TEXT_LIMIT).ok()
}

fn parse_json_loose(bytes: &[u8]) -> Option<Value> {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim().trim_start_matches('\u{feff}');
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return Some(value);
    }
    let start = text.find('(')?;
    let end = text.rfind(')')?;
    if end <= start {
        return None;
    }
    serde_json::from_str(&text[start + 1..end]).ok()
}

fn info_id(info: &Value) -> Option<String> {
    ["songmid", "hash", "id", "rid", "songId"]
        .iter()
        .find_map(|key| info.get(*key).and_then(json_string))
        .filter(|id| !id.is_empty())
}

fn json_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            }
        }
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn json_i64(value: Option<&Value>) -> Option<i64> {
    match value {
        Some(Value::Number(number)) => number.as_i64().or_else(|| number.as_f64().map(|n| n as i64)),
        Some(Value::String(text)) => text.trim().parse().ok(),
        _ => None,
    }
}

fn json_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn lyric_time_ms(info: &Value, duration_sec: i64) -> u64 {
    let raw = info.get("interval").or_else(|| info.get("duration")).or_else(|| info.get("time"));
    if let Some(ms) = raw.and_then(parse_interval_ms) {
        return ms;
    }
    if duration_sec > 0 {
        (duration_sec as u64).saturating_mul(1000)
    } else {
        0
    }
}

fn parse_interval_ms(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => {
            let n = number.as_f64()?;
            if !n.is_finite() || n <= 0.0 {
                return None;
            }
            Some(if n > 10_000.0 { n.round() as u64 } else { (n * 1000.0).round() as u64 })
        }
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            if let Some(ms) = parse_clock(text) {
                return Some(ms);
            }
            let n: f64 = text.parse().ok()?;
            if !n.is_finite() || n <= 0.0 {
                return None;
            }
            Some(if n > 10_000.0 { n.round() as u64 } else { (n * 1000.0).round() as u64 })
        }
        _ => None,
    }
}

fn parse_clock(text: &str) -> Option<u64> {
    let (min, sec) = text.split_once(':')?;
    if sec.contains(':') {
        return None;
    }
    let min: u64 = min.parse().ok()?;
    let sec: u64 = sec.parse().ok()?;
    if sec >= 60 {
        return None;
    }
    Some((min * 60 + sec) * 1000)
}

fn b64_utf8(raw: &str) -> String {
    decode_b64(raw)
        .map(|bytes| decode_entities(&String::from_utf8_lossy(&bytes)))
        .unwrap_or_default()
}

fn decode_b64(raw: &str) -> Option<Vec<u8>> {
    let cleaned: String = raw.chars().filter(|ch| !ch.is_whitespace()).collect();
    if cleaned.is_empty() {
        return None;
    }
    let standard = base64::engine::general_purpose::STANDARD;
    let unpadded = base64::engine::general_purpose::STANDARD_NO_PAD;
    standard.decode(&cleaned).or_else(|_| unpadded.decode(&cleaned)).ok()
}

fn decode_entities(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        if let Some(end) = rest.find(';') {
            let token = &rest[..=end];
            if let Some(ch) = entity_char(token) {
                out.push(ch);
                rest = &rest[end + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}

fn entity_char(token: &str) -> Option<char> {
    match token {
        "&apos;" => Some('\''),
        "&quot;" => Some('"'),
        "&amp;" => Some('&'),
        "&lt;" => Some('<'),
        "&gt;" => Some('>'),
        _ => {
            let num = token.strip_prefix("&#")?.strip_suffix(';')?;
            let code = if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
                u32::from_str_radix(hex, 16).ok()?
            } else {
                num.parse().ok()?
            };
            char::from_u32(code)
        }
    }
}

fn percent_decode(raw: &str) -> String {
    let encoded = raw.replace('+', "%20");
    let bytes = encoded.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(value) = u8::from_str_radix(hex, 16) {
                    out.push(value);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| raw.to_string())
}

fn encode_component(raw: &str) -> String {
    let mut out = String::new();
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'('
            | b')' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn platform_referer(platform: &str) -> Option<&'static str> {
    match platform {
        "wy" => Some("https://music.163.com/"),
        "kw" => Some("https://www.kuwo.cn/"),
        "kg" => Some("https://www.kugou.com/"),
        "tx" => Some("https://y.qq.com/"),
        "mg" => Some("https://music.migu.cn/"),
        _ => None,
    }
}

fn image_ext(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use serde_json::json;
    use std::io::Write;

    #[test]
    fn merges_translation_and_rejects_a_bare_url() {
        let merged = merge_bilingual("[00:01.00]晴天\n[00:02.00]故事", "[00:01.00]Sunny\n[00:03.00]extra");
        assert_eq!(merged, "[00:01.00]晴天\n[00:01.00]Sunny\n[00:02.00]故事\n[00:03.00]extra");
        assert!(lyric_from_script(true, "s:http://127.0.0.1/undefined").is_none());
        let payload = r#"j:{"lyric":"[00:01.00]晴天","tlyric":"[00:01.00]Sunny","rlyric":null,"lxlyric":null}"#;
        assert_eq!(
            lyric_from_script(true, payload).as_deref(),
            Some("[00:01.00]晴天\n[00:01.00]Sunny")
        );
        assert_eq!(cover_from_script(true, "s:http://img.example/a.jpg").as_deref(), Some("http://img.example/a.jpg"));
    }

    #[test]
    fn parses_platform_lyric_bodies() {
        let wy = json!({"lrc": {"lyric": "[00:01.00]晴天"}, "tlyric": {"lyric": "[00:01.00]Sunny"}});
        assert_eq!(lyric_from_wy_body(&wy).as_deref(), Some("[00:01.00]晴天\n[00:01.00]Sunny"));

        let kw = vec![
            json!({"time": "1.5", "lineLyric": "晴天"}),
            json!({"time": "1.5", "lineLyric": "Sunny"}),
            json!({"time": "3", "lineLyric": "下"}),
        ];
        assert_eq!(
            lyric_from_kw_list(&kw).as_deref(),
            Some("[00:01.50]晴天\n[00:01.50]Sunny\n[00:03.00]下")
        );

        let lyric = base64::engine::general_purpose::STANDARD.encode("[00:01.00]晴天&#20320;");
        let trans = base64::engine::general_purpose::STANDARD.encode("[00:01.00]Sunny");
        let tx = json!({"code": 0, "lyric": lyric, "trans": trans});
        let text = lyric_from_tx_legacy(&tx).unwrap();
        assert!(text.contains("[00:01.00]晴天你"));
        assert!(text.contains("[00:01.00]Sunny"));
    }

    #[test]
    fn decodes_krc_into_lrc() {
        let language = base64::engine::general_purpose::STANDARD.encode(
            r#"{"content":[{"type":1,"lyricContent":[["你好"]]}]}"#,
        );
        let plain = format!("[id:$0001]\n[language:{language}]\n[1000,500]<0,100,0>hel<100,100,0>lo\n");
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(plain.as_bytes()).unwrap();
        let mut compressed = encoder.finish().unwrap();
        for (i, byte) in compressed.iter_mut().enumerate() {
            *byte ^= KRC_KEY[i % KRC_KEY.len()];
        }
        let mut raw = vec![0, 0, 0, 0];
        raw.extend(compressed);
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        assert_eq!(
            decode_krc_base64(&encoded).as_deref(),
            Some("[00:01.000]hello\n[00:01.000]你好")
        );
    }
}
