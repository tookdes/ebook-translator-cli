# ebook-translator

无头命令行批量电子书翻译工具。Rust 单文件程序，运行时无需 Python 和 pip 依赖。

## 安装

从 Release 下载对应平台的 `ebook-translator` 单文件即可。源码构建：

```bash
cargo build --release
./target/release/ebook-translator --version
```

开发自测：`cargo fmt --check && cargo test --locked && cargo clippy --all-targets --all-features -- -D warnings`。CI 还会执行依赖安全检查、版本/tag 一致性检查和安装后 `--version` smoke test。

## 什么时候需要 calibre？

工具静态内置了 [libmobi](https://github.com/bfabiszewski/libmobi)，MOBI 和 AZW3 输入转换为 EPUB 时通常**不需要 calibre**；内置转换失败时会尝试用 calibre 回退。

| 操作 | 需要 calibre |
|------|:---:|
| MOBI / AZW3 输入 -> EPUB | **通常不需要**（内置 KindleUnpack） |
| AZW / PDF / DOCX 等其他非 EPUB 输入 | 需要 |
| 输出为非 EPUB 格式 | 需要 |
| 输入输出都是 EPUB | **不需要** |

### 安装 calibre（仅在需要时）

**macOS:**
```bash
brew install --cask calibre
```

**Ubuntu / Debian / VPS:**
```bash
sudo wget -nv -O- https://download.calibre-ebook.com/linux-installer.sh | sudo sh /dev/stdin
```

工具会自动查找 `ebook-convert`，无需额外配置。如路径不在默认位置，在 `config.json` 中设置：
```json
{ "ebook_convert_path": "/path/to/ebook-convert" }
```

## 快速开始

```bash
# 1. 配置
cp config.example.json config.json
# 填入 API 密钥

# 2. 翻译目录中的所有书籍
ebook-translator /path/to/books /path/to/output -c config.json

# 3. MOBI 输入也直接支持（无需 calibre）
ebook-translator /path/to/book.mobi /path/to/output -c config.json

# 4. 预览
ebook-translator /path/to/books /path/to/output --dry-run
```

## 用法

```
ebook-translator 输入 [输出] [选项]

位置参数:
  输入                    输入目录或单个电子书文件
  输出                    输出目录；--review-export 时可省略

选项:
  --output-format, -o     输出格式，默认: epub；非 epub 输出需要 calibre
  --config, -c            配置文件路径
  --engine, -e            翻译引擎 (openai, claude, deepseek)
  --source-lang, -s       源语言
  --target-lang, -t       目标语言
  --target-lang-code      译文 BCP-47 语言标签
  --target-direction      译文方向 (auto, ltr, rtl)
  --concurrency           并发数
  --force, -f             覆盖已存在的输出
  --no-cache              禁用缓存（不支持断点续翻）
  --skip-failed           跳过翻译失败的段落，保留原文继续生成输出
  --log-file              日志文件
  --dry-run               预览模式
  --test                  测试模式：仅翻译前几段
  --test-num              测试模式翻译段落数，默认: 10
  --retranslate-file      重翻译的页面文件名
  --retranslate-start     重翻译起始文本
  --retranslate-end       重翻译结束文本
  --retranslate-all       清除全部可翻译段落缓存
  --translate-metadata    翻译 OPF 元数据
  --translate-title       翻译书名并用于文件名
  --custom-title          自定义书名（仅单书）
  --input-encoding        ebook-convert 输入编码
  --review-export         导出校审 JSON，不调用 API
  --review-import         导入部分或完整校审 JSON
  --version, -V           版本号
```

## 配置

```json
{
    "engine": "openai",
    "source_lang": "English",
    "target_lang": "Chinese",
    "engines": {
        "openai": {
            "api_key": "sk-你的密钥",
            "base_url": "https://api.openai.com/v1",
            "model": "gpt-4o-mini",
            "stream": false,
            "concurrency": 3,
            "request_interval": 1.0
        }
    }
}
```

完整配置见 `config.example.json`。支持 `openai`（兼容所有 OpenAI 格式）、`claude`、`deepseek`。

`openai.base_url` 可以填网关根地址（如 `https://example.com/v1`），也可以直接填完整端点（如 `https://example.com/v1/chat/completions`）；程序会避免重复拼接路径。自定义兼容端点可以把 `model` 留空，程序不会强行填默认模型。

每个引擎支持 `extra`，并兼容别名 `extra_body`，用于把任意 JSON 顶层字段原样并入请求体。程序不解释这些字段，也不会针对厂商或模型自动补写 thinking/reasoning 参数。例如需要关闭某个兼容端点的思考模式时，可以自行配置：

```json
{
  "extra_body": {
    "chat_template_kwargs": {"enable_thinking": false},
    "thinking": {"type": "disabled"},
    "reasoning_effort": "none",
    "enable_thinking": false
  }
}
```

具体字段由所使用的模型、供应商或网关定义；本项目只负责透传。

## 支持格式

| 输入 | 后端 | 说明 |
|------|------|------|
| epub | 内置 | 直接处理 |
| mobi, azw3 | 内置 libmobi | 输出 EPUB 时通常无需 calibre；失败时可回退 calibre |
| azw, fb2, pdf, rtf, txt, docx, html, htm, odt, pdb, cbz, cbr | calibre | 需安装 |

| 输出 | 后端 | 说明 |
|------|------|------|
| epub | 内置 | 默认格式 |
| mobi, azw3 | calibre | 需安装 |

## 断点续翻

进度缓存在 `~/.cache/ebook-translator/books/`。中断后重跑同一命令自动续翻。

缓存 key 会绑定源文件内容、提取后的段落、引擎、模型、地址、采样参数、额外请求参数、源/目标语言、prompt 和术语表。修改这些内容后会自动使用新的缓存，避免误用旧译文；切换合并批次或译文样式会继续复用已有的逐段译文。缓存表以稳定段落 `id` 为主键，signature/md5 仅建普通索引，因此书中重复标题、脚注或重复可见段落不会因为相同 signature 被丢弃。

使用 `--no-cache` 会改用内存缓存：本次运行仍能完成写回，但不会落盘，也不会在下次运行复用译文。

如果某本书仍有段落翻译失败，程序会保留缓存进度但不生成半翻译输出文件；修好配置或换 key 后重跑即可继续。

`merge_enabled` 默认开启，`merge_length` 默认 `100000`，单位是 Unicode 字符而不是 token。合并不会跨 XHTML/章节边界。合并请求会发送带稳定段落 ID 的 JSON 数组，并严格校验返回 ID；遇到非法 JSON、ID 缺失/重复、占位符损坏或长请求超时时，会自动二分缩小批次重试，直到必要时降到单段。每个成功子批次会立即写入缓存，因此在大组合并降级过程中按 Ctrl+C，中间已经完成的段落也可直接断点续翻。带内联 HTML 的段落可以参与合并，HTML/术语占位符必须原样保留。 如果原书自身存在数万字的单一 DOM 文本块，且模型返回上下文超限，程序会在句界/空白处自动拆分该单段并递归重试，同时避开 HTML/术语占位符边界；大上下文模型仍优先使用完整单段请求。写回 XHTML 时，如果候选节点位于 `span` 等 phrasing 父级中，译文会降级为节点内部的行内 `span`，避免额外注入块级 `p`/heading 破坏 EPUB 内容模型。

## 退出码与日志

| 退出码 | 含义 |
|------:|------|
| 0 | 全部成功 |
| 1 | 至少一本书失败、批内输出重名或输出会覆盖输入 |
| 130 | 用户中断 |

`--log-file /path/to/run.log` 会写入批处理、缓存、失败原因和注入统计，适合配合 `nohup`、systemd 或 cron 排查。

默认输出名沿用输入文件名。显式使用自定义书名、翻译书名或元数据翻译时，Rust 版本会改用最终书名。文件名会跨平台清理并限制为 200 字节；已有输出默认跳过，`--force` 仅允许原子覆盖非输入、非批内冲突的文件。

## 术语表

```
source term
target term

another source
another target
```

`config.json` 中设置：`"glossary_path": "/path/to/glossary.txt"`

也可内联配置，内联值会覆盖文件中的同名源词：

```json
{"glossary": {"AI model": "AI 模型", "OpenAI": "OpenAI"}}
```

## 人工校审

```bash
# 只抽取并加载缓存，不调用 API、不生成译本
ebook-translator book.epub --review-export review.json

# 可编辑 translation、ignored 和 action（keep/retranslate），再导入生成
ebook-translator book.epub output --review-import review.json

# 清除全部可翻译段落缓存
ebook-translator book.epub output --retranslate-all
```

导入会原子校验源文件哈希、全书元素签名及每段 ID/原文/签名；未知、重复或过期记录会整体拒绝。

## VPS 部署

```bash
# 复制 Release 中的单文件程序
install -m 755 ebook-translator /usr/local/bin/ebook-translator

# 如需 AZW / PDF / DOCX 等输入，或输出非 EPUB 格式，再安装 calibre
# wget -nv -O- https://download.calibre-ebook.com/linux-installer.sh | sudo sh /dev/stdin

# 配置
cp config.example.json config.json

# 运行
nohup ebook-translator /books /output -o epub -c config.json > translate.log 2>&1 &

# 断点续翻：中断后直接重跑同一命令
```
