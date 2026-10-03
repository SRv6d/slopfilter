use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

pub mod article;
pub mod matter;
pub mod pangram;
pub mod store;

#[derive(Debug, Parser)]
#[command(about = "Find AI-generated articles in Matter")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Scan {
        #[arg(
            long,
            default_value_t = 1,
            help = "Maximum items to inspect (1-20)",
            value_parser = parse_limit
        )]
        limit: u8,

        #[arg(long)]
        dry_run: bool,
        #[arg(
            long,
            env = "MATTER_API_TOKEN",
            hide_env_values = true,
            value_name = "TOKEN"
        )]
        matter_api_token: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Scan {
            limit,
            dry_run,
            matter_api_token,
        } => scan(matter_api_token, limit, dry_run).await,
    }
}

async fn scan(token: String, limit: u8, dry_run: bool) -> Result<()> {
    if !dry_run {
        bail!("scan currently only supports --dry-run");
    }

    let result = matter::Client::new(token)
        .queue_articles(limit)
        .await
        .context("failed to fetch queued Matter articles")?;

    for unavailable in &result.unavailable {
        eprintln!("Unavailable {}: {}", unavailable.id, unavailable.reason);
    }

    if result.articles.is_empty() {
        println!("No eligible articles found.");
    }

    for article in result.articles {
        println!(
            "{}\n  title: {}\n  url: {}\n  words: {}",
            article.source_id,
            article.title,
            article.url,
            article.word_count(),
        );
    }

    Ok(())
}

fn parse_limit(value: &str) -> std::result::Result<u8, String> {
    let limit = value
        .parse::<u8>()
        .map_err(|_| "limit must be an integer between 1 and 20".to_owned())?;

    if (1..=20).contains(&limit) {
        Ok(limit)
    } else {
        Err("limit must be between 1 and 20".to_owned())
    }
}
