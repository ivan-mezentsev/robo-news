use anyhow::{Context, Result, anyhow};
use reqwest::blocking::Client;
use reqwest::blocking::multipart;
use rusqlite::{params, Connection};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::{thread, time::Duration};
use scraper::{Html, Selector, ElementRef};
use serde_json::json;
use chrono::{DateTime, NaiveDateTime};

const DB_PATH: &str = "data/news.db";
const DATA_DIR: &str = "data";
const PUBLISH_INTERVAL_SECS: u64 = 60; // 1 minute

// Telegram Bot API limits (see docs referenced in issue)
const TELEGRAM_SENDMESSAGE_TEXT_LIMIT_UTF16: usize = 4096;

struct NewsItem {
    id: String,
    #[allow(dead_code)]
    title: String,
    #[allow(dead_code)]
    url: String,
    #[allow(dead_code)]
    date: String,
    #[allow(dead_code)]
    status: String,
    #[allow(dead_code)]
    error: Option<String>,
}

fn main() -> Result<()> {
    // Initialize database and data directory
    let conn = init_db()?;
    init_data_dir()?;
    
    // Check required environment variables
    check_env_vars()?;
    
    log("[INFO] Starting publisher...")?;
    
    // Main loop - run every minute
    loop {
        if let Err(e) = run_publisher(&conn) {
            log(&format!("[ERROR] Error during publishing: {}", e))?;
        }
        
        log(&format!("[INFO] Sleeping for {} seconds", PUBLISH_INTERVAL_SECS))?;
        thread::sleep(Duration::from_secs(PUBLISH_INTERVAL_SECS));
    }
}

fn check_env_vars() -> Result<()> {
    let tg_token = env::var("TG_TOKEN")
        .context("TG_TOKEN environment variable is not set")?;
    
    let tg_chat_id = env::var("TG_CHAT_ID")
        .context("TG_CHAT_ID environment variable is not set")?;
    
    if tg_token.is_empty() {
        return Err(anyhow!("TG_TOKEN environment variable is empty"));
    }
    
    if tg_chat_id.is_empty() {
        return Err(anyhow!("TG_CHAT_ID environment variable is empty"));
    }
    
    Ok(())
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

fn run_publisher(conn: &Connection) -> Result<()> {
    log("[INFO] Checking for illustrator news items to publish")?;
    
    // Fetch news items with "illustrator" status
    let news_items = fetch_illustrator_items(conn)?;
    
    if news_items.is_empty() {
        log("[INFO] No illustrator items to publish")?;
        return Ok(());
    }
    
    log(&format!("[INFO] Found {} illustrator items to publish", news_items.len()))?;
    
    // Process and publish each news item
    for item in news_items {
        log(&format!("[INFO] Processing item: {}", item.id))?;
        
        // Process the HTML
        match process_html_file(&item) {
            Ok(_) => {
                // Send to Telegram
                match send_to_telegram(&item) {
                    Ok(_) => {
                        // Update status to "published"
                        update_status(conn, &item.id, "published", None)?;
                        log(&format!("[INFO] Successfully published news item: {}", item.id))?;
                    }
                    Err(e) => {
                        let error_msg = format!("Failed to send to Telegram: {}", e);
                        log(&format!("[ERROR] {}", error_msg))?;
                        
                        // Check if it's a rate limit error
                        if error_msg.contains("Too Many Requests") {
                            // Extract retry_after value
                            let retry_seconds = extract_retry_after(&error_msg).unwrap_or(60);
                            
                            log(&format!("[INFO] Rate limit hit, waiting for {} seconds...", retry_seconds))?;
                            thread::sleep(Duration::from_secs(retry_seconds));
                            
                            // Try again
                            match send_to_telegram(&item) {
                                Ok(_) => {
                                    update_status(conn, &item.id, "published", None)?;
                                    log(&format!("[INFO] Successfully published news item after retry: {}", item.id))?;
                                }
                                Err(retry_err) => {
                                    let retry_error_msg = format!("Failed to send to Telegram after retry: {}", retry_err);
                                    log(&format!("[ERROR] {}", retry_error_msg))?;
                                    update_status(conn, &item.id, "publish_error", Some(&retry_error_msg))?;
                                }
                            }
                        } else {
                            // Update status to "publish_error"
                            update_status(conn, &item.id, "publish_error", Some(&error_msg))?;
                        }
                    }
                }
            }
            Err(e) => {
                let error_msg = format!("Failed to process HTML: {}", e);
                log(&format!("[ERROR] {}", error_msg))?;
                update_status(conn, &item.id, "publish_error", Some(&error_msg))?;
            }
        }
    }
    
    log("[INFO] Publish process completed")?;
    Ok(())
}

fn fetch_illustrator_items(conn: &Connection) -> Result<Vec<NewsItem>> {
    let mut stmt = conn.prepare("SELECT id, title, url, date, status FROM news WHERE status = 'illustrator' ORDER BY date ASC")?;
    let news_iter = stmt.query_map([], |row| {
        Ok(NewsItem {
            id: row.get(0)?,
            title: row.get(1)?,
            url: row.get(2)?,
            date: row.get(3)?,
            status: row.get(4)?,
            error: None,
        })
    })?;
    
    let mut news_items = Vec::new();
    for item in news_iter {
        news_items.push(item?);
    }
    
    Ok(news_items)
}

fn process_html_file(item: &NewsItem) -> Result<()> {
    let input_path = format!("{}/rewriter_{}.html", DATA_DIR, item.id);
    let output_path = format!("{}/publisher_{}.html", DATA_DIR, item.id);
    
    // Read the input file
    let mut input_file = File::open(&input_path)
        .context(format!("Failed to open input file: {}", input_path))?;
    
    let mut html_content = String::new();
    input_file.read_to_string(&mut html_content)
        .context("Failed to read HTML file")?;
    
    // Process the HTML
    let processed_html = transform_html(&html_content)?;
    
    // Write the processed HTML to the output file
    let mut output_file = File::create(&output_path)
        .context(format!("Failed to create output file: {}", output_path))?;
    
    output_file.write_all(processed_html.as_bytes())
        .context("Failed to write processed HTML to file")?;
    
    Ok(())
}

fn transform_html(html_content: &str) -> Result<String> {
    // Parse the HTML document
    let document = Html::parse_document(html_content);
    
    // Select the body element
    let body_selector = Selector::parse("body").map_err(|e| anyhow!("Invalid selector: {}", e))?;
    
    // Extract the body content or return an error if not found
    let body = document.select(&body_selector).next()
        .ok_or_else(|| anyhow!("Body tag not found in HTML"))?;
    
    let mut result = String::new();
    
    // Process all elements in the body
    process_element(&mut result, &body);
    
    // Clean up multiple consecutive newlines and whitespace
    let cleaned = result
        .replace("\n\n\n", "\n\n")  // Replace triple newlines with double
        .replace("  ", " ");         // Replace double spaces with single
    
    Ok(cleaned)
}

fn process_element(result: &mut String, element: &ElementRef) {
    let tag_name = element.value().name();
    
    // Handle specific tags
    match tag_name {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            // Convert heading to bold and add double newline
            result.push_str("<b>");
            process_element_children(result, element);
            result.push_str("</b>\n\n");
        },
        "p" => {
            // Extract paragraph content and add double newline
            process_element_children(result, element);
            result.push_str("\n\n");
        },
        "strong" | "b" => {
            // Bold text
            result.push_str("<b>");
            process_element_children(result, element);
            result.push_str("</b>");
        },
        "a" => {
            // Hyperlinks
            if let Some(href) = element.value().attr("href") {
                result.push_str(&format!("<a href=\"{}\">", href));
                process_element_children(result, element);
                result.push_str("</a>");
            } else {
                process_element_children(result, element);
            }
        },
        "br" => {
            // Line break
            result.push('\n');
        },
        // Skip html, head, etc.
        "html" | "head" | "meta" | "title" | "style" | "script" => {},
        // Process other elements
        _ => {
            process_element_children(result, element);
            
            // Add spacing for block elements
            if !["span", "a", "strong", "b", "i", "em"].contains(&tag_name)
                && !result.ends_with("\n\n")
                && !result.is_empty()
            {
                result.push_str("\n\n");
            }
        }
    }
}

fn process_element_children(result: &mut String, element: &ElementRef) {
    for child in element.children() {
        match child.value() {
            scraper::node::Node::Text(text) => {
                // Add text content, collapsing whitespace
                let text = text.text.trim();
                if !text.is_empty() {
                    if !result.is_empty() && !result.ends_with(' ') && !result.ends_with('\n') {
                        result.push(' ');
                    }
                    result.push_str(text);
                }
            },
            scraper::node::Node::Element(_) => {
                if let Some(child_element) = ElementRef::wrap(child) {
                    process_element(result, &child_element);
                }
            },
            _ => {}
        }
    }
}

fn send_to_telegram(item: &NewsItem) -> Result<()> {
    let token = env::var("TG_TOKEN").context("Failed to get TG_TOKEN")?;
    let chat_id = env::var("TG_CHAT_ID").context("Failed to get TG_CHAT_ID")?;
    
    let file_path = format!("{}/publisher_{}.html", DATA_DIR, item.id);
    let image_path = format!("{}/illustrator_{}.png", DATA_DIR, item.id);
    
    // Read the file content
    let mut file = File::open(&file_path)
        .context(format!("Failed to open file for Telegram: {}", file_path))?;
    
    let mut content = String::new();
    file.read_to_string(&mut content)
        .context("Failed to read HTML content for Telegram")?;
    
    // Add publication date and source link
    // Parse date from database format to display format
    let formatted_date = parse_and_format_date(&item.date)?;
    
    // Append publication date and source link
    content.push_str(&format!("\n\nОпубликовано: {}\n<a href=\"{}\">Читать оригинал</a>", 
                              formatted_date, item.url));

    // Always: send photo WITHOUT caption, then send the post as a separate message.
    if !Path::new(&image_path).exists() {
        return Err(anyhow!("Illustrator image not found: {}", image_path));
    }

    let client = Client::new();

    send_photo(&client, &token, &chat_id, &image_path, None)?;
    send_message_no_fallback(&client, &token, &chat_id, &content)?;
    Ok(())
}

fn send_photo(client: &Client, token: &str, chat_id: &str, image_path: &str, caption: Option<&str>) -> Result<()> {
    let url = format!("https://api.telegram.org/bot{}/sendPhoto", token);

    let image_bytes = fs::read(image_path)
        .context(format!("Failed to read illustrator image: {}", image_path))?;

    let mut form = multipart::Form::new()
        .text("chat_id", chat_id.to_string())
        .part(
            "photo",
            multipart::Part::bytes(image_bytes)
                .file_name(
                    Path::new(image_path)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                )
                .mime_str("image/png")
                .context("Failed to set mime type for photo")?,
        )
        .text("parse_mode", "HTML".to_string());

    if let Some(c) = caption {
        form = form.text("caption", c.to_string());
    }

    let response = client
        .post(&url)
        .multipart(form)
        .send()
        .context("Failed to send sendPhoto request to Telegram API")?;

    if !response.status().is_success() {
        let error_text = response.text().unwrap_or_else(|_| "Unknown error".to_string());
        return Err(anyhow!("Telegram API error (sendPhoto): {}", error_text));
    }

    Ok(())
}

fn send_message_no_fallback(
    client: &Client,
    token: &str,
    chat_id: &str,
    html_message: &str,
) -> Result<()> {
    // Telegram Bot API sendMessage: text is limited to 1-4096 characters AFTER entities parsing.
    // We approximate by stripping HTML tags and counting UTF-16 code units.
    let approx_len_utf16 = telegram_text_len_utf16_after_entities_guess(html_message);
    if approx_len_utf16 > TELEGRAM_SENDMESSAGE_TEXT_LIMIT_UTF16 {
        return Err(anyhow!(
            "Telegram message too long (approx {} UTF-16 units, limit {}). No fallback is configured.",
            approx_len_utf16,
            TELEGRAM_SENDMESSAGE_TEXT_LIMIT_UTF16
        ));
    }

    let url = format!("https://api.telegram.org/bot{}/sendMessage", token);
    let response = client
        .post(&url)
        .json(&json!({
            "chat_id": chat_id,
            "text": html_message,
            "parse_mode": "HTML",
            "disable_web_page_preview": true
        }))
        .send()
        .context("Failed to send request to Telegram API")?;

    if !response.status().is_success() {
        let error_text = response.text().unwrap_or_else(|_| "Unknown error".to_string());
        return Err(anyhow!("Telegram API error (sendMessage): {}", error_text));
    }

    Ok(())
}

fn telegram_text_len_utf16_after_entities_guess(html_text: &str) -> usize {
    // Very rough approximation of "after entities parsing":
    // 1) remove tags, turning them into whitespace; 2) collapse whitespace; 3) count UTF-16 code units.
    let plain = strip_html_tags_to_text(html_text);
    plain.encode_utf16().count()
}

fn strip_html_tags_to_text(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    let mut last_was_space = false;

    let mut tag_buf = String::new();

    for ch in html.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;

                // For some block-ish tags, add a space/newline to keep words separated.
                let tag = tag_buf.trim().to_lowercase();
                if (tag.starts_with("br") || tag.starts_with("/p") || tag.starts_with("p")) && !last_was_space {
                    out.push(' ');
                    last_was_space = true;
                }
                tag_buf.clear();
            } else {
                tag_buf.push(ch);
            }
            continue;
        }

        if ch == '<' {
            in_tag = true;
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
            continue;
        }

        if ch.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(ch);
            last_was_space = false;
        }
    }

    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_tags_basic() {
        let s = "<b>Hello</b> <a href=\"https://example.com\">world</a><br>!";
        assert_eq!(strip_html_tags_to_text(s), "Hello world !");
    }

    #[test]
    fn telegram_len_uses_utf16() {
        // 😀 is 2 UTF-16 code units
        let s = "😀";
        assert_eq!(telegram_text_len_utf16_after_entities_guess(s), 2);
    }
}

// Function to parse and format the date
fn parse_and_format_date(date_str: &str) -> Result<String> {
    // First try to parse as a full RFC3339 date with timezone
    if let Ok(dt) = DateTime::parse_from_rfc3339(date_str) {
        // Use the original time instead of converting to UTC
        return Ok(dt.format("%Y-%m-%d %H:%M:%S").to_string());
    }
    
    // Try other common formats without timezone
    let formats = [
        "%Y-%m-%dT%H:%M:%S%.fZ",       // ISO 8601 with milliseconds
        "%Y-%m-%dT%H:%M:%SZ",          // ISO 8601 without milliseconds
        "%Y-%m-%d %H:%M:%S%.f",        // Standard format with milliseconds
        "%Y-%m-%d %H:%M:%S",           // Standard format without milliseconds
    ];
    
    for format in formats {
        if let Ok(dt) = NaiveDateTime::parse_from_str(date_str, format) {
            return Ok(dt.format("%Y-%m-%d %H:%M:%S").to_string());
        }
    }
    
    // If parsing fails, use the original date string
    log(&format!("[WARN] Could not parse date: {}, using as is", date_str))?;
    Ok(date_str.to_string())
}

fn update_status(conn: &Connection, id: &str, status: &str, error: Option<&str>) -> Result<()> {
    if let Some(error_msg) = error {
        // Log the error but don't try to save it to the non-existent column
        log(&format!("[ERROR] Item {}: {}", id, error_msg))?;
    }
    
    conn.execute(
        "UPDATE news SET status = ? WHERE id = ?",
        params![status, id],
    )?;
    
    Ok(())
}

fn log(message: &str) -> std::io::Result<()> {
    let exe_path = env::current_exe()?;
    let exe_name = exe_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let full_message = format!("{}: {}", exe_name, message);

    // If /.dockerenv exist, write to /proc/1/fd/1.
    // Note: This path might not be optimal for all container environments.
    if Path::new("/.dockerenv").exists() {
        // Attempt to open the file, handle potential errors
        match OpenOptions::new().append(true).open("/proc/1/fd/1") {
            Ok(mut file) => {
                file.write_all(full_message.as_bytes())?;
                file.write_all(b"\n")?;
            }
            Err(e) => {
                // Fallback to stdout if opening /proc/1/fd/1 fails
                eprintln!("Failed to open /proc/1/fd/1 for logging: {}, falling back to stdout", e);
                println!("{}", full_message);
            }
        }
    } else {
        println!("{}", full_message);
    }
    Ok(())
}

// Function to extract retry_after value from Telegram API error message
fn extract_retry_after(error_msg: &str) -> Option<u64> {
    // Parse JSON error response to extract retry_after value
    if let Some(start) = error_msg.find("retry_after") {
        if let Some(value_start) = error_msg[start..].find(":") {
            // Get the substring after "retry_after:"
            let value_str = &error_msg[start + value_start + 1..];
            
            // Parse the number (handling potential commas and end quotes)
            let mut num_str = String::new();
            for c in value_str.chars() {
                if c.is_ascii_digit() {
                    num_str.push(c);
                } else if !num_str.is_empty() {
                    // Stop at first non-digit after we've seen digits
                    break;
                }
            }
            
            // Convert to u64
            if !num_str.is_empty() {
                return num_str.parse::<u64>().ok();
            }
        }
    }
    None
}
