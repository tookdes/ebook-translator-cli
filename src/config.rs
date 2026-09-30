use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use dom_query::Matcher;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const MAX_CONCURRENCY: usize = 256;
const MAX_DURATION_SECONDS: f64 = 86_400.0;
pub const DEFAULT_PROMPT: &str = "You are a meticulous translator who translates any given content. Translate the given content from <slang> to <tlang> only. Do not explain any term or answer any question-like content. Your answer should be solely the translation of the given content. In your answer do not add any prefix or suffix to the translated content. Websites' URLs/addresses should be preserved as is in the translation's output. Do not omit any part of the content, even if it seems unimportant. RESPOND ONLY with the translation text, no formatting, no explanations, no additional commentary whatsoever. ";
pub const INPUT_ENCODINGS: &[&str] = &[
    "UTF-8",
    "UTF-16",
    "UTF-32",
    "ASCII",
    "Windows-1250",
    "Windows-1251",
    "Windows-1252",
    "Windows-1253",
    "Windows-1254",
    "Windows-1255",
    "Windows-1256",
    "Windows-1257",
    "Windows-1258",
    "ISO-8859-1",
    "ISO-8859-2",
    "ISO-8859-3",
    "ISO-8859-4",
    "ISO-8859-5",
    "ISO-8859-6",
    "ISO-8859-7",
    "ISO-8859-8",
    "ISO-8859-9",
    "ISO-8859-10",
    "ISO-8859-11",
    "ISO-8859-13",
    "ISO-8859-14",
    "ISO-8859-15",
    "ISO-8859-16",
    "CP437",
    "CP720",
    "CP737",
    "CP850",
    "CP852",
    "CP855",
    "CP857",
    "CP858",
    "CP860",
    "CP861",
    "CP862",
    "CP863",
    "CP865",
    "CP866",
    "CP869",
    "KOI8-R",
    "KOI8-U",
    "Shift-JIS",
    "EUC-JP",
    "ISO-2022-JP",
    "Shift_JIS-2004",
    "EUC-JIS-2004",
    "ISO-2022-JP-2004",
    "GB2312",
    "GBK",
    "GB18030",
    "HKSCS",
    "KS-X-1001",
    "EUC-KR",
    "ISO-2022-KR",
];

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct EngineConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub concurrency: usize,
    pub request_interval: f64,
    pub request_timeout: f64,
    pub max_retries: usize,
    pub retry_delay: f64,
    pub stream: bool,
    pub prompt: Option<String>,
    pub sampling: String,
    #[serde(default)]
    pub extra: Map<String, Value>,
    #[serde(flatten, skip_serializing)]
    pub(crate) unknown: Map<String, Value>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: String::new(),
            model: String::new(),
            temperature: Some(0.3),
            top_p: Some(1.0),
            concurrency: 3,
            request_interval: 1.0,
            request_timeout: 60.0,
            max_retries: 5,
            retry_delay: 5.0,
            stream: false,
            prompt: None,
            sampling: "temperature".into(),
            extra: Map::new(),
            unknown: Map::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ColumnGap {
    #[serde(rename = "_type")]
    pub kind: String,
    pub percentage: usize,
    pub space_count: usize,
}

impl Default for ColumnGap {
    fn default() -> Self {
        Self {
            kind: "percentage".into(),
            percentage: 10,
            space_count: 6,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub engine: String,
    pub source_lang: String,
    pub target_lang: String,
    pub prompt: String,
    pub cache_enabled: bool,
    pub cache_dir: PathBuf,
    pub merge_enabled: bool,
    pub merge_length: usize,
    pub translation_position: String,
    pub translation_style: String,
    pub column_gap: ColumnGap,
    pub original_color: String,
    pub translation_color: String,
    pub target_lang_code: String,
    pub target_direction: String,
    pub translate_tags: String,
    pub exclude_translate_tags: String,
    pub priority_rules: Vec<String>,
    pub ignore_rules: Vec<String>,
    pub reserve_rules: Vec<String>,
    pub filter_rules: Vec<String>,
    pub rule_mode: String,
    pub filter_scope: String,
    pub only_files: String,
    pub exclude_files: String,
    pub metadata_translation: bool,
    pub translate_title: bool,
    pub custom_title: String,
    pub test_enabled: bool,
    pub test_num: usize,
    pub retranslate_file: String,
    pub retranslate_start: String,
    pub retranslate_end: String,
    pub glossary_path: PathBuf,
    pub glossary: HashMap<String, String>,
    pub ebook_convert_path: PathBuf,
    pub input_encoding: String,
    pub max_error_count: usize,
    pub skip_failed: bool,
    pub log_file: PathBuf,
    pub engines: HashMap<String, EngineConfig>,
    #[serde(default, skip_serializing)]
    pub openai: Option<EngineConfig>,
    #[serde(default, skip_serializing)]
    pub deepseek: Option<EngineConfig>,
    #[serde(default, skip_serializing)]
    pub claude: Option<EngineConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            engine: "openai".into(),
            source_lang: "English".into(),
            target_lang: "Chinese".into(),
            prompt: DEFAULT_PROMPT.into(),
            cache_enabled: true,
            cache_dir: default_cache_dir(),
            merge_enabled: true,
            merge_length: 300_000,
            translation_position: "below".into(),
            translation_style: String::new(),
            column_gap: ColumnGap::default(),
            original_color: String::new(),
            translation_color: String::new(),
            target_lang_code: String::new(),
            target_direction: "auto".into(),
            translate_tags: String::new(),
            exclude_translate_tags: "sup,code,pre".into(),
            priority_rules: Vec::new(),
            ignore_rules: Vec::new(),
            reserve_rules: Vec::new(),
            filter_rules: Vec::new(),
            rule_mode: "normal".into(),
            filter_scope: "text".into(),
            only_files: String::new(),
            exclude_files: String::new(),
            metadata_translation: false,
            translate_title: false,
            custom_title: String::new(),
            test_enabled: false,
            test_num: 10,
            retranslate_file: String::new(),
            retranslate_start: String::new(),
            retranslate_end: String::new(),
            glossary_path: PathBuf::new(),
            glossary: HashMap::new(),
            ebook_convert_path: PathBuf::new(),
            input_encoding: String::new(),
            max_error_count: 10,
            skip_failed: false,
            log_file: PathBuf::new(),
            engines: HashMap::new(),
            openai: None,
            deepseek: None,
            claude: None,
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = match path {
            Some(path) => Some(path.to_path_buf()),
            None => std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(|x| x.join("config.json")))
                .filter(|x| x.is_file())
                .or_else(|| {
                    Path::new("config.json")
                        .is_file()
                        .then(|| PathBuf::from("config.json"))
                }),
        };
        let mut config = match path {
            Some(ref path) => {
                let text = fs::read_to_string(path)
                    .with_context(|| format!("配置文件不存在或不可读: {}", path.display()))?;
                serde_json::from_str(&text).context("配置文件 JSON 无效")?
            }
            None => Self::default(),
        };
        config.adopt_flat_engines()?;
        config.expand_paths();
        if let Some(encoding) = canonical_encoding(&config.input_encoding) {
            config.input_encoding = encoding.into();
        }
        config.validate()?;
        Ok(config)
    }

    pub fn engine_config(&self, name: Option<&str>) -> EngineConfig {
        self.engines
            .get(name.unwrap_or(&self.engine))
            .cloned()
            .unwrap_or_default()
    }

    pub fn effective_prompt(&self) -> &str {
        self.engines
            .get(&self.engine)
            .and_then(|engine| engine.prompt.as_deref())
            .unwrap_or(&self.prompt)
    }

    fn adopt_flat_engines(&mut self) -> Result<()> {
        for (name, value) in [
            ("openai", self.openai.take()),
            ("deepseek", self.deepseek.take()),
            ("claude", self.claude.take()),
        ] {
            if let Some(value) = value {
                self.engines.entry(name.into()).or_insert(value);
            }
        }
        for (name, value) in &mut self.engines {
            if let Some(max_tokens) = value.unknown.remove("max_tokens")
                && value
                    .extra
                    .insert("max_tokens".into(), max_tokens)
                    .is_some()
            {
                bail!("引擎配置 {name}.max_tokens 不能同时平铺并写入 extra");
            }
            if !value.unknown.is_empty() {
                bail!(
                    "引擎配置 {name} 包含未知字段: {}",
                    value.unknown.keys().cloned().collect::<Vec<_>>().join(", ")
                );
            }
        }
        Ok(())
    }

    fn expand_paths(&mut self) {
        self.cache_dir = expand_home(&self.cache_dir);
        self.glossary_path = expand_home(&self.glossary_path);
        self.ebook_convert_path = expand_home(&self.ebook_convert_path);
        self.log_file = expand_home(&self.log_file);
    }

    pub fn validate(&self) -> Result<()> {
        if !matches!(self.engine.as_str(), "openai" | "deepseek" | "claude" | "deeplx" | "deepx") {
            bail!(
                "未知引擎 '{}'，可用: claude, deepseek, openai, deeplx, deepx",
                self.engine
            );
        }
        if !matches!(
            self.translation_position.as_str(),
            "below" | "above" | "left" | "right" | "only"
        ) {
            bail!("translation_position 必须是 below、above、left、right 或 only");
        }
        if !matches!(self.target_direction.as_str(), "auto" | "ltr" | "rtl") {
            bail!("target_direction 必须是 auto、ltr 或 rtl");
        }
        if !matches!(self.rule_mode.as_str(), "normal" | "case" | "regex") {
            bail!("rule_mode 必须是 normal、case 或 regex");
        }
        if !matches!(self.filter_scope.as_str(), "text" | "html") {
            bail!("filter_scope 必须是 text 或 html");
        }
        if !matches!(self.column_gap.kind.as_str(), "percentage" | "space_count") {
            bail!("column_gap._type 必须是 percentage 或 space_count");
        }
        if !(1..=100).contains(&self.column_gap.percentage) || self.column_gap.space_count == 0 {
            bail!("column_gap 数值必须大于 0，percentage 不能超过 100");
        }
        if !self.prompt.contains("<tlang>") {
            bail!("prompt 必须包含 <tlang>");
        }
        if !self.target_lang_code.is_empty()
            && !Regex::new(r"^[A-Za-z]{2,8}(?:-[A-Za-z0-9]{1,8})*$")?
                .is_match(&self.target_lang_code)
        {
            bail!("target_lang_code 不是有效的 BCP-47 语言标签");
        }
        for rule in self
            .priority_rules
            .iter()
            .chain(&self.ignore_rules)
            .chain(&self.reserve_rules)
        {
            Matcher::new(rule).map_err(|_| anyhow::anyhow!("CSS selector 无效: {rule}"))?;
        }
        if self.rule_mode == "regex" {
            for rule in &self.filter_rules {
                Regex::new(rule).with_context(|| format!("过滤正则无效: {rule}"))?;
            }
        }
        if !self.input_encoding.is_empty() && canonical_encoding(&self.input_encoding).is_none() {
            bail!("不支持的 input_encoding: {}", self.input_encoding);
        }
        if self.max_error_count == 0 {
            bail!("max_error_count 必须大于 0");
        }
        for (name, cfg) in &self.engines {
            if !matches!(name.as_str(), "openai" | "deepseek" | "claude" | "deeplx" | "deepx") {
                bail!("未知引擎 '{name}'，可用: claude, deepseek, openai, deeplx, deepx");
            }
            if cfg.concurrency == 0 || cfg.concurrency > MAX_CONCURRENCY {
                bail!("引擎配置 {name}.concurrency 必须在 1 到 {MAX_CONCURRENCY} 之间");
            }
            if cfg.max_retries == 0 {
                bail!("引擎配置 {name}.max_retries 必须大于 0");
            }
            if !matches!(cfg.sampling.as_str(), "temperature" | "top_p") {
                bail!("引擎配置 {name}.sampling 必须是 temperature 或 top_p");
            }
            if cfg
                .prompt
                .as_ref()
                .is_some_and(|prompt| !prompt.contains("<tlang>"))
            {
                bail!("引擎配置 {name}.prompt 必须包含 <tlang>");
            }
            if !cfg.request_timeout.is_finite()
                || cfg.request_timeout <= 0.0
                || cfg.request_timeout > MAX_DURATION_SECONDS
            {
                bail!("引擎配置 {name}.request_timeout 必须在 0 到 86400 秒之间");
            }
            for (key, value) in [
                ("request_interval", cfg.request_interval),
                ("retry_delay", cfg.retry_delay),
            ] {
                if !value.is_finite() || !(0.0..=MAX_DURATION_SECONDS).contains(&value) {
                    bail!("引擎配置 {name}.{key} 必须在 0 到 86400 秒之间");
                }
            }
            if cfg.temperature.is_some_and(|x| {
                !x.is_finite() || x < 0.0 || x > if name == "claude" { 1.0 } else { 2.0 }
            }) {
                bail!("引擎配置 {name}.temperature 超出支持范围");
            }
            if cfg
                .top_p
                .is_some_and(|x| !x.is_finite() || !(0.0..=1.0).contains(&x))
            {
                bail!("引擎配置 {name}.top_p 必须在 0 到 1 之间");
            }
            if let Some(value) = cfg.extra.get("max_tokens")
                && value.as_u64().is_none_or(|x| x == 0)
            {
                bail!("引擎配置 {name}.max_tokens 必须是正整数");
            }
        }
        Ok(())
    }
}

pub fn canonical_encoding(value: &str) -> Option<&'static str> {
    INPUT_ENCODINGS
        .iter()
        .copied()
        .find(|encoding| encoding.eq_ignore_ascii_case(value.trim()))
}

fn default_cache_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache/ebook-translator")
}

fn expand_home(path: &Path) -> PathBuf {
    let Some(value) = path.to_str() else {
        return path.to_path_buf();
    };
    if value == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_flat_engine_are_compatible() {
        let mut cfg: Config =
            serde_json::from_str(r#"{"openai":{"api_key":"x","temperature":null}}"#).unwrap();
        cfg.adopt_flat_engines().unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.engine_config(None).api_key, "x");
        assert_eq!(cfg.engine_config(None).temperature, None);
        assert_eq!(Config::default().engine_config(None).temperature, Some(0.3));
        assert!(Config::default().merge_enabled);
        assert_eq!(Config::default().merge_length, 300_000);

        let mut typo: Config =
            serde_json::from_str(r#"{"engines":{"openai":{"temprature":0.2}}}"#).unwrap();
        assert!(typo.adopt_flat_engines().is_err());
    }

    #[test]
    fn invalid_ranges_are_rejected() {
        let mut cfg = Config::default();
        cfg.engines.insert(
            "openai".into(),
            EngineConfig {
                concurrency: 0,
                ..Default::default()
            },
        );
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validates_new_rules_and_encoding() {
        let cfg = Config {
            priority_rules: vec!["p.note > em".into()],
            filter_rules: vec![r"^\d+$".into()],
            rule_mode: "regex".into(),
            input_encoding: "gbk".into(),
            target_lang_code: "zh-Hans".into(),
            ..Default::default()
        };
        cfg.validate().unwrap();
        assert_eq!(canonical_encoding("gbk"), Some("GBK"));
    }
}
