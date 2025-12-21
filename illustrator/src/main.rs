use anyhow::{Context, Result, anyhow};
use base64::Engine;
use rusqlite::{params, Connection, Row};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write, stdout};
use std::path::Path;
use std::{thread, time::Duration};
use std::sync::Arc;
use thiserror::Error;

const DB_PATH: &str = "data/news.db";
const DATA_DIR: &str = "data";
const ILLUSTRATE_INTERVAL_SECS: u64 = 60; // Reduce interval for testing

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AiProviderType {
    OpenRouter,
    Gemini,
}

impl AiProviderType {
    fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "openrouter" => Ok(Self::OpenRouter),
            "gemini" => Ok(Self::Gemini),
            other => Err(anyhow!(
                "AI_PROVIDER_ILLUSTRATOR_TYPE must be either 'OpenRouter' or 'Gemini' for illustrator service (got '{}')",
                other
            )),
        }
    }
}

#[derive(Debug, Clone)]
struct AiProviderConfig {
    provider_type: AiProviderType,
    api_key: String,
    model: String,
    prompt: String,
    reasoning: Option<ReasoningConfig>,
}

struct NewsItem {
    id: String,
    // Keep these fields even though they're not directly used in our code
    // because they are part of the database schema and are returned by the query
    #[allow(dead_code)]
    title: String,
    #[allow(dead_code)]
    url: String,
    #[allow(dead_code)]
    date: String,
    status: String,
}

#[derive(Serialize)]
struct OpenRouterChatRequest {
    model: String,
    messages: Vec<Message>,
    modalities: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningConfig>,
}

// Gemini image generation (text-to-image) docs:
// - https://ai.google.dev/gemini-api/docs/image-generation
// Endpoint:
//   POST https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent
// Auth:
//   x-goog-api-key: <API_KEY>
// Request:
//   generationConfig.responseModalities: ["TEXT", "IMAGE"]
#[derive(Serialize)]
struct GeminiGenerateContentRequest {
    contents: Vec<GeminiContent>,
    #[serde(rename = "generationConfig")]
    generation_config: GeminiGenerationConfig,
}

#[derive(Serialize)]
struct GeminiContent {
    parts: Vec<GeminiPart>,
}

#[derive(Serialize)]
struct GeminiPart {
    text: String,
}

#[derive(Serialize)]
struct GeminiGenerationConfig {
    #[serde(rename = "responseModalities")]
    response_modalities: Vec<String>,
}

#[derive(Deserialize, Debug)]
struct GeminiGenerateContentResponse {
    candidates: Vec<GeminiCandidate>,
}

#[derive(Deserialize, Debug)]
struct GeminiCandidate {
    content: GeminiCandidateContent,
}

#[derive(Deserialize, Debug)]
struct GeminiCandidateContent {
    parts: Vec<GeminiResponsePart>,
}

#[derive(Deserialize, Debug)]
struct GeminiResponsePart {
    #[allow(dead_code)]
    text: Option<String>,
    #[serde(default)]
    #[serde(alias = "inline_data")]
    #[serde(alias = "inlineData")]
    inline_data: Option<GeminiInlineData>,
}

#[derive(Deserialize, Debug)]
struct GeminiInlineData {
    #[serde(default)]
    #[serde(alias = "mime_type")]
    #[serde(alias = "mimeType")]
    mime_type: Option<String>,
    data: String,
}

#[derive(Serialize, Debug, Clone)]
struct ReasoningConfig {
    /// When set, explicitly enables/disables reasoning.
    ///
    /// OpenRouter docs: https://openrouter.ai/docs/guides/best-practices/reasoning-tokens
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,

    /// Reasoning effort level.
    /// Allowed values include: xhigh, high, medium, low, minimal, none.
    ///
    /// OpenRouter docs: https://openrouter.ai/docs/guides/best-practices/reasoning-tokens
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
}

#[derive(Serialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Deserialize, Debug)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize, Debug)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize, Debug)]
struct ResponseMessage {
    #[allow(dead_code)]
    role: Option<String>,
    #[allow(dead_code)]
    content: Option<String>,
    #[serde(default)]
    images: Vec<ResponseImage>,
}

#[derive(Deserialize, Debug)]
struct ResponseImage {
    #[serde(rename = "type")]
    #[allow(dead_code)]
    image_type: Option<String>,
    #[serde(default)]
    #[serde(alias = "imageUrl")]
    image_url: Option<ResponseImageUrl>,
}

#[derive(Deserialize, Debug)]
struct ResponseImageUrl {
    url: String,
}

fn main() -> Result<()> {
    // Check required environment variables
    let provider_type = AiProviderType::parse(
        &env::var("AI_PROVIDER_ILLUSTRATOR_TYPE")
            .context("AI_PROVIDER_ILLUSTRATOR_TYPE environment variable not set")?,
    )?;

    let model = env::var("AI_PROVIDER_ILLUSTRATOR_MODEL")
        .context("AI_PROVIDER_ILLUSTRATOR_MODEL environment variable not set")?;
    let prompt = env::var("AI_PROVIDER_ILLUSTRATOR_PROMPT")
        .context("AI_PROVIDER_ILLUSTRATOR_PROMPT environment variable not set")?;
    let api_key = env::var("AI_PROVIDER_ILLUSTRATOR_API_KEY")
        .context("AI_PROVIDER_ILLUSTRATOR_API_KEY environment variable not set")?;

    let reasoning = read_ai_provider_reasoning_from_env();

    let provider = AiProviderConfig {
        provider_type,
        api_key,
        model,
        prompt,
        reasoning,
    };
    
    // Initialize database and data directory
    let conn = init_db()?;
    init_data_dir()?;
    
    // Use write_log
    write_log("[INFO] Starting illustrator...")?;
    
    // Main loop - run every minute
    loop {
        if let Err(e) = run_illustrator(&conn, &provider) {
            // Use write_log
            let _ = write_log(&format!("[ERROR] Error in run_illustrator loop: {}", e));
        }
        
        // Use write_log
        let _ = write_log(&format!(
            "[INFO] Sleeping for {} seconds",
            ILLUSTRATE_INTERVAL_SECS
        ));
        thread::sleep(Duration::from_secs(ILLUSTRATE_INTERVAL_SECS));
    }
}

fn init_db() -> Result<Connection> {
    let conn = Connection::open(DB_PATH)
        .context("Failed to open database connection")?;
    
    // No need to create table here as it should already exist
    // We only connect to the existing database
    
    Ok(conn)
}

fn init_data_dir() -> Result<()> {
    if !Path::new(DATA_DIR).exists() {
        fs::create_dir_all(DATA_DIR).context("Failed to create data directory")?;
    }
    Ok(())
}

fn run_illustrator(conn: &Connection, provider: &AiProviderConfig) -> Result<()> {
    // Use write_log
    write_log("[INFO] Checking for news items to illustrate")?;
    
    // Fetch news items with "rewriter" or "illustrator_retry" status
    let news_items = fetch_items_to_illustrate(conn)?;
    
    if news_items.is_empty() {
        // Use write_log
        write_log("[INFO] No items to illustrate")?;
        return Ok(());
    }
    
    // Use write_log
    write_log(&format!(
        "[INFO] Found {} items to illustrate",
        news_items.len()
    ))?;
    
    // Process each news item
    for item in news_items {
        let item_id = item.id.clone(); // Clone id for logging in case of error
        let current_status = item.status.clone(); // Clone status for logic

        match process_news_item(&item, provider) {
            Ok(finish_reason_opt) => {
                let next_status = match finish_reason_opt.as_deref() {
                    Some("error") | Some("length") => {
                        if current_status == "illustrator_retry" {
                            write_log(&format!(
                                "[ERROR] Illustration failed again for item {} (finish_reason={:?}). Setting status to illustrator_error.",
                                item_id, finish_reason_opt
                            ))?;
                            "illustrator_error"
                        } else {
                            write_log(&format!(
                                "[WARN] Illustration failed for item {} (finish_reason={:?}). Setting status to illustrator_retry.",
                                item_id, finish_reason_opt
                            ))?;
                            "illustrator_retry"
                        }
                    }
                    Some(_) | None => {
                        write_log(&format!(
                            "[INFO] Successfully processed news item: {}",
                            item_id
                        ))?;
                        "illustrator"
                    }
                };
                update_status(conn, &item_id, next_status)?;
            }
            Err(e) => {
                let next_status = if current_status == "illustrator_retry" {
                    write_log(&format!(
                        "[ERROR] Critical error processing item {} (second attempt): {}. Setting status to illustrator_error.",
                        item_id, e
                    ))?;
                    "illustrator_error"
                } else {
                    write_log(&format!(
                        "[ERROR] Critical error processing item {}: {}. Setting status to illustrator_retry.",
                        item_id, e
                    ))?;
                    "illustrator_retry"
                };

                update_status(conn, &item_id, next_status)?;
            }
        }
    }
    
    // Use write_log
    write_log("[INFO] Illustration cycle completed")?;
    Ok(())
}

fn fetch_items_to_illustrate(conn: &Connection) -> Result<Vec<NewsItem>> {
    let mut stmt = conn.prepare(
        "SELECT id, title, url, date, status FROM news WHERE status = 'rewriter' OR status = 'illustrator_retry' ORDER BY date ASC",
    )?;
    let news_iter = stmt.query_map([], news_item_from_row)?;
    
    let mut news_items = Vec::new();
    for item in news_iter {
        news_items.push(item?);
    }
    
    Ok(news_items)
}

fn news_item_from_row(row: &Row) -> rusqlite::Result<NewsItem> {
    Ok(NewsItem {
        id: row.get(0)?,
        title: row.get(1)?,
        url: row.get(2)?,
        date: row.get(3)?,
        status: row.get(4)?,
    })
}

fn process_news_item(item: &NewsItem, provider: &AiProviderConfig) -> Result<Option<String>> {
    let input_file_path = format!("{}/rewriter_{}.html", DATA_DIR, item.id);
    let output_file_path = format!("{}/illustrator_{}.png", DATA_DIR, item.id);
    
    // Use write_log
    write_log(&format!("[DEBUG] Processing item: {}", item.id))?;
    
    // Ensure file exists before trying to open
    if !Path::new(&input_file_path).exists() {
        return Err(anyhow!("Input file not found: {}", input_file_path));
    }
    let mut file = File::open(&input_file_path)
        .context(format!("Failed to open input file: {}", input_file_path))?;
    let mut html_content = String::new();
    file.read_to_string(&mut html_content)
        .context(format!("Failed to read content from file: {}", input_file_path))?;
    
    // Send to AI provider API and get image bytes + finish_reason
    let illustrate_result = illustrate_content(&html_content, provider, &provider.prompt);
    
    // Match on the actual Result, not a reference
    match &illustrate_result {
        Ok((ref image_bytes, _)) => {
            write_log(&format!(
                "[DEBUG] Writing successful image to: {}",
                output_file_path
            ))?;
            let mut output_file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&output_file_path)
                .context(format!("Failed to open/create output file: {}", output_file_path))?;
            output_file
                .write_all(image_bytes)
                .context(format!(
                    "Failed to write image bytes to output file: {}",
                    output_file_path
                ))?;
        }
        Err(ref e @ ApiError::RequestError(_)) => {
            // Borrow the error to avoid moving it
            // Use write_log
            write_log(&format!(
                "[ERROR] API request failed for item {}: {}. No image to save.",
                item.id, e
            ))?;
            // Convert ApiError directly to anyhow::Error
            return Err(anyhow!(e.clone()));
        }
        Err(ref e @ ApiError::ParseError(_)) => {
            // Borrow the error
            // Use write_log
             write_log(&format!(
                "[ERROR] Failed to parse API response for item {}: {}. No image to save.",
                item.id, e
            ))?;
            // Convert ApiError directly to anyhow::Error
            return Err(anyhow!(e.clone()));
        }
        Err(ApiError::ApiReturnedError { .. }) => {
            // Controlled error: we return finish_reason to let caller set illustrator_retry.
        }
        Err(ref e @ ApiError::EmptyImageData) => {
            write_log(&format!(
                "[ERROR] AI provider returned empty image data for item {}: {}. No image to save.",
                item.id, e
            ))?;
            return Err(anyhow!(e.clone()));
        }
    }

    // Return the finish_reason if successful or if API returned a controlled error
    match illustrate_result {
        Ok((_, finish_reason)) => Ok(finish_reason),
        Err(ApiError::ApiReturnedError { finish_reason, .. }) => Ok(finish_reason),
        // Other errors were already returned as Err(anyhow::Error)
        Err(e) => Err(anyhow!(e)), // Convert remaining ApiError variants
    }
}

fn illustrate_content(content: &str, provider: &AiProviderConfig, prompt: &str) -> Result<(Vec<u8>, Option<String>), ApiError> {
    let client = Client::builder()
        .timeout(Duration::from_secs(120)) // Set timeout to 120 seconds
        .build()
        .map_err(|e| ApiError::RequestError(Arc::new(e)))?;

    match provider.provider_type {
        AiProviderType::OpenRouter => {
            // OpenRouter image generation (docs):
            // - Request: POST https://openrouter.ai/api/v1/chat/completions with modalities including "image"
            // - Response: choices[0].message.images[...].image_url.url containing a base64 data URL
            // Sources:
            // - https://openrouter.ai/docs/features/multimodal/image-generation
            // - https://openrouter.ai/docs/guides/overview/multimodal/image-generation
            // Some image-generation models may ignore system messages.
            // To guarantee that AI_PROVIDER_ILLUSTRATOR_PROMPT is applied, embed it into the user prompt.
            let user_prompt = format!("{}\n\n{}", prompt, content);
            let messages = vec![Message {
                role: "user".to_string(),
                content: user_prompt,
            }];

            let _ = write_log(&format!(
                "[DEBUG] Request summary: model='{}', prompt_len={}, html_len={}",
                provider.model,
                prompt.len(),
                content.len()
            ));

            if let Some(reasoning) = &provider.reasoning {
                let _ = write_log(&format!(
                    "[DEBUG] OpenRouter reasoning config: enabled={:?}, effort={:?}",
                    reasoning.enabled, reasoning.effort
                ));
            }

            let request = OpenRouterChatRequest {
                model: provider.model.clone(),
                messages,
                modalities: vec!["image".to_string(), "text".to_string()],
                reasoning: provider.reasoning.clone(),
            };

            let _ = write_log(&format!(
                "[DEBUG] Sending chat completion (image generation) request to OpenRouter with model: {}",
                provider.model
            ));

            let response = client
                .post("https://openrouter.ai/api/v1/chat/completions")
                .header("Authorization", format!("Bearer {}", provider.api_key))
                .header("Content-Type", "application/json")
                .json(&request)
                .send()
                .map_err(|e| ApiError::RequestError(Arc::new(e)))?;

            parse_openrouter_image_from_chat_response(&client, response)
        }
        AiProviderType::Gemini => {
            // Gemini image generation uses models:generateContent and returns inlineData with base64 image bytes.
            // Source: https://ai.google.dev/gemini-api/docs/image-generation

            let user_prompt = format!("{}\n\n{}", prompt, content);
            let request = GeminiGenerateContentRequest {
                contents: vec![GeminiContent {
                    parts: vec![GeminiPart { text: user_prompt }],
                }],
                generation_config: GeminiGenerationConfig {
                    response_modalities: vec!["TEXT".to_string(), "IMAGE".to_string()],
                },
            };

            let _ = write_log(&format!(
                "[DEBUG] Request summary: provider='Gemini', model='{}', prompt_len={}, html_len={}",
                provider.model,
                prompt.len(),
                content.len()
            ));

            let url = format!(
                "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
                provider.model
            );

            let response = client
                .post(url)
                .header("x-goog-api-key", provider.api_key.clone())
                .header("Content-Type", "application/json")
                .json(&request)
                .send()
                .map_err(|e| ApiError::RequestError(Arc::new(e)))?;

            parse_gemini_image_from_generate_content_response(response)
        }
    }
}

fn parse_gemini_image_from_generate_content_response(
    response: reqwest::blocking::Response,
) -> Result<(Vec<u8>, Option<String>), ApiError> {
    let status = response.status();
    let response_text = response
        .text()
        .map_err(|e| ApiError::RequestError(Arc::new(e)))?;

    if !status.is_success() {
        let _ = write_log(&format!(
            "[WARN] Gemini returned non-success status: {}. Body: {}",
            status,
            truncate_for_log(&response_text, 2000)
        ));
        return Err(ApiError::ApiReturnedError {
            status,
            content: response_text,
            finish_reason: Some("error".to_string()),
        });
    }

    let response_data: GeminiGenerateContentResponse = match serde_json::from_str(&response_text) {
        Ok(data) => data,
        Err(e) => {
            let _ = write_log(&format!(
                "[ERROR] Failed to parse Gemini generateContent JSON. Status: {}. Body: {}",
                status,
                truncate_for_log(&response_text, 2000)
            ));
            return Err(ApiError::ParseError(Arc::new(e.into())));
        }
    };

    let _ = write_log(&format!(
        "[DEBUG] Gemini response summary: candidates={}",
        response_data.candidates.len()
    ));

    let candidate = response_data
        .candidates
        .first()
        .ok_or(ApiError::EmptyImageData)?;

    let image_part = candidate
        .content
        .parts
        .iter()
        .find(|p| p.inline_data.is_some())
        .ok_or(ApiError::EmptyImageData)?;

    let inline = image_part.inline_data.as_ref().ok_or(ApiError::EmptyImageData)?;
    if let Some(mime) = inline.mime_type.as_deref() {
        let _ = write_log(&format!("[DEBUG] Gemini inlineData mime_type={}", mime));
    }

    let _ = write_log(&format!(
        "[DEBUG] Decoding Gemini inlineData base64 payload (chars={})",
        inline.data.len()
    ));

    let image_bytes = base64::engine::general_purpose::STANDARD
        .decode(inline.data.as_str())
        .map_err(|e| ApiError::ParseError(Arc::new(anyhow!(e))))?;

    let _ = write_log(&format!("[DEBUG] Image bytes received: {}", image_bytes.len()));

    if !looks_like_png(&image_bytes) {
        let _ = write_log(
            "[WARN] Gemini returned image bytes, but they do not look like a PNG. Forcing finish_reason='error' to trigger retry."
        );
        return Err(ApiError::ApiReturnedError {
            status,
            content: "Image bytes are not a valid PNG".to_string(),
            finish_reason: Some("error".to_string()),
        });
    }

    Ok((image_bytes, None))
}

fn parse_openrouter_image_from_chat_response(
    client: &Client,
    response: reqwest::blocking::Response,
) -> Result<(Vec<u8>, Option<String>), ApiError> {
    let status = response.status();

    let response_text = response
        .text()
        .map_err(|e| ApiError::RequestError(Arc::new(e)))?;

    if !status.is_success() {
        let _ = write_log(&format!(
            "[WARN] AI provider returned non-success status: {}. Body: {}",
            status,
            truncate_for_log(&response_text, 2000)
        ));
        return Err(ApiError::ApiReturnedError {
            status,
            content: response_text,
            finish_reason: Some("error".to_string()),
        });
    }

    let response_data: ChatResponse = match serde_json::from_str(&response_text) {
        Ok(data) => data,
        Err(e) => {
            let _ = write_log(&format!(
                "[ERROR] Failed to parse AI provider image response JSON. Status: {}. Body: {}",
                status,
                truncate_for_log(&response_text, 2000)
            ));
            return Err(ApiError::ParseError(Arc::new(e.into())));
        }
    };

    let _ = write_log(&format!(
        "[DEBUG] Response summary: choices={}",
        response_data.choices.len()
    ));

    let choice = response_data.choices.first().ok_or(ApiError::EmptyImageData)?;
    let image = choice.message.images.first().ok_or(ApiError::EmptyImageData)?;
    let url = image
        .image_url
        .as_ref()
        .map(|u| u.url.as_str())
        .ok_or(ApiError::EmptyImageData)?;

    let _ = write_log(&format!(
        "[DEBUG] Image URL kind: {}",
        if url.to_ascii_lowercase().starts_with("data:image/") {
            "data_url"
        } else {
            "http_url"
        }
    ));

    let image_bytes = if let Some(b64) = extract_base64_from_data_url(url) {
        let _ = write_log(&format!("[DEBUG] Decoding base64 image payload (chars={})", b64.len()));
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| ApiError::ParseError(Arc::new(anyhow!(e))))?
    } else {
        let _ = write_log(&format!("[DEBUG] Downloading image from URL: {}", url));
        client
            .get(url)
            .send()
            .map_err(|e| ApiError::RequestError(Arc::new(e)))?
            .bytes()
            .map_err(|e| ApiError::RequestError(Arc::new(e)))?
            .to_vec()
    };

    let _ = write_log(&format!("[DEBUG] Image bytes received: {}", image_bytes.len()));

    if !looks_like_png(&image_bytes) {
        let _ = write_log(
            "[WARN] AI provider returned image bytes, but they do not look like a PNG. Forcing finish_reason='error' to trigger retry."
        );
        return Err(ApiError::ApiReturnedError {
            status,
            content: "Image bytes are not a valid PNG".to_string(),
            finish_reason: Some("error".to_string()),
        });
    }

    Ok((image_bytes, None))
}

fn extract_base64_from_data_url(url: &str) -> Option<&str> {
    // OpenRouter image generation commonly returns a base64 data URL, e.g.:
    // data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAA...
    // Source: https://openrouter.ai/docs/features/multimodal/image-generation
    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("data:image/") {
        return None;
    }

    // Standard data URL base64 marker is ';base64,' (per RFC 2397 style and OpenRouter examples).
    let marker = ";base64,";
    if let Some(idx) = lower.find(marker) {
        return Some(&url[idx + marker.len()..]);
    }

    // Be permissive just in case a provider returns a non-standard ',base64,' marker.
    let fallback_marker = ",base64,";
    let idx = lower.find(fallback_marker)?;
    Some(&url[idx + fallback_marker.len()..])
}

fn truncate_for_log(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    format!(
        "{}... [truncated, total_len={}]",
        &s[..max_len],
        s.len()
    )
}

fn read_ai_provider_reasoning_from_env() -> Option<ReasoningConfig> {
    // Env-driven, optional behavior:
    // - if neither env is provided (or both empty), behave as before (no `reasoning` field)
    // - if provided, attach `reasoning` object to request
    let enabled_raw = env::var("AI_PROVIDER_ILLUSTRATOR_REASONING_ENABLED").ok();
    let effort_raw = env::var("AI_PROVIDER_ILLUSTRATOR_REASONING_EFFORT").ok();

    let mut enabled = enabled_raw
        .as_deref()
        .and_then(parse_optional_bool_env);

    let effort = effort_raw
        .as_deref()
        .and_then(parse_optional_effort_env);

    // Convenience + explicitness:
    // If effort is provided but enabled isn't, set enabled based on effort.
    if enabled.is_none() {
        if let Some(e) = effort.as_deref() {
            if e == "none" {
                enabled = Some(false);
            } else {
                enabled = Some(true);
            }
        }
    }

    if enabled.is_none() && effort.is_none() {
        return None;
    }

    Some(ReasoningConfig { enabled, effort })
}

fn parse_optional_bool_env(value: &str) -> Option<bool> {
    let v = value.trim();
    if v.is_empty() || v == "-" {
        return None;
    }

    match v.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "no" | "n" | "off" => Some(false),
        _ => {
            let _ = write_log(&format!(
                "[WARN] AI_PROVIDER_ILLUSTRATOR_REASONING_ENABLED has invalid value '{}'. Ignoring.",
                v
            ));
            None
        }
    }
}

fn parse_optional_effort_env(value: &str) -> Option<String> {
    let v = value.trim();
    if v.is_empty() || v == "-" {
        return None;
    }

    // Docs allow: xhigh, high, medium, low, minimal, none
    let normalized = v.to_ascii_lowercase();
    match normalized.as_str() {
        "xhigh" | "high" | "medium" | "low" | "minimal" | "none" => Some(normalized),
        _ => {
            let _ = write_log(&format!(
                "[WARN] AI_PROVIDER_ILLUSTRATOR_REASONING_EFFORT has invalid value '{}'. Allowed: xhigh|high|medium|low|minimal|none. Ignoring.",
                v
            ));
            None
        }
    }
}

fn looks_like_png(bytes: &[u8]) -> bool {
    const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.len() >= PNG_SIGNATURE.len() && bytes[..PNG_SIGNATURE.len()] == PNG_SIGNATURE
}

fn update_status(conn: &Connection, id: &str, status: &str) -> Result<()> {
    conn.execute(
        "UPDATE news SET status = ? WHERE id = ?",
        params![status, id],
    )?;
    
    // Use write_log
    write_log(&format!("[INFO] Updated status to '{}' for id '{}'", status, id))?;
    Ok(())
}

// Renamed to write_log for clarity
fn write_log(message: &str) -> std::io::Result<()> {
    // Simple stdout logging for now
    println!("illustrator: {}", message);
    // flush stdout to ensure messages appear immediately
    stdout().flush()
}

// Custom error type for rewrite_content
#[derive(Debug, Error, Clone)]
enum ApiError {
    #[error("Reqwest error: {0}")]
    RequestError(#[from] Arc<reqwest::Error>),
    #[error("Failed to parse API response: {0}")]
    ParseError(#[from] Arc<anyhow::Error>),
    #[error("AI provider returned status {status} with finish_reason '{finish_reason:?}'. Body: {content}")]
    ApiReturnedError {
        status: reqwest::StatusCode,
        content: String, // Include response body or diagnostic text
        finish_reason: Option<String>, // Used to trigger illustrator_retry logic
    },
    #[error("AI provider returned empty image data")]
    EmptyImageData,
}