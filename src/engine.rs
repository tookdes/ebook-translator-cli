use std::{
    collections::HashSet,
    fmt,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use reqwest::{Client, RequestBuilder, Response, Url, redirect};
use serde_json::{Map, Value, json};

use crate::config::EngineConfig;

const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_ERROR_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineKind {
    OpenAi,
    DeepSeek,
    Claude,
    DeepLx,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: Option<u16>,
    pub retry_after: Option<Duration>,
    message: String,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

pub struct Engine {
    pub name: String,
    pub kind: EngineKind,
    pub config: EngineConfig,
    pub endpoint: Url,
    pub model: String,
    source_lang: String,
    target_lang: String,
    client: Client,
}

impl Engine {
    pub fn new(
        name: &str,
        config: EngineConfig,
        source_lang: &str,
        target_lang: &str,
    ) -> Result<Self> {
        let (kind, default_base, suffix, default_model) = match name {
            "openai" => (
                EngineKind::OpenAi,
                "https://api.openai.com/v1",
                "/chat/completions",
                "gpt-4o-mini",
            ),
            "deepseek" => (
                EngineKind::DeepSeek,
                "https://api.deepseek.com/v1",
                "/chat/completions",
                "deepseek-chat",
            ),
            "claude" => (
                EngineKind::Claude,
                "https://api.anthropic.com",
                "/v1/messages",
                "claude-sonnet-4-20250514",
            ),
            "deeplx" | "deepx" => (
                EngineKind::DeepLx,
                "http://127.0.0.1:1188",
                "/translate",
                "",
            ),
            _ => bail!("未知翻译引擎: {name}"),
        };
        if config.api_key.is_empty() && !matches!(kind, EngineKind::DeepLx) {
            bail!(
                "{} 引擎需要设置 api_key",
                match kind {
                    EngineKind::Claude => "Anthropic",
                    EngineKind::DeepSeek => "DeepSeek",
                    EngineKind::OpenAi => "OpenAI",
                    EngineKind::DeepLx => "DeepLX",
                }
            );
        }
        let base = if config.base_url.is_empty() {
            default_base
        } else {
            &config.base_url
        };
        let endpoint = endpoint_url(base, suffix)?;
        let model = if !config.model.is_empty() {
            config.model.clone()
        } else if config.base_url.is_empty()
            || endpoint.host_str() == Url::parse(default_base)?.host_str()
        {
            default_model.into()
        } else {
            String::new()
        };
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::try_from_secs_f64(config.request_timeout).unwrap_or(Duration::MAX))
            .pool_max_idle_per_host(config.concurrency.max(4))
            .redirect(redirect::Policy::custom(|attempt| {
                // API key 位于自定义头(x-api-key),reqwest 跨主机重定向不会剥离,
                // 因此仅允许同 origin 重定向。
                if attempt.previous().len() > 10 {
                    return attempt.error("重定向次数过多");
                }
                let same_origin = attempt.previous().last().is_some_and(|prev| {
                    prev.scheme() == attempt.url().scheme()
                        && prev.host_str() == attempt.url().host_str()
                        && prev.port_or_known_default() == attempt.url().port_or_known_default()
                });
                if same_origin {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()?;
        Ok(Self {
            name: name.into(),
            kind,
            config,
            endpoint,
            model,
            source_lang: source_lang.into(),
            target_lang: target_lang.into(),
            client,
        })
    }

    pub async fn translate(&self, text: &str, prompt: &str) -> Result<String> {
        if matches!(self.kind, EngineKind::DeepLx) {
            return self.translate_deeplx(text).await;
        }
        let source_lang = if matches!(
            self.source_lang.trim().to_ascii_lowercase().as_str(),
            "auto" | "auto detect" | "auto-detect"
        ) {
            "detected language"
        } else {
            &self.source_lang
        };
        let prompt = prompt
            .replace("<tlang>", &self.target_lang)
            .replace("<slang>", source_lang);
        let body = self.body(text, &prompt, self.config.stream)?;
        let response = self.request(&body).send().await?;
        let response = check_status(response).await?;
        let result = if self.config.stream {
            self.parse_stream(response).await?
        } else {
            self.parse_response(read_json_limited(response).await?)
                .await?
        };
        let result = result.trim().to_owned();
        if !result.is_empty() {
            return Ok(result);
        }
        bail!("API 返回空译文")
    }

    fn request(&self, body: &Value) -> RequestBuilder {
        let request = self
            .client
            .post(self.endpoint.clone())
            .json(body)
            .header("content-type", "application/json");
        match self.kind {
            EngineKind::Claude => request
                .header("x-api-key", &self.config.api_key)
                .header("anthropic-version", "2023-06-01"),
            _ => request.bearer_auth(&self.config.api_key),
        }
    }

    async fn translate_deeplx(&self, text: &str) -> Result<String> {
        let body = serde_json::json!({
            "text": text,
            "source_lang": deepl_lang(&self.source_lang),
            "target_lang": deepl_lang(&self.target_lang),
        });
        let mut request = self
            .client
            .post(self.endpoint.clone())
            .json(&body)
            .header("content-type", "application/json")
            // Cloudflare checks browser fingerprints; a plain reqwest UA gets 403/1010.
            .header(
                "user-agent",
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36",
            )
            .header("accept", "application/json");
        if let Some(host) = self.endpoint.host_str() {
            request = request.header("origin", format!("https://{host}"));
            request = request.header("referer", format!("https://{host}/"));
        }
        if !self.config.api_key.is_empty() {
            request = request.header("authorization", format!("Bearer {}", self.config.api_key));
        }
        let response = request.send().await?;
        let response = check_status(response).await?;
        let data = read_json_limited(response).await?;
        let text = data
            .get("data")
            .and_then(|data| match data {
                Value::String(value) => Some(value.as_str()),
                _ => data.get("text").and_then(Value::as_str).or_else(|| {
                    data.get("translations")
                        .and_then(Value::as_array)
                        .and_then(|items| items.first())
                        .and_then(|item| item.get("text").and_then(Value::as_str))
                }),
            })
            .or_else(|| data.get("text").and_then(Value::as_str))
            .ok_or_else(|| anyhow!("DeepLX 返回格式无法识别: {}", truncate_json(&data)))?;
        let text = text.trim().to_owned();
        if text.is_empty() {
            bail!("DeepLX 返回空译文");
        }
        Ok(text)
    }

    pub fn body(&self, text: &str, prompt: &str, stream: bool) -> Result<Value> {
        let reserved: HashSet<&str> = match self.kind {
            EngineKind::Claude => [
                "model",
                "system",
                "messages",
                "temperature",
                "top_p",
                "stream",
            ]
            .into_iter()
            .collect(),
            _ => ["messages", "model", "temperature", "top_p", "stream"]
                .into_iter()
                .collect(),
        };
        if let Some(key) = self
            .config
            .extra
            .keys()
            .find(|key| reserved.contains(key.as_str()))
        {
            bail!("引擎 extra 不能覆盖保留字段: {key}");
        }
        let mut body = Map::new();
        match self.kind {
            EngineKind::Claude => {
                if !self.model.is_empty() {
                    body.insert("model".into(), json!(self.model));
                }
                body.insert(
                    "max_tokens".into(),
                    self.config
                        .extra
                        .get("max_tokens")
                        .cloned()
                        .unwrap_or(json!(4096)),
                );
                body.insert("system".into(), json!(prompt));
                body.insert("messages".into(), json!([{"role":"user", "content":text}]));
            }
            _ => {
                body.insert(
                    "messages".into(),
                    json!([
                        {"role":"system", "content":prompt}, {"role":"user", "content":text}
                    ]),
                );
                if !self.model.is_empty() {
                    body.insert("model".into(), json!(self.model));
                }
            }
        }
        match self.config.sampling.as_str() {
            "top_p" => {
                if let Some(value) = self.config.top_p {
                    body.insert("top_p".into(), json!(value));
                }
            }
            _ => {
                if let Some(value) = self.config.temperature {
                    body.insert("temperature".into(), json!(value));
                }
            }
        }
        if stream {
            body.insert("stream".into(), json!(true));
        }
        for (key, value) in &self.config.extra {
            if self.kind == EngineKind::Claude && key == "max_tokens" {
                continue;
            }
            body.insert(key.clone(), value.clone());
        }
        Ok(Value::Object(body))
    }

    async fn parse_response(&self, data: Value) -> Result<String> {
        match self.kind {
            EngineKind::Claude => {
                match data.get("stop_reason").and_then(Value::as_str) {
                    Some("max_tokens") => bail!("API 输出被截断 (stop_reason=max_tokens)"),
                    Some("refusal") => bail!("API 输出被内容过滤 (stop_reason=refusal)"),
                    _ => {}
                }
                let text = data
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<String>();
                Ok(text)
            }
            _ => {
                let choice = data
                    .get("choices")
                    .and_then(Value::as_array)
                    .and_then(|x| x.first())
                    .ok_or_else(|| anyhow!("API 返回空结果: {}", truncate_json(&data)))?;
                check_openai_finish(choice)?;
                let message = choice.get("message").unwrap_or(&Value::Null);
                if message.get("refusal").is_some_and(|x| !x.is_null()) {
                    bail!("API 输出被内容过滤 (refusal)");
                }
                let content = message
                    .get("content")
                    .and_then(Value::as_str)
                    .filter(|x| !x.is_empty())
                    .map(str::to_owned)
                    .or_else(|| {
                        // GLM 网关有时把译文写进 reasoning_content 而 content 为空
                        message
                            .get("reasoning_content")
                            .and_then(Value::as_str)
                            .map(extract_tail_translation)
                    })
                    .or_else(|| {
                        choice
                            .get("text")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    });
                Ok(content.unwrap_or_default())
            }
        }
    }

    async fn parse_stream(&self, response: Response) -> Result<String> {
        let mut bytes = response.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut pending = String::new();
        let mut output = String::new();
        let mut completed = false;
        let mut received = 0usize;
        while let Some(chunk) = bytes.next().await {
            let chunk = chunk?;
            received = received.saturating_add(chunk.len());
            if received > MAX_RESPONSE_BYTES {
                bail!(
                    "API 响应过大（超过 {} MiB）",
                    MAX_RESPONSE_BYTES / 1024 / 1024
                );
            }
            // 网络分块边界可能落在多字节 UTF-8 序列中间,残缺尾部留在 buf 等下一块。
            buf.extend_from_slice(&chunk);
            drain_valid_utf8(&mut buf, &mut pending)?;
            while let Some(pos) = pending.find('\n') {
                let line = pending[..pos].trim_end_matches('\r').trim().to_owned();
                pending.drain(..=pos);
                parse_sse_line(self.kind, &line, &mut output, &mut completed)?;
            }
        }
        if !buf.is_empty() {
            bail!("API 流式响应不是 UTF-8");
        }
        if !pending.trim().is_empty() {
            parse_sse_line(
                self.kind,
                pending.trim_end_matches('\r').trim(),
                &mut output,
                &mut completed,
            )?;
        }
        if !completed {
            bail!("API 流式响应未完整结束");
        }
        Ok(output)
    }
}

fn parse_sse_line(
    kind: EngineKind,
    line: &str,
    output: &mut String,
    completed: &mut bool,
) -> Result<()> {
    if !line.starts_with("data:") {
        return Ok(());
    }
    let data = line[5..].trim();
    if data == "[DONE]" {
        *completed = true;
        return Ok(());
    }
    let event: Value = serde_json::from_str(data).context("API 流式响应 JSON 无效")?;
    match kind {
        EngineKind::Claude => parse_anthropic_event(&event, output, completed),
        _ => parse_openai_event(&event, output, completed),
    }
}

const RETRY_AFTER_MAX: Duration = Duration::from_secs(300);

fn parse_retry_after(value: &str) -> Option<Duration> {
    value
        .parse::<f64>()
        .ok()
        // Retry-After 由服务器控制,负数/NaN/inf/超大值都必须拒绝而非 panic。
        .and_then(|secs| Duration::try_from_secs_f64(secs).ok())
        .or_else(|| {
            httpdate::parse_http_date(value)
                .ok()
                .and_then(|at| at.duration_since(SystemTime::now()).ok())
        })
        .map(|delay| delay.min(RETRY_AFTER_MAX))
}

fn drain_valid_utf8(buf: &mut Vec<u8>, pending: &mut String) -> Result<()> {
    let valid_len = match std::str::from_utf8(buf) {
        Ok(_) => buf.len(),
        Err(err) if err.error_len().is_none() => err.valid_up_to(),
        Err(err) => return Err(err).context("API 流式响应不是 UTF-8"),
    };
    pending.push_str(std::str::from_utf8(&buf[..valid_len]).unwrap());
    buf.drain(..valid_len);
    Ok(())
}

async fn read_bytes_limited(response: Response, limit: usize) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        bail!("API 响应过大（上限 {} 字节）", limit);
    }
    let mut stream = response.bytes_stream();
    let mut output = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if output.len().saturating_add(chunk.len()) > limit {
            bail!("API 响应过大（上限 {} 字节）", limit);
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

async fn read_json_limited(response: Response) -> Result<Value> {
    let bytes = read_bytes_limited(response, MAX_RESPONSE_BYTES).await?;
    serde_json::from_slice(&bytes).context("API 响应 JSON 无效")
}

async fn check_status(response: Response) -> Result<Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(parse_retry_after);
    let body = read_bytes_limited(response, MAX_ERROR_RESPONSE_BYTES)
        .await
        .unwrap_or_default();
    let body = String::from_utf8_lossy(&body);
    Err(ApiError {
        status: Some(status),
        retry_after,
        message: format!(
            "HTTP {status}: {}",
            body.chars().take(2000).collect::<String>()
        ),
    }
    .into())
}

fn extract_tail_translation(reasoning: &str) -> String {
    // reasoning_content 末尾通常是模型给出的最终译文，常混有"只输出译文"的英文说明。
    // 真实形态: "...without any explanations.在我看来，这似乎和茶很相似"
    // 优先取最后一个英文句点后含非 ASCII 文本的部分；否则取末尾连续非 ASCII 段。
    let trimmed = reasoning.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // 1) 最后一个 ASCII 句点后跟非 ASCII 文本 => 取句点后的部分
    for (idx, b) in trimmed.bytes().enumerate().rev() {
        if b == b'.' {
            let tail = trimmed[idx + 1..].trim();
            if !tail.is_ascii() {
                return tail.to_owned();
            }
        }
    }
    // 2) 取末尾最后一段连续非 ASCII 文本
    let last_non_ascii = trimmed
        .char_indices()
        .rev()
        .find(|&(_, c)| !c.is_ascii())
        .map(|(idx, c)| idx + c.len_utf8());
    if let Some(last_end) = last_non_ascii {
        let mut start = last_end;
        while start > 0 {
            let prev = trimmed[..start].chars().next_back().unwrap();
            if prev.is_ascii() && !matches!(prev, ' ' | '.' | ',' | ';' | ':' | '-' | '—') {
                break;
            }
            start -= prev.len_utf8();
        }
        return trimmed[start..].trim().to_owned();
    }
    trimmed.to_owned()
}
pub fn deepl_lang(language: &str) -> String {
    let normalized = language.trim().to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "auto" | "auto detect" | "auto-detect" | "auto_detect"
    ) {
        return "auto".to_owned();
    }
    let code = normalized.split(['-', '_']).next().unwrap_or("").to_owned();
    let english = normalized
        .replace([' ', '-', '_'], "")
        .replace("chinese", "zh")
        .replace("english", "en")
        .replace("japanese", "ja")
        .replace("korean", "ko")
        .replace("french", "fr")
        .replace("german", "de")
        .replace("spanish", "es")
        .replace("portuguese", "pt")
        .replace("italian", "it")
        .replace("russian", "ru")
        .replace("arabic", "ar")
        .replace("dutch", "nl")
        .replace("polish", "pl")
        .replace("turkish", "tr")
        .replace("vietnamese", "vi")
        .replace("indonesian", "id")
        .replace("thai", "th")
        .replace("hindi", "hi")
        .replace("ukrainian", "uk")
        .replace("greek", "el")
        .replace("swedish", "sv")
        .replace("norwegian", "nb")
        .replace("finnish", "fi")
        .replace("czech", "cs")
        .replace("romanian", "ro")
        .replace("hungarian", "hu")
        .replace("bulgarian", "bg")
        .replace("danish", "da")
        .replace("slovak", "sk")
        .replace("slovenian", "sl")
        .replace("lithuanian", "lt")
        .replace("latvian", "lv")
        .replace("estonian", "et")
        .replace("croatian", "hr")
        .replace("serbian", "sr")
        .replace("hebrew", "he")
        .replace("persian", "fa")
        .replace("urdu", "ur")
        .replace("bengali", "bn")
        .replace("tamil", "ta")
        .replace("malay", "ms")
        .replace("catalan", "ca")
        .replace("welsh", "cy")
        .replace("中文", "zh")
        .replace("英语", "en")
        .replace("日语", "ja")
        .replace("韩语", "ko");
    let code = if code.len() == 2 && code.chars().all(|c| c.is_ascii_alphabetic()) {
        code
    } else {
        english
    };
    if matches!(
        code.as_str(),
        "zh" | "en"
            | "ja"
            | "ko"
            | "fr"
            | "de"
            | "es"
            | "pt"
            | "it"
            | "ru"
            | "ar"
            | "nl"
            | "pl"
            | "tr"
            | "vi"
            | "id"
            | "th"
            | "hi"
            | "uk"
            | "el"
            | "sv"
            | "nb"
            | "fi"
            | "cs"
            | "ro"
            | "hu"
            | "bg"
            | "da"
            | "sk"
            | "sl"
            | "lt"
            | "lv"
            | "et"
            | "hr"
            | "sr"
            | "he"
            | "fa"
            | "ur"
            | "bn"
            | "ta"
            | "ms"
            | "ca"
            | "cy"
    ) {
        code.to_ascii_uppercase()
    } else if code.eq_ignore_ascii_case("auto")
        || code.eq_ignore_ascii_case("auto detect")
        || code.eq_ignore_ascii_case("auto-detect")
    {
        "auto".to_owned()
    } else {
        "EN".to_owned()
    }
}

fn endpoint_url(base: &str, suffix: &str) -> Result<Url> {
    let mut url = Url::parse(base).with_context(|| format!("API base_url 无效: {base}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("API base_url 无效: {base}");
    }
    let path = url.path().trim_end_matches('/');
    if !path.ends_with(suffix) {
        let addition = if suffix.starts_with("/v1/") && path.ends_with("/v1") {
            &suffix[3..]
        } else {
            suffix
        };
        url.set_path(&format!("{path}{addition}"));
    }
    Ok(url)
}

fn check_openai_finish(choice: &Value) -> Result<()> {
    match choice.get("finish_reason").and_then(Value::as_str) {
        Some("length") => bail!("API 输出被截断 (finish_reason=length)"),
        Some("content_filter") => bail!("API 输出被内容过滤 (finish_reason=content_filter)"),
        Some("insufficient_system_resource") => {
            bail!("API 资源暂时不足 (finish_reason=insufficient_system_resource)")
        }
        _ => Ok(()),
    }
}

fn parse_openai_event(event: &Value, output: &mut String, completed: &mut bool) -> Result<()> {
    if let Some(error) = event.get("error") {
        bail!("API 流式错误: {error}");
    }
    let Some(choices) = event.get("choices").and_then(Value::as_array) else {
        if event.get("usage").is_some() || event.get("type").and_then(Value::as_str) == Some("ping")
        {
            return Ok(());
        }
        bail!("API 流式响应缺少 choices");
    };
    // usage chunk(include_usage)与 Azure 首个 prompt_filter chunk 的 choices 为空数组。
    let Some(choice) = choices.first() else {
        return Ok(());
    };
    check_openai_finish(choice)?;
    if choice.get("finish_reason").is_some_and(|x| !x.is_null()) {
        *completed = true;
    }
    let delta = choice
        .get("delta")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("API 流式响应 delta 格式无效"))?;
    if delta.get("refusal").is_some_and(|x| !x.is_null()) {
        bail!("API 输出被内容过滤 (refusal)");
    }
    // deepseek-reasoner 等模型在 reasoning 阶段发送 "content": null,跳过而非报错。
    if let Some(value) = delta.get("content").filter(|value| !value.is_null()) {
        output.push_str(
            value
                .as_str()
                .ok_or_else(|| anyhow!("API 流式译文不是字符串"))?,
        );
    }
    Ok(())
}

fn parse_anthropic_event(event: &Value, output: &mut String, completed: &mut bool) -> Result<()> {
    match event.get("type").and_then(Value::as_str).unwrap_or("") {
        "error" => bail!("API 流式错误: {}", event.get("error").unwrap_or(event)),
        "message_stop" => *completed = true,
        "message_delta" => match event.pointer("/delta/stop_reason").and_then(Value::as_str) {
            Some("max_tokens") => bail!("API 输出被截断 (stop_reason=max_tokens)"),
            Some("refusal") => bail!("API 输出被内容过滤 (stop_reason=refusal)"),
            _ => {}
        },
        "content_block_delta" => {
            if let Some(value) = event.pointer("/delta/text") {
                output.push_str(
                    value
                        .as_str()
                        .ok_or_else(|| anyhow!("API 流式译文不是字符串"))?,
                );
            }
        }
        _ => {}
    }
    Ok(())
}

fn truncate_json(value: &Value) -> String {
    value.to_string().chars().take(500).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_request_builder_keeps_auth_headers() {
        let openai = Engine::new(
            "openai",
            EngineConfig {
                api_key: "openai-secret".into(),
                base_url: "http://127.0.0.1:9/v1".into(),
                ..Default::default()
            },
            "English",
            "Chinese",
        )
        .unwrap();
        let request = openai.request(&json!({})).build().unwrap();
        assert_eq!(
            request.headers().get("authorization").unwrap(),
            "Bearer openai-secret"
        );

        let claude = Engine::new(
            "claude",
            EngineConfig {
                api_key: "claude-secret".into(),
                base_url: "http://127.0.0.1:9".into(),
                ..Default::default()
            },
            "English",
            "Chinese",
        )
        .unwrap();
        let request = claude.request(&json!({})).build().unwrap();
        assert_eq!(request.headers().get("x-api-key").unwrap(), "claude-secret");
        assert_eq!(
            request.headers().get("anthropic-version").unwrap(),
            "2023-06-01"
        );
    }

    fn cfg(base_url: &str) -> EngineConfig {
        EngineConfig {
            api_key: "x".into(),
            base_url: base_url.into(),
            ..Default::default()
        }
    }

    #[test]
    fn endpoint_is_not_duplicated_and_keeps_query() {
        assert_eq!(
            endpoint_url("https://x/v1/chat/completions?q=1", "/chat/completions")
                .unwrap()
                .as_str(),
            "https://x/v1/chat/completions?q=1"
        );
        assert_eq!(
            endpoint_url("https://x/v1?q=1", "/v1/messages")
                .unwrap()
                .as_str(),
            "https://x/v1/messages?q=1"
        );
        assert!(endpoint_url("x/v1", "/chat/completions").is_err());
    }

    #[test]
    fn custom_openai_endpoint_does_not_force_model() {
        let engine = Engine::new("openai", cfg("https://x/v1"), "en", "zh").unwrap();
        assert!(engine.model.is_empty());
        assert!(engine.body("x", "p", false).unwrap().get("model").is_none());
    }

    #[test]
    fn only_selected_sampling_is_sent() {
        let mut config = cfg("https://example.com/v1");
        config.temperature = Some(0.2);
        config.top_p = Some(0.8);
        config.sampling = "top_p".into();
        let engine = Engine::new("openai", config, "auto", "Chinese").unwrap();
        let body = engine.body("x", "translate", false).unwrap();
        assert_eq!(body.get("top_p"), Some(&json!(0.8)));
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn thinking_strategies_toggle_all_variants() {
        let mut body = json!({"model": "free", "messages": []});
        apply_thinking_strategy(&mut body, "thinking_disabled");
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert!(body.get("reasoning_effort").is_none());
        apply_thinking_strategy(&mut body, "reasoning_none");
        assert_eq!(body["reasoning_effort"], json!("none"));
        assert!(body.get("thinking").is_none());
        apply_thinking_strategy(&mut body, "enable_thinking_false");
        assert_eq!(body["enable_thinking"], json!(false));
        apply_thinking_strategy(&mut body, "none");
        assert!(body.get("thinking").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("enable_thinking").is_none());
    }

    #[test]
    fn reasoning_content_tail_is_extracted() {
        let reasoning = r#"The given content is in German. Let me translate it to Chinese:
"Ich bin munter" translates to "我还是精神抖擞的".
I'll provide only the translation as requested.我还是精神抖擞的"#;
        assert_eq!(extract_tail_translation(reasoning), "我还是精神抖擞的");
        assert_eq!(extract_tail_translation("abc"), "abc");
    }

    #[test]
    fn deeplx_string_data_response_is_supported() {
        let engine = Engine::new(
            "deeplx",
            EngineConfig {
                api_key: String::new(),
                base_url: "http://127.0.0.1:1188".into(),
                ..Default::default()
            },
            "auto",
            "Chinese",
        )
        .unwrap();
        assert_eq!(engine.kind, EngineKind::DeepLx);
    }

    #[test]
    fn deepl_lang_maps_common_languages() {
        assert_eq!(deepl_lang("Chinese"), "ZH");
        assert_eq!(deepl_lang("Auto"), "auto");
        assert_eq!(deepl_lang("auto-detect"), "auto");
        assert_eq!(deepl_lang("English"), "EN");
        assert_eq!(deepl_lang("日本語"), "EN");
        assert_eq!(deepl_lang("ja"), "JA");
    }

    #[test]
    fn deepseek_defaults_are_distinct() {
        let engine = Engine::new("deepseek", cfg(""), "en", "zh").unwrap();
        assert_eq!(engine.model, "deepseek-chat");
        assert_eq!(
            engine.endpoint.as_str(),
            "https://api.deepseek.com/v1/chat/completions"
        );
    }

    #[test]
    fn missing_api_key_names_the_right_engine() {
        let err = Engine::new("deepseek", EngineConfig::default(), "en", "zh")
            .err()
            .expect("缺少 api_key 应当报错");
        assert!(err.to_string().contains("DeepSeek"));
        let err = Engine::new("claude", EngineConfig::default(), "en", "zh")
            .err()
            .expect("缺少 api_key 应当报错");
        assert!(err.to_string().contains("Anthropic"));
    }

    #[test]
    fn official_url_variants_keep_default_model() {
        let engine =
            Engine::new("claude", cfg("https://api.anthropic.com/v1"), "en", "zh").unwrap();
        assert_eq!(engine.model, "claude-sonnet-4-20250514");
        let engine = Engine::new("openai", cfg("https://api.openai.com/v1/"), "en", "zh").unwrap();
        assert_eq!(engine.model, "gpt-4o-mini");
    }

    #[test]
    fn claude_custom_endpoint_omits_empty_model() {
        let engine = Engine::new("claude", cfg("https://x/v1"), "en", "zh").unwrap();
        assert!(engine.model.is_empty());
        assert!(engine.body("x", "p", false).unwrap().get("model").is_none());
    }

    #[test]
    fn retry_after_rejects_invalid_values_and_caps() {
        assert_eq!(parse_retry_after("3"), Some(Duration::from_secs(3)));
        assert_eq!(parse_retry_after("-1"), None);
        assert_eq!(parse_retry_after("NaN"), None);
        assert_eq!(parse_retry_after("inf"), None);
        assert_eq!(parse_retry_after("1e300"), None);
        assert_eq!(parse_retry_after("10000"), Some(RETRY_AFTER_MAX));
        let far = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(100_000));
        assert_eq!(parse_retry_after(&far), Some(RETRY_AFTER_MAX));
    }

    #[test]
    fn stream_null_content_delta_is_skipped() {
        let mut output = String::new();
        let mut completed = false;
        let event: Value = serde_json::from_str(
            r#"{"choices":[{"delta":{"content":null,"reasoning_content":"想"}}]}"#,
        )
        .unwrap();
        parse_openai_event(&event, &mut output, &mut completed).unwrap();
        assert!(output.is_empty());
        let event: Value =
            serde_json::from_str(r#"{"choices":[{"delta":{"content":"好"}}]}"#).unwrap();
        parse_openai_event(&event, &mut output, &mut completed).unwrap();
        assert_eq!(output, "好");
    }

    #[test]
    fn stream_empty_choices_chunk_is_tolerated() {
        let mut output = String::new();
        let mut completed = false;
        for raw in [
            r#"{"choices":[],"usage":{"total_tokens":1}}"#,
            r#"{"choices":[],"prompt_filter_results":[]}"#,
            r#"{"usage":{"total_tokens":1}}"#,
        ] {
            let event: Value = serde_json::from_str(raw).unwrap();
            parse_openai_event(&event, &mut output, &mut completed).unwrap();
        }
        assert!(output.is_empty());
        assert!(!completed);
    }

    #[test]
    fn utf8_split_across_chunks_is_buffered() {
        let text = "汉字".as_bytes();
        let mut buf = Vec::new();
        let mut pending = String::new();
        buf.extend_from_slice(&text[..4]);
        drain_valid_utf8(&mut buf, &mut pending).unwrap();
        assert_eq!(pending, "汉");
        assert_eq!(buf.len(), 1);
        buf.extend_from_slice(&text[4..]);
        drain_valid_utf8(&mut buf, &mut pending).unwrap();
        assert_eq!(pending, "汉字");
        assert!(buf.is_empty());
        buf.push(0xFF);
        assert!(drain_valid_utf8(&mut buf, &mut pending).is_err());
    }
}
