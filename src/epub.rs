use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{self, Read, Write},
    path::Path,
};

use anyhow::{Context, Result, anyhow, bail};
use dom_query::{Document, Matcher, NodeRef, SerializableNodeRef};
use html5ever::{
    QualName,
    serialize::{AttrRef, Serialize, Serializer, TraversalScope},
};
use percent_encoding::percent_decode_str;
use quick_xml::{
    Reader, Writer,
    escape::{resolve_xml_entity, unescape},
    events::{BytesText, Event},
};
use regex::{Captures, Regex};
use tempfile::NamedTempFile;
use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};

use crate::{
    cache::{Paragraph, md5},
    config::Config,
    fsutil::replace,
};

const MAX_MEMBER_SIZE: u64 = 256 * 1024 * 1024;
const MAX_TOTAL_SIZE: u64 = 4 * 1024 * 1024 * 1024;
const MAX_COMPRESSION_RATIO: u64 = 1000;
const METADATA_FIELDS: &[&str] = &[
    "title",
    "creator",
    "publisher",
    "rights",
    "subject",
    "contributor",
    "description",
];
const PRIORITY_TAGS: &[&str] = &["p", "pre", "h1", "h2", "h3", "h4", "h5", "h6", "blockquote"];
const NON_INLINE_TAGS: &[&str] = &[
    "address",
    "blockquote",
    "dialog",
    "div",
    "figure",
    "figcaption",
    "footer",
    "header",
    "legend",
    "li",
    "main",
    "p",
    "pre",
    "search",
    "article",
    "aside",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hgroup",
    "nav",
    "section",
    "dd",
    "dl",
    "dt",
    "menu",
    "ol",
    "ul",
    "table",
    "caption",
    "colgroup",
    "col",
    "thead",
    "tbody",
    "tfoot",
    "tr",
    "td",
    "th",
];
const OPAQUE_TAGS: &[&str] = &["img", "br", "hr", "code", "math", "svg", "script", "style"];
const DANGEROUS_ROOT_TAGS: &[&str] = &["script", "style", "svg", "math"];
const GROUP_TAGS: &[&str] = &["li", "th", "td", "caption"];

#[derive(Clone, Debug)]
pub struct ExtractedElement {
    pub uid: String,
    pub kind: String,
    pub raw_html: String,
    pub original: String,
    pub ignored: bool,
    pub page_href: String,
    pub signature: String,
}

#[derive(Clone, Debug, Default)]
pub struct EpubMeta {
    pub title: String,
    pub title_uid: Option<String>,
}

#[derive(Clone, Debug)]
struct ManifestItem {
    id: String,
    href: String,
    media_type: String,
    properties: HashSet<String>,
}

#[derive(Clone, Debug, Default)]
struct Package {
    manifest: Vec<ManifestItem>,
    spine: Vec<String>,
    title: String,
    spine_toc: String,
}

struct Rules {
    priority: Vec<Matcher>,
    ignore: Vec<Matcher>,
    reserve: Vec<Matcher>,
    excluded: Vec<Matcher>,
    translate_tags: HashSet<String>,
    filter: Vec<Regex>,
    filter_scope_html: bool,
}

#[derive(Clone, Copy)]
struct DomCandidate<'a> {
    node: NodeRef<'a>,
    ignored: bool,
}

pub fn extract_from_epub(
    path: &Path,
    config: &Config,
) -> Result<(Vec<ExtractedElement>, EpubMeta)> {
    let mut archive = ZipArchive::new(File::open(path)?)?;
    validate_archive(&mut archive)?;
    let opf_path = read_container(&mut archive)?;
    let opf = read_member(&mut archive, &opf_path)?;
    let package = parse_package(&opf)?;
    let rules = Rules::new(config)?;
    let names = archive
        .file_names()
        .map(str::to_owned)
        .collect::<HashSet<_>>();
    let mut elements = Vec::new();

    let metadata = extract_metadata(&opf, &opf_path, config)?;
    let title_uid = metadata
        .iter()
        .find(|element| element.raw_html == "title")
        .map(|element| element.uid.clone());
    elements.extend(metadata);

    let nav = package
        .manifest
        .iter()
        .find(|item| item.properties.contains("nav"));
    let ncx = package.manifest.iter().find(|item| {
        item.media_type
            .eq_ignore_ascii_case("application/x-dtbncx+xml")
            || (!package.spine_toc.is_empty() && item.id == package.spine_toc)
    });
    if let Some(item) = nav {
        let resolved = resolve_href(&opf_path, &item.href);
        if names.contains(&resolved) {
            elements.extend(extract_nav(
                &read_member(&mut archive, &resolved)?,
                &item.href,
            )?);
        }
    } else if let Some(item) = ncx {
        let resolved = resolve_href(&opf_path, &item.href);
        if names.contains(&resolved) {
            elements.extend(extract_ncx(
                &read_member(&mut archive, &resolved)?,
                &item.href,
            )?);
        }
    }

    let content_items = ordered_content_items(
        &package,
        nav.map(|item| item.id.as_str()),
        ncx.map(|item| item.id.as_str()),
    );
    for item in &content_items {
        if !file_selected(&item.href, &config.only_files, &config.exclude_files) {
            continue;
        }
        let resolved = resolve_href(&opf_path, &item.href);
        if !names.contains(&resolved) {
            bail!("EPUB 正文文件不存在: {}", item.href);
        }
        let data = read_member(&mut archive, &resolved)?;
        elements.extend(extract_body(&data, &item.href, &rules)?);
    }

    Ok((
        elements,
        EpubMeta {
            title: package.title,
            title_uid,
        },
    ))
}

pub fn cache_rows(elements: &[ExtractedElement]) -> Vec<Paragraph> {
    elements
        .iter()
        .map(|element| Paragraph {
            id: element.uid.clone(),
            md5: element.signature.clone(),
            raw: element.raw_html.clone(),
            original: element.original.clone(),
            ignored: element.ignored,
            attributes: Some(element.kind.clone()),
            page: Some(element.page_href.clone()),
            translation: None,
            engine_name: None,
            target_lang: None,
        })
        .collect()
}

pub fn write_translated_epub(
    input: &Path,
    output: &Path,
    translations: &HashMap<String, String>,
    config: &Config,
    expected_count: usize,
    final_title: &str,
) -> Result<usize> {
    let mut archive = ZipArchive::new(File::open(input)?)?;
    validate_archive(&mut archive)?;
    let opf_path = read_container(&mut archive)?;
    let opf = read_member(&mut archive, &opf_path)?;
    let package = parse_package(&opf)?;
    let rules = Rules::new(config)?;
    let names = archive
        .file_names()
        .map(str::to_owned)
        .collect::<HashSet<_>>();
    let nav = package
        .manifest
        .iter()
        .find(|item| item.properties.contains("nav"));
    let ncx = package.manifest.iter().find(|item| {
        item.media_type
            .eq_ignore_ascii_case("application/x-dtbncx+xml")
            || (!package.spine_toc.is_empty() && item.id == package.spine_toc)
    });
    let mut modified = HashMap::new();
    let (new_opf, mut injected) = rewrite_metadata(
        &opf,
        &opf_path,
        translations,
        &config.translation_position,
        final_title,
    )?;
    modified.insert(opf_path.clone(), new_opf);

    if let Some(item) = nav {
        let resolved = resolve_href(&opf_path, &item.href);
        if names.contains(&resolved) {
            let (data, count) = inject_nav(
                &read_member(&mut archive, &resolved)?,
                &item.href,
                translations,
                &config.translation_position,
            )?;
            modified.insert(resolved, data);
            injected += count;
        }
    } else if let Some(item) = ncx {
        let resolved = resolve_href(&opf_path, &item.href);
        if names.contains(&resolved) {
            let (data, count) = rewrite_ncx(
                &read_member(&mut archive, &resolved)?,
                &item.href,
                translations,
                &config.translation_position,
            )?;
            modified.insert(resolved, data);
            injected += count;
        }
    }

    for item in ordered_content_items(
        &package,
        nav.map(|item| item.id.as_str()),
        ncx.map(|item| item.id.as_str()),
    ) {
        if !file_selected(&item.href, &config.only_files, &config.exclude_files) {
            continue;
        }
        let resolved = resolve_href(&opf_path, &item.href);
        let (data, count) = inject_body(
            &read_member(&mut archive, &resolved)?,
            &item.href,
            translations,
            config,
            &rules,
        )?;
        modified.insert(resolved, data);
        injected += count;
    }

    if injected != expected_count {
        bail!("译文注入数量不匹配: 预期 {expected_count}, 实际 {injected}");
    }
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let temp = NamedTempFile::new_in(parent)?;
    let mut writer = ZipWriter::new(temp.reopen()?);
    for index in 0..archive.len() {
        let member = archive.by_index(index)?;
        let name = member.name().to_owned();
        if let Some(data) = modified.remove(&name) {
            let options = SimpleFileOptions::default()
                .compression_method(member.compression())
                .unix_permissions(member.unix_mode().unwrap_or(0o644));
            writer.start_file(name, options)?;
            writer.write_all(&data)?;
        } else {
            writer.raw_copy_file(member)?;
        }
    }
    writer.finish()?;
    let (_, temp_path) = temp.keep()?;
    if let Err(error) = replace(&temp_path, output) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    Ok(injected)
}

pub fn validate_markup_tokens(original: &str, translated: &str) -> Result<()> {
    let translated = normalize_markup_tokens(translated);
    ensure_markup_tokens(original, &translated)
}

pub fn accept_markup_translation(original: &str, translated: &str) -> Result<String> {
    let translated = normalize_markup_tokens(translated);
    if ensure_markup_tokens(original, &translated).is_ok() {
        return Ok(translated);
    }
    if let Some(repaired) = repair_markup_tokens(original, &translated) {
        return Ok(repaired);
    }
    bail!("译文破坏了 HTML 占位符")
}

fn ensure_markup_tokens(original: &str, translated: &str) -> Result<()> {
    let pattern = markup_token_regex();
    let mut expected = pattern
        .captures_iter(original)
        .map(|capture| capture[0].to_owned())
        .collect::<Vec<_>>();
    let mut actual = pattern
        .captures_iter(translated)
        .map(|capture| capture[0].to_owned())
        .collect::<Vec<_>>();
    expected.sort();
    actual.sort();
    if expected != actual {
        bail!("译文破坏了 HTML 占位符");
    }
    let mut stack = Vec::new();
    for capture in pattern.captures_iter(translated) {
        match &capture[1] {
            "o" => stack.push(capture[2].to_owned()),
            "c" if stack.pop().as_deref() == Some(&capture[2]) => {}
            "c" => bail!("译文 HTML 占位符嵌套无效"),
            _ => {}
        }
    }
    if !stack.is_empty() {
        bail!("译文 HTML 占位符没有闭合");
    }
    Ok(())
}

pub fn normalize_markup_tokens(value: &str) -> String {
    let pattern = Regex::new(r"\{\{\s*etm\s*_\s*([ocn])\s*_\s*((?:\d\s*)+)\}\}").unwrap();
    pattern
        .replace_all(value, |capture: &Captures<'_>| {
            let digits = capture[2]
                .chars()
                .filter(|character| character.is_ascii_digit())
                .collect::<String>();
            format!("{{{{etm_{}_{digits}}}}}", &capture[1])
        })
        .into_owned()
}

// ponytail: free models drop tokens often; rebuild from original skeleton when safe
pub fn repair_markup_tokens(original: &str, translated: &str) -> Option<String> {
    let pattern = markup_token_regex();
    if !pattern.is_match(original) {
        return None;
    }
    // only repair complete drop/rewrite cases; partial reordering is left to retry/fail
    if pattern.is_match(translated) {
        return None;
    }
    let plain = translated.trim();
    if plain.is_empty() {
        return None;
    }
    let mut output = String::new();
    let mut last = 0usize;
    let mut filled = false;
    for mat in pattern.find_iter(original) {
        let before = &original[last..mat.start()];
        if !before.trim().is_empty() {
            if !filled {
                output.push_str(plain);
                filled = true;
            }
        } else {
            output.push_str(before);
        }
        output.push_str(mat.as_str());
        last = mat.end();
    }
    let tail = &original[last..];
    if !tail.trim().is_empty() {
        if !filled {
            output.push_str(plain);
            filled = true;
        }
    } else {
        output.push_str(tail);
    }
    if !filled {
        return None;
    }
    ensure_markup_tokens(original, &output).ok()?;
    Some(output)
}

fn markup_token_regex() -> Regex {
    Regex::new(r"\{\{etm_(o|c|n)_(\d+)\}\}").unwrap()
}

impl Rules {
    fn new(config: &Config) -> Result<Self> {
        let priority = PRIORITY_TAGS
            .iter()
            .map(|selector| {
                Matcher::new(selector).map_err(|_| anyhow!("CSS selector 无效: {selector}"))
            })
            .chain(config.priority_rules.iter().map(|selector| {
                Matcher::new(selector).map_err(|_| anyhow!("CSS selector 无效: {selector}"))
            }))
            .collect::<Result<Vec<_>>>()?;
        let compile = |selectors: &[String]| {
            selectors
                .iter()
                .map(|selector| {
                    Matcher::new(selector).map_err(|_| anyhow!("CSS selector 无效: {selector}"))
                })
                .collect::<Result<Vec<_>>>()
        };
        let excluded_selectors = parse_set(&config.exclude_translate_tags)
            .into_iter()
            .collect::<Vec<_>>();
        let mut filter = vec![Regex::new(
            r#"^[-\d\s\.'\\\"‘’“”,=~!@#$%^&º*|≈<>?/`—…+:–_(){}\[\]]+$"#,
        )?];
        for rule in &config.filter_rules {
            filter.push(match config.rule_mode.as_str() {
                "normal" => Regex::new(&format!("(?i){}", regex::escape(rule)))?,
                "case" => Regex::new(&regex::escape(rule))?,
                _ => Regex::new(rule)?,
            });
        }
        Ok(Self {
            priority,
            ignore: compile(&config.ignore_rules)?,
            reserve: compile(&config.reserve_rules)?,
            excluded: compile(&excluded_selectors)?,
            translate_tags: parse_set(&config.translate_tags),
            filter,
            filter_scope_html: config.filter_scope == "html",
        })
    }

    fn matches(node: &NodeRef<'_>, matchers: &[Matcher]) -> bool {
        matchers.iter().any(|matcher| node.is_match(matcher))
    }

    fn ignored(&self, node: &NodeRef<'_>) -> bool {
        Self::matches(node, &self.ignore) || Self::matches(node, &self.excluded)
    }

    fn reserved(&self, node: &NodeRef<'_>) -> bool {
        Self::matches(node, &self.reserve)
            || Self::matches(node, &self.ignore)
            || Self::matches(node, &self.excluded)
    }

    fn filtered(&self, node: &NodeRef<'_>) -> bool {
        let text = normalize_text(&node.text());
        self.filter.iter().any(|pattern| pattern.is_match(&text))
            || (self.filter_scope_html
                && self
                    .filter
                    .iter()
                    .any(|pattern| pattern.is_match(&node.html())))
    }
}

fn validate_archive(archive: &mut ZipArchive<File>) -> Result<()> {
    let mut total = 0u64;
    let mut names = HashSet::new();
    for index in 0..archive.len() {
        let member = archive.by_index(index)?;
        if !names.insert(member.name().to_owned()) {
            bail!("EPUB 包含重复成员: {}", member.name());
        }
        if member.size() > MAX_MEMBER_SIZE {
            bail!("EPUB 成员过大: {}", member.name());
        }
        total = total.saturating_add(member.size());
        if total > MAX_TOTAL_SIZE {
            bail!("EPUB 解压后总大小过大");
        }
        if member.size() > 10 * 1024 * 1024
            && member.compressed_size() > 0
            && member.size() / member.compressed_size() > MAX_COMPRESSION_RATIO
        {
            bail!("EPUB 成员压缩比异常: {}", member.name());
        }
    }
    Ok(())
}

fn read_member(archive: &mut ZipArchive<File>, name: &str) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    archive
        .by_name(name)
        .with_context(|| format!("EPUB 成员不存在: {name}"))?
        .read_to_end(&mut data)?;
    Ok(data)
}

fn read_container(archive: &mut ZipArchive<File>) -> Result<String> {
    let data = read_member(archive, "META-INF/container.xml")?;
    let mut reader = Reader::from_reader(data.as_slice());
    loop {
        match reader.read_event()? {
            Event::Start(event) | Event::Empty(event)
                if event.local_name().as_ref() == b"rootfile" =>
            {
                for attr in event.attributes() {
                    let attr = attr?;
                    if attr.key.local_name().as_ref() == b"full-path" {
                        let path = attr.decode_and_unescape_value(reader.decoder())?;
                        return Ok(percent_decode_str(&path)
                            .decode_utf8_lossy()
                            .replace('\\', "/"));
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    bail!("No rootfile found in container.xml")
}

fn parse_package(data: &[u8]) -> Result<Package> {
    let mut reader = Reader::from_reader(data);
    let mut package = Package::default();
    let mut id_to_href = HashMap::new();
    let mut spine_ids = Vec::new();
    let mut in_title = false;
    loop {
        match reader.read_event()? {
            Event::Start(event) | Event::Empty(event) if event.local_name().as_ref() == b"item" => {
                let mut item = ManifestItem {
                    id: String::new(),
                    href: String::new(),
                    media_type: String::new(),
                    properties: HashSet::new(),
                };
                for attr in event.attributes() {
                    let attr = attr?;
                    let value = attr
                        .decode_and_unescape_value(reader.decoder())?
                        .into_owned();
                    match attr.key.local_name().as_ref() {
                        b"id" => item.id = value,
                        b"href" => item.href = value,
                        b"media-type" => item.media_type = value,
                        b"properties" => {
                            item.properties = value.split_whitespace().map(str::to_owned).collect()
                        }
                        _ => {}
                    }
                }
                id_to_href.insert(item.id.clone(), item.href.clone());
                package.manifest.push(item);
            }
            Event::Start(event) | Event::Empty(event)
                if event.local_name().as_ref() == b"itemref" =>
            {
                for attr in event.attributes() {
                    let attr = attr?;
                    if attr.key.local_name().as_ref() == b"idref" {
                        spine_ids.push(
                            attr.decode_and_unescape_value(reader.decoder())?
                                .into_owned(),
                        );
                    }
                }
            }
            Event::Start(event) if event.local_name().as_ref() == b"spine" => {
                for attr in event.attributes() {
                    let attr = attr?;
                    if attr.key.local_name().as_ref() == b"toc" {
                        package.spine_toc = attr
                            .decode_and_unescape_value(reader.decoder())?
                            .into_owned();
                    }
                }
            }
            Event::Start(event)
                if event.local_name().as_ref() == b"title" && package.title.is_empty() =>
            {
                in_title = true
            }
            Event::Text(text) if in_title => {
                package
                    .title
                    .push_str(&decode_xml_text(text.decode()?.as_ref())?);
            }
            Event::CData(text) if in_title => package.title.push_str(text.decode()?.as_ref()),
            Event::GeneralRef(reference) if in_title => {
                push_general_ref(&mut package.title, &reference)?;
            }
            Event::End(event) if event.local_name().as_ref() == b"title" => {
                in_title = false;
                package.title = normalize_text(&package.title);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    package.spine = spine_ids
        .into_iter()
        .filter_map(|id| id_to_href.get(&id).cloned())
        .collect();
    Ok(package)
}

fn ordered_content_items<'a>(
    package: &'a Package,
    nav_id: Option<&str>,
    ncx_id: Option<&str>,
) -> Vec<&'a ManifestItem> {
    let is_content = |item: &&ManifestItem| {
        Some(item.id.as_str()) != nav_id
            && Some(item.id.as_str()) != ncx_id
            && !item
                .media_type
                .eq_ignore_ascii_case("application/x-dtbncx+xml")
            && (item.media_type.contains("html")
                || matches!(item.media_type.as_str(), "application/xml" | "text/xml")
                || [".xhtml", ".html", ".htm", ".xht", ".xml"]
                    .iter()
                    .any(|suffix| item.href.to_ascii_lowercase().ends_with(suffix)))
    };
    let mut ordered = Vec::new();
    let mut seen = HashSet::new();
    for href in &package.spine {
        if let Some(item) = package
            .manifest
            .iter()
            .find(|item| item.href == *href)
            .filter(is_content)
            && seen.insert(item.href.as_str())
        {
            ordered.push(item);
        }
    }
    let mut remaining = package
        .manifest
        .iter()
        .filter(|item| !seen.contains(item.href.as_str()))
        .filter(is_content)
        .collect::<Vec<_>>();
    remaining.sort_by(|left, right| left.href.cmp(&right.href));
    ordered.extend(remaining);
    ordered
}

fn extract_metadata(data: &[u8], resource: &str, config: &Config) -> Result<Vec<ExtractedElement>> {
    let mut reader = Reader::from_reader(data);
    let mut current: Option<(String, usize, String)> = None;
    let mut occurrences = HashMap::<String, usize>::new();
    let mut output = Vec::new();
    loop {
        match reader.read_event()? {
            Event::Start(event) => {
                let name = String::from_utf8_lossy(event.local_name().as_ref()).to_lowercase();
                if current.is_none() && METADATA_FIELDS.contains(&name.as_str()) {
                    let index = *occurrences.entry(name.clone()).or_default();
                    current = Some((name, index, String::new()));
                }
            }
            Event::Text(text) => {
                if let Some((_, _, value)) = &mut current {
                    value.push_str(&decode_xml_text(text.decode()?.as_ref())?);
                }
            }
            Event::CData(text) => {
                if let Some((_, _, value)) = &mut current {
                    value.push_str(text.decode()?.as_ref());
                }
            }
            Event::GeneralRef(reference) => {
                if let Some((_, _, value)) = &mut current {
                    push_general_ref(value, &reference)?;
                }
            }
            Event::End(event) => {
                let name = String::from_utf8_lossy(event.local_name().as_ref()).to_lowercase();
                if current.as_ref().is_some_and(|(field, _, _)| *field == name) {
                    let (field, index, value) = current.take().unwrap();
                    *occurrences.entry(field.clone()).or_default() += 1;
                    let value = normalize_text(&value);
                    if !value.is_empty() {
                        let ignored = if field == "title" {
                            !(config.metadata_translation || config.translate_title)
                        } else {
                            !config.metadata_translation
                        };
                        output.push(make_element(
                            "metadata",
                            resource,
                            &format!("{field}:{index}"),
                            field,
                            value,
                            ignored,
                        ));
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(output)
}

fn extract_ncx(data: &[u8], resource: &str) -> Result<Vec<ExtractedElement>> {
    let mut reader = Reader::from_reader(data);
    let mut in_label = false;
    let mut in_text = false;
    let mut value = String::new();
    let mut index = 0usize;
    let mut output = Vec::new();
    loop {
        match reader.read_event()? {
            Event::Start(event) if event.local_name().as_ref() == b"navLabel" => in_label = true,
            Event::Start(event) if in_label && event.local_name().as_ref() == b"text" => {
                in_text = true;
                value.clear();
            }
            Event::Text(text) if in_text => {
                value.push_str(&decode_xml_text(text.decode()?.as_ref())?)
            }
            Event::CData(text) if in_text => value.push_str(text.decode()?.as_ref()),
            Event::GeneralRef(reference) if in_text => push_general_ref(&mut value, &reference)?,
            Event::End(event) if in_text && event.local_name().as_ref() == b"text" => {
                let value = normalize_text(&value);
                if !value.is_empty() {
                    output.push(make_element(
                        "toc",
                        resource,
                        &index.to_string(),
                        "text".into(),
                        value,
                        false,
                    ));
                    index += 1;
                }
                in_text = false;
            }
            Event::End(event) if event.local_name().as_ref() == b"navLabel" => in_label = false,
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(output)
}

fn extract_nav(data: &[u8], resource: &str) -> Result<Vec<ExtractedElement>> {
    let document = parse_page(data)?;
    Ok(nav_nodes(&document)
        .into_iter()
        .enumerate()
        .filter_map(|(index, node)| {
            let value = normalize_text(&node.text());
            (!value.is_empty()).then(|| {
                make_element(
                    "toc",
                    resource,
                    &index.to_string(),
                    node.html().to_string(),
                    value,
                    false,
                )
            })
        })
        .collect())
}

fn extract_body(data: &[u8], resource: &str, rules: &Rules) -> Result<Vec<ExtractedElement>> {
    let document = parse_page(data)?;
    let Some(body) = document.body() else {
        return Ok(Vec::new());
    };
    if rules.ignored(&body) {
        return Ok(Vec::new());
    }
    let mut candidates = Vec::new();
    collect_candidates(&body, rules, &mut candidates);
    Ok(candidates
        .into_iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            let visible = normalize_text(&candidate.node.text());
            if visible.is_empty() {
                return None;
            }
            let original = tokenize(&candidate.node, rules, true).ok()?;
            if original.is_empty() {
                return None;
            }
            let ignored = candidate.ignored
                || rules.filtered(&candidate.node)
                || is_non_translatable(&visible);
            Some(make_element(
                "body",
                resource,
                &index.to_string(),
                candidate.node.html().to_string(),
                original,
                ignored,
            ))
        })
        .collect())
}

fn collect_candidates<'a>(root: &NodeRef<'a>, rules: &Rules, output: &mut Vec<DomCandidate<'a>>) {
    for node in root.element_children() {
        let name = node.node_name().unwrap_or_default().to_ascii_lowercase();
        if DANGEROUS_ROOT_TAGS.contains(&name.as_str()) {
            continue;
        }
        let ignored = rules.ignored(&node);
        if ignored {
            if !normalize_text(&node.text()).is_empty() {
                output.push(DomCandidate {
                    node,
                    ignored: true,
                });
            }
            continue;
        }
        if !rules.translate_tags.is_empty() && !rules.translate_tags.contains(&name) {
            collect_candidates(&node, rules, output);
            continue;
        }
        let priority = Rules::matches(&node, &rules.priority);
        let inline_only = node.element_children().iter().all(|child| {
            child
                .node_name()
                .is_none_or(|name| !NON_INLINE_TAGS.contains(&name.as_ref()))
        });
        if priority || inline_only || !normalize_text(&node.immediate_text()).is_empty() {
            output.push(DomCandidate {
                node,
                ignored: false,
            });
        } else {
            collect_candidates(&node, rules, output);
        }
    }
}

fn tokenize(node: &NodeRef<'_>, rules: &Rules, keep_ids: bool) -> Result<String> {
    fn walk(
        node: &NodeRef<'_>,
        rules: &Rules,
        keep_ids: bool,
        index: &mut usize,
        output: &mut String,
    ) {
        for child in node.children_it(false) {
            if child.is_text() {
                output.push_str(&child.text());
            } else if child.is_element() {
                let name = child.node_name().unwrap_or_default().to_ascii_lowercase();
                if OPAQUE_TAGS.contains(&name.as_str()) || rules.reserved(&child) {
                    output.push_str(&format!("{{{{etm_n_{:05}}}}}", *index));
                    *index += 1;
                } else {
                    let current = *index;
                    *index += 1;
                    output.push_str(&format!("{{{{etm_o_{current:05}}}}}"));
                    walk(&child, rules, keep_ids, index, output);
                    output.push_str(&format!("{{{{etm_c_{current:05}}}}}"));
                }
            } else if !child.html().is_empty() {
                output.push_str(&format!("{{{{etm_n_{:05}}}}}", *index));
                *index += 1;
            }
        }
        let _ = keep_ids;
    }
    let mut output = String::new();
    walk(node, rules, keep_ids, &mut 0, &mut output);
    Ok(normalize_text(&output))
}

fn restore_markup(
    node: &NodeRef<'_>,
    translated: &str,
    rules: &Rules,
    keep_ids: bool,
) -> Result<String> {
    let original = tokenize(node, rules, keep_ids)?;
    let translated = normalize_markup_tokens(translated);
    validate_markup_tokens(&original, &translated)?;
    let mut literals = HashMap::new();
    fn collect(
        node: &NodeRef<'_>,
        rules: &Rules,
        keep_ids: bool,
        index: &mut usize,
        literals: &mut HashMap<String, String>,
    ) {
        for child in node.children_it(false) {
            if child.is_text() {
                continue;
            }
            if child.is_element() {
                let name = child.node_name().unwrap_or_default().to_ascii_lowercase();
                if OPAQUE_TAGS.contains(&name.as_str()) || rules.reserved(&child) {
                    literals.insert(
                        format!("{{{{etm_n_{:05}}}}}", *index),
                        sanitize_ids(&child.html(), keep_ids),
                    );
                    *index += 1;
                } else {
                    let current = *index;
                    *index += 1;
                    literals.insert(
                        format!("{{{{etm_o_{current:05}}}}}"),
                        start_tag(&child, keep_ids, None, None, None),
                    );
                    collect(&child, rules, keep_ids, index, literals);
                    literals.insert(
                        format!("{{{{etm_c_{current:05}}}}}"),
                        format!("</{}>", child.node_name().unwrap_or_default()),
                    );
                }
            } else if !child.html().is_empty() {
                literals.insert(
                    format!("{{{{etm_n_{:05}}}}}", *index),
                    child.html().to_string(),
                );
                *index += 1;
            }
        }
    }
    collect(node, rules, keep_ids, &mut 0, &mut literals);
    let token = Regex::new(r"\{\{etm_[ocn]_\d+\}\}")?;
    let mut output = String::new();
    let mut end = 0;
    for found in token.find_iter(&translated) {
        output.push_str(&escape_html_text(&translated[end..found.start()]));
        output.push_str(
            literals
                .get(found.as_str())
                .ok_or_else(|| anyhow!("未知 HTML 占位符"))?,
        );
        end = found.end();
    }
    output.push_str(&escape_html_text(&translated[end..]));
    Ok(output)
}

fn inject_body(
    data: &[u8],
    resource: &str,
    translations: &HashMap<String, String>,
    config: &Config,
    rules: &Rules,
) -> Result<(Vec<u8>, usize)> {
    let document = parse_page(data)?;
    let Some(body) = document.body() else {
        return Ok((data.to_vec(), 0));
    };
    if rules.ignored(&body) {
        return Ok((data.to_vec(), 0));
    }
    let mut candidates = Vec::new();
    collect_candidates(&body, rules, &mut candidates);
    let mut injected = 0;
    for (index, candidate) in candidates.into_iter().enumerate() {
        let uid = make_uid("body", resource, &index.to_string());
        let Some(translation) = translations.get(&uid) else {
            continue;
        };
        if translation.trim().is_empty() {
            bail!("空译文不能写入 EPUB: {uid}");
        }
        let keep_ids = config.translation_position == "only";
        let restored = restore_markup(&candidate.node, translation, rules, keep_ids)?;
        inject_translation(&candidate.node, &restored, config)?;
        injected += 1;
    }
    if injected == 0 {
        return Ok((data.to_vec(), 0));
    }
    Ok((serialize_xhtml(&document, data)?, injected))
}

fn inject_translation(node: &NodeRef<'_>, restored: &str, config: &Config) -> Result<()> {
    let position = config.translation_position.as_str();
    let name = node.node_name().unwrap_or_default().to_ascii_lowercase();
    if position == "only" {
        let translated = element_html(node, restored, config, true, false);
        node.replace_with_html(translated);
        return Ok(());
    }
    if !config.original_color.is_empty() {
        set_original_color(node, &config.original_color);
    }
    if matches!(position, "above" | "below")
        && let Some(inner) = interleave_line_breaks(node, restored, position, config)
    {
        node.set_html(inner);
        return Ok(());
    }
    if GROUP_TAGS.contains(&name.as_str()) {
        let span = translation_span(restored, config);
        match position {
            "above" => node.prepend_html(format!("{span}<br>")),
            "below" => node.append_html(format!("<br>{span}")),
            "left" => node.prepend_html(format!("{span}&nbsp;")),
            _ => node.append_html(format!("&nbsp;{span}")),
        }
        return Ok(());
    }
    let translated = element_html(node, restored, config, false, true);
    match position {
        "above" => node.before_html(translated),
        "below" => node.after_html(translated),
        "left" | "right" => {
            let original = node.html().to_string();
            node.replace_with_html(column_table(&original, &translated, position, config));
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn element_html(
    node: &NodeRef<'_>,
    inner: &str,
    config: &Config,
    keep_id: bool,
    block: bool,
) -> String {
    let mut style = String::new();
    if block {
        style.push_str("display:block");
    }
    if !config.translation_color.is_empty() {
        append_style(&mut style, &format!("color:{}", config.translation_color));
    }
    if !config.translation_style.is_empty() {
        append_style(&mut style, &config.translation_style);
    }
    let lang = target_lang_code(config);
    let direction = target_direction(config, lang.as_deref());
    let start = start_tag(
        node,
        keep_id,
        Some("et-translation"),
        Some(&style),
        Some((lang.as_deref(), direction.as_str())),
    );
    format!("{start}{inner}</{}>", node.node_name().unwrap_or_default())
}

fn translation_span(inner: &str, config: &Config) -> String {
    let lang = target_lang_code(config);
    let direction = target_direction(config, lang.as_deref());
    let mut attrs = format!(
        " class=\"et-translation\" dir=\"{}\"",
        escape_attr(&direction)
    );
    if let Some(lang) = lang {
        attrs.push_str(&format!(" lang=\"{}\"", escape_attr(&lang)));
    }
    let mut style = String::new();
    if !config.translation_color.is_empty() {
        append_style(&mut style, &format!("color:{}", config.translation_color));
    }
    if !config.translation_style.is_empty() {
        append_style(&mut style, &config.translation_style);
    }
    if !style.is_empty() {
        attrs.push_str(&format!(" style=\"{}\"", escape_attr(&style)));
    }
    format!("<span{attrs}>{inner}</span>")
}

fn column_table(original: &str, translated: &str, position: &str, config: &Config) -> String {
    let (left, right) = if position == "left" {
        (translated, original)
    } else {
        (original, translated)
    };
    let (left_width, middle_width, right_width, gap) = if config.column_gap.kind == "space_count" {
        (
            "50%".into(),
            String::new(),
            "50%".into(),
            "&nbsp;".repeat(config.column_gap.space_count),
        )
    } else {
        let side =
            (100usize.saturating_sub(config.column_gap.percentage) as f64 / 2.0).round() as usize;
        (
            format!("{side}%"),
            format!("{}%", config.column_gap.percentage),
            format!("{side}%"),
            String::new(),
        )
    };
    format!(
        "<table class=\"et-translation-table\" width=\"100%\"><tbody><tr><td width=\"{left_width}\" valign=\"top\">{left}</td><td{}>{gap}</td><td width=\"{right_width}\" valign=\"top\">{right}</td></tr></tbody></table>",
        if middle_width.is_empty() {
            String::new()
        } else {
            format!(" width=\"{middle_width}\"")
        }
    )
}

fn interleave_line_breaks(
    node: &NodeRef<'_>,
    translated: &str,
    position: &str,
    config: &Config,
) -> Option<String> {
    let breaks = Regex::new(r"(?i)<br\s*/?>").unwrap();
    let original = node.inner_html();
    let original_lines = breaks.split(&original).collect::<Vec<_>>();
    let translated_lines = breaks.split(translated).collect::<Vec<_>>();
    if original_lines.len() <= 1
        || original_lines.len() != translated_lines.len()
        || translated_lines.iter().any(|line| !balanced_fragment(line))
    {
        return None;
    }
    let mut output = String::new();
    for (index, (original, translated)) in
        original_lines.into_iter().zip(translated_lines).enumerate()
    {
        if index > 0 {
            output.push_str("<br>");
        }
        if position == "above" && !translated.trim().is_empty() {
            output.push_str(&translation_span(translated, config));
            output.push_str("<br>");
        }
        output.push_str(original);
        if position == "below" && !translated.trim().is_empty() {
            output.push_str("<br>");
            output.push_str(&translation_span(translated, config));
        }
    }
    Some(output)
}

fn balanced_fragment(value: &str) -> bool {
    let tags = Regex::new(r"(?is)<\s*(/?)\s*([A-Za-z][\w:-]*)\b[^>]*>").unwrap();
    let mut stack = Vec::new();
    for capture in tags.captures_iter(value) {
        let name = capture[2].to_ascii_lowercase();
        let whole = &capture[0];
        if capture
            .get(1)
            .is_some_and(|value| !value.as_str().is_empty())
        {
            if stack.pop().as_deref() != Some(name.as_str()) {
                return false;
            }
        } else if !whole.trim_end().ends_with("/>")
            && !matches!(
                name.as_str(),
                "br" | "hr" | "img" | "input" | "meta" | "link"
            )
        {
            stack.push(name);
        }
    }
    stack.is_empty()
}

fn set_original_color(node: &NodeRef<'_>, color: &str) {
    fn apply(node: &NodeRef<'_>, color: &str) {
        let mut style = node
            .attr("style")
            .map_or_else(String::new, |value| value.to_string());
        append_style(&mut style, &format!("color:{color}"));
        node.set_attr("style", &style);
        for child in node.element_children() {
            apply(&child, color);
        }
    }
    apply(node, color);
}

fn start_tag(
    node: &NodeRef<'_>,
    keep_id: bool,
    extra_class: Option<&str>,
    extra_style: Option<&str>,
    language: Option<(Option<&str>, &str)>,
) -> String {
    let name = node.node_name().unwrap_or_default();
    let mut output = format!("<{name}");
    let mut class = String::new();
    let mut style = String::new();
    for attr in node.attrs() {
        let local = attr.name.local.as_ref();
        if local == "id" && !keep_id {
            continue;
        }
        if local == "class" {
            class = attr.value.to_string();
            continue;
        }
        if local == "style" {
            style = attr.value.to_string();
            continue;
        }
        if language.is_some() && matches!(local, "lang" | "dir") {
            continue;
        }
        let attr_name = attr
            .name
            .prefix
            .as_ref()
            .map_or_else(|| local.to_owned(), |prefix| format!("{prefix}:{local}"));
        output.push_str(&format!(" {attr_name}=\"{}\"", escape_attr(&attr.value)));
    }
    if let Some(extra) = extra_class
        && !class.split_whitespace().any(|value| value == extra)
    {
        if !class.is_empty() {
            class.push(' ');
        }
        class.push_str(extra);
    }
    if !class.is_empty() {
        output.push_str(&format!(" class=\"{}\"", escape_attr(&class)));
    }
    if let Some(extra) = extra_style.filter(|value| !value.is_empty()) {
        append_style(&mut style, extra);
    }
    if !style.is_empty() {
        output.push_str(&format!(" style=\"{}\"", escape_attr(&style)));
    }
    if let Some((lang, direction)) = language {
        if let Some(lang) = lang {
            output.push_str(&format!(" lang=\"{}\"", escape_attr(lang)));
        }
        output.push_str(&format!(" dir=\"{}\"", escape_attr(direction)));
    }
    output.push('>');
    output
}

fn rewrite_metadata(
    data: &[u8],
    resource: &str,
    translations: &HashMap<String, String>,
    position: &str,
    final_title: &str,
) -> Result<(Vec<u8>, usize)> {
    let mut reader = Reader::from_reader(data);
    let mut writer = Writer::new(Vec::with_capacity(data.len()));
    let mut occurrences = HashMap::<String, usize>::new();
    let mut capture: Option<XmlCapture> = None;
    let mut injected = 0;
    loop {
        let event = reader.read_event()?.into_owned();
        if let Some(active) = &mut capture {
            active.push(&event)?;
            if active.depth == 0 {
                let active = capture.take().unwrap();
                let uid = make_uid(
                    "metadata",
                    resource,
                    &format!("{}:{}", active.name, active.index),
                );
                let translation = translations.get(&uid);
                if translation.is_some() {
                    injected += 1;
                }
                let replacement = if active.name == "title" && active.index == 0 {
                    Some(final_title.to_owned())
                } else {
                    translation.map(|translation| {
                        combine_plain(&normalize_text(&active.text), translation, position)
                    })
                };
                active.write(&mut writer, replacement.as_deref())?;
            }
            continue;
        }
        match &event {
            Event::Start(start) => {
                let name = String::from_utf8_lossy(start.local_name().as_ref()).to_lowercase();
                if METADATA_FIELDS.contains(&name.as_str()) {
                    let index = *occurrences.entry(name.clone()).or_default();
                    *occurrences.entry(name.clone()).or_default() += 1;
                    capture = Some(XmlCapture::new(name, index, event));
                    continue;
                }
            }
            Event::Eof => break,
            _ => {}
        }
        writer.write_event(event)?;
    }
    if capture.is_some() {
        bail!("OPF 元数据标签未闭合");
    }
    Ok((writer.into_inner(), injected))
}

fn rewrite_ncx(
    data: &[u8],
    resource: &str,
    translations: &HashMap<String, String>,
    position: &str,
) -> Result<(Vec<u8>, usize)> {
    let mut reader = Reader::from_reader(data);
    let mut writer = Writer::new(Vec::with_capacity(data.len()));
    let mut in_label = false;
    let mut capture: Option<XmlCapture> = None;
    let mut index = 0usize;
    let mut injected = 0;
    loop {
        let event = reader.read_event()?.into_owned();
        if let Some(active) = &mut capture {
            active.push(&event)?;
            if active.depth == 0 {
                let active = capture.take().unwrap();
                let original = normalize_text(&active.text);
                if original.is_empty() {
                    active.write(&mut writer, None)?;
                } else {
                    let uid = make_uid("toc", resource, &index.to_string());
                    index += 1;
                    if let Some(translation) = translations.get(&uid) {
                        injected += 1;
                        let value = combine_plain(&original, translation, position);
                        active.write(&mut writer, Some(&value))?;
                    } else {
                        active.write(&mut writer, None)?;
                    }
                }
            }
            continue;
        }
        match &event {
            Event::Start(start) if start.local_name().as_ref() == b"navLabel" => in_label = true,
            Event::Start(start) if in_label && start.local_name().as_ref() == b"text" => {
                capture = Some(XmlCapture::new("text".into(), index, event));
                continue;
            }
            Event::End(end) if end.local_name().as_ref() == b"navLabel" => in_label = false,
            Event::Eof => break,
            _ => {}
        }
        writer.write_event(event)?;
    }
    if capture.is_some() {
        bail!("NCX text 标签未闭合");
    }
    Ok((writer.into_inner(), injected))
}

struct XmlCapture {
    name: String,
    index: usize,
    depth: usize,
    text: String,
    events: Vec<Event<'static>>,
}

impl XmlCapture {
    fn new(name: String, index: usize, start: Event<'static>) -> Self {
        Self {
            name,
            index,
            depth: 1,
            text: String::new(),
            events: vec![start],
        }
    }

    fn push(&mut self, event: &Event<'static>) -> Result<()> {
        match event {
            Event::Start(_) => self.depth += 1,
            Event::End(_) => self.depth = self.depth.saturating_sub(1),
            Event::Text(text) => self
                .text
                .push_str(&decode_xml_text(text.decode()?.as_ref())?),
            Event::CData(text) => self.text.push_str(text.decode()?.as_ref()),
            Event::GeneralRef(reference) => push_general_ref(&mut self.text, reference)?,
            _ => {}
        }
        self.events.push(event.clone());
        Ok(())
    }

    fn write(self, writer: &mut Writer<Vec<u8>>, replacement: Option<&str>) -> Result<()> {
        if let Some(replacement) = replacement {
            let mut events = self.events.into_iter();
            let start = events.next().ok_or_else(|| anyhow!("XML capture 为空"))?;
            let end = events.last().ok_or_else(|| anyhow!("XML 标签未闭合"))?;
            writer.write_event(start)?;
            writer.write_event(Event::Text(BytesText::new(replacement)))?;
            writer.write_event(end)?;
        } else {
            for event in self.events {
                writer.write_event(event)?;
            }
        }
        Ok(())
    }
}

fn inject_nav(
    data: &[u8],
    resource: &str,
    translations: &HashMap<String, String>,
    position: &str,
) -> Result<(Vec<u8>, usize)> {
    let document = parse_page(data)?;
    let nodes = nav_nodes(&document);
    let mut injected = 0;
    for (index, node) in nodes.into_iter().enumerate() {
        let uid = make_uid("toc", resource, &index.to_string());
        if let Some(translation) = translations.get(&uid) {
            let original = normalize_text(&node.text());
            node.set_text(combine_plain(&original, translation, position));
            injected += 1;
        }
    }
    if injected == 0 {
        return Ok((data.to_vec(), 0));
    }
    Ok((serialize_xhtml(&document, data)?, injected))
}

fn nav_nodes<'a>(document: &'a Document) -> Vec<NodeRef<'a>> {
    document
        .select("nav a, nav span")
        .nodes()
        .iter()
        .copied()
        .filter(|node| {
            !node.ancestors_it(None).any(|parent| {
                parent
                    .node_name()
                    .is_some_and(|name| matches!(name.as_ref(), "a" | "span"))
            })
        })
        .collect()
}

fn make_element(
    kind: &str,
    resource: &str,
    key: &str,
    raw_html: String,
    original: String,
    ignored: bool,
) -> ExtractedElement {
    let uid = make_uid(kind, resource, key);
    let signature = md5(&format!(
        "v2\0{kind}\0{resource}\0{key}\0{raw_html}\0{original}"
    ));
    ExtractedElement {
        uid,
        kind: kind.into(),
        raw_html,
        original,
        ignored,
        page_href: resource.into(),
        signature,
    }
}

fn make_uid(kind: &str, resource: &str, key: &str) -> String {
    md5(&format!("v2:{kind}:{resource}:{key}"))
}

fn file_selected(href: &str, only_files: &str, exclude_files: &str) -> bool {
    let basename = href.rsplit('/').next().unwrap_or(href).to_ascii_lowercase();
    let href = href.to_ascii_lowercase();
    let only = parse_set(only_files);
    let excluded = parse_set(exclude_files);
    (only.is_empty() || only.contains(&basename) || only.contains(&href))
        && !excluded.contains(&basename)
        && !excluded.contains(&href)
}

pub fn resolve_href(base: &str, href: &str) -> String {
    let base = base.replace('\\', "/");
    let base_dir = base.rsplit_once('/').map(|value| value.0).unwrap_or("");
    let href = href
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .replace('\\', "/");
    let decoded = percent_decode_str(&href).decode_utf8_lossy();
    let joined = format!("{base_dir}/{decoded}");
    let mut parts = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/")
}

fn parse_page(data: &[u8]) -> Result<Document> {
    Ok(Document::from(strip_doctype(&decode_page(data)?)))
}

fn strip_doctype(text: &str) -> String {
    let Some((start, end)) = doctype_range(text) else {
        return text.into();
    };
    let mut output = text.to_owned();
    output.replace_range(start..end, "");
    output
}

fn doctype_range(text: &str) -> Option<(usize, usize)> {
    let start = text.to_ascii_lowercase().find("<!doctype")?;
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut quote = None;
    for (index, byte) in bytes.iter().copied().enumerate().skip(start) {
        if quote == Some(byte) {
            quote = None;
            continue;
        }
        if quote.is_none() && matches!(byte, b'\'' | b'\"') {
            quote = Some(byte);
            continue;
        }
        if quote.is_none() {
            if byte == b'[' {
                depth += 1;
            }
            if byte == b']' {
                depth = depth.saturating_sub(1);
            }
            if byte == b'>' && depth == 0 {
                return Some((start, index + 1));
            }
        }
    }
    Some((start, text.len()))
}

fn decode_page(data: &[u8]) -> Result<String> {
    if let Some(bytes) = data.strip_prefix(&[0xff, 0xfe]) {
        if bytes.len() % 2 != 0 {
            bail!("UTF-16LE XHTML 字节数无效");
        }
        let units = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16(&units).context("UTF-16LE XHTML 无效");
    }
    if let Some(bytes) = data.strip_prefix(&[0xfe, 0xff]) {
        if bytes.len() % 2 != 0 {
            bail!("UTF-16BE XHTML 字节数无效");
        }
        let units = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16(&units).context("UTF-16BE XHTML 无效");
    }
    Ok(
        std::str::from_utf8(data.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(data))
            .context("XHTML 不是有效的 UTF-8/UTF-16")?
            .to_owned(),
    )
}

fn serialize_xhtml(document: &Document, original: &[u8]) -> Result<Vec<u8>> {
    let original = decode_page(original)?;
    let mut output = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n".to_vec();
    if let Some((start, end)) = doctype_range(&original) {
        output.extend_from_slice(&original.as_bytes()[start..end]);
        output.push(b'\n');
    }
    let node: SerializableNodeRef<'_> = document.root().into();
    node.serialize(&mut XmlSerializer(&mut output), TraversalScope::IncludeNode)?;
    // Validate the exact bytes written before placing them back in the EPUB.
    let mut reader = Reader::from_reader(output.as_slice());
    loop {
        if matches!(reader.read_event()?, Event::Eof) {
            break;
        }
    }
    Ok(output)
}

struct XmlSerializer<W: Write>(W);

impl<W: Write> XmlSerializer<W> {
    fn name(&mut self, name: &QualName) -> io::Result<()> {
        if let Some(prefix) = &name.prefix {
            write!(self.0, "{prefix}:")?;
        }
        self.0.write_all(name.local.as_bytes())
    }

    fn escaped(&mut self, value: &str, attribute: bool) -> io::Result<()> {
        for character in value.chars() {
            match character {
                '&' => self.0.write_all(b"&amp;")?,
                '<' => self.0.write_all(b"&lt;")?,
                '>' => self.0.write_all(b"&gt;")?,
                '"' if attribute => self.0.write_all(b"&quot;")?,
                _ => write!(self.0, "{character}")?,
            }
        }
        Ok(())
    }
}

impl<W: Write> Serializer for XmlSerializer<W> {
    fn start_elem<'a, A>(&mut self, name: QualName, attrs: A) -> io::Result<()>
    where
        A: Iterator<Item = AttrRef<'a>>,
    {
        self.0.write_all(b"<")?;
        self.name(&name)?;
        for (name, value) in attrs {
            self.0.write_all(b" ")?;
            self.name(name)?;
            self.0.write_all(b"=\"")?;
            self.escaped(value, true)?;
            self.0.write_all(b"\"")?;
        }
        self.0.write_all(b">")
    }

    fn end_elem(&mut self, name: QualName) -> io::Result<()> {
        self.0.write_all(b"</")?;
        self.name(&name)?;
        self.0.write_all(b">")
    }

    fn write_text(&mut self, text: &str) -> io::Result<()> {
        self.escaped(text, false)
    }

    fn write_comment(&mut self, text: &str) -> io::Result<()> {
        write!(self.0, "<!--{text}-->")
    }

    fn write_doctype(&mut self, name: &str) -> io::Result<()> {
        write!(self.0, "<!DOCTYPE {name}>")
    }

    fn write_processing_instruction(&mut self, target: &str, data: &str) -> io::Result<()> {
        write!(self.0, "<?{target} {data}?>")
    }
}

pub fn is_non_translatable(text: &str) -> bool {
    let value = text.trim();
    if value.is_empty() {
        return true;
    }
    Regex::new(r"(?i)^(?:https?://|www\.)\S+$")
        .unwrap()
        .is_match(value)
        || Regex::new(r"(?i)^ISBN(?:-1[03])?\s*:?\s*[0-9X][0-9X\-\s]{8,}$")
            .unwrap()
            .is_match(value)
        || Regex::new(r"(?i)^(?:Figure|Fig\.?|Table)\s*[:#.]?\s*[A-Z0-9]+(?:[.\-][A-Z0-9]+)*[.:]?$")
            .unwrap()
            .is_match(value)
        || (value.chars().any(|character| character.is_ascii_digit())
            && Regex::new(r"^[\d,.\-+eE\s%]+$").unwrap().is_match(value))
}

fn parse_set(value: &str) -> HashSet<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn normalize_text(value: &str) -> String {
    value
        .replace(['\u{00a0}', '\u{3000}'], " ")
        .replace(['\u{200b}', '\u{feff}'], "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn decode_xml_text(value: &str) -> Result<String> {
    Ok(unescape(value)?.into_owned())
}

fn push_general_ref(
    output: &mut String,
    reference: &quick_xml::events::BytesRef<'_>,
) -> Result<()> {
    if let Some(character) = reference.resolve_char_ref()? {
        output.push(character);
        return Ok(());
    }
    let name = reference.decode()?;
    if let Some(value) = resolve_xml_entity(&name) {
        output.push_str(value);
    } else if name == "nbsp" {
        output.push('\u{00a0}');
    } else {
        output.push('&');
        output.push_str(&name);
        output.push(';');
    }
    Ok(())
}

fn combine_plain(original: &str, translation: &str, position: &str) -> String {
    match position {
        "only" => translation.trim().into(),
        "above" | "left" => format!("{} {}", translation.trim(), original.trim()),
        _ => format!("{} {}", original.trim(), translation.trim()),
    }
}

fn escape_html_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace("\r\n", "<br>")
        .replace(['\r', '\n'], "<br>")
}

fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn sanitize_ids(value: &str, keep_ids: bool) -> String {
    if keep_ids {
        return value.into();
    }
    let mut reader = Reader::from_str(value);
    let mut writer = Writer::new(Vec::with_capacity(value.len()));
    loop {
        let event = match reader.read_event() {
            Ok(Event::Start(start)) => Event::Start(without_id(start)),
            Ok(Event::Empty(start)) => Event::Empty(without_id(start)),
            Ok(Event::Eof) => break,
            Ok(event) => event.into_owned(),
            Err(_) => return value.into(),
        };
        if writer.write_event(event).is_err() {
            return value.into();
        }
    }
    String::from_utf8(writer.into_inner()).unwrap_or_else(|_| value.into())
}

fn without_id(start: quick_xml::events::BytesStart<'_>) -> quick_xml::events::BytesStart<'static> {
    let attributes = start
        .attributes()
        .filter_map(|attribute| attribute.ok())
        .filter(|attribute| {
            !attribute
                .key
                .local_name()
                .as_ref()
                .eq_ignore_ascii_case(b"id")
        })
        .map(|attribute| {
            (
                attribute.key.as_ref().to_vec(),
                attribute.value.into_owned(),
            )
        })
        .collect::<Vec<_>>();
    let mut start = start.into_owned();
    start.clear_attributes();
    for (key, value) in &attributes {
        start.push_attribute((key.as_slice(), value.as_slice()));
    }
    start
}

fn append_style(style: &mut String, value: &str) {
    if value.trim().is_empty() {
        return;
    }
    if !style.trim().is_empty() && !style.trim_end().ends_with(';') {
        style.push(';');
    }
    style.push_str(value.trim());
}

pub fn target_lang_code(config: &Config) -> Option<String> {
    if !config.target_lang_code.is_empty() {
        return Some(config.target_lang_code.clone());
    }
    let language = config.target_lang.to_ascii_lowercase();
    [
        ("chinese", "zh"),
        ("中文", "zh"),
        ("english", "en"),
        ("japanese", "ja"),
        ("日本", "ja"),
        ("korean", "ko"),
        ("french", "fr"),
        ("german", "de"),
        ("spanish", "es"),
        ("portuguese", "pt"),
        ("italian", "it"),
        ("russian", "ru"),
        ("arabic", "ar"),
        ("hebrew", "he"),
        ("persian", "fa"),
        ("urdu", "ur"),
        ("hindi", "hi"),
        ("turkish", "tr"),
        ("dutch", "nl"),
        ("polish", "pl"),
        ("ukrainian", "uk"),
        ("vietnamese", "vi"),
        ("thai", "th"),
        ("indonesian", "id"),
        ("malay", "ms"),
    ]
    .into_iter()
    .find(|(name, _)| language.contains(name))
    .map(|(_, code)| code.into())
}

pub fn target_direction(config: &Config, lang: Option<&str>) -> String {
    if config.target_direction != "auto" {
        return config.target_direction.clone();
    }
    if lang.is_some_and(|lang| {
        matches!(
            lang.split('-')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase()
                .as_str(),
            "ar" | "fa" | "he" | "ur" | "ps" | "sd" | "ug" | "yi"
        )
    }) {
        "rtl".into()
    } else {
        "auto".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::write::SimpleFileOptions;

    fn make_epub(path: &Path, body: Option<&str>) {
        let file = File::create(path).unwrap();
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        for (name, data) in [
            ("mimetype", "application/epub+zip"),
            (
                "META-INF/container.xml",
                "<container><rootfiles><rootfile full-path='OEBPS/content.opf'/></rootfiles></container>",
            ),
            (
                "OEBPS/content.opf",
                "<package><metadata><dc:title xmlns:dc='x'>Book</dc:title><dc:creator xmlns:dc='x'>Author</dc:creator></metadata><manifest><item id='c' href='c.xhtml' media-type='application/xhtml+xml'/><item id='n' href='notes.xhtml' media-type='application/xhtml+xml'/><item id='nav' href='nav.xhtml' media-type='application/xhtml+xml' properties='nav'/></manifest><spine><itemref idref='c'/></spine></package>",
            ),
            (
                "OEBPS/c.xhtml",
                body.unwrap_or("<html><body><p id='p'>Hello <em class='x'>world</em><sup>1</sup><br>line</p></body></html>"),
            ),
            (
                "OEBPS/notes.xhtml",
                "<html><body><p>Footnote</p></body></html>",
            ),
            (
                "OEBPS/nav.xhtml",
                "<html><body><nav><ol><li><a href='c.xhtml'>Chapter</a></li></ol></nav></body></html>",
            ),
        ] {
            zip.start_file(name, options).unwrap();
            zip.write_all(data.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn extracts_manifest_metadata_nav_and_preserves_markup() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.epub");
        make_epub(&input, None);
        let mut config = Config {
            metadata_translation: true,
            translate_title: true,
            ..Default::default()
        };
        config.exclude_translate_tags = "sup,code,pre".into();
        let (elements, meta) = extract_from_epub(&input, &config).unwrap();
        assert_eq!(meta.title, "Book");
        assert!(elements.iter().any(|element| element.kind == "metadata"));
        assert!(elements.iter().any(|element| element.kind == "toc"));
        assert!(
            elements
                .iter()
                .any(|element| element.page_href == "notes.xhtml")
        );
        let body = elements
            .iter()
            .find(|element| element.page_href == "c.xhtml")
            .unwrap();
        assert!(body.original.contains("{{etm_o_"));
        assert!(body.original.contains("{{etm_n_"));
    }

    #[test]
    fn repeated_visible_text_keeps_every_extracted_element() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("duplicates.epub");
        make_epub(
            &input,
            Some(
                "<html><body><h1>Book</h1><p>Repeated</p><p>Repeated</p><blockquote>Repeated</blockquote></body></html>",
            ),
        );
        let config = Config {
            metadata_translation: true,
            ..Default::default()
        };
        let (elements, _) = extract_from_epub(&input, &config).unwrap();
        let repeated = elements
            .iter()
            .filter(|element| element.original == "Repeated")
            .collect::<Vec<_>>();
        assert_eq!(repeated.len(), 3);
        assert_eq!(
            repeated
                .iter()
                .map(|element| element.signature.as_str())
                .collect::<std::collections::HashSet<_>>()
                .len(),
            3
        );

        let cache = crate::cache::TranslationCache::open(Path::new("unused"), false).unwrap();
        cache.save_paragraphs(&cache_rows(&elements)).unwrap();
        assert_eq!(cache.all_with_ignored().unwrap().len(), elements.len());
    }

    #[test]
    fn xml_entities_and_event_based_rewrites_stay_aligned() {
        let opf = br#"<?xml version='1.0'?><package><metadata>
            <!--<dc:title xmlns:dc='x'>Draft</dc:title>-->
            <dc:title xmlns:dc='x'>Tom &amp; Jerry &#8212; Book</dc:title>
            <dc:creator xmlns:dc='x'/><dc:creator xmlns:dc='x'><![CDATA[Jane Doe]]></dc:creator>
            </metadata></package>"#;
        let package = parse_package(opf).unwrap();
        assert_eq!(package.title, "Tom & Jerry — Book");
        let config = Config {
            metadata_translation: true,
            ..Default::default()
        };
        let metadata = extract_metadata(opf, "content.opf", &config).unwrap();
        assert!(
            metadata
                .iter()
                .any(|item| item.original == "Tom & Jerry — Book")
        );
        assert!(metadata.iter().any(|item| item.original == "Jane Doe"));
        let title = metadata
            .iter()
            .find(|item| item.raw_html == "title")
            .unwrap();
        let rewritten = rewrite_metadata(
            opf,
            "content.opf",
            &[(title.uid.clone(), "汤姆与杰瑞".into())]
                .into_iter()
                .collect(),
            "below",
            "Tom & Jerry — Book 汤姆与杰瑞",
        )
        .unwrap()
        .0;
        let mut reader = Reader::from_reader(rewritten.as_slice());
        while !matches!(reader.read_event().unwrap(), Event::Eof) {}
        let rewritten = String::from_utf8(rewritten).unwrap();
        assert!(rewritten.contains("<!--<dc:title"));
        assert!(rewritten.contains("<dc:creator xmlns:dc='x'/><dc:creator"));

        let ncx = br#"<ncx><navMap>
            <navPoint><navLabel><text> </text></navLabel></navPoint>
            <navPoint><navLabel><text>One &amp; Two</text></navLabel></navPoint>
            </navMap></ncx>"#;
        let toc = extract_ncx(ncx, "toc.ncx").unwrap();
        assert_eq!(toc.len(), 1);
        assert_eq!(toc[0].original, "One & Two");
        let (rewritten, count) = rewrite_ncx(
            ncx,
            "toc.ncx",
            &[(toc[0].uid.clone(), "一和二".into())]
                .into_iter()
                .collect(),
            "only",
        )
        .unwrap();
        assert_eq!(count, 1);
        assert!(
            String::from_utf8(rewritten)
                .unwrap()
                .contains("<text>一和二</text>")
        );

        let package = Package {
            manifest: vec![ManifestItem {
                id: "c".into(),
                href: "c.xhtml".into(),
                media_type: "application/xhtml+xml".into(),
                properties: HashSet::new(),
            }],
            spine: vec!["c.xhtml".into(), "c.xhtml".into()],
            ..Default::default()
        };
        assert_eq!(ordered_content_items(&package, None, None).len(), 1);
    }

    #[test]
    fn xhtml_write_preserves_doctype_and_is_well_formed_xml() {
        let data = br#"<!DOCTYPE html><html xmlns='http://www.w3.org/1999/xhtml'><body><p>A&nbsp;B<br/>C</p></body></html>"#;
        let config = Config::default();
        let rules = Rules::new(&config).unwrap();
        let elements = extract_body(data, "c.xhtml", &rules).unwrap();
        let body = elements.first().unwrap();
        let translation = body.original.replace("A B", "甲乙").replace('C', "丙");
        let (output, count) = inject_body(
            data,
            "c.xhtml",
            &[(body.uid.clone(), translation)].into_iter().collect(),
            &config,
            &rules,
        )
        .unwrap();
        assert_eq!(count, 1);
        assert!(output.windows(15).any(|part| part == b"<!DOCTYPE html>"));
        assert!(!output.windows(6).any(|part| part == b"&nbsp;"));
        let mut reader = Reader::from_reader(output.as_slice());
        while !matches!(reader.read_event().unwrap(), Event::Eof) {}
    }

    #[test]
    fn writes_sibling_clone_without_duplicate_ids() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.epub");
        let output = dir.path().join("out.epub");
        make_epub(&input, None);
        let config = Config::default();
        let (elements, _) = extract_from_epub(&input, &config).unwrap();
        let body = elements
            .iter()
            .find(|element| element.page_href == "c.xhtml")
            .unwrap();
        let mut translations = HashMap::new();
        translations.insert(
            body.uid.clone(),
            body.original
                .replace("Hello", "你好")
                .replace("world", "世界"),
        );
        write_translated_epub(&input, &output, &translations, &config, 1, "Book").unwrap();
        let mut archive = ZipArchive::new(File::open(output).unwrap()).unwrap();
        let html = String::from_utf8(read_member(&mut archive, "OEBPS/c.xhtml").unwrap()).unwrap();
        assert_eq!(html.matches("id=\"p\"").count(), 1);
        assert!(html.contains("et-translation"));
        assert!(html.contains("<em class=\"x\">世界</em>"));
    }

    #[test]
    fn tokens_language_direction_and_href_are_validated() {
        assert!(
            validate_markup_tokens(
                "{{etm_o_00000}}x{{etm_c_00000}}",
                "{{etm_o_00000}}y{{etm_c_00000}}"
            )
            .is_ok()
        );
        assert_eq!(
            normalize_markup_tokens("{{ etm _ o _ 0 0 0 0 0 }}x{{etm_c_00000}}"),
            "{{etm_o_00000}}x{{etm_c_00000}}"
        );
        assert_eq!(
            normalize_markup_tokens("{{ etm _ n _ 1 0 0 0 0 0 }}"),
            "{{etm_n_100000}}"
        );
        assert_eq!(
            accept_markup_translation("{{etm_o_00000}}Hello{{etm_c_00000}}", "你好").unwrap(),
            "{{etm_o_00000}}你好{{etm_c_00000}}"
        );
        assert_eq!(
            accept_markup_translation(
                "{{etm_o_00000}}Hello {{etm_o_00001}}world{{etm_c_00001}}{{etm_c_00000}}",
                "你好 世界"
            )
            .unwrap(),
            "{{etm_o_00000}}你好 世界{{etm_o_00001}}{{etm_c_00001}}{{etm_c_00000}}"
        );
        assert!(
            validate_markup_tokens(
                "{{etm_o_00000}}x{{etm_c_00000}}",
                "{{etm_c_00000}}y{{etm_o_00000}}"
            )
            .is_err()
        );
        assert!(
            validate_markup_tokens("{{etm_n_00000}}", "{{etm_n_00000}}{{etm_n_00000}}").is_err()
        );
        assert!(
            validate_markup_tokens(
                "{{etm_o_00000}}{{etm_o_00001}}{{etm_c_00001}}{{etm_c_00000}}",
                "{{etm_o_00000}}{{etm_o_00001}}{{etm_c_00000}}{{etm_c_00001}}"
            )
            .is_err()
        );
        let config = Config {
            target_lang_code: "ar-EG".into(),
            ..Default::default()
        };
        assert_eq!(target_direction(&config, Some("ar-EG")), "rtl");
        assert_eq!(
            resolve_href("OEBPS/content.opf", "Text/chapter%201.xhtml?q=1#x"),
            "OEBPS/Text/chapter 1.xhtml"
        );
    }

    #[test]
    fn css_rules_filters_and_reserved_subtrees_are_applied() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("rules.epub");
        make_epub(
            &input,
            Some(
                "<html><body><div class='whole'>A <span class='keep'>KEEP</span><em>B</em></div><p class='ignored'>X</p><p data-no='yes'>Y</p><p>SKIP me</p></body></html>",
            ),
        );
        let config = Config {
            priority_rules: vec!["div.whole".into()],
            ignore_rules: vec!["p.ignored".into()],
            reserve_rules: vec!["span.keep".into()],
            filter_rules: vec!["SKIP".into(), "data-no".into()],
            filter_scope: "html".into(),
            ..Default::default()
        };
        let (elements, _) = extract_from_epub(&input, &config).unwrap();
        let whole = elements
            .iter()
            .find(|element| element.original.contains("A "))
            .unwrap();
        assert!(whole.original.contains("{{etm_n_"));
        assert!(whole.original.contains("{{etm_o_"));
        assert!(
            elements
                .iter()
                .any(|element| element.original == "X" && element.ignored)
        );
        assert!(
            elements
                .iter()
                .any(|element| element.original == "Y" && element.ignored)
        );
        assert!(
            elements
                .iter()
                .any(|element| element.original == "SKIP me" && element.ignored)
        );
        let (elements, _) = extract_from_epub(
            &input,
            &Config {
                ignore_rules: vec!["body".into()],
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!elements.iter().any(|element| element.kind == "body"));
    }

    #[test]
    fn all_positions_styles_direction_breaks_and_group_elements_write_validly() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.epub");
        make_epub(&input, None);
        for position in ["above", "below", "only", "left", "right"] {
            let output = dir.path().join(format!("{position}.epub"));
            let config = Config {
                translation_position: position.into(),
                original_color: "red".into(),
                translation_color: "blue".into(),
                translation_style: "color:green;font-weight:bold".into(),
                target_lang_code: "ar-EG".into(),
                ..Default::default()
            };
            let (elements, _) = extract_from_epub(&input, &config).unwrap();
            let body = elements
                .iter()
                .find(|element| element.page_href == "c.xhtml")
                .unwrap();
            let translation = body
                .original
                .replace("Hello", "مرحبا")
                .replace("world", "عالم")
                .replace("line", "سطر\nثان");
            write_translated_epub(
                &input,
                &output,
                &[(body.uid.clone(), translation)].into_iter().collect(),
                &config,
                1,
                "Book",
            )
            .unwrap();
            let mut archive = ZipArchive::new(File::open(output).unwrap()).unwrap();
            let html =
                String::from_utf8(read_member(&mut archive, "OEBPS/c.xhtml").unwrap()).unwrap();
            assert!(html.contains("lang=\"ar-EG\""));
            assert!(html.contains("dir=\"rtl\""));
            assert!(html.contains("color:green"));
            assert!(html.contains("<br></br>ثان"));
            assert_eq!(html.matches("id=\"p\"").count(), 1);
            if matches!(position, "left" | "right") {
                assert!(html.contains("et-translation-table"));
                assert!(html.contains("width=\"45%\""));
                assert!(html.contains("width=\"10%\""));
            } else if position == "only" {
                assert_eq!(html.matches("<p").count(), 1);
            } else {
                assert_eq!(html.matches("<p").count(), 2);
                assert!(html.contains("color:red"));
            }
        }

        let line_output = dir.path().join("line.epub");
        let config = Config::default();
        let (elements, _) = extract_from_epub(&input, &config).unwrap();
        let body = elements
            .iter()
            .find(|element| element.page_href == "c.xhtml")
            .unwrap();
        write_translated_epub(
            &input,
            &line_output,
            &[((body.uid.clone()), body.original.replace("Hello", "你好"))]
                .into_iter()
                .collect(),
            &config,
            1,
            "Book",
        )
        .unwrap();
        let mut archive = ZipArchive::new(File::open(line_output).unwrap()).unwrap();
        let html = String::from_utf8(read_member(&mut archive, "OEBPS/c.xhtml").unwrap()).unwrap();
        assert_eq!(html.matches("<p").count(), 1);
        assert!(html.contains("<span class=\"et-translation\""));

        let list_input = dir.path().join("list.epub");
        let list_output = dir.path().join("list-out.epub");
        make_epub(
            &list_input,
            Some("<html><body><ul><li id='item'>List item</li></ul></body></html>"),
        );
        let config = Config {
            translate_tags: "li".into(),
            ..Default::default()
        };
        let (elements, _) = extract_from_epub(&list_input, &config).unwrap();
        let item = elements
            .iter()
            .find(|element| element.original == "List item")
            .unwrap();
        write_translated_epub(
            &list_input,
            &list_output,
            &[(item.uid.clone(), "列表项".into())].into_iter().collect(),
            &config,
            1,
            "Book",
        )
        .unwrap();
        let mut archive = ZipArchive::new(File::open(list_output).unwrap()).unwrap();
        let html = String::from_utf8(read_member(&mut archive, "OEBPS/c.xhtml").unwrap()).unwrap();
        assert_eq!(html.matches("<li").count(), 1);
        assert!(html.contains("<span class=\"et-translation\""));
    }

    #[test]
    fn epub2_ncx_and_metadata_are_translated_before_body() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("epub2.epub");
        let output = dir.path().join("epub2-out.epub");
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
                "<package><metadata><dc:title xmlns:dc='x'>Old title</dc:title><dc:creator xmlns:dc='x'>Old author</dc:creator></metadata><manifest><item id='c' href='c.xhtml' media-type='application/xhtml+xml'/><item id='ncx' href='toc.ncx' media-type='application/x-dtbncx+xml'/></manifest><spine toc='ncx'><itemref idref='c'/></spine></package>",
            ),
            (
                "toc.ncx",
                "<ncx><navMap><navPoint><navLabel><text>Old chapter</text></navLabel><content src='c.xhtml'/></navPoint></navMap></ncx>",
            ),
            ("c.xhtml", "<html><body><p>Body</p></body></html>"),
        ] {
            zip.start_file(name, options).unwrap();
            zip.write_all(data.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        let config = Config {
            metadata_translation: true,
            translate_title: true,
            translation_position: "only".into(),
            ..Default::default()
        };
        let (elements, meta) = extract_from_epub(&input, &config).unwrap();
        assert_eq!(
            elements
                .iter()
                .map(|element| element.kind.as_str())
                .collect::<Vec<_>>(),
            ["metadata", "metadata", "toc", "body"]
        );
        let translations = elements
            .iter()
            .map(|element| {
                let value = match element.original.as_str() {
                    "Old title" => "New title",
                    "Old author" => "New author",
                    "Old chapter" => "New chapter",
                    _ => "New body",
                };
                (element.uid.clone(), value.into())
            })
            .collect::<HashMap<_, _>>();
        write_translated_epub(
            &input,
            &output,
            &translations,
            &config,
            translations.len(),
            "New title",
        )
        .unwrap();
        let mut archive = ZipArchive::new(File::open(output).unwrap()).unwrap();
        let opf = String::from_utf8(read_member(&mut archive, "content.opf").unwrap()).unwrap();
        let ncx = String::from_utf8(read_member(&mut archive, "toc.ncx").unwrap()).unwrap();
        assert!(opf.contains("New title"));
        assert!(opf.contains("New author"));
        assert!(ncx.contains("New chapter"));
        assert!(meta.title_uid.is_some());
    }
}
