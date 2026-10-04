//! Command-line entrypoint for listing and scoring articles.
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
const DEFAULT_MAX_SCORE_WORDS: usize = 2_000;

#[derive(Debug, Parser)]
#[command(about = "Find AI-generated articles in Matter")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List articles without contacting Pangram.
    List {
        #[command(subcommand)]
        source: ListSource,
    },

    /// Submit one article to Pangram for billable analysis.
    Score {
        #[command(subcommand)]
        source: ScoreSource,
    },
}

#[derive(Debug, Subcommand)]
enum ListSource {
    Matter {
        #[arg(
            long,
            default_value_t = 1,
            help = "Maximum items to inspect (1-20)",
            value_parser = parse_limit
        )]
        limit: u8,

        #[arg(
            long,
            env = "MATTER_API_TOKEN",
            hide_env_values = true,
            value_name = "TOKEN"
        )]
        matter_api_token: String,
    },
}

#[derive(Debug, Subcommand)]
enum ScoreSource {
    Matter {
        #[arg(
            value_name = "ITEM_ID",
            help = "Matter item ID reported by `list matter`"
        )]
        item_id: String,

        #[arg(
            long,
            default_value_t = DEFAULT_MAX_SCORE_WORDS,
            help = "Maximum article words authorized for this submission",
            value_parser = parse_max_words
        )]
        max_words: usize,

        #[arg(
            long,
            env = "MATTER_API_TOKEN",
            hide_env_values = true,
            value_name = "TOKEN"
        )]
        matter_api_token: String,

        #[arg(
            long,
            env = "PANGRAM_API_KEY",
            hide_env_values = true,
            value_name = "KEY"
        )]
        pangram_api_key: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::List {
            source:
                ListSource::Matter {
                    limit,
                    matter_api_token,
                },
        } => list_matter(matter_api_token, limit).await,
        Command::Score {
            source:
                ScoreSource::Matter {
                    item_id,
                    max_words,
                    matter_api_token,
                    pangram_api_key,
                },
        } => score_matter(matter_api_token, pangram_api_key, item_id, max_words).await,
    }
}

async fn list_matter(matter_api_token: String, limit: u8) -> Result<()> {
    let client = matter::Client::new(matter_api_token);
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

async fn score_matter(
    matter_api_token: String,
    pangram_api_key: String,
    item_id: String,
    max_words: usize,
) -> Result<()> {
    let matter_client = matter::Client::new(matter_api_token);
    let article = match matter_client
        .fetch_article(&item_id)
        .await
        .with_context(|| format!("failed to fetch Matter item {item_id}"))?
    {
        matter::ItemOutcome::Available(article) => article,
        matter::ItemOutcome::Unavailable(item) => {
            bail!("Matter item {} cannot be scored: {}", item.id, item.reason);
        }
    };
    enforce_word_limit(&article, max_words)?;

    let pangram_client = pangram::Client::new(pangram_api_key);
    let model = pangram_client
        .discover_model()
        .await
        .context("failed to discover an available Pangram model")?;
    let outcome = pangram_client
        .score(&article.markdown, &model)
        .await
        .with_context(|| format!("failed to score Matter item {}", article.source_id))?;
    let color = color_enabled();

    match outcome {
        pangram::ScoreOutcome::Complete(score) => {
            println!("{}", format_scored_article(&article, &score, color));
        }
        pangram::ScoreOutcome::Pending { task, stage } => {
            println!("{}", format_article(&article, color));
            println!(
                "  Pangram task {} ({}) remains {stage} and can be resumed.",
                task.task_id, task.model
            );
        }
    }

    Ok(())
}

fn enforce_word_limit(article: &article::Article, max_words: usize) -> Result<()> {
    let word_count = article.word_count();
    if word_count > max_words {
        bail!(
            "refusing to score {word_count} words from Matter item {}; maximum is {max_words}. \
             Use --max-words {word_count} to authorize this submission",
            article.source_id
        );
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

fn format_scored_article(
    article: &article::Article,
    score: &pangram::Score,
    color: bool,
) -> String {
    format!(
        "{}\n  {}\n  {} words · Matter: {}\n  Pangram: {} / {} · {:.1}% AI · {:.1}% AI-assisted · {:.1}% human\n  {}",
        render_title(&article.title, color),
        article.url,
        article.word_count(),
        article.source_id,
        score.model,
        score.version,
        score.fractions.ai * 100.0,
        score.fractions.ai_assisted * 100.0,
        score.fractions.human * 100.0,
        score.prediction.headline,
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

fn parse_max_words(value: &str) -> std::result::Result<usize, String> {
    let max_words = value
        .parse::<usize>()
        .map_err(|_| "max words must be a positive integer".to_owned())?;

    if max_words > 0 {
        Ok(max_words)
    } else {
        Err("max words must be greater than zero".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::enforce_word_limit;
    use crate::article::Article;
    use url::Url;

    #[test]
    fn scoring_above_the_word_ceiling_requires_an_explicit_override() {
        let article = Article {
            source_id: "itm-123".to_owned(),
            title: "A long article".to_owned(),
            url: Url::parse("https://example.com/article").unwrap(),
            markdown: "word ".repeat(2_001),
        };

        let error = enforce_word_limit(&article, 2_000).unwrap_err();
        assert!(error.to_string().contains("--max-words 2001"));
        enforce_word_limit(&article, 2_001).unwrap();
    }
}
