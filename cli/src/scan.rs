//! 导入 `.js`、目录、zip 或 http(s) 地址。同名时优先 `latest.js`，否则取更高的 `@version`。

use crate::header::parse_script_header;
use crate::host::MAX_SCRIPT_BYTES;
use crate::probe;
use crate::store::{content_hash, NewSource, Store};
use crate::versioncmp::cmp_version;
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::cmp::Ordering;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;
use zip::ZipArchive;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictMode {
    Skip,
    Overwrite,
}

#[derive(Debug, Clone)]
pub struct ImportOpts {
    pub conflict: ConflictMode,
    pub probe: bool,
    pub progress: bool,
}

impl Default for ImportOpts {
    fn default() -> Self {
        Self {
            conflict: ConflictMode::Skip,
            probe: false,
            progress: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportItem {
    pub name: String,
    pub version: String,
    pub action: String,
    pub detail: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportReport {
    pub items: Vec<ImportItem>,
}

#[derive(Clone)]
struct Candidate {
    name: String,
    version: String,
    author: String,
    description: String,
    homepage: String,
    origin: String,
    hash: String,
    script: String,
    is_latest: bool,
    enabled: bool,
}

pub fn import(store: &Store, target: &str, opts: &ImportOpts) -> Result<ImportReport> {
    let target = target.trim();
    if target.is_empty() {
        bail!("请提供 .js 文件、目录、zip，或 http(s) 地址");
    }
    if target.starts_with("local://") {
        bail!("local:// 只在觅音导出包里有效，不能再当网址下载。请导入 zip 或 .js 文件");
    }
    if target.starts_with("file://") {
        bail!("请直接写本地路径，不要用 file://");
    }
    let mut found = Vec::new();
    let mut items = Vec::new();
    if target.starts_with("https://") || target.starts_with("http://") {
        found.push(fetch_url(target)?);
    } else {
        let path = PathBuf::from(target);
        if !path.exists() {
            bail!("找不到 {target}");
        }
        if path.is_dir() {
            collect_dir(&path, &mut found, &mut items)?;
        } else if is_zip(&path) {
            collect_zip(&path, &mut found, &mut items)?;
        } else if is_js(&path) {
            match load_file(&path) {
                Ok(c) => found.push(c),
                Err(item) => items.push(item),
            }
        } else {
            bail!("请提供 .js 文件、目录或 zip");
        }
    }
    if found.is_empty() && items.is_empty() {
        bail!("没有找到 .js 音源");
    }
    let (kept, skipped) = dedupe(found);
    items.extend(skipped);
    for candidate in kept {
        items.push(commit(store, candidate, opts)?);
    }
    Ok(ImportReport { items })
}

fn commit(store: &Store, candidate: Candidate, opts: &ImportOpts) -> Result<ImportItem> {
    if let Some(existing) = store.get_source_by_hash(&candidate.hash)? {
        return Ok(item(
            &candidate,
            "skipped-same",
            &format!("内容与已导入的「{}」相同", existing.name),
            &existing.status,
        ));
    }
    let src = NewSource {
        name: candidate.name.clone(),
        version: candidate.version.clone(),
        author: candidate.author.clone(),
        description: candidate.description.clone(),
        homepage: candidate.homepage.clone(),
        origin: candidate.origin.clone(),
        content_hash: candidate.hash.clone(),
        script: candidate.script.clone(),
        enabled: candidate.enabled,
    };
    let (id, action, detail) = if let Some(existing) = store.get_source_by_name(&candidate.name)? {
        match opts.conflict {
            ConflictMode::Skip => {
                return Ok(item(
                    &candidate,
                    "skipped-name",
                    &format!("已有同名音源 {}，未覆盖", existing.version),
                    &existing.status,
                ));
            }
            ConflictMode::Overwrite => {
                store.overwrite_source(existing.id, &src)?;
                (existing.id, "overwritten", "已覆盖同名音源".to_string())
            }
        }
    } else {
        let id = store.insert_source(&src)?;
        (id, "imported", "已导入".to_string())
    };
    if !opts.probe {
        return Ok(item(&candidate, action, &detail, "unknown"));
    }
    let Some(row) = store.get_source_by_name(&candidate.name)? else {
        bail!("导入后找不到「{}」", candidate.name);
    };
    if opts.progress {
        eprintln!("探测 {}…", row.name);
    }
    let probed = probe::probe_source(store, &row)?;
    let _ = id;
    Ok(item(
        &candidate,
        action,
        &detail,
        &probed.status,
    ))
}

fn dedupe(items: Vec<Candidate>) -> (Vec<Candidate>, Vec<ImportItem>) {
    let mut items = items;
    items.sort_by(|a, b| a.origin.cmp(&b.origin));
    let mut skipped = Vec::new();
    let mut by_hash: Vec<Candidate> = Vec::new();
    for item in items {
        if let Some(pos) = by_hash.iter().position(|c| c.hash == item.hash) {
            if prefer(&item, &by_hash[pos]) {
                let old = std::mem::replace(&mut by_hash[pos], item);
                skipped.push(dup_item(&old));
            } else {
                skipped.push(dup_item(&item));
            }
        } else {
            by_hash.push(item);
        }
    }
    let mut by_name: Vec<Candidate> = Vec::new();
    for item in by_hash {
        if let Some(pos) = by_name.iter().position(|c| c.name == item.name) {
            if prefer(&item, &by_name[pos]) {
                let old = std::mem::replace(&mut by_name[pos], item);
                skipped.push(dup_item(&old));
            } else {
                skipped.push(dup_item(&item));
            }
        } else {
            by_name.push(item);
        }
    }
    (by_name, skipped)
}

fn prefer(incoming: &Candidate, kept: &Candidate) -> bool {
    if incoming.is_latest != kept.is_latest {
        return incoming.is_latest;
    }
    cmp_version(&incoming.version, &kept.version) == Ordering::Greater
}

fn collect_dir(dir: &Path, found: &mut Vec<Candidate>, items: &mut Vec<ImportItem>) -> Result<()> {
    let mut files = Vec::new();
    walk_js(dir, &mut files)?;
    if files.is_empty() {
        bail!("目录里没有 .js 音源");
    }
    if files.len() > 200 {
        bail!("一次最多导入 200 个脚本");
    }
    for path in files {
        match load_file(&path) {
            Ok(c) => found.push(c),
            Err(item) => items.push(item),
        }
    }
    Ok(())
}

fn walk_js(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("无法读取 {}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            walk_js(&path, out)?;
        } else if is_js(&path) {
            out.push(path);
        }
    }
    Ok(())
}

fn collect_zip(path: &Path, found: &mut Vec<Candidate>, items: &mut Vec<ImportItem>) -> Result<()> {
    let file = File::open(path).with_context(|| format!("无法打开 {}", path.display()))?;
    let mut archive = ZipArchive::new(file).map_err(|_| anyhow!("无法解析 zip"))?;
    let mut manifests: Vec<(String, Vec<u8>)> = Vec::new();
    let mut scripts: Vec<String> = Vec::new();
    for i in 0..archive.len() {
        let entry = match archive.by_index(i) {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.is_file() {
            continue;
        }
        let Some(safe) = entry.enclosed_name() else {
            continue;
        };
        let original = entry.name().replace('\\', "/");
        let normalized = normalize_zip_path(&safe.to_string_lossy());
        if normalized.starts_with("__MACOSX/") || normalized.split('/').any(|p| p == "..") {
            continue;
        }
        if is_manifest_name(&normalized) {
            let mut buf = Vec::new();
            entry.take((64 * 1024) as u64).read_to_end(&mut buf)?;
            manifests.push((normalized, buf));
            continue;
        }
        if normalized.to_ascii_lowercase().ends_with(".js") {
            scripts.push(original);
        }
    }
    if let Some(entries) = pick_manifest(&manifests) {
        if scripts_and_manifests_too_many(entries.len()) {
            bail!("一次最多导入 200 个脚本");
        }
        for entry in entries {
            let bytes = match read_zip_file(&mut archive, &entry.script) {
                Some(bytes) => bytes,
                None => {
                    items.push(error_item(&entry.name, &format!("包内没有 {}", entry.script)));
                    continue;
                }
            };
            let file_name = file_name_of(&entry.script);
            let fallback = if entry.name.is_empty() {
                stem_of(&file_name)
            } else {
                entry.name.clone()
            };
            match bytes_to_candidate(bytes, format!("{}#{}", path.display(), entry.script), &file_name, &fallback, entry.enabled)
            {
                Ok(c) => found.push(c),
                Err(item) => items.push(item),
            }
        }
        return Ok(());
    }
    if scripts.len() > 200 {
        bail!("一次最多导入 200 个脚本");
    }
    for name in scripts {
        let Some(bytes) = read_zip_file(&mut archive, &name) else {
            items.push(error_item(&name, "无法读取"));
            continue;
        };
        let file_name = file_name_of(&name);
        let fallback = stem_of(&file_name);
        match bytes_to_candidate(bytes, format!("{}#{}", path.display(), name), &file_name, &fallback, true) {
            Ok(c) => found.push(c),
            Err(item) => items.push(item),
        }
    }
    Ok(())
}

struct ManifestEntry {
    script: String,
    name: String,
    enabled: bool,
}

fn pick_manifest(manifests: &[(String, Vec<u8>)]) -> Option<Vec<ManifestEntry>> {
    let mut ranked: Vec<&(String, Vec<u8>)> = manifests.iter().collect();
    ranked.sort_by_key(|(name, _)| {
        let rank = if name == "manifest.json" { 0 } else { 1 };
        (rank, name.len())
    });
    for (_, bytes) in ranked {
        if let Some(entries) = parse_manifest(bytes) {
            return Some(entries);
        }
    }
    None
}

fn parse_manifest(bytes: &[u8]) -> Option<Vec<ManifestEntry>> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    if value.get("version").filter(|v| !v.is_null()).is_none() {
        return None;
    }
    let sources = value.get("sources")?.as_array()?;
    let mut out = Vec::new();
    for source in sources {
        let script = source.get("script").and_then(|v| v.as_str()).unwrap_or("").trim();
        let script = script.trim_start_matches("./").replace('\\', "/");
        if script.is_empty() {
            continue;
        }
        out.push(ManifestEntry {
            script,
            name: source.get("name").and_then(|v| v.as_str()).unwrap_or("").trim().to_string(),
            enabled: source.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
        });
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn read_zip_file(archive: &mut ZipArchive<File>, name: &str) -> Option<Vec<u8>> {
    let want = normalize_zip_path(name);
    let len = archive.len();
    for i in 0..len {
        let entry = match archive.by_index(i) {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let Some(safe) = entry.enclosed_name() else {
            continue;
        };
        if normalize_zip_path(&safe.to_string_lossy()) != want {
            continue;
        }
        if entry.size() > MAX_SCRIPT_BYTES as u64 {
            return Some(vec![0; MAX_SCRIPT_BYTES + 1]);
        }
        let mut buf = Vec::new();
        if entry.take(MAX_SCRIPT_BYTES as u64 + 1).read_to_end(&mut buf).is_err() {
            return None;
        }
        return Some(buf);
    }
    None
}

fn fetch_url(url: &str) -> Result<Candidate> {
    let resp = crate::host::browser_get(url, &[], Duration::from_secs(20), MAX_SCRIPT_BYTES)
        .with_context(|| format!("下载音源失败 {url}"))?;
    let ctype = resp
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.to_ascii_lowercase())
        .unwrap_or_default();
    if ctype.contains("text/html") || looks_like_html(&resp.body) {
        bail!("这个地址返回的是网页，不是 .js 音源");
    }
    let file_name = url
        .split('?')
        .next()
        .and_then(|p| p.rsplit('/').next())
        .unwrap_or("source.js");
    bytes_to_candidate(resp.body, url.to_string(), file_name, &stem_of(file_name), true)
        .map_err(|item| anyhow!("{}: {}", item.name, item.detail))
}

fn load_file(path: &Path) -> Result<Candidate, ImportItem> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => return Err(error_item(&path.display().to_string(), &err.to_string())),
    };
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "source.js".into());
    bytes_to_candidate(
        bytes,
        path.display().to_string(),
        &file_name,
        &stem_of(&file_name),
        true,
    )
}

fn bytes_to_candidate(
    bytes: Vec<u8>,
    origin: String,
    file_name: &str,
    fallback_name: &str,
    enabled: bool,
) -> Result<Candidate, ImportItem> {
    let label = if fallback_name.trim().is_empty() {
        file_name
    } else {
        fallback_name
    };
    if bytes.len() > MAX_SCRIPT_BYTES {
        return Err(error_item(label, "脚本超过 2MB"));
    }
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Err(error_item(label, "脚本是空的"));
    }
    let script = match String::from_utf8(bytes) {
        Ok(script) => script,
        Err(_) => return Err(error_item(label, "脚本不是 UTF-8")),
    };
    let header = parse_script_header(script.trim_start_matches('\u{feff}'));
    let mut name = header.name.trim().to_string();
    if name.is_empty() {
        name = fallback_name.trim().to_string();
    }
    name = sanitize_source_name(&name);
    if name.is_empty() {
        name = "未命名".into();
    }
    let hash = content_hash(script.as_bytes());
    Ok(Candidate {
        name,
        version: header.version.trim().to_string(),
        author: header.author,
        description: header.description,
        homepage: header.homepage,
        origin,
        hash,
        script,
        is_latest: is_latest_file(file_name),
        enabled,
    })
}

fn sanitize_source_name(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_control() {
            continue;
        }
        if ch.is_whitespace() {
            if !out.ends_with(' ') && !out.is_empty() {
                out.push(' ');
            }
            continue;
        }
        out.push(ch);
        if out.chars().count() >= 120 {
            break;
        }
    }
    out.trim().to_string()
}

fn item(candidate: &Candidate, action: &str, detail: &str, status: &str) -> ImportItem {
    ImportItem {
        name: candidate.name.clone(),
        version: candidate.version.clone(),
        action: action.into(),
        detail: detail.into(),
        status: status.into(),
    }
}

fn dup_item(candidate: &Candidate) -> ImportItem {
    item(
        candidate,
        "skipped-duplicate",
        "同一批里已有相同内容或同名的更合适版本（优先 latest.js，否则更高版本）",
        "",
    )
}

fn error_item(name: &str, detail: &str) -> ImportItem {
    ImportItem {
        name: name.to_string(),
        version: String::new(),
        action: "error".into(),
        detail: detail.into(),
        status: String::new(),
    }
}

fn is_js(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("js"))
}

fn is_zip(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("zip"))
}

fn is_latest_file(file_name: &str) -> bool {
    file_name.eq_ignore_ascii_case("latest.js")
}

fn is_manifest_name(name: &str) -> bool {
    name == "manifest.json" || name.ends_with("/manifest.json")
}

fn normalize_zip_path(path: &str) -> String {
    path.trim_start_matches("./").replace('\\', "/").trim_start_matches('/').to_string()
}

fn file_name_of(path: &str) -> String {
    normalize_zip_path(path)
        .rsplit('/')
        .next()
        .unwrap_or("source.js")
        .to_string()
}

fn stem_of(file_name: &str) -> String {
    file_name
        .rsplit_once('.')
        .map(|(stem, _)| stem.to_string())
        .unwrap_or_else(|| file_name.to_string())
}

fn looks_like_html(body: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&body[..body.len().min(64)]).trim_start().to_ascii_lowercase();
    head.starts_with("<!doctype html") || head.starts_with("<html")
}

fn scripts_and_manifests_too_many(n: usize) -> bool {
    n > 200
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        (dir, store)
    }

    fn write_js(dir: &Path, rel: &str, body: &str) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, body).unwrap();
        path
    }

    fn script(name: &str, version: &str, body: &str) -> String {
        format!("/**\n * @name {name}\n * @version {version}\n */\n{body}\n")
    }

    #[test]
    fn directory_prefers_latest_then_higher_version() {
        let (dir, store) = store();
        let root = dir.path().join("src");
        write_js(&root, "old.js", &script("同源", "9.0.0", "var old = 1"));
        write_js(&root, "nested/latest.js", &script("同源", "0.1.0", "var latest = 1"));
        write_js(&root, "a.js", &script("版本源", "1.2.0", "var a = 1"));
        write_js(&root, "b.js", &script("版本源", "1.10.0", "var b = 1"));
        let same = script("重复", "1", "var same = 1");
        write_js(&root, "c.js", &same);
        write_js(&root, "d.js", &same);

        let report = import(&store, root.to_str().unwrap(), &ImportOpts::default()).unwrap();
        let imported: Vec<_> = report.items.iter().filter(|i| i.action == "imported").collect();
        assert_eq!(imported.len(), 3);
        assert!(imported.iter().any(|i| i.name == "同源" && i.version == "0.1.0"));
        assert!(imported.iter().any(|i| i.name == "版本源" && i.version == "1.10.0"));
        assert!(report.items.iter().any(|i| i.action == "skipped-duplicate" && i.version == "9.0.0"));
        assert_eq!(store.list_sources().unwrap().len(), 3);
    }

    #[test]
    fn name_clash_skips_unless_overwrite() {
        let (dir, store) = store();
        let first = write_js(dir.path(), "a.js", &script("甲", "1.0.0", "var a = 1"));
        import(&store, first.to_str().unwrap(), &ImportOpts::default()).unwrap();
        let second = write_js(dir.path(), "b.js", &script("甲", "2.0.0", "var b = 2"));
        let skipped = import(&store, second.to_str().unwrap(), &ImportOpts::default()).unwrap();
        assert_eq!(skipped.items[0].action, "skipped-name");
        assert_eq!(store.get_source_by_name("甲").unwrap().unwrap().version, "1.0.0");

        let mut opts = ImportOpts::default();
        opts.conflict = ConflictMode::Overwrite;
        let overwritten = import(&store, second.to_str().unwrap(), &opts).unwrap();
        assert_eq!(overwritten.items[0].action, "overwritten");
        let row = store.get_source_by_name("甲").unwrap().unwrap();
        assert_eq!(row.version, "2.0.0");
        assert_eq!(row.status, "unknown");
    }

    #[test]
    fn zip_manifest_imports_only_listed_scripts() {
        let (dir, store) = store();
        let zip_path = dir.path().join("sources.zip");
        write_zip(
            &zip_path,
            &[
                (
                    "manifest.json",
                    r#"{"version":1,"sources":[{"id":"a","name":"清单名","url":"local://a","script":"scripts/a.js"}]}"#,
                ),
                ("scripts/a.js", &script("头部名", "3", "var a = 1")),
                ("extra.js", &script("不该导入", "1", "var b = 1")),
            ],
        );
        let report = import(&store, zip_path.to_str().unwrap(), &ImportOpts::default()).unwrap();
        assert_eq!(report.items.len(), 1);
        assert_eq!(report.items[0].name, "头部名");
        assert!(store.get_source_by_name("不该导入").unwrap().is_none());
    }

    #[test]
    fn zip_without_miyin_manifest_collects_js() {
        let (dir, store) = store();
        let zip_path = dir.path().join("loose.zip");
        write_zip(
            &zip_path,
            &[
                ("manifest.json", r#"{"note":"not a miyin bundle"}"#),
                ("a.js", &script("甲", "1", "var a = 1")),
                ("b.js", &script("乙", "1", "var b = 1")),
            ],
        );
        let report = import(&store, zip_path.to_str().unwrap(), &ImportOpts::default()).unwrap();
        let names: Vec<_> = report
            .items
            .iter()
            .filter(|i| i.action == "imported")
            .map(|i| i.name.as_str())
            .collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"甲") && names.contains(&"乙"));
    }

    #[test]
    fn local_scheme_is_rejected() {
        let (_dir, store) = store();
        let err = import(&store, "local://abc", &ImportOpts::default()).unwrap_err();
        assert!(err.to_string().contains("local://"));
    }

    fn write_zip(path: &Path, files: &[(&str, &str)]) {
        let mut zip = zip::ZipWriter::new(File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        for (name, body) in files {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }
}
