//! 下载专用命令行。搜索在 Rust 里完成。播放地址只走脚本的 `musicUrl`。
//! 歌词和封面优先走脚本的 `lyric` / `pic`，没有再走各平台公开接口。

pub mod audio;
pub mod companion;
pub mod download;
pub mod header;
pub mod host;
pub mod loose_json;
pub mod paths;
pub mod probe;
pub mod scan;
pub mod search;
pub mod store;
pub mod versioncmp;
