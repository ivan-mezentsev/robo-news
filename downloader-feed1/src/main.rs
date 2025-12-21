use anyhow::{Context, Result};
use reqwest::blocking::Client;
use rusqlite::{params, Connection};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::{thread, time::Duration};

const DB_PATH: &str = "data/news.db";
const DATA_DIR: &str = "data";
const DOWNLOAD_INTERVAL_SECS: u64 = 60; // 1 minute

struct NewsItem {
    id: String,
    title: String,
    url: String,
    // Keep these fields even though they're not directly used in our code
    // because they are part of the database schema and are returned by the query
    #[allow(dead_code)]
    date: String,
    #[allow(dead_code)]
    status: String,
}

fn main() -> Result<()> {
    // Initialize database and data directory
    let conn = init_db()?;
    init_data_dir()?;
    
    log("[INFO] Starting downloader...")?;
    
    // Main loop - run every minute
    loop {
        if let Err(e) = run_downloader(&conn) {
            log(&format!("[ERROR] Error during downloading: {}", e))?;
        }
        
        log(&format!("[INFO] Sleeping for {} seconds", DOWNLOAD_INTERVAL_SECS))?;
        thread::sleep(Duration::from_secs(DOWNLOAD_INTERVAL_SECS));
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

fn run_downloader(conn: &Connection) -> Result<()> {
    log("[INFO] Checking for new news items to download")?;
    
    // Fetch news items with "new" status
    let news_items = fetch_new_items(conn)?;
    
    if news_items.is_empty() {
        log("[INFO] No new items to download")?;
        return Ok(());
    }
    
    log(&format!("[INFO] Found {} new items to download", news_items.len()))?;
    
    // Download each news item
    for item in news_items {
        match download_news_item(&item) {
            Ok(_) => {
                // Update status to "downloaded"
                update_status(conn, &item.id, "downloaded")?;
                log(&format!("[INFO] Successfully downloaded news item: {}", item.title))?;
            }
            Err(e) => {
                log(&format!("[ERROR] Failed to download news item {}: {}", item.id, e))?;
                // Continue with the next item
            }
        }
    }
    
    log("[INFO] Download process completed")?;
    Ok(())
}

fn fetch_new_items(conn: &Connection) -> Result<Vec<NewsItem>> {
    let mut stmt = conn.prepare("SELECT id, title, url, date, status FROM news WHERE status = 'new' ORDER BY date ASC")?;
    let news_iter = stmt.query_map([], |row| {
        Ok(NewsItem {
            id: row.get(0)?,
            title: row.get(1)?,
            url: row.get(2)?,
            date: row.get(3)?,
            status: row.get(4)?,
        })
    })?;
    
    let mut news_items = Vec::new();
    for item in news_iter {
        news_items.push(item?);
    }
    
    Ok(news_items)
}

fn download_news_item(item: &NewsItem) -> Result<()> {
    let client = Client::new();
    let response = client
        .get(&item.url)
        .send()
        .context("Failed to send request")?;
    
    if !response.status().is_success() {
        return Err(anyhow::anyhow!("HTTP error: {}", response.status()));
    }
    
    let html = response
        .text()
        .context("Failed to get response text")?;
    
    let file_path = format!("{}/news_{}.html", DATA_DIR, item.id);
    let mut file = File::create(&file_path)
        .context(format!("Failed to create file: {}", file_path))?;
    
    file.write_all(html.as_bytes())
        .context("Failed to write HTML to file")?;
    
    Ok(())
}

fn update_status(conn: &Connection, id: &str, status: &str) -> Result<()> {
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
