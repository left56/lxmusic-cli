//! 用搜索得到的短 id 下载。只使用已启用且不是 `dead` 的音源。
//! 音质从高往低试。试听直接丢掉；声称和文件对不上时只留一个兜底。
//! 下载失败不改音源状态。音频成功后默认再写歌词、内嵌标签和封面。

use crate::audio::{
    estimate_kbps, is_preview_clip, lossless_claim_mismatch, qualities_to_try, quality_from_bitrate,
    quality_plausible, sniff_audio_ext, QUALITY_LADDER,
};
use crate::companion::{attach, Attached};
use crate::host::{browser_download, SourceSession, AUDIO_LIMIT};
use crate::paths::expand_user_path;
use crate::search::platform_label;
use crate::store::{Hit, SourceRow, Store};
use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct DownloadOpts {
    pub out: Option<String>,
    pub quality: String,
    pub progress: bool,
    /// 跟音频一起写 `.lrc`、标准内嵌标签和封面。命令行默认开。
    pub companions: bool,
    /// 音源没有歌词时再请求平台公开接口。测试里关掉，避免打到外网。
    pub native_lyric: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadReport {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub platform: String,
    pub path: String,
    pub bytes: u64,
    pub quality: String,
    pub claimed_quality: String,
    pub ext: String,
    pub source: String,
    pub fallback: bool,
    pub note: String,
    /// 歌名、歌手、专辑和封面已写进音频文件的标准标签。
    pub tagged: bool,
    pub info_note: String,
    pub lyric_path: Option<String>,
    pub cover_path: Option<String>,
    pub lyric_note: String,
    pub cover_note: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JudgementKind {
    Reject,
    Fallback,
    Accept,
}

#[derive(Debug, Clone)]
pub(crate) struct Judgement {
    pub kind: JudgementKind,
    pub quality: String,
    pub ext: String,
    pub kbps: Option<f64>,
    pub note: String,
}

pub fn normalize_quality(raw: &str) -> Result<String> {
    let quality = raw.trim();
    if quality.is_empty() || quality == "highest" {
        return Ok("highest".into());
    }
    if QUALITY_LADDER.contains(&quality) {
        return Ok(quality.to_string());
    }
    bail!("不支持的音质 {quality}。可选：highest、flac24bit、flac、320k、192k、128k")
}

pub fn download(store: &Store, hit_id: &str, opts: &DownloadOpts) -> Result<DownloadReport> {
    let hit_id = hit_id.trim();
    if hit_id.is_empty() {
        bail!("请提供搜索结果里的 id");
    }
    let hit = store.get_hit(hit_id)?.ok_or_else(|| {
        anyhow!("找不到曲目 {hit_id}。请先搜索，再用结果里的 id 下载。记录最多保留 2000 条")
    })?;
    let out_dir = output_dir(store, opts.out.as_deref())?;
    let preferred = normalize_quality(&opts.quality)?;
    let sources = selectable_sources(store, &hit.platform)?;
    let duration = duration_secs(&hit);
    let mut errors = Vec::new();
    let mut fallback: Option<Kept> = None;

    for row in sources {
        if opts.progress {
            eprintln!("音源 {}…", row.name);
        }
        let script = match store.read_script(&row) {
            Ok(script) => script,
            Err(err) => {
                errors.push(format!("{}: {err}", row.name));
                continue;
            }
        };
        let meta = row.script_meta(&script);
        let session = match SourceSession::open(&script, &meta) {
            Ok(session) => session,
            Err(err) => {
                errors.push(format!("{}: {err}", row.name));
                continue;
            }
        };
        let info = match session.init() {
            Ok(info) => info,
            Err(err) => {
                errors.push(format!("{}: {err}", row.name));
                continue;
            }
        };
        if let Err(err) = store.save_runtime_meta(row.id, &info.platforms, &info.quality_map) {
            errors.push(format!("{}: {err}", row.name));
            continue;
        }
        if !info.platforms.is_empty() && !info.platforms.iter().any(|p| p == &hit.platform) {
            errors.push(format!(
                "{}: 不支持 {}",
                row.name,
                platform_label(&hit.platform)
            ));
            continue;
        }
        let available = info
            .quality_map
            .get(&hit.platform)
            .cloned()
            .unwrap_or_default();
        let qualities = qualities_to_try(
            &preferred,
            if available.is_empty() {
                None
            } else {
                Some(available.as_slice())
            },
        );
        for quality in qualities {
            if opts.progress {
                eprintln!("  尝试 {quality}");
            }
            let url = match session.music_url(&hit.platform, &quality, &hit.music_info) {
                Ok(url) => url,
                Err(err) => {
                    errors.push(format!("{}@{quality}: {err}", row.name));
                    continue;
                }
            };
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                errors.push(format!("{}@{quality}: 返回的不是 http(s) 地址", row.name));
                continue;
            }
            let temp = scratch_path(&out_dir);
            if let Err(err) = browser_download(&url, &temp, Duration::from_secs(60), AUDIO_LIMIT) {
                discard(&temp);
                errors.push(format!("{}@{quality}: {err}", row.name));
                continue;
            }
            let bytes = std::fs::metadata(&temp).map(|m| m.len()).unwrap_or(0);
            let head = match read_prefix(&temp) {
                Ok(head) => head,
                Err(err) => {
                    discard(&temp);
                    errors.push(format!("{}@{quality}: {err}", row.name));
                    continue;
                }
            };
            let judged = judge_audio(&head, bytes, duration, &quality);
            match judged.kind {
                JudgementKind::Reject => {
                    discard(&temp);
                    errors.push(format!("{}@{quality}: {}", row.name, judged.note));
                    if opts.progress {
                        eprintln!("  跳过：{}", judged.note);
                    }
                }
                JudgementKind::Fallback => {
                    let replace = fallback
                        .as_ref()
                        .is_none_or(|kept| better_kbps(judged.kbps, kept.kbps));
                    if replace {
                        if let Some(old) = fallback.take() {
                            discard(&old.temp);
                        }
                        if opts.progress {
                            eprintln!("  留作兜底：{}", judged.note);
                        }
                        fallback = Some(Kept {
                            temp,
                            source: row.name.clone(),
                            claimed: quality,
                            quality: judged.quality,
                            ext: judged.ext,
                            bytes,
                            kbps: judged.kbps,
                            note: judged.note,
                        });
                    } else {
                        discard(&temp);
                    }
                }
                JudgementKind::Accept => {
                    if let Some(old) = fallback.take() {
                        discard(&old.temp);
                    }
                    return finish(
                        &hit,
                        &out_dir,
                        &row.name,
                        &quality,
                        &judged,
                        bytes,
                        &temp,
                        false,
                        opts,
                        Some(&session),
                    );
                }
            }
        }
    }

    if let Some(kept) = fallback {
        let judged = Judgement {
            kind: JudgementKind::Fallback,
            quality: kept.quality,
            ext: kept.ext,
            kbps: kept.kbps,
            note: kept.note.clone(),
        };
        let reopened = if opts.companions {
            reopen_session(store, &kept.source)
        } else {
            None
        };
        return finish(
            &hit,
            &out_dir,
            &kept.source,
            &kept.claimed,
            &judged,
            kept.bytes,
            &kept.temp,
            true,
            opts,
            reopened.as_ref(),
        );
    }
    let detail = if errors.is_empty() {
        "没有下载到音频".to_string()
    } else {
        errors.into_iter().take(12).collect::<Vec<_>>().join(" | ")
    };
    bail!("下载失败：{detail}")
}

struct Kept {
    temp: PathBuf,
    source: String,
    claimed: String,
    quality: String,
    ext: String,
    bytes: u64,
    kbps: Option<f64>,
    note: String,
}

fn finish(
    hit: &Hit,
    out_dir: &Path,
    source: &str,
    claimed: &str,
    judged: &Judgement,
    bytes: u64,
    temp: &Path,
    fallback: bool,
    opts: &DownloadOpts,
    session: Option<&SourceSession>,
) -> Result<DownloadReport> {
    let dest = allocate_path(out_dir, &file_stem(&hit.artist, &hit.title), &judged.ext);
    if let Err(err) = commit_file(temp, &dest) {
        discard(temp);
        return Err(err);
    }
    let attached = if opts.companions {
        attach(session, hit, &dest, opts.native_lyric, opts.progress)
    } else {
        Attached::default()
    };
    let bytes = std::fs::metadata(&dest).map(|meta| meta.len()).unwrap_or(bytes);
    Ok(DownloadReport {
        id: hit.id.clone(),
        title: hit.title.clone(),
        artist: hit.artist.clone(),
        album: hit.album.clone(),
        platform: hit.platform.clone(),
        path: dest.display().to_string(),
        bytes,
        quality: judged.quality.clone(),
        claimed_quality: claimed.to_string(),
        ext: judged.ext.clone(),
        source: source.to_string(),
        fallback,
        note: judged.note.clone(),
        tagged: attached.tagged,
        info_note: attached.info_note,
        lyric_path: attached.lyric_path,
        cover_path: attached.cover_path,
        lyric_note: attached.lyric_note,
        cover_note: attached.cover_note,
    })
}

pub(crate) fn output_dir(store: &Store, out: Option<&str>) -> Result<PathBuf> {
    if let Some(out) = out {
        let out = out.trim();
        if out.is_empty() {
            bail!("--out 不能是空路径");
        }
        return prepare_dir(out);
    }
    match store.get_setting("download-dir")? {
        Some(value) if !value.trim().is_empty() => prepare_dir(&value),
        _ => bail!(
            "还没有设置下载目录。请先运行 lxmusic-cli config set download-dir <目录>，或本次加上 --out <目录>"
        ),
    }
}

fn prepare_dir(raw: &str) -> Result<PathBuf> {
    let path = expand_user_path(raw);
    if path.as_os_str().is_empty() {
        bail!("目录不能为空");
    }
    if path.is_file() {
        bail!("{} 是文件，下载目录必须是文件夹", path.display());
    }
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

fn reopen_session(store: &Store, name: &str) -> Option<SourceSession> {
    let row = store.get_source_by_name(name).ok()??;
    let script = store.read_script(&row).ok()?;
    let meta = row.script_meta(&script);
    let session = SourceSession::open(&script, &meta).ok()?;
    session.init().ok()?;
    Some(session)
}

fn selectable_sources(store: &Store, platform: &str) -> Result<Vec<SourceRow>> {
    let all = store.list_sources()?;
    let usable: Vec<_> = all.into_iter().filter(|row| row.usable()).collect();
    if usable.is_empty() {
        bail!(
            "没有可用音源。请先导入脚本。unknown 可以直接用；dead 要重新探测或换脚本。下载不会把音源标成 dead"
        );
    }
    let matched: Vec<_> = usable
        .into_iter()
        .filter(|row| source_supports(row, platform))
        .collect();
    if matched.is_empty() {
        bail!("没有可用音源声明支持 {}", platform_label(platform));
    }
    Ok(matched)
}

pub(crate) fn source_supports(row: &SourceRow, platform: &str) -> bool {
    if !row.usable() {
        return false;
    }
    let platforms = row.platforms();
    platforms.is_empty() || platforms.iter().any(|item| item == platform)
}

pub(crate) fn judge_audio(prefix: &[u8], bytes: u64, duration_sec: Option<f64>, claimed: &str) -> Judgement {
    let Some(sniffed) = sniff_audio_ext(prefix) else {
        return Judgement {
            kind: JudgementKind::Reject,
            quality: claimed.to_string(),
            ext: String::new(),
            kbps: duration_sec.and_then(|dur| estimate_kbps(bytes, dur)),
            note: "下载内容不是音频".into(),
        };
    };
    let kbps = duration_sec.and_then(|dur| estimate_kbps(bytes, dur));
    if is_preview_clip(bytes, duration_sec) {
        let note = match kbps {
            Some(value) => format!("试听片段（约 {:.0}kbps，低于 80kbps）", value),
            None => "试听片段".into(),
        };
        return Judgement {
            kind: JudgementKind::Reject,
            quality: claimed.to_string(),
            ext: sniffed.to_string(),
            kbps,
            note,
        };
    }
    let mismatch = lossless_claim_mismatch(claimed, Some(sniffed));
    let plausible = quality_plausible(bytes, duration_sec, claimed);
    if mismatch || !plausible {
        let mut parts = Vec::new();
        if mismatch {
            parts.push(format!("声称 {claimed}，实际是 {sniffed}"));
        }
        if !plausible {
            if let Some(value) = kbps {
                parts.push(format!("约 {:.0}kbps，低于声称的 {claimed}", value));
            }
        }
        return Judgement {
            kind: JudgementKind::Fallback,
            quality: kbps
                .map(quality_from_bitrate)
                .unwrap_or(claimed)
                .to_string(),
            ext: sniffed.to_string(),
            kbps,
            note: parts.join("；"),
        };
    }
    Judgement {
        kind: JudgementKind::Accept,
        quality: claimed.to_string(),
        ext: sniffed.to_string(),
        kbps,
        note: String::new(),
    }
}

pub(crate) fn file_stem(artist: &str, title: &str) -> String {
    let title = sanitize_component(title);
    let artist = sanitize_component(artist);
    let title = if title.is_empty() {
        "未知".into()
    } else {
        title
    };
    if artist.is_empty() || artist == "未知" {
        title
    } else {
        format!("{artist} - {title}")
    }
}

fn sanitize_component(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        match ch {
            '/' | '\\' => out.push('／'),
            ':' | '*' | '?' | '"' | '<' | '>' | '|' => out.push('_'),
            c if c.is_control() => {}
            c => out.push(c),
        }
        if out.chars().count() >= 80 {
            break;
        }
    }
    out.trim().trim_end_matches(['.', ' ']).trim().to_string()
}

pub(crate) fn allocate_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let ext = ext.trim_start_matches('.');
    let mut path = dir.join(format!("{stem}.{ext}"));
    let mut n = 2u32;
    while path.exists() {
        path = dir.join(format!("{stem} ({n}).{ext}"));
        n += 1;
        if n > 1000 {
            break;
        }
    }
    path
}

fn better_kbps(new_kbps: Option<f64>, old_kbps: Option<f64>) -> bool {
    match (new_kbps, old_kbps) {
        (Some(new), Some(old)) => new > old,
        (Some(_), None) => true,
        _ => false,
    }
}

fn duration_secs(hit: &Hit) -> Option<f64> {
    if hit.duration > 0 {
        Some(hit.duration as f64)
    } else {
        None
    }
}

fn scratch_path(dir: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".lxmusic-cli-{}-{n}.part", std::process::id()))
}

fn read_prefix(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = [0u8; 32];
    let n = file.read(&mut buf)?;
    Ok(buf[..n].to_vec())
}

fn commit_file(temp: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    if std::fs::rename(temp, dest).is_err() {
        std::fs::copy(temp, dest)?;
        discard(temp);
    }
    Ok(())
}

fn discard(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{content_hash, NewSource};
    use lofty::file::TaggedFileExt;
    use lofty::tag::Accessor;
    use serde_json::json;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        (dir, store)
    }

    fn insert(store: &Store, name: &str, body: &str) -> SourceRow {
        let script = format!("/**\n * @name {name}\n * @version 1\n */\n{body}\n");
        store
            .insert_source(&NewSource {
                name: name.into(),
                version: "1".into(),
                author: String::new(),
                description: String::new(),
                homepage: String::new(),
                origin: "test".into(),
                content_hash: content_hash(script.as_bytes()),
                script,
                enabled: true,
            })
            .unwrap();
        store.get_source_by_name(name).unwrap().unwrap()
    }

    fn remember(store: &Store, duration: i64) -> String {
        let hit = Hit {
            id: "abcd1234".into(),
            platform: "wy".into(),
            external_id: "1".into(),
            title: "晴天".into(),
            artist: "周杰伦".into(),
            album: "叶惠美".into(),
            duration,
            qualitys: vec!["flac".into(), "128k".into()],
            music_info: json!({"name": "晴天", "singer": "周杰伦", "songmid": "1", "source": "wy"}),
        };
        store.upsert_hit(&hit).unwrap();
        hit.id
    }

    fn opts(out: &Path) -> DownloadOpts {
        DownloadOpts {
            out: Some(out.display().to_string()),
            quality: "highest".into(),
            progress: false,
            companions: false,
            native_lyric: false,
        }
    }

    #[test]
    fn names_do_not_overwrite_and_quality_judgement() {
        assert_eq!(file_stem("周杰伦", "晴天"), "周杰伦 - 晴天");
        assert_eq!(file_stem("A/B", "x:y"), "A／B - x_y");
        assert_eq!(file_stem("未知", "只剩歌名"), "只剩歌名");
        let dir = tempfile::tempdir().unwrap();
        let first = allocate_path(dir.path(), "周杰伦 - 晴天", "flac");
        std::fs::write(&first, b"1").unwrap();
        let second = allocate_path(dir.path(), "周杰伦 - 晴天", "flac");
        assert!(second.ends_with("周杰伦 - 晴天 (2).flac"));

        let preview = judge_audio(b"ID3xxxx", 1000, Some(200.0), "flac");
        assert_eq!(preview.kind, JudgementKind::Reject);
        assert!(preview.note.contains("试听"));

        let lied = judge_audio(&padded(b"ID3", 64), 500_000, None, "flac24bit");
        assert_eq!(lied.kind, JudgementKind::Fallback);
        assert_eq!(lied.ext, "mp3");

        let low = judge_audio(&padded(b"fLaC", 64), 500_000, Some(10.0), "flac");
        assert_eq!(low.kind, JudgementKind::Fallback);
        assert_eq!(low.quality, "320k");
        assert_eq!(low.ext, "flac");

        let unknown = judge_audio(b"fLaCxxxx", 1000, None, "flac24bit");
        assert_eq!(unknown.kind, JudgementKind::Accept);
    }

    #[test]
    fn missing_download_dir_and_failed_script_do_not_change_state() {
        let (dir, store) = temp_store();
        let id = remember(&store, 10);
        let err = download(
            &store,
            &id,
            &DownloadOpts {
                out: None,
                quality: "highest".into(),
                progress: false,
                companions: false,
                native_lyric: false,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("下载目录"));
        assert!(store.get_setting("download-dir").unwrap().is_none());

        let out = dir.path().join("once");
        let kept = dir.path().join("default-dir");
        store
            .set_setting("download-dir", kept.to_str().unwrap())
            .unwrap();
        let resolved = output_dir(&store, Some(out.to_str().unwrap())).unwrap();
        assert_eq!(resolved, out);
        assert_eq!(
            store.get_setting("download-dir").unwrap().as_deref(),
            Some(kept.to_str().unwrap())
        );

        insert(&store, "坏了", "throw new Error('boom')");
        let err = download(&store, &id, &opts(&out)).unwrap_err();
        assert!(err.to_string().contains("boom") || err.to_string().contains("下载失败"));
        let row = store.get_source_by_name("坏了").unwrap().unwrap();
        assert_eq!(row.status, "unknown");
        assert!(store.root().join(&row.script_rel).is_file());
        store.mark_status(row.id, "dead", "探针失败").unwrap();
        let err = download(&store, &id, &opts(&out)).unwrap_err();
        assert!(err.to_string().contains("没有可用音源"));
        assert_eq!(store.get_source_by_name("坏了").unwrap().unwrap().status, "dead");
    }

    #[test]
    fn skips_other_platforms_without_marking_dead() {
        let (dir, store) = temp_store();
        let id = remember(&store, 10);
        insert(
            &store,
            "只要酷我",
            r#"
            lx.on('request', function () { return 'http://127.0.0.1/no' })
            lx.send('inited', { sources: { kw: { qualitys: ['128k'] } } })
            "#,
        );
        let err = download(&store, &id, &opts(&dir.path().join("out"))).unwrap_err();
        assert!(err.to_string().contains("不支持"));
        let row = store.get_source_by_name("只要酷我").unwrap().unwrap();
        assert_eq!(row.status, "unknown");
        assert_eq!(row.platforms(), vec!["kw".to_string()]);
    }

    #[test]
    fn prefers_a_real_file_and_can_keep_one_fallback() {
        let (dir, store) = temp_store();
        let out = dir.path().join("music");
        let server = Server::start(HashMap::from([
            ("/flac".into(), padded(b"ID3", 500_000)),
            ("/128k".into(), padded(b"fLaC", 160_000)),
        ]));
        insert(
            &store,
            "本地",
            &url_script(server.port, "['flac', '128k']"),
        );
        let id = remember(&store, 10);
        let report = download(&store, &id, &opts(&out)).unwrap();
        assert!(!report.fallback);
        assert_eq!(report.quality, "128k");
        assert_eq!(report.ext, "flac");
        assert!(std::fs::read(&report.path).unwrap().starts_with(b"fLaC"));
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 1);
        assert!(store.get_setting("download-dir").unwrap().is_none());
        assert_eq!(store.get_source_by_name("本地").unwrap().unwrap().status, "unknown");
        drop(server);

        let (dir, store) = temp_store();
        let out = dir.path().join("music");
        let server = Server::start(HashMap::from([("/flac".into(), padded(b"ID3", 500_000))]));
        insert(&store, "谎报", &url_script(server.port, "['flac']"));
        let id = remember(&store, 10);
        let report = download(&store, &id, &opts(&out)).unwrap();
        assert!(report.fallback);
        assert_eq!(report.claimed_quality, "flac");
        assert_eq!(report.quality, "320k");
        assert_eq!(report.ext, "mp3");
        assert!(report.note.contains("实际是 mp3"));
        assert!(std::fs::read(&report.path).unwrap().starts_with(b"ID3"));
        drop(server);

        let (dir, store) = temp_store();
        let out = dir.path().join("music");
        let server = Server::start(HashMap::from([("/128k".into(), padded(b"ID3", 1000))]));
        insert(&store, "试听", &url_script(server.port, "['128k']"));
        let id = remember(&store, 200);
        let err = download(&store, &id, &opts(&out)).unwrap_err();
        assert!(err.to_string().contains("试听"));
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0);
        assert_eq!(store.get_source_by_name("试听").unwrap().unwrap().status, "unknown");
        drop(server);
    }

    #[test]
    fn saves_lyric_song_info_and_cover_beside_the_audio() {
        let (dir, store) = temp_store();
        let out = dir.path().join("music");
        let server = Server::start(HashMap::from([
            ("/128k".into(), mp3_body(160_000)),
            ("/cover.jpg".into(), vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0]),
        ]));
        insert(
            &store,
            "本地歌词",
            &format!(
                r#"
                lx.on('request', function (req) {{
                  if (req.action === 'lyric') {{
                    return {{
                      lyric: '[00:01.00]晴天\n[00:05.00]故事的小黄花',
                      tlyric: '[00:01.00]Sunny',
                      rlyric: null,
                      lxlyric: null
                    }}
                  }}
                  if (req.action === 'pic') return 'http://127.0.0.1:{port}/cover.jpg'
                  return 'http://127.0.0.1:{port}/' + req.info.type
                }})
                lx.send('inited', {{ sources: {{ wy: {{ qualitys: ['128k'] }} }} }})
                "#,
                port = server.port
            ),
        );
        let id = remember(&store, 10);
        let report = download(&store, &id, &companion_opts(&out)).unwrap();
        assert!(!report.fallback);
        assert!(report.lyric_note.is_empty());
        assert!(report.cover_note.is_empty());
        let lyric = std::fs::read_to_string(report.lyric_path.as_ref().unwrap()).unwrap();
        assert_eq!(lyric, "[00:01.00]晴天\n[00:01.00]Sunny\n[00:05.00]故事的小黄花\n");
        assert!(report.tagged);
        assert!(report.info_note.is_empty());
        let tagged = lofty::read_from_path(&report.path).unwrap();
        let tag = tagged.primary_tag().unwrap();
        assert_eq!(tag.title().as_deref(), Some("晴天"));
        assert_eq!(tag.artist().as_deref(), Some("周杰伦"));
        assert_eq!(tag.album().as_deref(), Some("叶惠美"));
        assert!(tag.pictures()[0].data().starts_with(&[0xFF, 0xD8, 0xFF]));
        assert!(std::fs::read(report.cover_path.as_ref().unwrap()).unwrap().starts_with(&[0xFF, 0xD8, 0xFF]));
        assert_eq!(
            std::path::Path::new(report.lyric_path.as_ref().unwrap()).file_stem(),
            std::path::Path::new(&report.path).file_stem()
        );
        assert!(std::fs::read_dir(&out).unwrap().all(|entry| {
            entry.unwrap().path().extension().and_then(|ext| ext.to_str()) != Some("json")
        }));
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 3);
        assert_eq!(store.get_source_by_name("本地歌词").unwrap().unwrap().status, "unknown");
        drop(server);
    }

    #[test]
    fn keeps_the_audio_when_lyric_and_cover_are_missing() {
        let (dir, store) = temp_store();
        let out = dir.path().join("music");
        let server = Server::start(HashMap::from([("/128k".into(), mp3_body(160_000))]));
        insert(
            &store,
            "没有歌词",
            &format!(
                r#"
                lx.on('request', function (req) {{
                  if (req.action !== 'musicUrl') throw new Error('no lyric')
                  return 'http://127.0.0.1:{port}/' + req.info.type
                }})
                lx.send('inited', {{ sources: {{ wy: {{ qualitys: ['128k'] }} }} }})
                "#,
                port = server.port
            ),
        );
        let id = remember(&store, 10);
        let mut hit = store.get_hit(&id).unwrap().unwrap();
        hit.external_id.clear();
        hit.music_info = json!({"name": "晴天", "singer": "周杰伦", "source": "wy"});
        store.upsert_hit(&hit).unwrap();
        let report = download(&store, &id, &companion_opts(&out)).unwrap();
        assert!(report.path.ends_with(".mp3"));
        assert!(report.lyric_path.is_none());
        assert!(report.lyric_note.starts_with("未获取"));
        assert!(report.cover_path.is_none());
        assert!(report.cover_note.is_empty());
        assert!(report.tagged);
        assert!(report.info_note.is_empty());
        let tagged = lofty::read_from_path(&report.path).unwrap();
        let tag = tagged.primary_tag().unwrap();
        assert_eq!(tag.title().as_deref(), Some("晴天"));
        assert_eq!(tag.artist().as_deref(), Some("周杰伦"));
        assert_eq!(tag.album().as_deref(), Some("叶惠美"));
        assert!(tag.pictures().is_empty());
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 1);
        assert_eq!(store.get_source_by_name("没有歌词").unwrap().unwrap().status, "unknown");
        drop(server);
    }

    fn companion_opts(out: &Path) -> DownloadOpts {
        DownloadOpts {
            out: Some(out.display().to_string()),
            quality: "highest".into(),
            progress: false,
            companions: true,
            native_lyric: false,
        }
    }

    fn url_script(port: u16, qualities: &str) -> String {
        format!(
            r#"
            lx.on('request', function (req) {{
              return 'http://127.0.0.1:{port}/' + req.info.type
            }})
            lx.send('inited', {{ sources: {{ wy: {{ qualitys: {qualities} }} }} }})
            "#
        )
    }

    fn mp3_body(min_len: usize) -> Vec<u8> {
        // MPEG-1 Layer III, 128 kbps, 44100 Hz, stereo, no padding.
        let mut frame = vec![0u8; 417];
        frame[0] = 0xFF;
        frame[1] = 0xFB;
        frame[2] = 0x90;
        frame[3] = 0x04;
        let mut body = Vec::new();
        while body.len() < min_len {
            body.extend_from_slice(&frame);
        }
        body
    }

    fn padded(magic: &[u8], len: usize) -> Vec<u8> {
        let mut body = vec![0u8; len];
        let n = magic.len().min(len);
        body[..n].copy_from_slice(&magic[..n]);
        body
    }

    struct Server {
        port: u16,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Server {
        fn start(routes: HashMap<String, Vec<u8>>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            let thread = thread::spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((sock, _)) => {
                            // accept 会继承监听套接字的非阻塞。大文件一次写不完，
                            // 客户端就会按 Content-Length 读到半截。
                            let _ = sock.set_nonblocking(false);
                            handle_conn(sock, &routes);
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(15));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                port,
                stop,
                thread: Some(thread),
            }
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn handle_conn(mut sock: TcpStream, routes: &HashMap<String, Vec<u8>>) {
        let _ = sock.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = sock.set_write_timeout(Some(Duration::from_secs(2)));
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 8192 {
                break;
            }
            match sock.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        let text = String::from_utf8_lossy(&buf);
        let path = text.split_whitespace().nth(1).unwrap_or("/");
        let path = path.split('?').next().unwrap_or(path);
        let payload = routes.get(path).cloned();
        let (status, body) = match payload {
            Some(body) => ("200 OK", body),
            None => ("404 Not Found", b"no".to_vec()),
        };
        let header = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = sock.write_all(header.as_bytes());
        let _ = sock.write_all(&body);
    }
}
