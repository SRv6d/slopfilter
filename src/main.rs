//! Command-line entrypoint for scanning Matter articles.
use std::{
    env,
    io::{self, IsTerminal},
    sync::Arc,
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

        #[arg(
            long,
            env = "PANGRAM_API_KEY",
            hide_env_values = true,
            required_unless_present = "dry_run",
            value_name = "KEY"
        )]
        pangram_api_key: Option<String>,
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
            pangram_api_key,
        } => scan(matter_api_token, pangram_api_key, limit, dry_run).await,
    }
}

async fn scan(
    matter_api_token: String,
    pangram_api_key: Option<String>,
    limit: u8,
    dry_run: bool,
) -> Result<()> {
    let pangram = if dry_run {
        None
    } else {
        let client = pangram::Client::new(
            pangram_api_key.expect("Clap requires a Pangram API key unless --dry-run is present"),
        );
        let model: Arc<str> = client
            .discover_model()
            .await
            .context("failed to discover an available Pangram model")?
            .into();
        Some((client, model))
    };

    let client = matter::Client::new(matter_api_token);
    let articles = client
        .queued_articles(limit)
        .await
        .context("failed to list queued Matter articles")?;
    let color = color_enabled();
    let fetches = stream::iter(articles)
        .map(|item| {
            let client = client.clone();
            let pangram = pangram.clone();

            async move {
                let outcome = client.fetch_article(&item.id).await;
                let score = match (&outcome, pangram) {
                    (Ok(matter::ItemOutcome::Available(article)), Some((client, model))) => {
                        Some(client.score(&article.markdown, &model).await)
                    }
                    _ => None,
                };
                (item, outcome, score)
            }
        })
        .buffer_unordered(MAX_CONCURRENT_FETCHES);
    pin_mut!(fetches);

    let mut available = 0;
    let mut failures = 0;

    while let Some((item, outcome, score)) = fetches.next().await {
        match outcome {
            Ok(matter::ItemOutcome::Available(article)) => {
                available += 1;

                match score {
                    None => println!("{}", format_article(&article, color)),
                    Some(Ok(pangram::ScoreOutcome::Complete(score))) => {
                        println!("{}", format_scored_article(&article, &score, color));
                    }
                    Some(Ok(pangram::ScoreOutcome::Pending { task, stage })) => {
                        println!("{}", format_article(&article, color));
                        println!(
                            "  Pangram task {} ({}) remains {stage} and can be resumed.",
                            task.task_id, task.model
                        );
                    }
                    Some(Err(error)) => {
                        failures += 1;
                        eprintln!(
                            "Failed: {}\n  {error} · Matter: {}",
                            render_title(&item.title, color),
                            item.id,
                        );
                    }
                }
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
        bail!("failed to process {failures} queued Matter article(s)");
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
