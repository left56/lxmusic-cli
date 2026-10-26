---
name: lxmusic-cli
description: 用本仓库的 lxmusic-cli 搜索并下载音乐。用户要搜歌、下载、导入洛雪音源，或设置下载目录时使用。音源必须由用户提供；不要上网找脚本，也不要把脚本内容读进对话。
---

# lxmusic-cli

只通过 `lxmusic-cli` 工作。仓库里还没装到 PATH 时，用：

```bash
cargo run --quiet --manifest-path cli/Cargo.toml --
```

下面写 `lxmusic-cli` 时，两种写法都可以。要给程序看的结果一律加全局 `--json`。这个工具只下载，没有播放器。

## 开始前先停下来核对

搜索和下载之前，两件事缺一就停，问用户。不要自己补。

1. 音源。运行 `lxmusic-cli sources list --json`。结果是数组。
   - 空数组：停。请用户给一个 `.js` 文件、一个目录，或一个 zip。不要从网上找音源，不要打开、打印或总结脚本正文，也不要改 `miyin/`。
   - 可用的是已启用、且 `status` 不是 `dead` 的。`unknown` 可以直接用。不要为了保险去跑 `probe`。
   - 全部是 `dead`，或全部停用：停，把状态告诉用户。只有用户明确要求时才探测、启用或删除。
2. 默认下载目录。运行 `lxmusic-cli config get download-dir --json`。
   - `value` 为 `null` 或空：停，问用户要一个目录，再用 `lxmusic-cli config set download-dir <目录>`。
   - 不要猜音乐库，不要用家目录，也不要用当前目录。

用户已经给了脚本路径时，可以先导入，不必等下载目录：

```bash
lxmusic-cli import --json <路径>
```

同名默认跳过。用户要求覆盖时才加 `--on-conflict overwrite`。不要加 `--probe`，除非用户要求探测。

## 搜索

```bash
lxmusic-cli search --json --page 1 -p wy,kw,kg,tx <关键词>
```

平台只有 `wy`、`kw`、`kg`、`tx`。用户指定了平台就只传那些。把每条的 `id`、标题、歌手、专辑、平台、时长列出来，让用户选。下载只用这个短 id，不要把 `musicInfo` 拼进命令。

某个平台失败、其它平台有结果时，照常列出，并说明谁失败了。全部失败就停。不要自己换关键词连着搜，除非用户要求。

## 下载

```bash
lxmusic-cli download --json <id>
```

音质默认从高往低：`flac24bit`、`flac`、`320k`、`192k`、`128k`。用户指定档位时加 `--quality`，取值就是这些，或者 `highest`。

只这一次换目录时加 `--out <目录>`。这不会改默认目录，不要接着 `config set`。用户要改默认目录时，才用 `config set download-dir`。

下载失败不会把音源标成 `dead`，也不要自己去标。把错误告诉用户。声称无损但实际是 mp3，或码率低于该档时，命令会留下一个兜底文件。知道时长且低于 80kbps 的试听片段会丢掉，不算成功。

下载成功后，同一目录默认还会尽量写下同名的 `.lrc`、`.json` 歌曲信息和封面图。音频成功即可：歌词或封面失败不会把整次下载算失败。把 `path`、`infoPath`、`lyricPath`、`coverPath` 告诉用户。`lyricNote` 或 `coverNote` 非空时说明那一项没拿到，不要为了补歌词再去读脚本或另找音源。

## 不要做

- 不要读取、粘贴或改写音源脚本。
- 不要下载别人的音源包，也不要提交 `miyin-sources-*.zip`。
- 不要改 `miyin/`。
- 不要调用播放器。
