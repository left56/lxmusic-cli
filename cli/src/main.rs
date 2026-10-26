//! 命令：import、sources、probe、search、download、config。

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use lxmusic_cli::download::{download, DownloadOpts};
use lxmusic_cli::paths::expand_user_path;
use lxmusic_cli::probe::probe_sources;
use lxmusic_cli::scan::{import, ConflictMode, ImportOpts};
use lxmusic_cli::search::{platform_label, search};
use lxmusic_cli::store::{SourceRow, Store};
use serde::Serialize;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "lxmusic-cli",
    version,
    about = "用你自己的洛雪音源搜索并下载音乐",
    arg_required_else_help = true
)]
struct Cli {
    /// 把结果打成 JSON
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 导入一个 .js、目录、zip，或 http(s) 上的单个脚本
    Import {
        /// .js 文件、目录、zip，或 http(s) 地址
        target: String,
        /// 同名时 skip 保留旧的，overwrite 覆盖并把状态重置为 unknown
        #[arg(long, value_enum, default_value = "skip")]
        on_conflict: OnConflict,
        /// 导入后立刻探测。默认不探测
        #[arg(long)]
        probe: bool,
    },
    /// 查看或修改已导入的音源
    Sources {
        #[command(subcommand)]
        action: SourcesCmd,
    },
    /// 用固定曲目试取链。失败标成 dead，不删除脚本
    Probe {
        /// 音源名字。不写则探测全部已启用的音源
        names: Vec<String>,
    },
    /// 搜索 wy、kw、kg、tx
    Search {
        /// 关键词，多个词会拼起来
        #[arg(required = true)]
        keyword: Vec<String>,
        /// 平台，可重复，也可用逗号分隔
        #[arg(short, long)]
        platform: Vec<String>,
        /// 页码，从 1 开始
        #[arg(long, default_value_t = 1)]
        page: u32,
    },
    /// 按搜索结果里的短 id 下载
    Download {
        /// 搜索结果里的 id
        id: String,
        /// 只这一次用的目录，不修改默认下载目录
        #[arg(long)]
        out: Option<String>,
        /// highest、flac24bit、flac、320k、192k、128k
        #[arg(long, default_value = "highest")]
        quality: String,
    },
    /// 读取或写入配置。可写的只有 download-dir
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },
}

#[derive(Subcommand)]
enum SourcesCmd {
    /// 列出音源
    List,
    /// 启用
    Enable { name: String },
    /// 停用。停用后不会参与下载
    Disable { name: String },
    /// 删除音源和脚本文件
    Remove { name: String },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// 读取 download-dir 或 data-dir
    Get { key: String },
    /// 设置 download-dir。data-dir 只读
    Set { key: String, value: String },
}

#[derive(Clone, Copy, ValueEnum)]
enum OnConflict {
    Skip,
    Overwrite,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            emit_error(json, &err);
            ExitCode::from(1)
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    let json = cli.json;
    match cli.command {
        Command::Import {
            target,
            on_conflict,
            probe,
        } => {
            let store = open_store()?;
            let report = import(
                &store,
                &target,
                &ImportOpts {
                    conflict: match on_conflict {
                        OnConflict::Skip => ConflictMode::Skip,
                        OnConflict::Overwrite => ConflictMode::Overwrite,
                    },
                    probe,
                    progress: !json,
                },
            )?;
            let failed = !report.items.is_empty() && report.items.iter().all(|item| item.action == "error");
            if json {
                print_json(&report)?;
            } else {
                for item in &report.items {
                    let version = if item.version.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", item.version)
                    };
                    let status = if item.status.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", item.status)
                    };
                    println!("[{}] {}{version}{status} {}", item.action, item.name, item.detail);
                }
            }
            Ok(if failed { ExitCode::from(1) } else { ExitCode::SUCCESS })
        }
        Command::Sources { action } => {
            let store = open_store()?;
            match action {
                SourcesCmd::List => {
                    let rows = store.list_sources()?;
                    if json {
                        print_json(&rows.iter().map(source_view).collect::<Vec<_>>())?;
                    } else if rows.is_empty() {
                        println!("还没有音源。导入一个 .js 文件、目录或 zip：");
                        println!("  lxmusic-cli import <路径>");
                    } else {
                        for row in &rows {
                            println!(
                                "{}  {}  {}  {}  {}",
                                row.name,
                                empty_dash(&row.version),
                                if row.enabled { "启用" } else { "停用" },
                                row.status,
                                list_or_dash(&row.platforms())
                            );
                            if !row.last_error.is_empty() {
                                println!("  {}", row.last_error);
                            }
                        }
                    }
                }
                SourcesCmd::Enable { name } => {
                    store.set_enabled(&name, true)?;
                    print_toggle(json, &name, true)?;
                }
                SourcesCmd::Disable { name } => {
                    store.set_enabled(&name, false)?;
                    print_toggle(json, &name, false)?;
                }
                SourcesCmd::Remove { name } => {
                    store.remove_source(&name)?;
                    if json {
                        print_json(&serde_json::json!({"name": name, "removed": true}))?;
                    } else {
                        println!("已删除「{name}」");
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Probe { names } => {
            let store = open_store()?;
            let reports = probe_sources(&store, &names, !json)?;
            if json {
                print_json(&reports)?;
            } else {
                for report in &reports {
                    println!(
                        "{}  {}  {}",
                        report.name,
                        report.status,
                        list_or_dash(&report.platforms)
                    );
                    if !report.last_error.is_empty() {
                        println!("  {}", report.last_error);
                    }
                    for alert in &report.alerts {
                        println!("  提示：{alert}");
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Search {
            keyword,
            platform,
            page,
        } => {
            let store = open_store()?;
            let keyword = keyword.join(" ");
            let output = search(&store, &keyword, &platform, page, !json)?;
            let failed = output.hits.is_empty() && !output.errors.is_empty();
            if json {
                print_json(&output)?;
            } else if output.hits.is_empty() {
                println!("没有结果");
            } else {
                for hit in &output.hits {
                    println!(
                        "{}  {}  {} - {}  {}  {}",
                        hit.id,
                        platform_label(&hit.platform),
                        hit.artist,
                        hit.title,
                        empty_dash(&hit.album),
                        clock(hit.duration)
                    );
                }
            }
            for warning in &output.warnings {
                eprintln!("提示：{warning}");
            }
            for error in &output.errors {
                eprintln!("{}：{}", platform_label(&error.platform), error.message);
            }
            Ok(if failed { ExitCode::from(1) } else { ExitCode::SUCCESS })
        }
        Command::Download { id, out, quality } => {
            let store = open_store()?;
            let report = download(
                &store,
                &id,
                &DownloadOpts {
                    out,
                    quality,
                    progress: !json,
                    companions: true,
                    native_lyric: true,
                },
            )?;
            if json {
                print_json(&report)?;
            } else {
                println!("{}", report.path);
                println!("{} - {}", report.artist, report.title);
                println!("音源：{}", report.source);
                if report.fallback {
                    println!(
                        "音质：{}（兜底，声称 {}）",
                        report.quality, report.claimed_quality
                    );
                    if !report.note.is_empty() {
                        println!("{}", report.note);
                    }
                } else {
                    println!("音质：{}", report.quality);
                }
                println!("大小：{}", bytes_text(report.bytes));
                if let Some(path) = &report.info_path {
                    println!("歌曲信息：{path}");
                }
                if let Some(path) = &report.lyric_path {
                    println!("歌词：{path}");
                } else if !report.lyric_note.is_empty() {
                    println!("歌词：{}", report.lyric_note);
                }
                if let Some(path) = &report.cover_path {
                    println!("封面：{path}");
                } else if !report.cover_note.is_empty() {
                    println!("封面：{}", report.cover_note);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Config { action } => match action {
            ConfigCmd::Get { key } => {
                let key = normalize_config_key(&key)?;
                let value = config_get(&key)?;
                print_config(json, &key, value.as_deref())?;
                Ok(ExitCode::SUCCESS)
            }
            ConfigCmd::Set { key, value } => {
                let key = normalize_config_key(&key)?;
                if key != "download-dir" {
                    bail!("data-dir 是只读的，由 LXMUSIC_CLI_DATA_DIR、XDG_DATA_HOME 或 ~/.local/share/lxmusic-cli 决定");
                }
                let stored = set_download_dir(&value)?;
                print_config(json, "download-dir", Some(&stored))?;
                Ok(ExitCode::SUCCESS)
            }
        },
    }
}

fn open_store() -> Result<Store> {
    Store::open_default().context("无法打开数据目录")
}

fn normalize_config_key(key: &str) -> Result<String> {
    let key = key.trim();
    if key == "download-dir" || key == "data-dir" {
        Ok(key.to_string())
    } else {
        bail!("只支持 download-dir 和 data-dir")
    }
}

fn config_get(key: &str) -> Result<Option<String>> {
    if key == "data-dir" {
        return Ok(Some(lxmusic_cli::paths::data_dir().display().to_string()));
    }
    open_store()?.get_setting(key)
}

fn set_download_dir(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("目录不能为空");
    }
    let path = expand_user_path(raw);
    if path.is_file() {
        bail!("{} 是文件，下载目录必须是文件夹", path.display());
    }
    std::fs::create_dir_all(&path)?;
    let stored = path.display().to_string();
    open_store()?.set_setting("download-dir", &stored)?;
    Ok(stored)
}

fn print_config(json: bool, key: &str, value: Option<&str>) -> Result<()> {
    if json {
        print_json(&serde_json::json!({"key": key, "value": value}))?;
    } else {
        println!("{key}={}", value.unwrap_or(""));
    }
    Ok(())
}

fn print_toggle(json: bool, name: &str, enabled: bool) -> Result<()> {
    if json {
        print_json(&serde_json::json!({"name": name, "enabled": enabled}))?;
    } else if enabled {
        println!("已启用「{name}」");
    } else {
        println!("已停用「{name}」");
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceView<'a> {
    name: &'a str,
    version: &'a str,
    author: &'a str,
    description: &'a str,
    homepage: &'a str,
    origin: &'a str,
    enabled: bool,
    status: &'a str,
    platforms: Vec<String>,
    last_error: &'a str,
}

fn source_view(row: &SourceRow) -> SourceView<'_> {
    SourceView {
        name: &row.name,
        version: &row.version,
        author: &row.author,
        description: &row.description,
        homepage: &row.homepage,
        origin: &row.origin,
        enabled: row.enabled,
        status: &row.status,
        platforms: row.platforms(),
        last_error: &row.last_error,
    }
}

fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn emit_error(json: bool, err: &anyhow::Error) {
    let text = err
        .chain()
        .map(|item| item.to_string())
        .collect::<Vec<_>>()
        .join("：");
    if json {
        println!("{}", serde_json::json!({"error": text}));
    } else {
        eprintln!("错误：{text}");
    }
}

fn empty_dash(value: &str) -> &str {
    if value.trim().is_empty() {
        "-"
    } else {
        value
    }
}

fn list_or_dash(items: &[String]) -> String {
    if items.is_empty() {
        "-".into()
    } else {
        items.join(",")
    }
}

fn clock(sec: i64) -> String {
    let sec = sec.max(0);
    format!("{}:{:02}", sec / 60, sec % 60)
}

fn bytes_text(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}
