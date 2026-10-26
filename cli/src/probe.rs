//! 试取链。失败只把状态标成 `dead`，不删除脚本。
//! 曲目与觅音 `sourceProbe.ts` 的 wy / kw / kg / tx 相同。

use crate::host::SourceSession;
use crate::store::{SourceRow, Store};
use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

const ORDER: [&str; 4] = ["wy", "kw", "kg", "tx"];

struct ProbeTrack {
    platform: &'static str,
    songmid: &'static str,
    name: &'static str,
    singer: &'static str,
}

const TRACKS: &[ProbeTrack] = &[
    ProbeTrack {
        platform: "wy",
        songmid: "421423806",
        name: "小半",
        singer: "陈粒",
    },
    ProbeTrack {
        platform: "kw",
        songmid: "228908",
        name: "测试",
        singer: "测试",
    },
    ProbeTrack {
        platform: "kg",
        songmid: "EEDBDCB8E3A9453390DBF897FEBB629",
        name: "测试",
        singer: "测试",
    },
    ProbeTrack {
        platform: "tx",
        songmid: "0039MnYb0qxYhV",
        name: "晴天",
        singer: "周杰伦",
    },
];

struct Target {
    platform: &'static str,
    quality: String,
    info: Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeReport {
    pub name: String,
    pub status: String,
    pub platforms: Vec<String>,
    pub last_error: String,
    pub alerts: Vec<String>,
}

pub fn probe_source(store: &Store, row: &SourceRow) -> Result<ProbeReport> {
    let script = match store.read_script(row) {
        Ok(script) => script,
        Err(err) => {
            return mark(store, row, "dead", row.platforms(), &err.to_string(), &[]);
        }
    };
    let meta = row.script_meta(&script);
    let session = match SourceSession::open(&script, &meta) {
        Ok(session) => session,
        Err(err) => {
            let msg = classify_source_error(&err.to_string(), &[]);
            return mark(store, row, "dead", row.platforms(), &msg, &[]);
        }
    };
    let info = match session.init() {
        Ok(info) => info,
        Err(err) => {
            let msg = classify_source_error(&err.to_string(), &[]);
            return mark(store, row, "dead", row.platforms(), &msg, &[]);
        }
    };
    store.save_runtime_meta(row.id, &info.platforms, &info.quality_map)?;
    let targets = list_targets(&info.platforms, &info.quality_map);
    if targets.is_empty() {
        return mark(store, row, "ok", info.platforms, "", &info.update_alerts);
    }
    let mut errors = Vec::new();
    for target in &targets {
        match session.music_url(&target.platform, &target.quality, &target.info) {
            Ok(url) if url.starts_with("http://") || url.starts_with("https://") => {
                return mark(store, row, "ok", info.platforms, "", &info.update_alerts);
            }
            Ok(_) => errors.push(format_failure(target, "未能获取播放地址", &info.update_alerts)),
            Err(err) => errors.push(format_failure(target, &err.to_string(), &info.update_alerts)),
        }
    }
    let primary = errors
        .first()
        .cloned()
        .unwrap_or_else(|| "取链探针失败".into());
    let extra = if errors.len() > 1 {
        format!("（已尝试 {} 个平台）", errors.len())
    } else {
        String::new()
    };
    mark(
        store,
        row,
        "dead",
        info.platforms,
        &format!("{primary}{extra}"),
        &info.update_alerts,
    )
}

/// 不给名字时只探测已启用的音源，停用的留着。点名时不管启用状态。
pub fn probe_sources(store: &Store, names: &[String], progress: bool) -> Result<Vec<ProbeReport>> {
    let rows = if names.iter().all(|n| n.trim().is_empty()) {
        store
            .list_sources()?
            .into_iter()
            .filter(|row| row.enabled)
            .collect::<Vec<_>>()
    } else {
        let mut rows = Vec::new();
        for name in names {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            match store.get_source_by_name(name)? {
                Some(row) => rows.push(row),
                None => anyhow::bail!("没有名为「{name}」的音源"),
            }
        }
        rows
    };
    if rows.is_empty() {
        anyhow::bail!("没有可探测的音源");
    }
    let mut reports = Vec::new();
    for row in &rows {
        if progress {
            eprintln!("探测 {}…", row.name);
        }
        reports.push(probe_source(store, row)?);
    }
    Ok(reports)
}

fn list_targets(platforms: &[String], quality_map: &BTreeMap<String, Vec<String>>) -> Vec<Target> {
    let mut out = Vec::new();
    for platform in ORDER {
        let Some(track) = TRACKS.iter().find(|track| track.platform == platform) else {
            continue;
        };
        if !platforms.iter().any(|p| p == platform) {
            continue;
        }
        let declared = quality_map.get(platform).map(Vec::as_slice).unwrap_or(&[]);
        out.push(Target {
            platform,
            quality: pick_quality(declared),
            info: json!({
                "songmid": track.songmid,
                "hash": track.songmid,
                "name": track.name,
                "singer": track.singer,
                "source": platform,
            }),
        });
    }
    out
}

fn pick_quality(qualities: &[String]) -> String {
    if qualities.iter().any(|q| q == "128k") {
        "128k".into()
    } else if qualities.iter().any(|q| q == "320k") {
        "320k".into()
    } else if let Some(first) = qualities.first() {
        first.clone()
    } else {
        "128k".into()
    }
}

fn format_failure(target: &Target, raw: &str, alerts: &[String]) -> String {
    let detail = classify_source_error(raw, alerts);
    format!(
        "取链探针失败（{}/{}）：{detail}",
        target.platform, target.quality
    )
}

fn mark(
    store: &Store,
    row: &SourceRow,
    status: &str,
    platforms: Vec<String>,
    last_error: &str,
    alerts: &[String],
) -> Result<ProbeReport> {
    let last_error = clip(last_error);
    store.mark_status(row.id, status, &last_error)?;
    Ok(ProbeReport {
        name: row.name.clone(),
        status: status.to_string(),
        platforms,
        last_error,
        alerts: alerts.to_vec(),
    })
}

fn clip(raw: &str) -> String {
    raw.trim().chars().take(1000).collect()
}

pub(crate) fn classify_source_error(raw: &str, alerts: &[String]) -> String {
    let msg = raw.trim();
    if msg.is_empty() {
        return "未知错误".into();
    }
    let lower = msg.to_lowercase();
    let version_alert = alerts.iter().find(|a| {
        let l = a.to_lowercase();
        l.contains("版本")
            || l.contains("更新")
            || l.contains("upgrade")
            || l.contains("download")
            || l.contains("下载最新")
            || a.contains("请") && a.contains("更新")
    });
    let with_version = |base: String| match version_alert {
        Some(alert) => format!("{base}；{alert}"),
        None => base,
    };
    if lower.contains("service suspended") || lower.contains("has been suspended") {
        return "API 服务已停服（Service Suspended）".into();
    }
    if lower.contains("enotfound") || lower.contains("getaddrinfo") {
        return "API 域名无法解析（DNS 失败），请检查网络或脚本中的 API 地址".into();
    }
    if lower.contains("econnrefused") || lower.contains("connection refused") {
        return "无法连接 API 服务端（连接被拒绝）".into();
    }
    if lower.contains("etimedout")
        || lower.contains("request timeout")
        || lower.contains("socket hang up")
        || lower.contains("network error")
    {
        return "API 请求超时或网络中断，远端服务无响应".into();
    }
    if lower.contains("invalid key")
        || lower.contains("api key")
        || lower.contains("api-key")
        || lower.contains("x-request-key")
        || msg.contains("鉴权失败")
        || msg.contains("key失效")
        || (msg.contains("key") && (msg.contains("无效") || msg.contains("失效") || msg.contains("为空")))
    {
        return "API Key 无效或未配置，请在音源脚本中填写有效密钥".into();
    }
    if lower.contains("block ip") || (lower.contains("拒绝访问") && lower.contains("ip")) {
        return "API 拒绝访问（IP 被封禁）".into();
    }
    if lower.contains("too many requests")
        || msg.contains("请求过速")
        || msg.contains("请求过于频繁")
        || has_code(&lower, "429")
    {
        return "API 限流（请求过于频繁）".into();
    }
    if has_code(&lower, "404") || lower.contains("not found") || msg.contains("接口不存在") {
        return with_version("API 接口不存在（HTTP 404），脚本可能过旧或 API 地址已变更".into());
    }
    if lower.contains("internal server error") || has_code(&lower, "500") || msg.contains("服务端错误") {
        return "API 服务端错误（HTTP 500）".into();
    }
    if lower.contains("param error") || msg.contains("参数错误") || lower.contains("source not match") {
        return "API 参数错误或平台不匹配（探针曲目可能不受该源支持）".into();
    }
    if lower.contains("unknow error")
        || lower.contains("unknown error")
        || msg.contains("无法解析")
        || lower.contains("unexpected token")
        || lower.contains("<!doctype")
    {
        return with_version("API 返回异常（可能服务已停服、返回 HTML 或响应格式变更）".into());
    }
    if lower.contains("未能获取播放地址")
        || lower.contains("get music url failed")
        || lower.contains("get url failed")
        || (msg.contains("取链") && msg.contains("失败"))
    {
        return "取链失败，API 未返回有效播放地址".into();
    }
    if msg.contains("初始化超时")
        || msg.contains("未完成初始化")
        || msg.contains("未注册 musicUrl")
        || msg.contains("脚本过大")
        || msg.contains("禁止 require")
        || msg.contains("熔断")
    {
        return msg.to_string();
    }
    if msg.contains("超时") {
        return "取链探针超时，远端 API 长时间无响应".into();
    }
    if let Some(alert) = version_alert {
        if !msg.contains("版本") && !msg.contains("更新") {
            return format!("{msg}；{alert}");
        }
    }
    msg.to_string()
}

fn has_code(lower: &str, code: &str) -> bool {
    lower
        .split(|c: char| !c.is_ascii_digit())
        .any(|part| part == code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{content_hash, NewSource};

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        (dir, store)
    }

    fn insert(store: &Store, name: &str, body: &str, enabled: bool) -> SourceRow {
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
                enabled,
            })
            .unwrap();
        store.get_source_by_name(name).unwrap().unwrap()
    }

    #[test]
    fn first_platform_failure_still_ok_when_another_url_works() {
        let (_dir, store) = store();
        let row = insert(
            &store,
            "能用",
            r#"
            lx.on('request', function (req) {
              if (req.source === 'wy') throw new Error('404 not found')
              return 'https://example.com/' + req.source + '.mp3'
            })
            lx.send('updateAlert', { log: '请更新音源' })
            lx.send('inited', { sources: {
              wy: { qualitys: ['128k', '320k'] },
              kw: { qualitys: ['320k'] }
            }})
            "#,
            true,
        );
        let report = probe_source(&store, &row).unwrap();
        assert_eq!(report.status, "ok");
        assert!(report.last_error.is_empty());
        assert_eq!(report.platforms, vec!["wy".to_string(), "kw".into()]);
        assert_eq!(report.alerts, vec!["请更新音源".to_string()]);
        let saved = store.get_source_by_name("能用").unwrap().unwrap();
        assert_eq!(saved.status, "ok");
        assert!(store.root().join(&saved.script_rel).is_file());
    }

    #[test]
    fn init_or_all_url_failures_mark_dead_without_deleting() {
        let (_dir, store) = store();
        let hung = insert(&store, "没初始化", "lx.on('request', function () { return 'https://example.com/a.mp3' })", true);
        let dead = probe_source(&store, &hung).unwrap();
        assert_eq!(dead.status, "dead");
        assert!(dead.last_error.contains("初始化") || dead.last_error.contains("inited"));
        let hung = store.get_source_by_name("没初始化").unwrap().unwrap();
        assert!(store.root().join(&hung.script_rel).is_file());

        let both = insert(
            &store,
            "全失败",
            r#"
            lx.on('request', function () { throw new Error('get music url failed') })
            lx.send('inited', { sources: {
              wy: { qualitys: ['128k'] },
              tx: { qualitys: ['flac'] }
            }})
            "#,
            true,
        );
        let report = probe_source(&store, &both).unwrap();
        assert_eq!(report.status, "dead");
        assert!(report.last_error.contains("取链探针失败"));
        assert!(report.last_error.contains("已尝试 2 个平台"));
        let both = store.get_source_by_name("全失败").unwrap().unwrap();
        assert!(store.root().join(&both.script_rel).is_file());
        assert_eq!(both.platforms(), vec!["wy".to_string(), "tx".into()]);
    }

    #[test]
    fn no_known_platform_is_ok_and_disabled_sources_are_skipped() {
        let (_dir, store) = store();
        let row = insert(
            &store,
            "只有咪咕",
            r#"
            lx.on('request', function () { throw new Error('no') })
            lx.send('inited', { sources: { mg: { qualitys: ['128k'] } } })
            "#,
            true,
        );
        let report = probe_source(&store, &row).unwrap();
        assert_eq!(report.status, "ok");
        assert_eq!(report.platforms, vec!["mg".to_string()]);

        insert(
            &store,
            "停用",
            r#"
            lx.on('request', function () { return 'https://example.com/a.mp3' })
            lx.send('inited', { sources: { wy: { qualitys: ['128k'] } } })
            "#,
            false,
        );
        let reports = probe_sources(&store, &[], false).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].name, "只有咪咕");
        let named = probe_sources(&store, &["停用".into()], false).unwrap();
        assert_eq!(named.len(), 1);
        assert_eq!(named[0].name, "停用");
        assert_eq!(named[0].status, "ok");
    }

    #[test]
    fn classifies_version_alert_with_404() {
        let msg = classify_source_error("HTTP 404 Not Found", &["请更新音源".into()]);
        assert!(msg.contains("404"));
        assert!(msg.contains("请更新音源"));
    }
}
