use std::time::Duration;
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tempfile::Builder;

use crate::{
    cache::{TranslationCache, md5},
    config::{Config, MAX_CONCURRENCY, canonical_encoding},
    converter::{convert, convert_to_epub, find_ebook_convert},
    engine::Engine,
    epub::{
        ExtractedElement, cache_rows, extract_from_epub, validate_markup_tokens,
        write_translated_epub,
    },
    fsutil::replace,
    glossary::Glossary,
    worker::TranslationWorker,
};

const SUPPORTED: &[&str] = &[
    "epub", "mobi", "azw3", "azw", "fb2", "pdf", "rtf", "txt", "docx", "html", "htm", "odt", "pdb",
    "cbz", "cbr",
];

static LOG: OnceLock<Mutex<File>> = OnceLock::new();

#[derive(Parser, Debug)]
#[command(
    name = "ebook-translator",
    version,
    about = "无头命令行批量电子书翻译工具"
)]
struct Args {
    /// 输入目录或单个电子书文件路径
    input: PathBuf,
    /// 输出目录
    output: Option<PathBuf>,
    /// 输出格式 (epub, mobi, azw3)
    #[arg(short = 'o', long, default_value = "epub", value_parser = ["epub", "mobi", "azw3"])]
    output_format: String,
    /// 配置文件路径
    #[arg(short = 'c', long)]
    config: Option<PathBuf>,
    /// 翻译引擎
    #[arg(short = 'e', long, value_parser = ["openai", "claude", "deepseek"])]
    engine: Option<String>,
    /// 源语言
    #[arg(short = 's', long)]
    source_lang: Option<String>,
    /// 目标语言
    #[arg(short = 't', long)]
    target_lang: Option<String>,
    /// 译文 BCP-47 语言标签
    #[arg(long)]
    target_lang_code: Option<String>,
    /// 译文方向 (auto, ltr, rtl)
    #[arg(long, value_parser = ["auto", "ltr", "rtl"])]
    target_direction: Option<String>,
    /// 并发翻译数
    #[arg(long)]
    concurrency: Option<usize>,
    /// 覆盖已存在的输出文件
    #[arg(short = 'f', long)]
    force: bool,
    /// 禁用翻译缓存
    #[arg(long)]
    no_cache: bool,
    /// 跳过翻译失败的段落
    #[arg(long)]
    skip_failed: bool,
    /// 日志输出到文件
    #[arg(long)]
    log_file: Option<PathBuf>,
    /// 预览模式
    #[arg(long)]
    dry_run: bool,
    /// 仅翻译前几段
    #[arg(long = "test")]
    test_enabled: bool,
    /// 测试模式翻译段落数
    #[arg(long)]
    test_num: Option<usize>,
    /// 重翻译的页面文件名
    #[arg(long)]
    retranslate_file: Option<String>,
    /// 重翻译起始文本
    #[arg(long)]
    retranslate_start: Option<String>,
    /// 重翻译结束文本
    #[arg(long)]
    retranslate_end: Option<String>,
    /// 翻译 OPF 元数据
    #[arg(long)]
    translate_metadata: bool,
    /// 翻译书名并用于输出文件名
    #[arg(long)]
    translate_title: bool,
    /// 自定义书名（仅单书输入）
    #[arg(long)]
    custom_title: Option<String>,
    /// ebook-convert 输入编码
    #[arg(long)]
    input_encoding: Option<String>,
    /// 导出人工校审 JSON 后退出
    #[arg(long)]
    review_export: Option<PathBuf>,
    /// 导入人工校审 JSON
    #[arg(long)]
    review_import: Option<PathBuf>,
    /// 清除全部可翻译段落缓存
    #[arg(long)]
    retranslate_all: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct ReviewDocument {
    schema_version: u32,
    source_content_hash: String,
    element_signature: String,
    title: String,
    paragraphs: Vec<ReviewParagraph>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ReviewParagraph {
    id: String,
    kind: String,
    page: String,
    original: String,
    translation: Option<String>,
    ignored: bool,
    action: String,
    signature: String,
}

pub async fn run() -> i32 {
    let args = Args::parse();
    match run_inner(args).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("错误: {error:#}");
            log("ERROR", &format!("{error:#}"));
            1
        }
    }
}

async fn run_inner(args: Args) -> Result<i32> {
    let mut config =
        Config::load(args.config.as_deref()).map_err(|x| anyhow!("配置错误: {x:#}"))?;
    apply_overrides(&args, &mut config)?;
    init_log(&config.log_file)?;
    log(
        "INFO",
        &format!("启动 ebook-translator v{}", env!("CARGO_PKG_VERSION")),
    );

    let books = collect_books(&args.input)?;
    if books.is_empty() {
        bail!(
            "在 {} 中未找到支持的电子书文件\n支持的格式: {}",
            args.input.display(),
            SUPPORTED.join(", ")
        );
    }
    if args.output.is_none() && args.review_export.is_none() {
        bail!("除 --review-export 外必须提供输出目录");
    }
    if args.dry_run {
        println!("找到 {} 本书:\n", books.len());
        for book in &books {
            println!(
                "  {}  ({})",
                book.file_name().unwrap_or_default().to_string_lossy(),
                human_size(book)
            );
        }
        return Ok(0);
    }
    if (args.review_export.is_some()
        || args.review_import.is_some()
        || !config.custom_title.is_empty())
        && books.len() != 1
    {
        bail!("review 和 custom_title 仅支持单书输入");
    }
    if args.review_import.is_some() && args.retranslate_all {
        bail!("--review-import 不能与 --retranslate-all 同时使用");
    }
    let output_dir = if let Some(output) = &args.output {
        fs::create_dir_all(output)?;
        Some(output.canonicalize().context("输出目录不可访问")?)
    } else if args.review_export.is_some() {
        None
    } else {
        bail!("除 --review-export 外必须提供输出目录");
    };
    let glossary = Glossary::load(&config.glossary_path, &config.glossary)
        .map_err(|x| anyhow!("配置错误: {x:#}"))?;
    if args.output_format != "epub" && args.review_export.is_none() {
        find_ebook_convert(&config.ebook_convert_path)?;
    }

    let progress = ProgressBar::new(books.len() as u64);
    progress.set_style(
        ProgressStyle::with_template("{msg} {bar:30} {pos}/{len} [{elapsed_precise}]").unwrap(),
    );
    progress.set_message("总进度");
    let start = Instant::now();
    let mut succeeded = 0;
    let mut failed = 0;
    let mut produced_outputs = Vec::new();
    for book in books {
        let stem = book.file_stem().and_then(|x| x.to_str()).unwrap_or("book");
        let result = tokio::select! {
            result = translate_book(
                &book,
                output_dir.as_deref(),
                &args.output_format,
                &config,
                &glossary,
                &args,
                (&progress, &mut produced_outputs),
            ) => result,
            _ = tokio::time::sleep(Duration::from_secs(14400)) => {
                progress.println(format!("  处理超时（14400 秒），跳过: {stem}"));
                log("ERROR", &format!("处理超时: {}", book.display()));
                failed += 1;
                progress.inc(1);
                continue;
            }
            _ = tokio::signal::ctrl_c() => {
                progress.finish_and_clear();
                eprintln!("翻译被中断，进度已保存");
                return Ok(130);
            }
        };
        match result {
            Ok(true) => succeeded += 1,
            Ok(false) => failed += 1,
            Err(error) => {
                progress.println(format!("  处理失败 {stem}: {error:#}"));
                log("ERROR", &format!("处理失败 {}: {error:#}", book.display()));
                failed += 1;
            }
        }
        progress.inc(1);
    }
    progress.finish_and_clear();
    eprintln!("\n  翻译完成\n\n  成功: {succeeded}");
    if failed > 0 {
        eprintln!("  失败: {failed}");
    }
    eprintln!("  用时: {:.1} 分钟\n", start.elapsed().as_secs_f64() / 60.0);
    Ok(if failed > 0 { 1 } else { 0 })
}

fn apply_overrides(args: &Args, config: &mut Config) -> Result<()> {
    if let Some(value) = &args.engine {
        config.engine.clone_from(value);
    }
    if let Some(value) = &args.source_lang {
        config.source_lang.clone_from(value);
    }
    if let Some(value) = &args.target_lang {
        config.target_lang.clone_from(value);
    }
    if let Some(value) = &args.target_lang_code {
        config.target_lang_code.clone_from(value);
    }
    if let Some(value) = &args.target_direction {
        config.target_direction.clone_from(value);
    }
    if let Some(value) = args.concurrency.filter(|value| *value > 0) {
        if value > MAX_CONCURRENCY {
            bail!("concurrency 不能大于 {MAX_CONCURRENCY}");
        }
        let mut engine = config.engine_config(None);
        engine.concurrency = value;
        config.engines.insert(config.engine.clone(), engine);
    }
    if args.no_cache {
        config.cache_enabled = false;
    }
    if args.skip_failed {
        config.skip_failed = true;
    }
    if let Some(value) = &args.log_file {
        config.log_file = value.clone();
    }
    if args.test_enabled {
        config.test_enabled = true;
    }
    if config.test_enabled {
        config.skip_failed = true;
    }
    if let Some(value) = args.test_num.filter(|value| *value > 0) {
        config.test_num = value;
    }
    if let Some(value) = &args.retranslate_file {
        config.retranslate_file.clone_from(value);
    }
    if let Some(value) = &args.retranslate_start {
        config.retranslate_start.clone_from(value);
    }
    if let Some(value) = &args.retranslate_end {
        config.retranslate_end.clone_from(value);
    }
    let retranslate_values = [
        config.retranslate_file.trim(),
        config.retranslate_start.trim(),
        config.retranslate_end.trim(),
    ];
    if retranslate_values.iter().any(|value| !value.is_empty())
        && (retranslate_values[0].is_empty() || retranslate_values[1].is_empty())
    {
        bail!("重翻译必须同时设置 retranslate_file 和 retranslate_start");
    }
    if args.translate_metadata {
        config.metadata_translation = true;
    }
    if args.translate_title {
        config.translate_title = true;
    }
    if let Some(value) = &args.custom_title {
        config.custom_title.clone_from(value);
    }
    if let Some(value) = &args.input_encoding {
        config.input_encoding = canonical_encoding(value)
            .ok_or_else(|| anyhow!("不支持的 input_encoding: {value}"))?
            .into();
    }
    config.validate()
}

async fn translate_book(
    input: &Path,
    output_dir: Option<&Path>,
    output_format: &str,
    config: &Config,
    glossary: &Glossary,
    args: &Args,
    state: (&ProgressBar, &mut Vec<PathBuf>),
) -> Result<bool> {
    let (progress, produced_outputs) = state;
    let input_format = extension(input);
    if !SUPPORTED.contains(&input_format.as_str()) {
        return Ok(false);
    }
    log("INFO", &format!("开始处理: {}", input.display()));
    let convert_input = input.to_owned();
    let converter = config.ebook_convert_path.clone();
    let encoding = (!config.input_encoding.is_empty()).then(|| config.input_encoding.clone());
    let converted =
        convert_to_epub(&convert_input, &converter, encoding.as_deref()).context("格式转换失败")?;
    let epub_path = converted.path.clone();
    let extraction_config = config.clone();
    let (elements, meta) =
        extract_from_epub(&epub_path, &extraction_config).context("EPUB 解析失败")?;
    if elements.is_empty() {
        eprintln!("  未找到可翻译内容: {}", input.display());
        return Ok(false);
    }
    let original_title = if meta.title.is_empty() {
        input
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    } else {
        meta.title.clone()
    };
    let early_output = if args.review_export.is_none()
        && (!config.custom_title.trim().is_empty()
            || (!config.translate_title && !config.metadata_translation))
    {
        let output_dir = output_dir.ok_or_else(|| anyhow!("缺少输出目录"))?;
        let output_title = if config.custom_title.trim().is_empty()
            && !config.translate_title
            && !config.metadata_translation
        {
            input
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or(&original_title)
                .to_owned()
        } else {
            select_title(config, &original_title, None)
        };
        let output = output_dir.join(output_filename(&output_title, output_format));
        validate_output_target(input, &output, produced_outputs)?;
        if output.exists() && !args.force {
            progress.println(format!("  输出已存在，跳过: {}", output.display()));
            return Ok(true);
        }
        Some(output)
    } else {
        None
    };
    let cache_key = cache_key(input, &elements, config, glossary)?;
    let cache_path = config
        .cache_dir
        .join("books")
        .join(format!("{cache_key}.db"));
    let cache = Arc::new(TranslationCache::open(&cache_path, config.cache_enabled)?);
    cache.set_info("title", &original_title)?;
    cache.set_info("engine", &config.engine)?;
    cache.set_info("target_lang", &config.target_lang)?;
    cache.set_info("source", &input.canonicalize()?.display().to_string())?;
    cache.save_paragraphs(&cache_rows(&elements))?;
    if let Some(path) = &args.review_import {
        import_review(
            path,
            input,
            &elements,
            &cache,
            &config.engine,
            &config.target_lang,
        )?;
    }
    if args.retranslate_all {
        let cleared = cache.clear_all_translations()?;
        eprintln!("  全量重翻译: 已清除 {cleared} 段缓存");
    }
    if !config.retranslate_file.is_empty() && !config.retranslate_start.is_empty() {
        retranslate(&cache, config)?;
    }
    if let Some(path) = &args.review_export {
        export_review(
            path,
            input,
            &select_title(config, &original_title, None),
            &elements,
            &cache,
            args.force,
        )?;
        eprintln!("  已导出校审文件: {}", path.display());
        return Ok(true);
    }
    let mut untranslated = cache.untranslated()?;
    let (already, total) = cache.counts()?;
    if config.test_enabled && config.test_num > 0 {
        untranslated.truncate(config.test_num);
    }

    if !untranslated.is_empty() {
        let engine = Engine::new(
            &config.engine,
            config.engine_config(None),
            &config.source_lang,
            &config.target_lang,
        )?;
        let mut worker =
            TranslationWorker::new(engine, cache.clone(), config.clone(), glossary.clone());
        let mut fallback_names = vec!["deeplx", "deepx"];
        fallback_names.retain(|name| *name != config.engine);
        for name in fallback_names {
            let Some(fallback_config) = config.engines.get(name) else {
                continue;
            };
            match Engine::new(
                name,
                fallback_config.clone(),
                &config.source_lang,
                &config.target_lang,
            ) {
                Ok(fallback) => {
                    eprintln!("  已启用 {name} 兜底渠道");
                    worker = worker.with_fallback(fallback);
                }
                Err(error) => eprintln!("  {name} 兜底不可用: {error:#}"),
            }
        }
        let (done, failed) = worker.translate_batch(untranslated).await;
        eprintln!("  {original_title}: {total} 段, 完成 {done}, 缓存 {already}, 失败 {failed}");
        if failed > 0 && !config.skip_failed {
            return Ok(false);
        }
    }
    let all = cache.all()?;
    let missing = all.iter().filter(|x| x.translation.is_none()).count();
    if missing > 0 && !config.skip_failed {
        return Ok(false);
    }
    let translations = all
        .into_iter()
        .filter_map(|x| x.translation.map(|value| (x.id, value)))
        .collect::<HashMap<_, _>>();
    let translated_title = meta
        .title_uid
        .as_ref()
        .and_then(|uid| translations.get(uid))
        .map(|value| value.trim())
        .filter(|value| !value.is_empty());
    let final_title = select_title(config, &original_title, translated_title);
    let output_dir = output_dir.ok_or_else(|| anyhow!("缺少输出目录"))?;
    let output = early_output
        .unwrap_or_else(|| output_dir.join(output_filename(&final_title, output_format)));
    validate_output_target(input, &output, produced_outputs)?;
    if output.exists() && !args.force {
        progress.println(format!("  输出已存在，跳过: {}", output.display()));
        return Ok(true);
    }
    let translated_path = Builder::new()
        .prefix(".et-translated-")
        .suffix(".epub")
        .tempfile_in(output_dir)?
        .into_temp_path();
    let write_input = converted.path.clone();
    let write_output = translated_path.to_path_buf();
    let write_config = config.clone();
    let write_title = final_title.clone();
    let expected_count = translations.len();
    tokio::task::spawn_blocking(move || {
        write_translated_epub(
            &write_input,
            &write_output,
            &translations,
            &write_config,
            expected_count,
            &write_title,
        )
    })
    .await
    .context("EPUB 写出任务异常结束")??;
    if output_format == "epub" {
        replace(&translated_path, &output)?;
        produced_outputs.push(output.clone());
    } else if let Err(error) = {
        let convert_input = translated_path.to_path_buf();
        let convert_output = output.clone();
        let output_format = output_format.to_owned();
        let converter = config.ebook_convert_path.clone();
        tokio::task::spawn_blocking(move || {
            convert(
                &convert_input,
                &convert_output,
                &output_format,
                &converter,
                None,
            )
        })
        .await
        .context("输出转换任务异常结束")?
    } {
        let fallback = fallback_epub(&output, input);
        replace(&translated_path, &fallback)?;
        produced_outputs.push(fallback.clone());
        eprintln!(
            "输出转换失败({error:#})，已回退保存为 EPUB: {}",
            fallback.display()
        );
        return Ok(false);
    } else {
        produced_outputs.push(output.clone());
    }
    log(
        "INFO",
        &format!("处理完成: {} -> {}", input.display(), output.display()),
    );
    Ok(true)
}

fn retranslate(cache: &TranslationCache, config: &Config) -> Result<()> {
    let mut ids = Vec::new();
    let mut in_range = false;
    for paragraph in cache.all_with_ignored()? {
        if !config.retranslate_file.is_empty() {
            let Some(page) = &paragraph.page else {
                continue;
            };
            if config.retranslate_file != *page
                && Path::new(page)
                    .file_name()
                    .is_none_or(|x| x != config.retranslate_file.as_str())
            {
                continue;
            }
        }
        if paragraph.original.contains(&config.retranslate_start) {
            in_range = true;
        }
        if in_range {
            ids.push(paragraph.id);
        }
        if in_range
            && !config.retranslate_end.is_empty()
            && paragraph.original.contains(&config.retranslate_end)
        {
            break;
        }
    }
    let cleared = cache.clear_translations(&ids)?;
    eprintln!("  重翻译: 已清除 {cleared} 段缓存");
    Ok(())
}

fn export_review(
    path: &Path,
    input: &Path,
    title: &str,
    elements: &[ExtractedElement],
    cache: &TranslationCache,
    force: bool,
) -> Result<()> {
    if path.exists() && !force {
        bail!("校审文件已存在: {}（使用 --force 覆盖）", path.display());
    }
    let cached = cache
        .all_with_ignored()?
        .into_iter()
        .map(|row| (row.id.clone(), row))
        .collect::<HashMap<_, _>>();
    let document = ReviewDocument {
        schema_version: 1,
        source_content_hash: file_md5(input)?,
        element_signature: element_signature(elements)?,
        title: title.into(),
        paragraphs: elements
            .iter()
            .map(|element| {
                let row = cached.get(&element.uid);
                ReviewParagraph {
                    id: element.uid.clone(),
                    kind: element.kind.clone(),
                    page: element.page_href.clone(),
                    original: element.original.clone(),
                    translation: row.and_then(|row| row.translation.clone()),
                    ignored: row.map_or(element.ignored, |row| row.ignored),
                    action: "keep".into(),
                    signature: element.signature.clone(),
                }
            })
            .collect(),
    };
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temp = Builder::new().prefix(".review-").tempfile_in(parent)?;
    serde_json::to_writer_pretty(&mut temp, &document)?;
    temp.write_all(b"\n")?;
    temp.flush()?;
    let (_, temp_path) = temp.keep()?;
    replace(&temp_path, path)
}

fn import_review(
    path: &Path,
    input: &Path,
    elements: &[ExtractedElement],
    cache: &TranslationCache,
    engine: &str,
    target_lang: &str,
) -> Result<()> {
    let document: ReviewDocument = serde_json::from_slice(
        &fs::read(path).with_context(|| format!("校审文件不存在或不可读: {}", path.display()))?,
    )
    .context("校审 JSON 无效")?;
    if document.schema_version != 1 {
        bail!("不支持的校审 schema_version: {}", document.schema_version);
    }
    if document.source_content_hash != file_md5(input)?
        || document.element_signature != element_signature(elements)?
    {
        bail!("校审文件已过期或不属于当前源文件");
    }
    let current = elements
        .iter()
        .map(|element| (element.uid.as_str(), element))
        .collect::<HashMap<_, _>>();
    let mut seen = HashSet::new();
    let mut updates = Vec::with_capacity(document.paragraphs.len());
    for paragraph in document.paragraphs {
        if !seen.insert(paragraph.id.clone()) {
            bail!("校审文件包含重复段落 ID: {}", paragraph.id);
        }
        let element = current
            .get(paragraph.id.as_str())
            .ok_or_else(|| anyhow!("校审文件包含未知段落 ID: {}", paragraph.id))?;
        if paragraph.original != element.original || paragraph.signature != element.signature {
            bail!("校审段落已过期: {}", paragraph.id);
        }
        if !matches!(paragraph.action.as_str(), "keep" | "retranslate") {
            bail!("校审 action 只能是 keep 或 retranslate: {}", paragraph.id);
        }
        if paragraph.action == "retranslate" && paragraph.ignored {
            bail!("retranslate 段落不能同时 ignored=true: {}", paragraph.id);
        }
        if let Some(translation) = &paragraph.translation
            && paragraph.kind == "body"
            && paragraph.action == "keep"
            && !paragraph.ignored
        {
            if translation.trim().is_empty() {
                bail!("校审 keep 译文不能为空: {}", paragraph.id);
            }
            validate_markup_tokens(&paragraph.original, translation)
                .with_context(|| format!("校审译文占位符无效: {}", paragraph.id))?;
        }
        updates.push((
            paragraph.id,
            paragraph.translation,
            paragraph.ignored,
            paragraph.action == "retranslate",
        ));
    }
    cache.apply_review(&updates, engine, target_lang)?;
    eprintln!("  已导入 {} 条校审记录", updates.len());
    Ok(())
}

fn element_signature(elements: &[ExtractedElement]) -> Result<String> {
    Ok(md5(&serde_json::to_string(
        &elements
            .iter()
            .map(|element| {
                json!([
                    element.uid,
                    element.kind,
                    element.page_href,
                    element.signature
                ])
            })
            .collect::<Vec<_>>(),
    )?))
}

fn output_filename(title: &str, extension: &str) -> String {
    let mut name = title
        .chars()
        .map(|character| {
            if character.is_control() || r#"<>:\"/\\|?*"#.contains(character) {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    name = name
        .trim()
        .trim_start_matches('.')
        .trim_end_matches(['.', ' '])
        .to_owned();
    if name.is_empty() {
        name = "book".into();
    }
    let upper = name.to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    ) {
        name.insert(0, '_');
    }
    let maximum = 200usize.saturating_sub(extension.len() + 1).max(1);
    while name.len() > maximum {
        name.pop();
    }
    format!("{name}.{extension}")
}

fn select_title(config: &Config, original: &str, translated: Option<&str>) -> String {
    if !config.custom_title.trim().is_empty() {
        config.custom_title.trim().into()
    } else if config.translate_title {
        translated
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(original)
            .into()
    } else if config.metadata_translation
        && let Some(translated) = translated.map(str::trim).filter(|value| !value.is_empty())
    {
        match config.translation_position.as_str() {
            "only" => translated.into(),
            "above" | "left" => format!("{translated} {original}"),
            _ => format!("{original} {translated}"),
        }
    } else {
        original.into()
    }
}

fn cache_key(
    input: &Path,
    elements: &[ExtractedElement],
    config: &Config,
    glossary: &Glossary,
) -> Result<String> {
    let element_signature = element_signature(elements)?;
    let engine = config.engine_config(None);
    let payload = json!({
        "cache_version": 5, "source_content_md5": file_md5(input)?, "element_signature": element_signature,
        "engine": config.engine, "source_lang": config.source_lang, "target_lang": config.target_lang,
        "prompt": config.effective_prompt(), "model": engine.model, "base_url": engine.base_url,
        "sampling": engine.sampling, "temperature": engine.temperature, "top_p": engine.top_p, "extra": engine.extra,
        "translate_tags": config.translate_tags, "exclude_translate_tags": config.exclude_translate_tags,
        "priority_rules": config.priority_rules, "ignore_rules": config.ignore_rules,
        "reserve_rules": config.reserve_rules, "filter_rules": config.filter_rules,
        "rule_mode": config.rule_mode, "filter_scope": config.filter_scope,
        "metadata_translation": config.metadata_translation, "translate_title": config.translate_title,
        "target_lang_code": config.target_lang_code, "input_encoding": config.input_encoding,
        "glossary": glossary.pairs,
    });
    Ok(md5(&python_json(&payload)))
}

fn python_json(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(x) => x.to_string(),
        Value::Number(x) => x.to_string(),
        Value::String(x) => serde_json::to_string(x).unwrap(),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(python_json)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => {
            let mut values = values.iter().collect::<Vec<_>>();
            values.sort_by_key(|x| x.0);
            format!(
                "{{{}}}",
                values
                    .into_iter()
                    .map(|(key, value)| format!(
                        "{}: {}",
                        serde_json::to_string(key).unwrap(),
                        python_json(value)
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

fn file_md5(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut context = md5::Context::new();
    let mut buffer = [0; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        context.consume(&buffer[..read]);
    }
    Ok(format!("{:x}", context.finalize()))
}

fn collect_books(input: &Path) -> Result<Vec<PathBuf>> {
    if input.is_file() {
        return Ok(if SUPPORTED.contains(&extension(input).as_str()) {
            vec![input.to_owned()]
        } else {
            Vec::new()
        });
    }
    if !input.is_dir() {
        return Ok(Vec::new());
    }
    let mut books = fs::read_dir(input)?
        .filter_map(|x| x.ok().map(|x| x.path()))
        .filter(|x| {
            x.is_file()
                && x.file_name()
                    .is_some_and(|name| !name.to_string_lossy().starts_with('.'))
                && SUPPORTED.contains(&extension(x).as_str())
        })
        .collect::<Vec<_>>();
    books.sort();
    Ok(books)
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|x| x.to_str())
        .unwrap_or("")
        .to_lowercase()
}

fn canonical_target(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return Ok(path.canonicalize()?);
    }
    let parent = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()?;
    Ok(normalize(
        &parent.join(path.file_name().unwrap_or_default()),
    ))
}

fn validate_output_target(input: &Path, output: &Path, produced: &[PathBuf]) -> Result<()> {
    if same_file(input, output)? {
        bail!("拒绝覆盖输入文件: {}", input.display());
    }
    if produced
        .iter()
        .any(|previous| same_file(previous, output).unwrap_or(previous == output))
    {
        bail!("批量输出文件名冲突: {}", output.display());
    }
    Ok(())
}

#[cfg_attr(windows, allow(unused_variables))]
fn same_file(left: &Path, right: &Path) -> Result<bool> {
    let (Ok(left_meta), Ok(right_meta)) = (fs::metadata(left), fs::metadata(right)) else {
        return Ok(canonical_target(left)? == canonical_target(right)?);
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(left_meta.dev() == right_meta.dev() && left_meta.ino() == right_meta.ino())
    }
    #[cfg(windows)]
    {
        Ok(canonical_target(left)? == canonical_target(right)?)
    }
    #[cfg(not(any(unix, windows)))]
    {
        Ok(canonical_target(left)? == canonical_target(right)?)
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                output.pop();
            }
            Component::CurDir => {}
            other => output.push(other.as_os_str()),
        }
    }
    output
}

fn fallback_epub(output: &Path, input: &Path) -> PathBuf {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let stem = output
        .file_stem()
        .and_then(|x| x.to_str())
        .unwrap_or("book");
    for suffix in std::iter::once(".translated.epub".into())
        .chain((2..).map(|x| format!(".translated-{x}.epub")))
    {
        let candidate = parent.join(format!("{stem}{suffix}"));
        if !candidate.exists() && canonical_target(&candidate).ok() != canonical_target(input).ok()
        {
            return candidate;
        }
    }
    unreachable!()
}

fn human_size(path: &Path) -> String {
    let Ok(size) = fs::metadata(path).map(|x| x.len()) else {
        return "?".into();
    };
    if size < 1024 {
        format!("{size} B")
    } else if size < 1024 * 1024 {
        format!("{:.1} KB", size as f64 / 1024.0)
    } else {
        format!("{:.1} MB", size as f64 / 1024.0 / 1024.0)
    }
}

fn init_log(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let _ = LOG.set(Mutex::new(file));
    Ok(())
}

fn log(level: &str, message: &str) {
    let Some(file) = LOG.get() else {
        return;
    };
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if let Ok(mut file) = file.lock() {
        let _ = writeln!(file, "{timestamp} [{level}] {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::TcpListener, thread};
    use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};

    #[test]
    fn python_json_is_sorted_and_spaced_like_python() {
        assert_eq!(
            python_json(&json!({"z":[1,"中"],"a":{"b":true}})),
            r#"{"a": {"b": true}, "z": [1, "中"]}"#
        );
    }

    #[test]
    fn collect_books_is_sorted_and_filters_formats() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("b.epub"), b"").unwrap();
        fs::write(dir.path().join("a.MOBI"), b"").unwrap();
        fs::write(dir.path().join("._hidden.epub"), b"").unwrap();
        fs::write(dir.path().join("x.exe"), b"").unwrap();
        assert_eq!(
            collect_books(dir.path())
                .unwrap()
                .iter()
                .map(|x| x.file_name().unwrap().to_string_lossy())
                .collect::<Vec<_>>(),
            ["a.MOBI", "b.epub"]
        );
    }

    #[test]
    fn cache_key_v5_covers_new_translation_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.bin");
        fs::write(&source, b"book").unwrap();
        let elements = vec![ExtractedElement {
            uid: "u".into(),
            kind: "body".into(),
            raw_html: String::new(),
            original: "text".into(),
            ignored: false,
            page_href: "p".into(),
            signature: "s".into(),
        }];
        let mut config = Config {
            engine: "openai".into(),
            ..Default::default()
        };
        let mut engine = crate::config::EngineConfig {
            api_key: "x".into(),
            temperature: Some(0.1),
            top_p: Some(0.8),
            ..Default::default()
        };
        engine.extra.insert("seed".into(), json!(1));
        config.engines.insert("openai".into(), engine);
        assert_eq!(
            cache_key(&source, &elements, &config, &Glossary::default()).unwrap(),
            "1e300af0d3401bb192a4d54b680dcd3e"
        );
    }

    #[test]
    fn review_round_trip_and_filename_rules() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("book.epub");
        let review = dir.path().join("review.json");
        fs::write(&source, b"book").unwrap();
        let elements = vec![ExtractedElement {
            uid: "u".into(),
            kind: "body".into(),
            raw_html: "<p>x</p>".into(),
            original: "x".into(),
            ignored: false,
            page_href: "p.xhtml".into(),
            signature: "sig".into(),
        }];
        let cache = TranslationCache::open(Path::new("unused"), false).unwrap();
        cache.save_paragraphs(&cache_rows(&elements)).unwrap();
        export_review(&review, &source, "Book", &elements, &cache, false).unwrap();
        let mut document: ReviewDocument =
            serde_json::from_slice(&fs::read(&review).unwrap()).unwrap();
        document.paragraphs[0].translation = Some("译文".into());
        fs::write(&review, serde_json::to_vec(&document).unwrap()).unwrap();
        import_review(&review, &source, &elements, &cache, "openai", "Chinese").unwrap();
        assert_eq!(cache.all().unwrap()[0].translation.as_deref(), Some("译文"));
        document.paragraphs[0].translation = Some(String::new());
        fs::write(&review, serde_json::to_vec(&document).unwrap()).unwrap();
        assert!(import_review(&review, &source, &elements, &cache, "openai", "Chinese").is_err());
        document.paragraphs[0].translation = None;
        document.paragraphs[0].action = "retranslate".into();
        fs::write(&review, serde_json::to_vec(&document).unwrap()).unwrap();
        import_review(&review, &source, &elements, &cache, "openai", "Chinese").unwrap();
        assert!(cache.all().unwrap()[0].translation.is_none());
        document.paragraphs[0].action = "keep".into();
        document.paragraphs[0].signature = "stale".into();
        fs::write(&review, serde_json::to_vec(&document).unwrap()).unwrap();
        assert!(import_review(&review, &source, &elements, &cache, "openai", "Chinese").is_err());
        assert_eq!(output_filename("CON", "epub"), "_CON.epub");
        assert_eq!(output_filename("a/b:*?", "epub"), "a_b___.epub");
        assert_eq!(output_filename("...Title", "epub"), "Title.epub");
        assert!(output_filename(&"书".repeat(200), "epub").len() <= 200);
        let alias = dir.path().join("alias.epub");
        fs::hard_link(&source, &alias).unwrap();
        assert!(same_file(&source, &alias).unwrap());
        assert_eq!(
            select_title(
                &Config {
                    translate_title: true,
                    custom_title: "Custom".into(),
                    ..Default::default()
                },
                "Original",
                Some("Translated")
            ),
            "Custom"
        );
        assert_eq!(
            select_title(
                &Config {
                    translate_title: true,
                    ..Default::default()
                },
                "Original",
                Some("Translated")
            ),
            "Translated"
        );
        assert_eq!(
            select_title(
                &Config {
                    metadata_translation: true,
                    translation_position: "below".into(),
                    ..Default::default()
                },
                "Original",
                Some("Translated")
            ),
            "Original Translated"
        );
        assert!(
            Args::try_parse_from([
                "ebook-translator",
                "book.epub",
                "--review-export",
                "review.json"
            ])
            .is_ok()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mock_api_generates_a_complete_epub() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.epub");
        let output_dir = dir.path().join("output");
        fs::create_dir(&output_dir).unwrap();
        let file = File::create(&input).unwrap();
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        for (name, data) in [
            ("mimetype", "application/epub+zip"),
            (
                "META-INF/container.xml",
                "<container><rootfiles><rootfile full-path='content.opf'/></rootfiles></container>",
            ),
            (
                "content.opf",
                "<package><metadata><dc:title xmlns:dc='x'>Book</dc:title></metadata><manifest><item id='c' href='c.xhtml' media-type='application/xhtml+xml'/></manifest><spine><itemref idref='c'/></spine></package>",
            ),
            ("c.xhtml", "<html><body><p>Hello</p></body></html>"),
        ] {
            zip.start_file(name, options).unwrap();
            zip.write_all(data.as_bytes()).unwrap();
        }
        zip.finish().unwrap();

        let review = dir.path().join("review-export.json");
        let review_args = Args::try_parse_from([
            "ebook-translator",
            input.to_str().unwrap(),
            "--review-export",
            review.to_str().unwrap(),
        ])
        .unwrap();
        let progress = ProgressBar::hidden();
        let mut produced = Vec::new();
        assert!(
            translate_book(
                &input,
                None,
                "epub",
                &Config {
                    cache_enabled: false,
                    ..Default::default()
                },
                &Glossary::default(),
                &review_args,
                (&progress, &mut produced),
            )
            .await
            .unwrap()
        );
        assert!(review.is_file());

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let read = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..read]);
                let header_end = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|index| index + 4);
                let Some(header_end) = header_end else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + length {
                    break;
                }
            }
            let body = r#"{"choices":[{"message":{"content":"你好"},"finish_reason":"stop"}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let mut config = Config {
            cache_enabled: false,
            custom_title: "Translated Book".into(),
            ..Default::default()
        };
        config.engines.insert(
            "openai".into(),
            crate::config::EngineConfig {
                api_key: "test".into(),
                base_url: format!("http://{address}/v1"),
                model: "mock".into(),
                request_interval: 0.0,
                ..Default::default()
            },
        );
        let args = Args::try_parse_from([
            "ebook-translator",
            input.to_str().unwrap(),
            output_dir.to_str().unwrap(),
            "--force",
        ])
        .unwrap();
        assert!(
            translate_book(
                &input,
                Some(&output_dir),
                "epub",
                &config,
                &Glossary::default(),
                &args,
                (&progress, &mut produced),
            )
            .await
            .unwrap()
        );
        server.join().unwrap();
        let output = output_dir.join("Translated Book.epub");
        let mut archive = ZipArchive::new(File::open(output).unwrap()).unwrap();
        let mut html = String::new();
        archive
            .by_name("c.xhtml")
            .unwrap()
            .read_to_string(&mut html)
            .unwrap();
        assert!(html.contains("你好"));
        assert!(html.contains("et-translation"));
    }
}
