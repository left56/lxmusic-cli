# lxmusic-cli

用你自己的洛雪音源脚本搜索并下载音乐。程序不内置脚本，也不带播放器。

搜索在本机完成，平台是网易云 `wy`、酷我 `kw`、酷狗 `kg`、QQ 音乐 `tx`。真正的播放地址只通过脚本里的 `musicUrl` 获取。脚本跑在 QuickJS 里，宿主对齐洛雪桌面端：`lx.env` 是 `desktop`，`lx.version` 是 `2.0.0`。

## 构建

需要 Rust 1.82 或更新。

```bash
cargo build --release --manifest-path cli/Cargo.toml
```

二进制在 `cli/target/release/lxmusic-cli`。Linux 预编译包在 [v0.1.0](https://github.com/left56/lxmusic-cli/releases/tag/v0.1.0)：`lxmusic-cli-linux-x86_64-skill.zip` 和 `lxmusic-cli-linux-arm64-skill.zip`，里面是 musl 静态链接的二进制和 skill，不需要本机安装 Rust。开发时也可以：

```bash
cargo run --manifest-path cli/Cargo.toml -- --help
```

## 数据放在哪

默认是 `$XDG_DATA_HOME/lxmusic-cli`，否则是 `~/.local/share/lxmusic-cli`。`LXMUSIC_CLI_DATA_DIR` 可以改掉整个目录。

- `lxmusic-cli.db`：音源头信息、设置、最近的搜索结果
- `scripts/`：脚本正文，按内容哈希存放

`data-dir` 只能看，不能写。下载目录要单独设置，程序不会猜一个音乐库，也不会落到当前目录。

```bash
lxmusic-cli config set download-dir ~/Music/lxmusic-cli
lxmusic-cli config get download-dir
lxmusic-cli config get data-dir
```

`~/` 会展开。某一次想换地方，用 `download --out <目录>`，这不会改上面的默认值。

## 音源

脚本由你自己提供。可以是一个 `.js`，一个目录，一个 zip，或某个 `http(s)` 上的单个脚本。

```bash
lxmusic-cli import ./source.js
lxmusic-cli import ./sources/
lxmusic-cli import ./sources.zip
lxmusic-cli import https://example.com/source.js
```

目录和普通 zip 会收集里面的 `.js`。若 zip 是觅音导出的包（`manifest.json` 里有 `version` 和 `sources[].script`），只导入清单里的那些。同一次导入里，同名时优先 `latest.js`，否则留 `@version` 更高的那个。内容完全相同的脚本只留一份。

库里已经有同名音源时，默认跳过。要覆盖：

```bash
lxmusic-cli import --on-conflict overwrite ./source.js
```

覆盖后状态回到 `unknown`。

```bash
lxmusic-cli sources list
lxmusic-cli sources disable 某个音源
lxmusic-cli sources enable 某个音源
lxmusic-cli sources remove 某个音源
```

`probe` 用固定曲目试一次取链。初始化失败，或四个平台都取不到地址，会标成 `dead`，脚本仍留着。没有声明 `wy` / `kw` / `kg` / `tx` 的音源不会因此被标死。

```bash
lxmusic-cli probe
lxmusic-cli probe 某个音源
```

不写名字时只探测已启用的。`unknown` 也可以直接拿去下载，不必先探测。下载失败不会把音源改成 `dead`。

## 搜索和下载

```bash
lxmusic-cli search 晴天
lxmusic-cli search -p wy,tx --page 1 周杰伦 晴天
lxmusic-cli download abcd1234
lxmusic-cli download --quality flac --out /tmp/once abcd1234
```

搜索结果里的 `id` 是短的十六进制，不是平台原始 id。下载只认这个 id，最多留最近 2000 条。

音质从 `flac24bit`、`flac`、`320k`、`192k`、`128k` 往下试。文件头不是音频就丢掉。知道时长、并且码率低于 80kbps，当成试听丢掉。声称是 flac / flac24bit 但实际是 mp3，或者码率低于所声称的那一档，先留作一个兜底；后面有更合适的文件就用后面的，没有才保存这个兜底。时长未知时不按试听拒绝。

文件名是 `歌手 - 歌名.扩展名`。同名文件已存在时会加上 `(2)`，不会直接覆盖。播放地址仍然只来自脚本的 `musicUrl`。`lx.env` 仍是 `desktop`。

音频保存成功后，还会尽量写入歌词、封面，以及音频文件里的标准标签。歌词、封面或标签失败，不会让这次下载失败，也不会把音源标成 `dead`。

- 内嵌标签：歌名、歌手、专辑，以及封面图。MP3 写 ID3，FLAC / Ogg 写 Vorbis comment，M4A 写 iTunes 标签。不另存 JSON。平台、音质和音源仍只出现在命令输出里，不写进标签。
- `歌手 - 歌名.lrc`：歌词。先问当前音源的 `lyric`。原文和翻译（没有翻译就用罗马音）合成一份。音源没有时，再按平台请求公开歌词：网易云、酷我、酷狗（含 KRC）、QQ 音乐旧版 LRC、以及 `musicInfo` 里现成的咪咕歌词地址。QQ 的 QRC 密文不解，避免把密文写成歌词。
- `歌手 - 歌名.jpg`（也可能是 png、webp、gif）：封面文件，同一张图也会写进音频标签。先问音源的 `pic`，否则用搜索结果里的封面地址。内容不是图片就丢掉。

全局加上 `--json`，结果打到标准输出，进度和平台错误在标准错误。

## 给代理

`skills/lxmusic-cli/SKILL.md` 约定了代理该怎么用这个命令：没有音源或没有下载目录时停下来问，不要自己找脚本，也不要读脚本正文。
