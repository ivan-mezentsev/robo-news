use anyhow::{Context, Result};
use chrono::{FixedOffset, Utc};
use reqwest::blocking::Client;
use rusqlite::{params, Connection};
use scraper::{Html, Selector};
use sha2::{Digest, Sha256};
use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::{thread, time::Duration};

const DB_PATH: &str = "data/news.db";
const PARSE_INTERVAL_SECS: u64 = 600; // 10 minutes

struct NewsItem {
    id: String,
    title: String,
    url: String,
    date: String,
    status: String,
}

fn main() -> Result<()> {
    // Initialize database
    let conn = init_db()?;

    let feed1_url = env::var("FEED1_URL").context("FEED1_URL environment variable is not set")?;
    let feed1_url = feed1_url.trim().to_string();
    if feed1_url.is_empty() {
        return Err(anyhow::anyhow!("FEED1_URL environment variable is empty"));
    }
    
    log("[INFO] Starting...")?;
    
    // Main loop - run every 10 minutes
    loop {
        if let Err(e) = run_parser(&conn, &feed1_url) {
            log(&format!("[ERROR] Error during parsing: {}", e))?;
        }
        
        log(&format!("[INFO] Sleeping for {} seconds", PARSE_INTERVAL_SECS))?;
        thread::sleep(Duration::from_secs(PARSE_INTERVAL_SECS));
    }
}

fn init_db() -> Result<Connection> {
    let conn = Connection::open(DB_PATH)
        .context("Failed to open database connection")?;
    
    conn.execute(
        "CREATE TABLE IF NOT EXISTS news (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            url TEXT NOT NULL,
            date TEXT NOT NULL,
            status TEXT NOT NULL
        )",
        [],
    )
    .context("Failed to create news table")?;
    
    Ok(conn)
}

fn run_parser(conn: &Connection, feed_url: &str) -> Result<()> {
    log(&format!("[INFO] Starting parsing {}\"", feed_url))?;
    
    // Fetch and parse the webpage
    let news_items = fetch_news(feed_url).context("Failed to fetch news")?;
    
    // Process and store new items
    let mut new_count = 0;
    for item in news_items {
        if !is_news_exists(conn, &item.id)? {
            store_news(conn, &item)?;
            new_count += 1;
            log(&format!("[INFO] Added new news: {}", item.title))?;
        }
    }
    
    log(&format!("[INFO] Parsing completed. Added {} new items", new_count))?;
    Ok(())
}

fn fetch_news(feed_url: &str) -> Result<Vec<NewsItem>> {
    let client = Client::new();
    let response = client
        .get(feed_url)
        .send()
        .context("Failed to send request")?;
    
    let html = response
        .text()
        .context("Failed to get response text")?;
    
    let document = Html::parse_document(&html);
    
    // Select headlines - handle error conversion manually
    let headline_selector = match Selector::parse("h3.entry-title.td-module-title") {
        Ok(selector) => selector,
        Err(e) => return Err(anyhow::anyhow!("Failed to create headline selector: {:?}", e)),
    };
    
    // Select dates - handle error conversion manually
    let date_selector = match Selector::parse("div.td-editor-date span.td-post-date time") {
        Ok(selector) => selector,
        Err(e) => return Err(anyhow::anyhow!("Failed to create date selector: {:?}", e)),
    };
    
    let headlines: Vec<_> = document.select(&headline_selector).collect();
    let dates: Vec<_> = document.select(&date_selector).collect();
    
    let mut news_items = Vec::new();
    
    // Create a fixed UTC+02:00 timezone offset for Belgrade/Serbia
    let belgrade_offset = FixedOffset::east_opt(2 * 3600).unwrap();
    
    for (i, headline) in headlines.iter().enumerate() {
        // Create link selector (this won't fail for a simple tag)
        let link_selector = Selector::parse("a").unwrap();
        
        // Extract title and URL
        if let Some(link) = headline.select(&link_selector).next() {
            // Get title from text content only
            let title = headline.text().collect::<Vec<_>>().join(" ").trim().to_string();
            
            // Skip news items without proper titles
            if title.is_empty() {
                continue;
            }
            
            let url = link.value().attr("href").unwrap_or("").to_string();
            
            // Skip news items with URLs that don't start with the base URL
            if !url.starts_with(feed_url) {
                continue;
            }
            
            // Generate ID from URL
            let id = generate_id(&url);
            
            // Extract date from datetime attribute if available
            let date = if i < dates.len() {
                // Try to get the datetime attribute first
                match dates[i].value().attr("datetime") {
                    Some(datetime_str) if !datetime_str.is_empty() => datetime_str.to_string(),
                    _ => {
                        // Fallback: Use current time in Belgrade timezone (UTC+02:00)
                        Utc::now()
                            .with_timezone(&belgrade_offset)
                            .to_rfc3339()
                    }
                }
            } else {
                // Fallback: Use current time in Belgrade timezone (UTC+02:00)
                Utc::now()
                    .with_timezone(&belgrade_offset)
                    .to_rfc3339()
            };
            
            news_items.push(NewsItem {
                id,
                title,
                url,
                date,
                status: "new".to_string(),
            });
        }
    }
    
    // Reverse the order to match the behavior of the old script
    news_items.reverse();
    
    Ok(news_items)
}

fn generate_id(url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    let result = hasher.finalize();
    hex::encode(result)
}

fn is_news_exists(conn: &Connection, id: &str) -> Result<bool> {
    let mut stmt = conn.prepare("SELECT 1 FROM news WHERE id = ? LIMIT 1")?;
    let exists = stmt.exists(params![id])?;
    Ok(exists)
}

fn store_news(conn: &Connection, item: &NewsItem) -> Result<()> {
    conn.execute(
        "INSERT INTO news (id, title, url, date, status) VALUES (?, ?, ?, ?, ?)",
        params![item.id, item.title, item.url, item.date, item.status],
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
