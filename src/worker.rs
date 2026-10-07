use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, bail};
use futures_util::{StreamExt, stream::FuturesUnordered};
use rand::RngExt;
use regex::Regex;
use tokio::sync::{Mutex, Semaphore};

use crate::{
    cache::{Paragraph, TranslationCache},
    config::{Config, MAX_CONCURRENCY},
    engine::{ApiError, Engine},
    epub::accept_markup_translation,
    glossary::Glossary,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ErrorKind {
    Permanent,
    RateLimit,
    ContextOverflow,
    Truncated,
    Empty,
    Transient,
}

struct RateLimiter {
    interval: Duration,
    next: Mutex<Instant>,
}

struct EngineRuntime {
    engine: Arc<Engine>,
    limiter: Arc<RateLimiter>,
    semaphore: Arc<Semaphore>,
}

impl EngineRuntime {
    fn new(engine: Engine) -> Self {
        let config = engine.config.clone();
        Self {
            engine: Arc::new(engine),
            limiter: Arc::new(RateLimiter::new(config.request_interval)),
            semaphore: Arc::new(Semaphore::new(config.concurrency.clamp(1, MAX_CONCURRENCY))),
        }
    }
}

#[derive(Clone, Debug)]
struct TranslationOutcome {
    text: String,
    engine_name: String,
}

impl RateLimiter {
    fn new(seconds: f64) -> Self {
        Self {
            interval: Duration::from_secs_f64(seconds.max(0.0)),
            next: Mutex::new(Instant::now()),
        }
    }

    async fn acquire(&self, stopped: &AtomicBool) -> Result<()> {
        loop {
            if stopped.load(Ordering::Relaxed) {
                bail!("翻译批次已停止");
            }
            let mut next = self.next.lock().await;
            let now = Instant::now();
            if *next <= now {
                *next = now + self.interval;
                return Ok(());
            }
            let wait = *next - now;
            drop(next);
            tokio::time::sleep(wait.min(Duration::from_millis(250))).await;
        }
    }

    async fn defer(&self, wait: Duration) {
        let mut next = self.next.lock().await;
        *next = (*next).max(Instant::now() + wait);
    }
}

pub struct TranslationWorker {
    engine: EngineRuntime,
    fallback: Vec<EngineRuntime>,
    cache: Arc<TranslationCache>,
    config: Arc<Config>,
    glossary: Arc<Glossary>,
    stopped: Arc<AtomicBool>,
    abort_count: Arc<AtomicUsize>,
}

impl TranslationWorker {
    pub fn new(
        engine: Engine,
        cache: Arc<TranslationCache>,
        config: Config,
        glossary: Glossary,
    ) -> Self {
        Self {
            engine: EngineRuntime::new(engine),
            fallback: Vec::new(),
            cache,
            config: Arc::new(config),
            glossary: Arc::new(glossary),
            stopped: Arc::new(AtomicBool::new(false)),
            abort_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn with_fallback(mut self, engine: Engine) -> Self {
        self.fallback.push(EngineRuntime::new(engine));
        self
    }

    fn message(&self, message: impl AsRef<str>) {
        use std::io::Write;
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(&mut stderr, "{}", message.as_ref());
        let _ = stderr.flush();
    }

    fn record_failure(&self) {
        let failures = self.abort_count.fetch_add(1, Ordering::Relaxed) + 1;
        if failures >= self.config.max_error_count.max(1) {
            self.stopped.store(true, Ordering::Relaxed);
            self.message(format!(
                "  连续失败达到 max_error_count={}，停止启动新的翻译任务",
                self.config.max_error_count
            ));
        }
    }

    pub async fn translate_batch(&self, paragraphs: Vec<Paragraph>) -> (usize, usize) {
        let groups = merge_groups(
            &paragraphs,
            self.config.merge_enabled,
            self.config.merge_length,
        );
        let total_paragraphs = paragraphs.len();
        let mut tasks = FuturesUnordered::new();
        for group in groups {
            tasks.push(async move {
                let result = if self.stopped.load(Ordering::Relaxed) {
                    Err(anyhow!("翻译批次已停止"))
                } else {
                    self.translate_group(&group).await
                };
                (group, result)
            });
        }
        let mut done = 0;
        let mut failed = 0;
        let mut last_heartbeat = Instant::now();
        while let Some((group, result)) = tasks.next().await {
            if done > 0 && done % 25 == 0 {
                let now = Instant::now();
                if now.duration_since(last_heartbeat) >= Duration::from_secs(30) {
                    self.message(format!(
                        "  进度: {done}/{total_paragraphs} 段, 失败 {failed}"
                    ));
                    last_heartbeat = now;
                }
            }
            match result {
                Ok(translations) => {
                    if translations.len() == group.len() {
                        done += group.len();
                        self.abort_count.store(0, Ordering::Relaxed);
                    } else {
                        failed += group.len();
                        self.message("  合并翻译内部结果数量不完整");
                        self.record_failure();
                    }
                }
                Err(error) => {
                    failed += group.len();
                    self.message(format!(
                        "  翻译失败: {} -> {error:#}",
                        group
                            .first()
                            .map(|x| x.original.chars().take(60).collect::<String>())
                            .unwrap_or_default()
                    ));
                    self.record_failure();
                }
            }
        }
        (done, failed)
    }

    async fn translate_group(
        &self,
        group: &[Paragraph],
    ) -> Result<HashMap<String, TranslationOutcome>> {
        let mut pending = VecDeque::from([group.to_vec()]);
        let mut completed = HashMap::new();

        while let Some(chunk) = pending.pop_front() {
            if self.stopped.load(Ordering::Relaxed) {
                bail!("翻译批次已停止");
            }

            match self.translate_group_once(&chunk).await {
                Ok(translations) => {
                    self.persist_translations(&chunk, &translations)?;
                    completed.extend(translations);
                }
                Err(error)
                    if chunk.len() > 1
                        && !matches!(
                            classify_error(&error),
                            ErrorKind::Permanent | ErrorKind::RateLimit
                        ) =>
                {
                    let middle = chunk.len() / 2;
                    let left = chunk[..middle].to_vec();
                    let right = chunk[middle..].to_vec();
                    self.message(format!(
                        "  合并翻译失败（{error:#}），拆分为 {}+{} 段后重试",
                        left.len(),
                        right.len()
                    ));
                    pending.push_front(right);
                    pending.push_front(left);
                }
                Err(error) => return Err(error),
            }
        }

        Ok(completed)
    }

    async fn translate_group_once(
        &self,
        group: &[Paragraph],
    ) -> Result<HashMap<String, TranslationOutcome>> {
        if group.len() == 1 {
            let paragraph = &group[0];
            return Ok([(
                paragraph.id.clone(),
                self.translate_paragraph(paragraph).await?,
            )]
            .into());
        }

        let protected = group
            .iter()
            .map(|paragraph| {
                serde_json::json!({
                    "id": paragraph.id,
                    "text": self.glossary.apply(&paragraph.original),
                })
            })
            .collect::<Vec<_>>();
        let original = serde_json::to_string(&protected)?;
        let prompt = format!(
            "{}\n\nThe user input is a JSON array of translation units. Return ONLY a valid JSON array with exactly the same ids, each exactly once, in any order, using objects of the form {{\"id\":\"...\",\"text\":\"translated text\"}}. Do not merge, split, omit, invent, or rename ids. Preserve immutable HTML/glossary tokens exactly.",
            self.prompt_for(&self.engine.engine)
        );
        let response = self
            .translate_one_with(&self.engine, &original, &prompt)
            .await?;
        self.parse_merged_response(group, &response, &self.engine.engine.name)
    }

    fn persist_translations(
        &self,
        group: &[Paragraph],
        translations: &HashMap<String, TranslationOutcome>,
    ) -> Result<()> {
        let updates = group
            .iter()
            .map(|paragraph| {
                translations
                    .get(&paragraph.id)
                    .filter(|value| !value.text.trim().is_empty())
                    .map(|outcome| {
                        (
                            paragraph.id.clone(),
                            outcome.text.clone(),
                            outcome.engine_name.clone(),
                            self.config.target_lang.clone(),
                        )
                    })
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| anyhow!("合并翻译缺少段落或返回空译文"))?;
        self.cache.update_translations(&updates)
    }

    fn parse_merged_response(
        &self,
        group: &[Paragraph],
        response: &str,
        engine_name: &str,
    ) -> Result<HashMap<String, TranslationOutcome>> {
        let value: serde_json::Value = serde_json::from_str(response.trim())?;
        let items = value
            .as_array()
            .ok_or_else(|| anyhow!("合并译文不是 JSON 数组"))?;
        if items.len() != group.len() {
            bail!(
                "合并译文数量不匹配：预期 {}，实际 {}",
                group.len(),
                items.len()
            );
        }
        let expected = group
            .iter()
            .map(|paragraph| paragraph.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        let mut raw = HashMap::new();
        for item in items {
            let id = item
                .get("id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow!("合并译文缺少字符串 id"))?;
            let text = item
                .get("text")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow!("合并译文 {id} 缺少字符串 text"))?;
            if !expected.contains(id) {
                bail!("合并译文包含未知 id: {id}");
            }
            if raw.insert(id.to_owned(), text.to_owned()).is_some() {
                bail!("合并译文包含重复 id: {id}");
            }
        }
        let mut translations = HashMap::new();
        for paragraph in group {
            let translated = raw
                .remove(&paragraph.id)
                .ok_or_else(|| anyhow!("合并译文缺少 id: {}", paragraph.id))?;
            let protected = self.glossary.apply(&paragraph.original);
            let restored = self.glossary.restore(&protected, translated.trim())?;
            let text = accept_markup_translation(&paragraph.original, &restored)?;
            translations.insert(
                paragraph.id.clone(),
                TranslationOutcome {
                    text,
                    engine_name: engine_name.to_owned(),
                },
            );
        }
        Ok(translations)
    }

    async fn translate_paragraph(&self, paragraph: &Paragraph) -> Result<TranslationOutcome> {
        self.translate_paragraph_inner(paragraph).await
    }

    async fn translate_paragraph_inner(&self, paragraph: &Paragraph) -> Result<TranslationOutcome> {
        let mut last_error = match self.translate_paragraph_with(&self.engine, paragraph).await {
            Ok(translation) => return Ok(translation),
            Err(error) => error,
        };
        for (index, fallback) in self.fallback.iter().enumerate() {
            self.message(format!(
                "  渠道 {} 失败，使用兜底渠道 #{}: {last_error:#}",
                index + 1,
                index + 2
            ));
            match self.translate_paragraph_with(fallback, paragraph).await {
                Ok(translation) => return Ok(translation),
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    }

    async fn translate_paragraph_with(
        &self,
        runtime: &EngineRuntime,
        paragraph: &Paragraph,
    ) -> Result<TranslationOutcome> {
        let mut pending = VecDeque::from([paragraph.original.clone()]);
        let mut translated_parts = Vec::new();

        while let Some(piece) = pending.pop_front() {
            match self.translate_piece_with(runtime, &piece).await {
                Ok(translated) => translated_parts.push(translated),
                Err(error) if classify_error(&error) == ErrorKind::ContextOverflow => {
                    let Some((left, right)) = split_protected_text(&piece) else {
                        return Err(error);
                    };
                    self.message(format!(
                        "  单段超过模型上下文（{} 字符），拆分为 {}+{} 字符后重试",
                        piece.chars().count(),
                        left.chars().count(),
                        right.chars().count()
                    ));
                    pending.push_front(right);
                    pending.push_front(left);
                }
                Err(error) => return Err(error),
            }
        }

        let text = translated_parts.join("\n");
        let text = accept_markup_translation(&paragraph.original, &text)?;
        Ok(TranslationOutcome {
            text,
            engine_name: runtime.engine.name.clone(),
        })
    }

    async fn translate_piece_with(&self, runtime: &EngineRuntime, source: &str) -> Result<String> {
        let original = self.glossary.apply(source);
        let has_markup = source.contains("{{etm_");
        let has_glossary = original.contains("{{etg_");
        let prompt = protected_prompt(self.prompt_for(&runtime.engine), has_markup, has_glossary);
        for attempt in 0..=usize::from(has_markup || has_glossary) {
            let result = self.translate_one_with(runtime, &original, &prompt).await?;
            let restored = match self.glossary.restore(&original, result.trim()) {
                Ok(restored) => restored,
                Err(error) if attempt == 0 => {
                    self.message(format!("  模型损坏术语占位符，自动重试一次: {error}"));
                    continue;
                }
                Err(error) => return Err(error),
            };
            match accept_markup_translation(source, &restored) {
                Ok(translation) => return Ok(translation),
                Err(error) if attempt == 0 && has_markup => {
                    self.message(format!("  模型损坏 HTML 占位符，自动重试一次: {error}"));
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!()
    }

    fn prompt_for<'a>(&'a self, engine: &'a Engine) -> &'a str {
        engine
            .config
            .prompt
            .as_deref()
            .unwrap_or(&self.config.prompt)
    }

    async fn translate_one_with(
        &self,
        runtime: &EngineRuntime,
        text: &str,
        prompt: &str,
    ) -> Result<String> {
        let config = &runtime.engine.config;
        for attempt in 1..=config.max_retries.max(1) {
            runtime.limiter.acquire(&self.stopped).await?;
            let permit = tokio::select! {
                permit = runtime.semaphore.acquire() => permit?,
                _ = self.wait_until_stopped() => bail!("翻译批次已停止"),
            };
            if self.stopped.load(Ordering::Relaxed) {
                drop(permit);
                bail!("翻译批次已停止");
            }
            let attempt_timeout = Duration::from_secs_f64(config.request_timeout);
            let translated = tokio::select! {
                result = runtime.engine.translate(text, prompt) => result,
                _ = tokio::time::sleep(attempt_timeout) => {
                    Err(anyhow!("请求超时（{attempt_timeout:?}）"))
                }
                _ = self.wait_until_stopped() => {
                    Err(anyhow!("翻译批次已停止"))
                }
            };
            drop(permit);
            match translated {
                Ok(result) if !result.trim().is_empty() => return Ok(result),
                Ok(_) => {
                    let allowed = config.max_retries.clamp(1, 2);
                    if attempt >= allowed {
                        bail!("API 返回空译文");
                    }
                    self.sleep_or_stop(Duration::from_secs_f64(
                        config.retry_delay.min(2.0) * attempt as f64,
                    ))
                    .await?;
                }
                Err(error) => {
                    let kind = classify_error(&error);
                    let allowed = match kind {
                        ErrorKind::Permanent
                        | ErrorKind::ContextOverflow
                        | ErrorKind::Truncated => 1,
                        ErrorKind::Empty => config.max_retries.min(2),
                        _ => config.max_retries,
                    }
                    .max(1);
                    if attempt >= allowed {
                        return Err(error);
                    }
                    let retry_after = error
                        .chain()
                        .find_map(|x| x.downcast_ref::<ApiError>())
                        .and_then(|x| x.retry_after);
                    let base = match kind {
                        ErrorKind::RateLimit => {
                            Duration::from_secs_f64(config.retry_delay * 2.0 * attempt as f64)
                        }
                        ErrorKind::Empty => {
                            Duration::from_secs_f64(config.retry_delay.min(2.0) * attempt as f64)
                        }
                        _ => Duration::from_secs_f64(config.retry_delay * attempt as f64),
                    };
                    let wait = retry_after
                        .unwrap_or_else(|| base.mul_f64(rand::rng().random_range(0.5..1.5)));
                    if kind == ErrorKind::RateLimit {
                        runtime.limiter.defer(wait).await;
                    }
                    self.sleep_or_stop(wait).await?;
                }
            }
        }
        bail!("翻译失败")
    }

    async fn wait_until_stopped(&self) {
        while !self.stopped.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn sleep_or_stop(&self, wait: Duration) -> Result<()> {
        let deadline = Instant::now() + wait;
        loop {
            if self.stopped.load(Ordering::Relaxed) {
                bail!("翻译批次已停止");
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(());
            }
            tokio::time::sleep((deadline - now).min(Duration::from_millis(250))).await;
        }
    }
}

fn protected_prompt(prompt: &str, has_markup: bool, has_glossary: bool) -> String {
    if !has_markup && !has_glossary {
        return prompt.into();
    }
    let mut tokens = Vec::new();
    if has_markup {
        tokens.push("HTML tokens such as {{etm_o_00000}}, {{etm_c_00000}}, and {{etm_n_00000}}");
    }
    if has_glossary {
        tokens.push("glossary tokens such as {{etg_0123456789ab_000000}}");
    }
    format!(
        "{prompt}\n\nThe input contains immutable {}. Copy every token exactly once, character-for-character, in the same order. Never translate, alter, add, remove, split, or surround these tokens with spaces. Translate only the human-readable text between them.",
        tokens.join(" and ")
    )
}

fn merge_groups(paragraphs: &[Paragraph], enabled: bool, limit: usize) -> Vec<Vec<Paragraph>> {
    if !enabled || limit == 0 {
        return paragraphs.iter().cloned().map(|x| vec![x]).collect();
    }
    let mut groups = Vec::new();
    let mut current = Vec::new();
    let mut length = 0usize;
    for paragraph in paragraphs {
        let size = paragraph.original.chars().count();
        let page_changed = current
            .last()
            .is_some_and(|previous: &Paragraph| previous.page != paragraph.page);
        if !current.is_empty() && (page_changed || length.saturating_add(size) > limit) {
            groups.push(std::mem::take(&mut current));
            length = 0;
        }
        current.push(paragraph.clone());
        length = length.saturating_add(size);
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

fn split_protected_text(value: &str) -> Option<(String, String)> {
    if value.chars().count() < 2 {
        return None;
    }

    let protected = Regex::new(r"\{\{et[mg]_[^{}]+\}\}").ok()?;
    let ranges = protected
        .find_iter(value)
        .map(|found| found.start()..found.end())
        .collect::<Vec<_>>();
    let safe = |index: usize| {
        !ranges
            .iter()
            .any(|range| range.start < index && index < range.end)
    };

    let target_char = value.chars().count() / 2;
    let target_byte = value
        .char_indices()
        .nth(target_char)
        .map(|(index, _)| index)
        .unwrap_or(value.len() / 2);
    let min_byte = value
        .char_indices()
        .nth(value.chars().count() / 4)
        .map(|(index, _)| index)
        .unwrap_or(0);
    let max_byte = value
        .char_indices()
        .nth(value.chars().count() * 3 / 4)
        .map(|(index, _)| index)
        .unwrap_or(value.len());

    let mut preferred = None;
    let mut preferred_distance = usize::MAX;
    for (index, ch) in value.char_indices() {
        if index <= min_byte || index >= max_byte || !safe(index) {
            continue;
        }
        if ch.is_whitespace() || matches!(ch, '.' | '!' | '?' | ';' | '。' | '！' | '？' | '；')
        {
            let split = index + ch.len_utf8();
            if safe(split) {
                let distance = split.abs_diff(target_byte);
                if distance < preferred_distance {
                    preferred = Some(split);
                    preferred_distance = distance;
                }
            }
        }
    }

    let split = preferred.or_else(|| {
        value
            .char_indices()
            .map(|(index, _)| index)
            .filter(|&index| index > 0 && index < value.len() && safe(index))
            .min_by_key(|&index| index.abs_diff(target_byte))
    })?;

    let (left, right) = value.split_at(split);
    (!left.is_empty() && !right.is_empty()).then(|| (left.to_owned(), right.to_owned()))
}

fn classify_error(error: &anyhow::Error) -> ErrorKind {
    if let Some(api) = error.chain().find_map(|x| x.downcast_ref::<ApiError>()) {
        if api.status == Some(429) {
            return ErrorKind::RateLimit;
        }
        if matches!(api.status, Some(401) | Some(403)) {
            return ErrorKind::Permanent;
        }
    }
    let text = error.to_string().to_lowercase();
    if [
        "exceeds the available context size",
        "maximum context length",
        "context length exceeded",
        "context window",
        "too many tokens",
        "上下文长度",
        "上下文窗口",
    ]
    .iter()
    .any(|x| text.contains(x))
    {
        return ErrorKind::ContextOverflow;
    }
    if [
        "输出被截断",
        "stop_reason=max_tokens",
        "finish_reason=length",
    ]
    .iter()
    .any(|x| text.contains(x))
    {
        return ErrorKind::Truncated;
    }
    if [
        "401",
        "403",
        "unauthorized",
        "forbidden",
        "invalid api key",
        "invalid_api_key",
        "incorrect api key",
        "api 密钥无效",
        "密钥无效",
        "已过期",
        "内容过滤",
        "content_filter",
        "stop_reason=refusal",
        "不能覆盖保留字段",
    ]
    .iter()
    .any(|x| text.contains(x))
    {
        return ErrorKind::Permanent;
    }
    if ["429", "频率超限", "too many", "rate limit"]
        .iter()
        .any(|x| text.contains(x))
    {
        return ErrorKind::RateLimit;
    }
    if ["空译文", "空结果", "empty"]
        .iter()
        .any(|x| text.contains(x))
    {
        return ErrorKind::Empty;
    }
    ErrorKind::Transient
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epub::validate_markup_tokens;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    #[test]
    fn grouping_and_markup_validation() {
        let paragraph = |id: &str, text: &str| Paragraph {
            id: id.into(),
            md5: id.into(),
            raw: String::new(),
            original: text.into(),
            ignored: false,
            attributes: None,
            page: None,
            translation: None,
            engine_name: None,
            target_lang: None,
        };
        assert_eq!(
            merge_groups(
                &[
                    paragraph("a", "12"),
                    paragraph("b", "34"),
                    paragraph("c", "5")
                ],
                true,
                4
            )
            .iter()
            .map(Vec::len)
            .collect::<Vec<_>>(),
            [2, 1]
        );
        assert_eq!(
            merge_groups(
                &[
                    paragraph("a", "{{etm_o_00000}}12{{etm_c_00000}}"),
                    paragraph("b", "34"),
                    paragraph("c", "5")
                ],
                true,
                40
            )
            .iter()
            .map(Vec::len)
            .collect::<Vec<_>>(),
            [3]
        );
        let mut page_a = paragraph("pa", "one");
        page_a.page = Some("a.xhtml".into());
        let mut page_b = paragraph("pb", "two");
        page_b.page = Some("a.xhtml".into());
        let mut page_c = paragraph("pc", "three");
        page_c.page = Some("b.xhtml".into());
        assert_eq!(
            merge_groups(&[page_a, page_b, page_c], true, 100)
                .iter()
                .map(Vec::len)
                .collect::<Vec<_>>(),
            [2, 1]
        );
        let original = "{{etm_o_00000}}a{{etm_n_00001}}{{etm_c_00000}}";
        assert!(validate_markup_tokens(original, original).is_ok());
        assert!(validate_markup_tokens(original, "a").is_err());
        assert!(
            validate_markup_tokens(original, "{{etm_c_00000}}{{etm_o_00000}}{{etm_n_00001}}")
                .is_err()
        );
        let prompt = protected_prompt("translate", true, false);
        assert!(prompt.contains("Copy every token exactly once"));
        assert_eq!(protected_prompt("translate", false, false), "translate");
        assert!(protected_prompt("translate", false, true).contains("glossary tokens"));
        assert_eq!(
            classify_error(&anyhow!("API 输出被截断 (finish_reason=length)")),
            ErrorKind::Truncated
        );
        assert_eq!(
            classify_error(&anyhow!(
                "HTTP 400: request (71971 tokens) exceeds the available context size (2048 tokens)"
            )),
            ErrorKind::ContextOverflow
        );
        let protected = "hello {{etm_o_00000}}world{{etm_c_00000}}. next sentence";
        let (left, right) = split_protected_text(protected).unwrap();
        assert_eq!(format!("{left}{right}"), protected);
        assert!(!left.ends_with("{{etm_o_"));
        assert!(!right.starts_with("00000}}"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn broken_markup_is_retried_once_and_normalized() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = requests.clone();
        let server = thread::spawn(move || {
            for response in ["译文"] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let read = stream.read(&mut buffer).unwrap();
                    request.extend_from_slice(&buffer[..read]);
                    let Some(header_end) = request
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map(|index| index + 4)
                    else {
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
                captured
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&request).into_owned());
                let body = serde_json::json!({
                    "choices": [{"message": {"content": response}, "finish_reason": "stop"}]
                })
                .to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        let mut config = Config::default();
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
        let engine = Engine::new(
            "openai",
            config.engine_config(None),
            &config.source_lang,
            &config.target_lang,
        )
        .unwrap();
        let worker = TranslationWorker::new(
            engine,
            Arc::new(TranslationCache::open(std::path::Path::new("unused"), false).unwrap()),
            config,
            Glossary::default(),
        );
        let paragraph = Paragraph {
            id: "a".into(),
            md5: "a".into(),
            raw: String::new(),
            original: "{{etm_o_00000}}text{{etm_c_00000}}".into(),
            ignored: false,
            attributes: None,
            page: None,
            translation: None,
            engine_name: None,
            target_lang: None,
        };
        let result = worker.translate_paragraph(&paragraph).await.unwrap();
        server.join().unwrap();
        assert_eq!(result.text, "{{etm_o_00000}}译文{{etm_c_00000}}");
        assert_eq!(result.engine_name, "openai");
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert!(requests.lock().unwrap()[0].contains("immutable HTML tokens"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn merge_failure_bisects_before_individual_fallback() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let server_captured = captured.clone();
        let server = thread::spawn(move || {
            for response in [
                r#"[{"id":"0","text":"只返回一段"}]"#,
                r#"[{"id":"a","text":"甲"},{"id":"b","text":"乙"}]"#,
                r#"[{"id":"c","text":"丙"},{"id":"d","text":"丁"}]"#,
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let read = stream.read(&mut buffer).unwrap();
                    request.extend_from_slice(&buffer[..read]);
                    let Some(header_end) = request
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map(|index| index + 4)
                    else {
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
                server_captured
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&request).into_owned());
                let body = serde_json::json!({
                    "choices": [{"message": {"content": response}, "finish_reason": "stop"}]
                })
                .to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        let mut config = Config {
            merge_enabled: true,
            source_lang: "Auto detect".into(),
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
        let engine = Engine::new(
            "openai",
            config.engine_config(None),
            &config.source_lang,
            &config.target_lang,
        )
        .unwrap();
        let paragraph = |id: &str, original: &str| Paragraph {
            id: id.into(),
            md5: id.into(),
            raw: String::new(),
            original: original.into(),
            ignored: false,
            attributes: None,
            page: None,
            translation: None,
            engine_name: None,
            target_lang: None,
        };
        let worker = TranslationWorker::new(
            engine,
            Arc::new(TranslationCache::open(std::path::Path::new("unused"), false).unwrap()),
            config,
            Glossary::default(),
        );
        let result = worker
            .translate_group(&[
                paragraph("a", "one"),
                paragraph("b", "two"),
                paragraph("c", "three"),
                paragraph("d", "four"),
            ])
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(result["a"].text, "甲");
        assert_eq!(result["b"].text, "乙");
        assert_eq!(result["c"].text, "丙");
        assert_eq!(result["d"].text, "丁");
        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].contains(r#"\"id\":\"a\""#));
        assert!(requests[0].contains(r#"\"id\":\"d\""#));
        assert!(requests[1].contains(r#"\"id\":\"a\""#));
        assert!(!requests[1].contains(r#"\"id\":\"c\""#));
        assert!(requests[2].contains(r#"\"id\":\"c\""#));
        assert!(!requests[2].contains(r#"\"id\":\"a\""#));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn successful_subgroups_are_checkpointed_before_later_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for (status, response) in [
                ("200 OK", r#"[{"id":"0","text":"只返回一段"}]"#),
                (
                    "200 OK",
                    r#"[{"id":"a","text":"甲"},{"id":"b","text":"乙"}]"#,
                ),
                ("401 Unauthorized", "unauthorized"),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let read = stream.read(&mut buffer).unwrap();
                    request.extend_from_slice(&buffer[..read]);
                    let Some(header_end) = request
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map(|index| index + 4)
                    else {
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
                let body = if status.starts_with("200") {
                    serde_json::json!({
                        "choices": [{"message": {"content": response}, "finish_reason": "stop"}]
                    })
                    .to_string()
                } else {
                    response.to_owned()
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });

        let paragraph = |id: &str, original: &str| Paragraph {
            id: id.into(),
            md5: id.into(),
            raw: String::new(),
            original: original.into(),
            ignored: false,
            attributes: None,
            page: Some("chapter.xhtml".into()),
            translation: None,
            engine_name: None,
            target_lang: None,
        };
        let paragraphs = vec![
            paragraph("a", "one"),
            paragraph("b", "two"),
            paragraph("c", "three"),
            paragraph("d", "four"),
        ];
        let cache =
            Arc::new(TranslationCache::open(std::path::Path::new("unused"), false).unwrap());
        cache.save_paragraphs(&paragraphs).unwrap();

        let mut config = Config {
            merge_enabled: true,
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
        let engine = Engine::new(
            "openai",
            config.engine_config(None),
            &config.source_lang,
            &config.target_lang,
        )
        .unwrap();
        let worker = TranslationWorker::new(engine, cache.clone(), config, Glossary::default());

        assert!(worker.translate_group(&paragraphs).await.is_err());
        server.join().unwrap();

        let rows = cache
            .all()
            .unwrap()
            .into_iter()
            .map(|row| (row.id, row.translation))
            .collect::<HashMap<_, _>>();
        assert_eq!(rows["a"].as_deref(), Some("甲"));
        assert_eq!(rows["b"].as_deref(), Some("乙"));
        assert!(rows["c"].is_none());
        assert!(rows["d"].is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn semaphore_wait_is_cancelled_when_worker_stops() {
        let mut config = Config::default();
        config.engines.insert(
            "openai".into(),
            crate::config::EngineConfig {
                api_key: "test".into(),
                base_url: "http://127.0.0.1:9/v1".into(),
                model: "mock".into(),
                concurrency: 1,
                request_interval: 0.0,
                ..Default::default()
            },
        );
        let engine = Engine::new(
            "openai",
            config.engine_config(None),
            &config.source_lang,
            &config.target_lang,
        )
        .unwrap();
        let worker = TranslationWorker::new(
            engine,
            Arc::new(TranslationCache::open(std::path::Path::new("unused"), false).unwrap()),
            config,
            Glossary::default(),
        );
        let held = worker.engine.semaphore.acquire().await.unwrap();

        let result = tokio::time::timeout(Duration::from_secs(1), async {
            let (result, _) = tokio::join!(
                worker.translate_one_with(&worker.engine, "text", "translate"),
                async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    worker.stopped.store(true, Ordering::Relaxed);
                }
            );
            result
        })
        .await
        .expect("semaphore wait should be cancellable");
        drop(held);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("翻译批次已停止"));
    }
}
