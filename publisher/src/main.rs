use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::sync::Arc;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::path::PathBuf;
use tokio::time::{sleep, Duration};
use scraper::{Html, Selector, ElementRef};
use chrono::{DateTime, NaiveDateTime};

use grammers_client::{Client as TgClient, InputMessage, SignInError};
use grammers_mtsender::SenderPool;
use grammers_session::types::PeerRef;
use grammers_session::Session;
use grammers_session::SessionData;
use grammers_session::types::{ChannelState, DcOption, PeerId, PeerInfo, UpdateState, UpdatesState};

const DB_PATH: &str = "data/news.db";
const DATA_DIR: &str = "data";
const PUBLISH_INTERVAL_SECS: u64 = 60; // 1 minute

// Telegram user API (grammers) session storage
const TG_SESSION_PATH: &str = "data/telegram.session";

struct TelegramContext {
    client: TgClient,
    target_chat: PeerRef,
    #[allow(dead_code)]
    session: Arc<FileSession>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedSessionData {
    home_dc: i32,
    dc_options: HashMap<i32, DcOption>,
    peer_infos: HashMap<PeerId, PeerInfo>,
    updates_state: UpdatesState,
}

impl Default for PersistedSessionData {
    fn default() -> Self {
        let data = SessionData::default();
        Self {
            home_dc: data.home_dc,
            dc_options: data.dc_options,
            peer_infos: data.peer_infos,
            updates_state: data.updates_state,
        }
    }
}

/// JSON-backed session storage for grammers.
///
/// We use this to persist the session under `data/telegram.session` without introducing a second
/// native sqlite3 dependency (rusqlite already links sqlite3).
struct FileSession {
    path: PathBuf,
    data: std::sync::Mutex<PersistedSessionData>,
}

impl FileSession {
    fn load_or_create<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        let data = if path.exists() {
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read telegram session file: {}", path.display()))?;
            serde_json::from_str::<PersistedSessionData>(&raw)
                .with_context(|| format!("Failed to parse telegram session JSON: {}", path.display()))?
        } else {
            PersistedSessionData::default()
        };

        let session = Self {
            path,
            data: std::sync::Mutex::new(data),
        };

        // Ensure the file exists on disk even before first login.
        session.save().ok();

        Ok(session)
    }

    fn save(&self) -> Result<()> {
        let tmp_path = self.path.with_extension("session.tmp");
        let data = self.data.lock().unwrap();
        let json = serde_json::to_string_pretty(&*data).context("Failed to serialize telegram session")?;
        fs::write(&tmp_path, json)
            .with_context(|| format!("Failed to write tmp telegram session: {}", tmp_path.display()))?;
        fs::rename(&tmp_path, &self.path)
            .with_context(|| format!("Failed to move tmp session into place: {}", self.path.display()))?;
        Ok(())
    }

    fn save_best_effort(&self) {
        if let Err(e) = self.save() {
            let _ = log(&format!("[WARN] Failed to persist telegram session: {}", e));
        }
    }
}

impl Session for FileSession {
    fn home_dc_id(&self) -> i32 {
        self.data.lock().unwrap().home_dc
    }

    fn set_home_dc_id(&self, dc_id: i32) {
        self.data.lock().unwrap().home_dc = dc_id;
        self.save_best_effort();
    }

    fn dc_option(&self, dc_id: i32) -> Option<DcOption> {
        self.data.lock().unwrap().dc_options.get(&dc_id).cloned()
    }

    fn set_dc_option(&self, dc_option: &DcOption) {
        self.data
            .lock()
            .unwrap()
            .dc_options
            .insert(dc_option.id, dc_option.clone());
        self.save_best_effort();
    }

    fn peer(&self, peer: PeerId) -> Option<PeerInfo> {
        self.data.lock().unwrap().peer_infos.get(&peer).cloned()
    }

    fn cache_peer(&self, peer: &PeerInfo) {
        self.data
            .lock()
            .unwrap()
            .peer_infos
            .insert(peer.id(), peer.clone());
        self.save_best_effort();
    }

    fn updates_state(&self) -> UpdatesState {
        self.data.lock().unwrap().updates_state.clone()
    }

    fn set_update_state(&self, update: UpdateState) {
        let mut data = self.data.lock().unwrap();
        match update {
            UpdateState::All(updates_state) => {
                data.updates_state = updates_state;
            }
            UpdateState::Primary { pts, date, seq } => {
                data.updates_state.pts = pts;
                data.updates_state.date = date;
                data.updates_state.seq = seq;
            }
            UpdateState::Secondary { qts } => {
                data.updates_state.qts = qts;
            }
            UpdateState::Channel { id, pts } => {
                data.updates_state.channels.retain(|c| c.id != id);
                data.updates_state.channels.push(ChannelState { id, pts });
            }
        }
        drop(data);
        self.save_best_effort();
    }
}

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

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // Initialize database and data directory
    let conn = init_db()?;
    init_data_dir()?;
    
    // Check required environment variables
    check_env_vars()?;

    // Initialize Telegram client (user API) and authorize if needed
    let tg = init_telegram().await?;
    
    log("[INFO] Starting publisher...")?;
    
    // Main loop - run every minute
    loop {
        if let Err(e) = run_publisher(&conn, &tg).await {
            log(&format!("[ERROR] Error during publishing: {}", e))?;
        }
        
        log(&format!("[INFO] Sleeping for {} seconds", PUBLISH_INTERVAL_SECS))?;
        sleep(Duration::from_secs(PUBLISH_INTERVAL_SECS)).await;
    }
}

fn check_env_vars() -> Result<()> {
    let api_id = env::var("TG_API_ID").context("TG_API_ID environment variable is not set")?;
    let api_hash = env::var("TG_API_HASH").context("TG_API_HASH environment variable is not set")?;
    let tg_chat_id =
        env::var("TG_CHAT_ID").context("TG_CHAT_ID environment variable is not set")?;

    if api_id.trim().is_empty() {
        return Err(anyhow!("TG_API_ID environment variable is empty"));
    }

    if api_hash.trim().is_empty() {
        return Err(anyhow!("TG_API_HASH environment variable is empty"));
    }

    if tg_chat_id.trim().is_empty() {
        return Err(anyhow!("TG_CHAT_ID environment variable is empty"));
    }

    Ok(())
}

async fn init_telegram() -> Result<TelegramContext> {
    // NOTE: API hash is required by the sign-in flow in the current grammers API.
    // Source evidence:
    // - https://github.com/Lonami/grammers/blob/master/grammers-client/src/client/auth.rs
    let api_id: i32 = env::var("TG_API_ID")
        .context("TG_API_ID is not set")?
        .trim()
        .parse()
        .context("TG_API_ID must be an integer")?;
    let api_hash = env::var("TG_API_HASH").context("TG_API_HASH is not set")?;

    // Ensure data/ exists (also used for telegram.session)
    init_data_dir()?;

    // Persistent session storage (JSON file, path name as requested)
    let session = Arc::new(
        FileSession::load_or_create(TG_SESSION_PATH)
            .context(format!("Failed to open telegram session at {}", TG_SESSION_PATH))?,
    );

    // Sender pool drives network I/O.
    let pool = SenderPool::new(Arc::clone(&session), api_id);
    let client = TgClient::new(&pool);
    let grammers_mtsender::SenderPool { runner, updates, .. } = pool;

    // We don't consume updates in this service. Dropping the receiver makes the sender side
    // stop delivering them (send() will fail), avoiding unbounded growth.
    drop(updates);

    tokio::spawn(async move {
        runner.run().await;
    });

    ensure_telegram_authorized(&client, &api_hash).await?;

    // Force a final best-effort save after authorization.
    session.save_best_effort();

    let target_chat = resolve_target_chat(&client).await?;

    Ok(TelegramContext {
        client,
        target_chat,
        session,
    })
}

async fn ensure_telegram_authorized(client: &TgClient, api_hash: &str) -> Result<()> {
    if client.is_authorized().await.context("Telegram authorization check failed")? {
        return Ok(());
    }

    log("[INFO] Telegram session is not authorized yet; starting first-run login flow")?;

    let phone = match env::var("TG_PHONE") {
        Ok(p) if !p.trim().is_empty() => p,
        _ => prompt_line("Enter phone number (international format, e.g. +14155550132): ")?,
    };

    let token = client
        .request_login_code(phone.trim(), api_hash)
        .await
        .context("Failed to request Telegram login code")?;

    let code = prompt_line("Enter the login code you received: ")?;

    match client.sign_in(&token, code.trim()).await {
        Ok(user) => {
            if let Some(first_name) = user.first_name() {
                log(&format!("[INFO] Telegram authorized as {}", first_name))?;
            } else {
                log("[INFO] Telegram authorized")?;
            }
            Ok(())
        }
        Err(SignInError::PasswordRequired(password_token)) => {
            log("[INFO] Telegram 2FA password required")?;
            let password = prompt_line("Enter 2FA password: ")?;
            let user = client
                .check_password(password_token, password.trim().as_bytes())
                .await
                .context("Failed to sign in with 2FA password")?;
            if let Some(first_name) = user.first_name() {
                log(&format!("[INFO] Telegram authorized as {}", first_name))?;
            } else {
                log("[INFO] Telegram authorized")?;
            }
            Ok(())
        }
        Err(SignInError::SignUpRequired { .. }) => Err(anyhow!(
            "Telegram sign-up required. Please log in with an official Telegram client first, then rerun publisher."
        )),
        Err(e) => Err(anyhow!("Telegram sign-in failed: {}", e)),
    }
}

async fn resolve_target_chat(client: &TgClient) -> Result<PeerRef> {
    let raw = env::var("TG_CHAT_ID").context("TG_CHAT_ID is not set")?;
    let s = raw.trim();
    if s.is_empty() {
        return Err(anyhow!("TG_CHAT_ID is empty"));
    }

    // If it looks like a username (recommended), resolve it once.
    let looks_like_username = s.starts_with('@') || s.chars().any(|c| c.is_ascii_alphabetic());
    if looks_like_username {
        let username = s.trim_start_matches('@');
        let peer = client
            .resolve_username(username)
            .await
            .context("Failed to resolve TG_CHAT_ID as username")?
            .ok_or_else(|| anyhow!("Chat @{} not found (TG_CHAT_ID)", username))?;

        return Ok((&peer).into());
    }

    // Otherwise, attempt to find by numeric ID in dialogs.
    let wanted_id: i64 = if let Some(rest) = s.strip_prefix("-100") {
        rest.parse().context("TG_CHAT_ID '-100...' is not numeric")?
    } else if let Some(rest) = s.strip_prefix('-') {
        rest.parse().context("TG_CHAT_ID '-...' is not numeric")?
    } else {
        s.parse().context("TG_CHAT_ID is not numeric")?
    };

    let mut dialogs = client.iter_dialogs();
    while let Some(dialog) = dialogs
        .next()
        .await
        .context("Failed while iterating Telegram dialogs")?
    {
        let peer = dialog.peer();
        if peer.id().bare_id() == wanted_id {
            return Ok(peer.into());
        }
    }

    Err(anyhow!(
        "TG_CHAT_ID={} was not found in your dialogs. Use a public username (e.g. @channel) in TG_CHAT_ID.",
        wanted_id
    ))
}

fn prompt_line(prompt: &str) -> Result<String> {
    // NOTE: Console prompts are used only for the first-run login.
    print!("{}", prompt);
    std::io::stdout().flush().ok();

    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("Failed to read from stdin")?;
    Ok(line)
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

async fn run_publisher(conn: &Connection, tg: &TelegramContext) -> Result<()> {
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
                match send_to_telegram(tg, &item).await {
                    Ok(_) => {
                        // Update status to "published"
                        update_status(conn, &item.id, "published", None)?;
                        log(&format!("[INFO] Successfully published news item: {}", item.id))?;
                    }
                    Err(e) => {
                        let error_msg = format!("Failed to send to Telegram: {}", e);
                        log(&format!("[ERROR] {}", error_msg))?;

                        // Update status to "publish_error"
                        update_status(conn, &item.id, "publish_error", Some(&error_msg))?;
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

async fn send_to_telegram(tg: &TelegramContext, item: &NewsItem) -> Result<()> {
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

    if !Path::new(&image_path).exists() {
        return Err(anyhow!("Illustrator image not found: {}", image_path));
    }

    // Post photo + HTML caption in a single message (user API via grammers).
    // Evidence (pinned grammers git revision used by Cargo):
    // - InputMessage::new().html(...).photo(...):
    //   https://github.com/Lonami/grammers/blob/e15d820169462839173b60c3b69e0aebbeae848d/grammers-client/src/types/input_message.rs
    // - Client::send_message(peer, message):
    //   https://github.com/Lonami/grammers/blob/e15d820169462839173b60c3b69e0aebbeae848d/grammers-client/src/client/messages.rs
    let uploaded = tg
        .client
        .upload_file(&image_path)
        .await
        .context("Failed to upload photo to Telegram")?;

    let message = InputMessage::new().html(&content).photo(uploaded);
    tg.client
        .send_message(tg.target_chat, message)
        .await
        .context("Failed to send message to Telegram")?;

    Ok(())
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

// Note: Bot API specific retry-after parsing was removed when migrating to user API.
