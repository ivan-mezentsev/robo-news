# Scraper

A Rust CLI application for extracting readable content from downloaded news pages.

## Overview

The scraper application performs the following steps:
1. Connects to an existing SQLite database
2. Finds all news entries with "downloaded" status
3. Processes the corresponding HTML files in the "data" directory
4. Extracts the main content using the readability-fork library
5. Saves the extracted content to new HTML files in the "data" directory
6. Updates the status to "scraper" after successful extraction
7. Repeats this process every minute

## Requirements

- Rust 1.56 or higher
- SQLite database with a "news" table containing at least the following columns:
  - id (TEXT)
  - title (TEXT)
  - url (TEXT)
  - date (TEXT)
  - status (TEXT)
- Downloaded HTML files in the "data" directory

## Usage

Build the application:

```bash
cargo build --release
```

Run the application:

```bash
./target/release/scraper
```

The application will:
- Create a "data" directory if it doesn't exist
- Process HTML files and extract readable content
- Save the extracted content as new HTML files
- Update the status in the database
- Run continuously, checking for new items every minute

## File Naming

- Input files are expected to follow the pattern: `data/news_<id>.html`
- Output files are named according to the pattern: `data/scraper_<id>.html`

Where `<id>` is the ID of the news item from the database.

## Logging

The application logs its activities to stdout, or to Docker logs if running in a container. 