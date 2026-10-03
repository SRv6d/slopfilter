//! Command-line entrypoint for scanning Matter articles.
use std::{
    env,
    io::{self, IsTerminal},
};

use anstyle::Style;
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use futures_util::{StreamExt, pin_mut, stream};

pub mod article;
pub mod matter;
pub mod pangram;
pub mod store;

const MAX_CONCURRENT_FETCHES: usize = 4;

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

    let client = matter::Client::new(token);
    let articles = client
        .queued_articles(limit)
        .await
        .context("failed to list queued Matter articles")?;
    let color = color_enabled();
    let fetches = stream::iter(articles)
        .map(|item| {
            let client = client.clone();

            async move {
                let outcome = client.fetch_article(&item.id).await;
                (item, outcome)
            }
        })
        .buffer_unordered(MAX_CONCURRENT_FETCHES);
    pin_mut!(fetches);

    let mut available = 0;
    let mut failures = 0;

    while let Some((item, outcome)) = fetches.next().await {
        match outcome {
            Ok(matter::ItemOutcome::Available(article)) => {
                available += 1;
                println!("{}", format_article(&article, color));
            }
            Ok(matter::ItemOutcome::Unavailable(item)) => {
                eprintln!("{}", format_unavailable(&item, color));
            }
            Err(error) => {
                failures += 1;
                eprintln!(
                    "Failed: {}\n  {error} · Matter: {}",
                    render_title(&item.title, color),
                    item.id,
                );
            }
        }
    }

    if available == 0 {
        println!("No eligible articles found.");
    }

    if failures > 0 {
        bail!("failed to fetch {failures} queued Matter article(s)");
    }

    Ok(())
}

fn color_enabled() -> bool {
    io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none()
}

fn format_article(article: &article::Article, color: bool) -> String {
    format!(
        "{}\n  {}\n  {} words · Matter: {}",
        render_title(&article.title, color),
        article.url,
        article.word_count(),
        article.source_id,
    )
}

fn format_unavailable(item: &matter::UnavailableItem, color: bool) -> String {
    format!(
        "Unavailable: {}\n  {} · Matter: {}",
        render_title(&item.title, color),
        item.reason,
        item.id,
    )
}

fn render_title(title: &str, color: bool) -> String {
    if color {
        let style = Style::new().bold();
        format!("{style}{title}{style:#}")
    } else {
        title.to_owned()
    }
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

#[cfg(test)]
mod tests {
    use super::{format_article, render_title};
    use crate::article::Article;
    use url::Url;

    #[test]
    fn plain_article_output_leads_with_the_title() {
        let article = Article {
            source_id: "itm_123".to_owned(),
            title: "A saved article".to_owned(),
            url: Url::parse("https://example.com/article").unwrap(),
            markdown: "Article body".to_owned(),
        };

        assert_eq!(
            format_article(&article, false),
            "A saved article\n  https://example.com/article\n  2 words · Matter: itm_123"
        );
    }

    #[test]
    fn styled_title_is_bold() {
        assert_eq!(
            render_title("A saved article", true),
            "\u{1b}[1mA saved article\u{1b}[0m"
        );
    }
}
