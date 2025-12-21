use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::{thread, time::Duration};
use readability::extractor;
use url::Url;

const DB_PATH: &str = "data/news.db";
const DATA_DIR: &str = "data";
const SCRAPE_INTERVAL_SECS: u64 = 60; // 1 minute

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
    #[allow(dead_code)]
    status: String,
}

fn main() -> Result<()> {
    // Initialize database and data directory
    let conn = init_db()?;
    init_data_dir()?;
    
    log("[INFO] Starting scraper...")?;
    
    // Main loop - run every minute
    loop {
        if let Err(e) = run_scraper(&conn) {
            log(&format!("[ERROR] Error during scraping: {}", e))?;
        }
        
        log(&format!("[INFO] Sleeping for {} seconds", SCRAPE_INTERVAL_SECS))?;
        thread::sleep(Duration::from_secs(SCRAPE_INTERVAL_SECS));
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

fn run_scraper(conn: &Connection) -> Result<()> {
    log("[INFO] Checking for news items to scrape")?;
    
    // Fetch news items with "downloaded" status
    let news_items = fetch_downloaded_items(conn)?;
    
    if news_items.is_empty() {
        log("[INFO] No items to scrape")?;
        return Ok(());
    }
    
    log(&format!("[INFO] Found {} items to scrape", news_items.len()))?;
    
    // Process each news item
    for item in news_items {
        match process_news_item(&item) {
            Ok(_) => {
                // Update status to "scraper"
                update_status(conn, &item.id, "scraper")?;
                log(&format!("[INFO] Successfully scraped news item: {}", item.id))?;
            }
            Err(e) => {
                log(&format!("[ERROR] Failed to scrape news item {}: {}", item.id, e))?;
                // Continue with the next item
            }
        }
    }
    
    log("[INFO] Scraping process completed")?;
    Ok(())
}

fn fetch_downloaded_items(conn: &Connection) -> Result<Vec<NewsItem>> {
    let mut stmt = conn.prepare("SELECT id, title, url, date, status FROM news WHERE status = 'downloaded' ORDER BY date ASC")?;
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

fn process_news_item(item: &NewsItem) -> Result<()> {
    // Read the HTML file
    let input_file_path = format!("{}/news_{}.html", DATA_DIR, item.id);
    let output_file_path = format!("{}/scraper_{}.html", DATA_DIR, item.id);
    
    let mut html_content = String::new();
    let mut file = File::open(&input_file_path)
        .context(format!("Failed to open file: {}", input_file_path))?;
    
    file.read_to_string(&mut html_content)
        .context("Failed to read HTML file")?;
    
    // Create a fake URL for the readability library
    let base_url = Url::parse("http://localhost/").context("Failed to parse base URL")?;
    
    // Extract readable content using readability
    // We need to create a cursor from our string to use it with readability
    let mut content_cursor = std::io::Cursor::new(html_content);
    let product = extractor::extract(&mut content_cursor, &base_url)
        .context("Failed to extract content with readability")?;
    
    // Save the extracted content
    let mut output_file = File::create(&output_file_path)
        .context(format!("Failed to create file: {}", output_file_path))?;
    
    // Create a simple HTML document with the extracted content
    let result_html = format!(
        "<!DOCTYPE html>
<html>
<head>
    <meta charset=\"UTF-8\">
    <title>{}</title>
</head>
<body>
    <h1>{}</h1>
    {}
</body>
</html>",
        product.title,
        product.title,
        product.content
    );
    
    output_file.write_all(result_html.as_bytes())
        .context("Failed to write extracted content to file")?;
    
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
