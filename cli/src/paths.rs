use std::path::PathBuf;

pub fn data_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("LXMUSIC_CLI_DATA_DIR") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("lxmusic-cli");
        }
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".local/share/lxmusic-cli")
}

/// 把 `~/...` 展开成家目录。其它路径原样保留，不要求已经存在。
pub fn expand_user_path(raw: &str) -> PathBuf {
    let raw = raw.trim();
    if raw == "~" || raw.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let mut path = PathBuf::from(home);
            if raw.len() > 2 {
                path.push(&raw[2..]);
            }
            return path;
        }
    }
    PathBuf::from(raw)
}
