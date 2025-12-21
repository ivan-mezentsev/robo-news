# Downloader

A Rust CLI application for downloading news pages from URLs stored in a SQLite database.

## Overview

The downloader application performs the following steps:
1. Connects to an existing SQLite database
2. Finds all news entries with "new" status
3. Downloads the pages from the URLs in these entries
4. Saves the content to HTML files in the "data" directory
5. Updates the status to "downloaded" after successful download
6. Logs errors if any downloads fail
7. Repeats this process every minute

## Requirements

- Rust 1.56 or higher
- SQLite database with a "news" table containing at least the following columns:
  - id (TEXT)
  - title (TEXT)
  - url (TEXT)
  - date (TEXT)
  - status (TEXT)

## Usage

Build the application:

```bash
cargo build --release
```

Run the application:

```bash
./target/release/downloader
```

The application will:
- Create a "data" directory if it doesn't exist
- Download news pages and save them as HTML files
- Update the status in the database
- Run continuously, checking for new items every minute

## File Naming

Downloaded files are named according to the pattern:
```
data/news_<id>.html
```
Where `<id>` is the ID of the news item from the database.

## Logging

The application logs its activities to stdout, or to Docker logs if running in a container. 