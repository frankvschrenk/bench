//! Optional reduction layer: turn a recalled episode into a few-sentence
//! natural-language recap via a local OpenAI-compatible LLM (Ollama). Port of
//! the Go `synth` package. Depends only on the format Event type; the server
//! attaches a Summarizer at boot when an LLM URL is configured, otherwise
//! episode.summarize replies that no LLM is configured and the other six
//! subjects are unaffected.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::format::Event;

/// Wall-clock budget for one Generate call when the caller gives no deadline.
/// Local LLMs answer in seconds, but a cold model load can take longer; 60s
/// covers both.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

const DEFAULT_OLLAMA_BASE_URL: &str = "http://127.0.0.1:11434";
const DEFAULT_MAX_CHARS: usize = 600;

#[derive(Debug, thiserror::Error)]
pub enum SynthError {
    #[error("synth: llm backend unreachable: {0}")]
    LlmUnreachable(String),
    #[error("synth: llm backend timeout: {0}")]
    LlmTimeout(String),
    #[error("synth: llm backend refused request: {0}")]
    LlmRefused(String),
    #[error("synth: episode has no events to summarise")]
    EmptyEpisode,
    #[error("synth: unsupported language")]
    UnsupportedLanguage,
}

// ── Episode ────────────────────────────────────────────

/// The input shape Summarize works on, built by the server from the store.
/// Time is expected oldest-first; the others preserve insertion order. An
/// all-empty episode is rejected with EmptyEpisode rather than summarised into
/// vapid prose.
#[derive(Debug, Default)]
pub struct Episode {
    pub stream_id: u64,
    pub name: String,
    pub space: Vec<Event>,
    pub time: Vec<Event>,
    pub action: Vec<Event>,
    pub untracked: Vec<Event>,
}

impl Episode {
    pub fn event_count(&self) -> usize {
        self.space.len() + self.time.len() + self.action.len() + self.untracked.len()
    }
}

/// English name of a supported language code, or None if unsupported.
fn language_name(code: &str) -> Option<&'static str> {
    match code {
        "de" => Some("German"),
        "en" => Some("English"),
        _ => None,
    }
}

// ── Ollama client ────────────────────────────────────────

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    temperature: f32,
    #[serde(skip_serializing_if = "is_zero_usize")]
    max_tokens: usize,
    #[serde(skip_serializing_if = "is_zero_i64")]
    seed: i64,
    stream: bool,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    error: Option<ChatError>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(Deserialize)]
struct ChatChoiceMessage {
    #[serde(default)]
    content: String,
}

#[derive(Deserialize)]
struct ChatError {
    #[serde(default)]
    message: String,
}

/// LLM backend over the OpenAI-compatible `/v1/chat/completions` endpoint. The
/// same client works against vLLM, llama.cpp server mode, or LiteLLM with only
/// a base-URL change.
pub struct OllamaClient {
    base_url: String,
    default_model: String,
    http: reqwest::Client,
}

impl OllamaClient {
    /// Build a client. Empty base_url falls back to the local Ollama default;
    /// zero timeout falls back to DEFAULT_TIMEOUT.
    pub fn new(base_url: &str, default_model: &str, timeout: Duration) -> OllamaClient {
        let base = if base_url.is_empty() { DEFAULT_OLLAMA_BASE_URL } else { base_url };
        let timeout = if timeout.is_zero() { DEFAULT_TIMEOUT } else { timeout };
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("build reqwest client");
        OllamaClient {
            base_url: base.trim_end_matches('/').to_string(),
            default_model: default_model.to_string(),
            http,
        }
    }

    async fn generate(
        &self,
        system: &str,
        user: &str,
        model: &str,
        max_tokens: usize,
        temperature: f32,
        seed: i64,
    ) -> Result<String, SynthError> {
        let model = if model.is_empty() { self.default_model.as_str() } else { model };
        let req = ChatRequest {
            model,
            messages: vec![
                ChatMessage { role: "system", content: system },
                ChatMessage { role: "user", content: user },
            ],
            temperature,
            max_tokens,
            seed,
            stream: false,
        };

        let resp = self
            .http
            .post(format!("{}/v1/chat/completions", self.base_url))
            .json(&req)
            .send()
            .await
            .map_err(classify_transport_error)?;

        let status = resp.status();
        let body = resp.text().await.map_err(|e| SynthError::LlmRefused(format!("read response: {e}")))?;
        if !status.is_success() {
            return Err(SynthError::LlmRefused(format!("http {}: {}", status.as_u16(), truncate(&body, 200))));
        }

        let parsed: ChatResponse =
            serde_json::from_str(&body).map_err(|e| SynthError::LlmRefused(format!("decode response: {e}")))?;
        if let Some(err) = parsed.error {
            return Err(SynthError::LlmRefused(err.message));
        }
        let content = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .unwrap_or_default();
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return Err(SynthError::LlmRefused("response message was empty".to_string()));
        }
        Ok(trimmed.to_string())
    }
}

// reqwest folds timeouts, connect failures, and DNS errors into one error
// type; classify by the timeout flag, everything else is "unreachable".
fn classify_transport_error(e: reqwest::Error) -> SynthError {
    if e.is_timeout() {
        SynthError::LlmTimeout(e.to_string())
    } else {
        SynthError::LlmUnreachable(e.to_string())
    }
}

// ── Summarizer ─────────────────────────────────────────

/// Top-level synth API: holds one LLM backend and the default model tag.
pub struct Summarizer {
    llm: OllamaClient,
    default_model: String,
}

impl Summarizer {
    pub fn new(llm: OllamaClient, default_model: &str) -> Summarizer {
        Summarizer { llm, default_model: default_model.to_string() }
    }

    /// Summarise one episode in the requested language ("de" or "en"). Rejects
    /// empty episodes and unsupported languages; on success returns the recap
    /// trimmed to `max_chars` (0 -> 600).
    pub async fn summarize(
        &self,
        ep: &Episode,
        language: &str,
        max_chars: usize,
        model: &str,
    ) -> Result<String, SynthError> {
        if ep.event_count() == 0 {
            return Err(SynthError::EmptyEpisode);
        }
        if language_name(language).is_none() {
            return Err(SynthError::UnsupportedLanguage);
        }
        let max_chars = if max_chars == 0 { DEFAULT_MAX_CHARS } else { max_chars };
        let user = build_user_prompt(ep, language, max_chars);
        let model = if model.is_empty() { self.default_model.as_str() } else { model };

        // temperature 0 + fixed seed so the same episode yields the same recap.
        let out = self
            .llm
            .generate(SYSTEM_PROMPT, &user, model, chars_to_token_budget(max_chars), 0.0, 1)
            .await?;
        Ok(trim_to_char_limit(out.trim(), max_chars))
    }
}

// ── Prompt building ──────────────────────────────────────

const SYSTEM_PROMPT: &str = "You are summarising one episode of past work for a colleague who is about to attempt something similar. Be neutral, factual, and brief. Do not invent details that are not in the source events. Do not list the events back; produce a flowing paragraph that reads naturally.";

// Translate a character budget into a token budget: 4 chars/token, a 1.5x
// overshoot so the model is not cut off mid-sentence (the hard char cap is
// enforced afterwards), plus flat reasoning headroom -- modern small models
// emit a private reasoning trace that counts against max_tokens.
fn chars_to_token_budget(max_chars: usize) -> usize {
    let output_budget = (max_chars / 4) * 3 / 2;
    let output_budget = output_budget.max(64);
    const REASONING_HEADROOM: usize = 512;
    output_budget + REASONING_HEADROOM
}

fn build_user_prompt(ep: &Episode, language: &str, max_chars: usize) -> String {
    use std::fmt::Write;
    let mut b = String::new();
    if ep.name.is_empty() {
        let _ = write!(b, "Episode #{}\n\n", ep.stream_id);
    } else {
        let _ = write!(b, "Episode: {}\n\n", ep.name);
    }
    write_section(&mut b, "Setting (where and why)", &ep.space);
    write_section(&mut b, "Time course (what happened, in order)", &ep.time);
    write_section(&mut b, "Actions and outcomes", &ep.action);
    if !ep.untracked.is_empty() {
        write_section(&mut b, "Additional context (unclassified)", &ep.untracked);
    }
    let lang_name = language_name(language).unwrap_or("English");
    let _ = write!(
        b,
        "Write 3 to 5 sentences in {lang_name}. Lead with whether the episode succeeded, failed, was abandoned, or remains open. Then the cause. Then the lesson for next time. Stay under {max_chars} characters.\n"
    );
    b
}

fn write_section(b: &mut String, heading: &str, events: &[Event]) {
    use std::fmt::Write;
    let _ = write!(b, "{heading}:\n");
    if events.is_empty() {
        b.push_str("(none recorded)\n\n");
        return;
    }
    for ev in events {
        let line = ev.content.trim();
        if line.is_empty() {
            continue;
        }
        if !ev.topic.is_empty() {
            let _ = write!(b, "- [{}] {}\n", ev.topic, line);
        } else {
            let _ = write!(b, "- {line}\n");
        }
    }
    b.push('\n');
}

// Shorten s to <= limit bytes, preferring to cut at the last sentence boundary
// in the kept region so the recap reads cleanly. Cuts only on char boundaries.
fn trim_to_char_limit(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        return s.to_string();
    }
    let mut end = limit;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let cut = &s[..end];
    if let Some(idx) = cut.rfind(['.', '!', '?']) {
        if idx > limit / 2 {
            return cut[..=idx].trim().to_string();
        }
    }
    cut.trim().to_string()
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\u{2026}", &s[..end])
}

fn is_zero_usize(v: &usize) -> bool {
    *v == 0
}
fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}
