use anyhow::{Context, Result, anyhow};
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
const REWRITE_INTERVAL_SECS: u64 = 60; // Reduce interval for testing

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AiProviderType {
    OpenRouter,
    Perplexity,
    Gemini,
}

impl AiProviderType {
    fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "openrouter" => Ok(Self::OpenRouter),
            "perplexity" => Ok(Self::Perplexity),
            "gemini" => Ok(Self::Gemini),
            other => Err(anyhow!(
                "AI_PROVIDER_REWRITER_TYPE must be either 'OpenRouter', 'Perplexity', or 'Gemini' (got '{}')",
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
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningConfig>,
}

#[derive(Serialize)]
struct PerplexityChatRequest {
    model: String,
    messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
}

#[derive(Serialize)]
struct GeminiChatRequest {
    model: String,
    messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
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
    #[allow(dead_code)]
    id: Option<String>,
    choices: Vec<Choice>,
}

#[derive(Deserialize, Debug)]
struct Choice {
    #[allow(dead_code)]
    index: Option<u32>,
    message: ResponseMessage,
    #[allow(dead_code)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Debug)]
struct ResponseMessage {
    #[allow(dead_code)]
    role: Option<String>,
    content: String,
}

fn main() -> Result<()> {
    // Check required environment variables
    let provider_type = AiProviderType::parse(
        &env::var("AI_PROVIDER_REWRITER_TYPE").context("AI_PROVIDER_REWRITER_TYPE environment variable not set")?,
    )?;

    let model = env::var("AI_PROVIDER_REWRITER_MODEL").context("AI_PROVIDER_REWRITER_MODEL environment variable not set")?;
    let prompt = env::var("AI_PROVIDER_REWRITER_PROMPT").context("AI_PROVIDER_REWRITER_PROMPT environment variable not set")?;
    let api_key = env::var("AI_PROVIDER_REWRITER_API_KEY").context("AI_PROVIDER_REWRITER_API_KEY environment variable not set")?;

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
    write_log("[INFO] Starting rewriter...")?;
    
    // Main loop - run every minute
    loop {
        if let Err(e) = run_rewriter(&conn, &provider) {
            // Use write_log
            let _ = write_log(&format!("[ERROR] Error in run_rewriter loop: {}", e));
        }
        
        // Use write_log
        let _ = write_log(&format!(
            "[INFO] Sleeping for {} seconds",
            REWRITE_INTERVAL_SECS
        ));
        thread::sleep(Duration::from_secs(REWRITE_INTERVAL_SECS));
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

fn run_rewriter(conn: &Connection, provider: &AiProviderConfig) -> Result<()> {
    // Use write_log
    write_log("[INFO] Checking for news items to rewrite")?;
    
    // Fetch news items with "translator" or "rewriter_retry" status
    let news_items = fetch_items_to_rewrite(conn)?;
    
    if news_items.is_empty() {
        // Use write_log
        write_log("[INFO] No items to rewrite")?;
        return Ok(());
    }
    
    // Use write_log
    write_log(&format!(
        "[INFO] Found {} items to rewrite",
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
                        if current_status == "rewriter_retry" {
                            write_log(&format!(
                                "[ERROR] Rewriting failed again for item {} (finish_reason={:?}). Setting status to rewriter_error.",
                                item_id, finish_reason_opt
                            ))?;
                            "rewriter_error"
                        } else {
                            write_log(&format!(
                                "[WARN] Rewriting failed for item {} (finish_reason={:?}). Setting status to rewriter_retry.",
                                item_id, finish_reason_opt
                            ))?;
                            "rewriter_retry"
                        }
                    }
                    Some(_) | None => {
                        write_log(&format!(
                            "[INFO] Successfully processed news item: {}",
                            item_id
                        ))?;
                        "rewriter"
                    }
                };
                update_status(conn, &item_id, next_status)?;
            }
            Err(e) => {
                let next_status = if current_status == "rewriter_retry" {
                    write_log(&format!(
                        "[ERROR] Critical error processing item {} (second attempt): {}. Setting status to rewriter_error.",
                        item_id, e
                    ))?;
                    "rewriter_error"
                } else {
                    write_log(&format!(
                        "[ERROR] Critical error processing item {}: {}. Setting status to rewriter_retry.",
                        item_id, e
                    ))?;
                    "rewriter_retry"
                };

                update_status(conn, &item_id, next_status)?;
            }
        }
    }
    
    // Use write_log
    write_log("[INFO] Rewriting cycle completed")?;
    Ok(())
}

fn fetch_items_to_rewrite(conn: &Connection) -> Result<Vec<NewsItem>> {
    let mut stmt = conn.prepare("SELECT id, title, url, date, status FROM news WHERE status = 'translated' OR status = 'rewriter_retry' ORDER BY date ASC")?;
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
    let input_file_path = format!("{}/translator_{}.html", DATA_DIR, item.id);
    let output_file_path = format!("{}/rewriter_{}.html", DATA_DIR, item.id);
    
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
    
    // Send to AI provider API and get content + finish_reason
    let rewrite_result = rewrite_content(&html_content, provider, &provider.prompt);
    
    // Match on the actual Result, not a reference
    match &rewrite_result {
        Ok((ref content, _)) => {
            // Content is now &String, so use as_bytes()
            write_log(&format!(
                "[DEBUG] Writing successful content to: {}",
                output_file_path
            ))?;
            // Use OpenOptions to create or truncate the file
            let mut output_file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&output_file_path)
                .context(format!("Failed to open/create output file: {}", output_file_path))?;
            output_file
                .write_all(content.as_bytes())
                .context(format!(
                    "Failed to write content to output file: {}",
                    output_file_path
                ))?;
        }
        Err(ApiError::ApiReturnedError { ref content, .. }) => {
            // Use 'ref content' to borrow from the error struct
            // Use write_log
            write_log(&format!(
                "[DEBUG] Writing partial content from API error to: {}",
                output_file_path
            ))?;
            let mut output_file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&output_file_path)
                .context(format!("Failed to open/create output file (on error): {}", output_file_path))?;
            output_file
                .write_all(content.as_bytes())
                .context(format!(
                    "Failed to write partial content to output file: {}",
                    output_file_path
                ))?;
        }
        Err(ref e @ ApiError::RequestError(_)) => {
            // Borrow the error to avoid moving it
            // Use write_log
            write_log(&format!(
                "[ERROR] API request failed for item {}: {}. No content to save.",
                item.id, e
            ))?;
            // Convert ApiError directly to anyhow::Error
            return Err(anyhow!(e.clone()));
        }
         Err(ref e @ ApiError::ParseError(_)) => {
            // Borrow the error
            // Use write_log
             write_log(&format!(
                "[ERROR] Failed to parse API response for item {}: {}. No content to save.",
                item.id, e
            ))?;
            // Convert ApiError directly to anyhow::Error
            return Err(anyhow!(e.clone()));
        }
        Err(ref e @ ApiError::EmptyChoices) => {
             // Use write_log
            write_log(&format!(
                "[ERROR] API returned empty choices for item {}: {}. No content to save.",
                item.id, e
            ))?;
            // Convert ApiError directly to anyhow::Error
            return Err(anyhow!(e.clone()));
        }
    }

    // Return the finish_reason if successful or if API returned a controlled error
    match rewrite_result {
        Ok((_, finish_reason)) => Ok(finish_reason),
        Err(ApiError::ApiReturnedError { finish_reason, .. }) => Ok(finish_reason),
        // Other errors were already returned as Err(anyhow::Error)
        Err(e) => Err(anyhow!(e)), // Convert remaining ApiError variants - this signals critical errors to run_rewriter
    }
}

fn rewrite_content(content: &str, provider: &AiProviderConfig, prompt: &str) -> Result<(String, Option<String>), ApiError> {
    let client = Client::builder()
        .timeout(Duration::from_secs(120)) // Set timeout to 120 seconds
        .build()
        .map_err(|e| ApiError::RequestError(Arc::new(e)))?;
    
    let messages = vec![
        Message {
            role: "system".to_string(),
            content: prompt.to_string(),
        },
        Message {
            role: "user".to_string(),
            content: content.to_string(),
        },
    ];
    
    match provider.provider_type {
        AiProviderType::OpenRouter => {
            if let Some(reasoning) = &provider.reasoning {
                let _ = write_log(&format!(
                    "[DEBUG] OpenRouter reasoning config applied: enabled={:?}, effort={:?}",
                    reasoning.enabled, reasoning.effort
                ));
            }

            let request = OpenRouterChatRequest {
                model: provider.model.clone(),
                messages,
                reasoning: provider.reasoning.clone(),
            };

            // Log before sending - ignore result
            let _ = write_log(&format!(
                "[DEBUG] Sending request to OpenRouter API with model: {}",
                provider.model
            ));

            let response = client
                .post("https://openrouter.ai/api/v1/chat/completions")
                .header("Authorization", format!("Bearer {}", provider.api_key))
                .header("Content-Type", "application/json")
                .json(&request)
                .send()
                .map_err(|e| ApiError::RequestError(Arc::new(e)))?;

            parse_chat_response(response)
        }
        AiProviderType::Perplexity => {
            let reasoning_effort = perplexity_reasoning_effort_from_reasoning(&provider.reasoning);
            if let Some(ref effort) = reasoning_effort {
                let _ = write_log(&format!(
                    "[DEBUG] Perplexity reasoning_effort applied: {}",
                    effort
                ));
            }

            let request = PerplexityChatRequest {
                model: provider.model.clone(),
                messages,
                reasoning_effort,
            };

            let _ = write_log(&format!(
                "[DEBUG] Sending request to Perplexity API with model: {}",
                provider.model
            ));

            let response = client
                .post("https://api.perplexity.ai/chat/completions")
                .header("Authorization", format!("Bearer {}", provider.api_key))
                .header("Content-Type", "application/json")
                .json(&request)
                .send()
                .map_err(|e| ApiError::RequestError(Arc::new(e)))?;

            parse_chat_response(response)
        }
        AiProviderType::Gemini => {
            // Gemini OpenAI compatibility docs:
            // https://ai.google.dev/gemini-api/docs/openai
            // Endpoint:
            //   POST https://generativelanguage.googleapis.com/v1beta/openai/chat/completions
            // Auth:
            //   Authorization: Bearer <GEMINI_API_KEY>
            let reasoning_effort = gemini_reasoning_effort_from_reasoning(&provider.reasoning);
            if let Some(ref effort) = reasoning_effort {
                let _ = write_log(&format!(
                    "[DEBUG] Gemini reasoning_effort applied: {}",
                    effort
                ));
            }

            let request = GeminiChatRequest {
                model: provider.model.clone(),
                messages,
                reasoning_effort,
            };

            let _ = write_log(&format!(
                "[DEBUG] Sending request to Gemini OpenAI-compatible API with model: {}",
                provider.model
            ));

            let response = client
                .post("https://generativelanguage.googleapis.com/v1beta/openai/chat/completions")
                .header("Authorization", format!("Bearer {}", provider.api_key))
                .header("Content-Type", "application/json")
                .json(&request)
                .send()
                .map_err(|e| ApiError::RequestError(Arc::new(e)))?;

            parse_chat_response(response)
        }
    }
}

fn gemini_reasoning_effort_from_reasoning(reasoning: &Option<ReasoningConfig>) -> Option<String> {
    let reasoning = reasoning.as_ref()?;

    // If explicitly disabled, do not send reasoning_effort.
    if reasoning.enabled == Some(false) {
        return None;
    }

    let effort = reasoning.effort.as_deref()?;

    // Gemini (OpenAI compatibility) docs mention reasoning_effort like:
    // minimal | low | medium | high
    // We map OpenRouter-style values to Gemini values:
    // xhigh/high -> high, medium -> medium, low -> low, minimal -> minimal, none -> omit.
    match effort {
        "xhigh" | "high" => Some("high".to_string()),
        "medium" => Some("medium".to_string()),
        "low" => Some("low".to_string()),
        "minimal" => Some("minimal".to_string()),
        "none" => None,
        other => {
            let _ = write_log(&format!(
                "[WARN] AI_PROVIDER_REWRITER_REASONING_EFFORT='{}' is not supported for Gemini. Omitting reasoning_effort.",
                other
            ));
            None
        }
    }
}

fn perplexity_reasoning_effort_from_reasoning(reasoning: &Option<ReasoningConfig>) -> Option<String> {
    let reasoning = reasoning.as_ref()?;

    // If explicitly disabled, do not send reasoning_effort.
    if reasoning.enabled == Some(false) {
        return None;
    }

    let effort = reasoning.effort.as_deref()?;

    // Perplexity docs allow: low | medium | high.
    // We map OpenRouter-style values to Perplexity values:
    // xhigh/high -> high, medium -> medium, low/minimal -> low, none -> omit.
    match effort {
        "xhigh" | "high" => Some("high".to_string()),
        "medium" => Some("medium".to_string()),
        "low" | "minimal" => Some("low".to_string()),
        "none" => None,
        // Note: effort is validated on input, so this branch is mainly defensive.
        other => {
            let _ = write_log(&format!(
                "[WARN] AI_PROVIDER_REWRITER_REASONING_EFFORT='{}' is not supported for Perplexity. Omitting reasoning_effort.",
                other
            ));
            None
        }
    }
}

fn parse_chat_response(response: reqwest::blocking::Response) -> Result<(String, Option<String>), ApiError> {
    let status = response.status();
    // Read the body text regardless of status code
    let response_text = response
        .text()
        .map_err(|e| ApiError::RequestError(Arc::new(e)))?;

    // Try to parse the JSON response
    let response_data: ChatResponse = match serde_json::from_str(&response_text) {
        Ok(data) => data,
        Err(e) => {
            // Log the raw text on parsing failure
            let _ = write_log(&format!(
                "[ERROR] Failed to parse AI provider response JSON. Status: {}. Body: {}",
                status, response_text
            ));
            return Err(ApiError::ParseError(Arc::new(e.into())));
        }
    };

    // Log the parsed response - ignore result
    let _ = write_log(&format!(
        "[DEBUG] Parsed response from AI provider: {:?}",
        response_data
    ));

    if response_data.choices.is_empty() {
        let _ = write_log("[ERROR] AI provider returned empty choices array.");
        return Err(ApiError::EmptyChoices);
    }

    let choice = &response_data.choices[0];
    let rewritten_content = choice.message.content.clone();
    let finish_reason = choice.finish_reason.clone();

    // Check HTTP status AFTER parsing, as API might return error status but valid JSON body
    if !status.is_success() {
        let cleaned_content = post_process_html_response(&rewritten_content);

        // Defensive validation: ensure we actually got HTML back.
        // If the model returns meta-text (reasoning, instructions, markdown), force a retry.
        if !looks_like_html(&cleaned_content) {
            let _ = write_log(&format!(
                "[WARN] AI provider returned non-success status ({}) AND content does not look like HTML. Forcing finish_reason='error' to trigger retry.",
                status
            ));
            return Err(ApiError::ApiReturnedError {
                status,
                content: cleaned_content,
                finish_reason: Some("error".to_string()),
            });
        }

        let _ = write_log(&format!(
            "[WARN] AI provider returned non-success status: {}. Finish Reason: {:?}. Content received: {} bytes.",
            status,
            finish_reason,
            cleaned_content.len()
        ));

        return Err(ApiError::ApiReturnedError {
            status,
            content: cleaned_content,
            finish_reason,
        });
    }

    // Check finish_reason even on success status
    if let Some(reason) = &finish_reason {
        if reason == "error" || reason == "length" {
            let _ = write_log(&format!(
                "[WARN] AI provider returned success status ({}) but finish_reason is '{}'.",
                status, reason
            ));

            let cleaned_content = post_process_html_response(&rewritten_content);

            if !looks_like_html(&cleaned_content) {
                let _ = write_log(
                    "[WARN] finish_reason is error/length AND cleaned content does not look like HTML (keeping finish_reason as-is)."
                );
            }
            return Err(ApiError::ApiReturnedError {
                status,
                content: cleaned_content,
                finish_reason: finish_reason.clone(),
            });
        }
    }

    let cleaned_content = post_process_html_response(&rewritten_content);
    if !looks_like_html(&cleaned_content) {
        let _ = write_log(
            "[WARN] AI provider returned success status but cleaned content does not look like HTML. Forcing finish_reason='error' to trigger retry."
        );
        return Err(ApiError::ApiReturnedError {
            status,
            content: cleaned_content,
            finish_reason: Some("error".to_string()),
        });
    }
    Ok((cleaned_content, finish_reason))
}

fn read_ai_provider_reasoning_from_env() -> Option<ReasoningConfig> {
    // Env-driven, optional behavior:
    // - if neither env is provided (or both empty), behave as before (no `reasoning` field)
    // - if provided, attach `reasoning` object to request
    let enabled_raw = env::var("AI_PROVIDER_REWRITER_REASONING_ENABLED").ok();
    let effort_raw = env::var("AI_PROVIDER_REWRITER_REASONING_EFFORT").ok();

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
                "[WARN] AI_PROVIDER_REWRITER_REASONING_ENABLED has invalid value '{}'. Ignoring.",
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
                "[WARN] AI_PROVIDER_REWRITER_REASONING_EFFORT has invalid value '{}'. Allowed: xhigh|high|medium|low|minimal|none. Ignoring.",
                v
            ));
            None
        }
    }
}

fn post_process_html_response(content: &str) -> String {
    let content = content.trim();

    // 1) Prefer extracting an HTML document if present anywhere in the response.
    if let Some(extracted) = extract_html_document_block(content) {
        return extracted;
    }

    // 2) Then try fenced blocks with explicit html language.
    if let Some(extracted) = extract_fenced_block(content, "```html") {
        return extracted;
    }

    // 3) Finally, try any fenced block.
    if let Some(extracted) = extract_any_fenced_block(content) {
        return extracted;
    }

    content.to_string()
}

fn looks_like_html(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    (lower.contains("<html") && lower.contains("</html>"))
        || (lower.contains("<body") && lower.contains("</body>"))
        || (lower.contains("<!doctype html") && lower.contains("</html>"))
}

fn extract_html_document_block(s: &str) -> Option<String> {
    // Try to extract a full HTML document if the model wrapped it with commentary.
    let start = s.find("<html").or_else(|| s.find("<!DOCTYPE")).or_else(|| s.find("<!doctype"))?;
    let end_tag = "</html>";
    let end = s.rfind(end_tag)? + end_tag.len();
    if start >= end {
        return None;
    }
    Some(s[start..end].trim().to_string())
}

fn extract_fenced_block(s: &str, fence_start: &str) -> Option<String> {
    let start_pos = s.find(fence_start)?;
    let after = &s[start_pos + fence_start.len()..];

    // If fence is followed by a newline, skip it. Otherwise keep the following text as-is.
    let after = if let Some(stripped) = after.strip_prefix("\r\n") {
        stripped
    } else if let Some(stripped) = after.strip_prefix('\n') {
        stripped
    } else {
        after
    };

    let end_pos = after.find("```")?;
    Some(after[..end_pos].trim().to_string())
}

fn extract_any_fenced_block(s: &str) -> Option<String> {
    let start_pos = s.find("```")?;
    let after = &s[start_pos + 3..];

    // Skip language id line if present.
    let (after, _) = if let Some(nl) = after.find('\n') {
        (&after[nl + 1..], true)
    } else {
        (after, false)
    };

    let end_pos = after.find("```")?;
    Some(after[..end_pos].trim().to_string())
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
    println!("rewriter: {}", message);
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
        content: String, // Include the (potentially partial) content
        finish_reason: Option<String>, // Include the finish reason if available
    },
    #[error("AI provider returned empty choices")]
    EmptyChoices,
}