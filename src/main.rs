//! Command-line entrypoint for listing and scoring articles.
use std::{
    env, fs,
    io::{self, IsTerminal, Read},
    path::PathBuf,
};

use anstyle::Style;
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use futures_util::{StreamExt, pin_mut, stream};

pub mod article;
pub mod article_set;
pub mod matter;
pub mod pangram;
pub mod store;

const MAX_CONCURRENT_FETCHES: usize = 4;
const DEFAULT_MAX_SCORE_WORDS: usize = 2_000;

#[derive(Debug, Parser)]
#[command(about = "List and score text for likely AI authorship")]
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

    /// Submit one explicit input to Pangram for billable analysis.
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

        #[arg(long, value_name = "PATH", help = "Save a portable article set")]
        save: Option<PathBuf>,

        #[arg(long, requires = "save", help = "Replace an existing article-set file")]
        force: bool,
    },
}

#[derive(Debug, Args)]
struct ScoreOptions {
    #[arg(
        long,
        default_value_t = DEFAULT_MAX_SCORE_WORDS,
        help = "Maximum input words authorized for this submission",
        value_parser = parse_max_words
    )]
    max_words: usize,

    #[arg(
        long,
        env = "PANGRAM_API_KEY",
        hide_env_values = true,
        value_name = "KEY"
    )]
    pangram_api_key: String,
}

#[derive(Debug, Subcommand)]
enum ScoreSource {
    /// Score a fetched Matter article.
    Matter {
        #[arg(
            value_name = "ITEM_ID",
            help = "Matter item ID reported by `list matter`"
        )]
        item_id: String,

        #[arg(
            long,
            env = "MATTER_API_TOKEN",
            hide_env_values = true,
            value_name = "TOKEN"
        )]
        matter_api_token: String,

        #[command(flatten)]
        score: ScoreOptions,
    },

    File {
        #[arg(
            value_name = "PATH",
            help = "UTF-8 text, Markdown, or article-set file"
        )]
        path: PathBuf,

        #[arg(
            long,
            value_name = "ITEM_ID",
            help = "Item to select from an article set"
        )]
        item: Option<String>,

        #[command(flatten)]
        score: ScoreOptions,
    },

    /// Score UTF-8 text or an article set piped on standard input.
    Stdin {
        #[arg(
            long,
            value_name = "ITEM_ID",
            help = "Item to select from an article set"
        )]
        item: Option<String>,

        #[command(flatten)]
        score: ScoreOptions,
    },
}

enum ScoreDocument {
    Matter(article::Article),
    ArticleSet(article_set::StoredArticle),
    File { path: PathBuf, text: String },
    Stdin { text: String },
}

impl ScoreDocument {
    fn text(&self) -> &str {
        match self {
            Self::Matter(article) => &article.markdown,
            Self::ArticleSet(article) => &article.markdown,
            Self::File { text, .. } | Self::Stdin { text } => text,
        }
    }

    fn word_count(&self) -> usize {
        self.text().split_whitespace().count()
    }
}

impl std::fmt::Display for ScoreDocument {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Matter(article) => write!(formatter, "Matter item {}", article.source_id),
            Self::ArticleSet(article) => write!(formatter, "article-set item {}", article.id),
            Self::File { path, .. } => write!(formatter, "file {}", path.display()),
            Self::Stdin { .. } => formatter.write_str("standard input"),
        }
    }
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
                    save,
                    force,
                },
        } => list_matter(matter_api_token, limit, save, force).await,
        Command::Score {
            source:
                ScoreSource::Matter {
                    item_id,
                    matter_api_token,
                    score,
                },
        } => score_matter(matter_api_token, item_id, score).await,
        Command::Score {
            source: ScoreSource::File { path, item, score },
        } => score_file(path, item, score).await,
        Command::Score {
            source: ScoreSource::Stdin { item, score },
        } => score_stdin(item, score).await,
    }
}

async fn list_matter(
    matter_api_token: String,
    limit: u8,
    save: Option<PathBuf>,
    force: bool,
) -> Result<()> {
    let client = matter::Client::new(matter_api_token);
    let articles = client
        .queued_articles(limit)
        .await
        .context("failed to list queued Matter articles")?;
    let mut stored_articles = save.as_ref().map(|_| Vec::with_capacity(articles.len()));
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
    let mut total_words = 0;
    let mut failures = 0;

    while let Some((item, outcome)) = fetches.next().await {
        match outcome {
            Ok(matter::ItemOutcome::Available(article)) => {
                available += 1;
                let word_count = article.word_count();
                total_words += word_count;
                println!("{}", format_article(&article, word_count, color));

                if let Some(articles) = &mut stored_articles {
                    articles.push(article_set::StoredArticle::from_article(
                        article, word_count,
                    ));
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

    let noun = if available == 1 {
        "article"
    } else {
        "articles"
    };
    println!("Total: {total_words} words across {available} eligible {noun}.");

    if failures > 0 {
        bail!("failed to fetch {failures} queued Matter article(s)");
    }

    if let (Some(path), Some(articles)) = (save, stored_articles) {
        let article_set = article_set::ArticleSet::matter(articles);
        article_set::write(&path, &article_set, force)?;
        println!("Saved article set to {}.", path.display());
    }

    Ok(())
}

async fn score_matter(
    matter_api_token: String,
    item_id: String,
    score: ScoreOptions,
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

    score_document(ScoreDocument::Matter(article), score).await
}

async fn score_file(path: PathBuf, item: Option<String>, score: ScoreOptions) -> Result<()> {
    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read UTF-8 text from {}", path.display()))?;
    let document = decode_score_input(text, item, Some(path))?;

    score_document(document, score).await
}

async fn score_stdin(item: Option<String>, score: ScoreOptions) -> Result<()> {
    if io::stdin().is_terminal() {
        bail!("no piped input; pass text on standard input");
    }

    let mut text = String::new();
    io::stdin()
        .read_to_string(&mut text)
        .context("failed to read UTF-8 text from standard input")?;
    let document = decode_score_input(text, item, None)?;

    score_document(document, score).await
}

fn decode_score_input(
    text: String,
    item: Option<String>,
    path: Option<PathBuf>,
) -> Result<ScoreDocument> {
    if let Some(article_set) = article_set::ArticleSet::parse(&text)? {
        let item =
            item.ok_or_else(|| anyhow::anyhow!("article-set input requires --item <ITEM_ID>"))?;
        return Ok(ScoreDocument::ArticleSet(article_set.select(&item)?));
    }

    if item.is_some() {
        bail!("--item is only valid for article-set input");
    }

    Ok(match path {
        Some(path) => ScoreDocument::File { path, text },
        None => ScoreDocument::Stdin { text },
    })
}

async fn score_document(document: ScoreDocument, score: ScoreOptions) -> Result<()> {
    if document.text().trim().is_empty() {
        bail!("{document} contains no text");
    }

    let word_count = document.word_count();
    enforce_word_limit(&document, word_count, score.max_words)?;

    let pangram_client = pangram::Client::new(score.pangram_api_key);
    let model = pangram_client
        .discover_model()
        .await
        .context("failed to discover an available Pangram model")?;
    let outcome = pangram_client
        .score(document.text(), &model)
        .await
        .with_context(|| format!("failed to score {document}"))?;
    let color = color_enabled();

    match outcome {
        pangram::ScoreOutcome::Complete(score) => {
            println!(
                "{}",
                format_scored_document(&document, word_count, &score, color)
            );
        }
        pangram::ScoreOutcome::Pending { task, stage } => {
            println!("{}", format_document(&document, word_count, color));
            println!(
                "  Pangram task {} ({}) remains {stage} and can be resumed.",
                task.task_id, task.model
            );
        }
    }

    Ok(())
}

fn enforce_word_limit(document: &ScoreDocument, word_count: usize, max_words: usize) -> Result<()> {
    if word_count > max_words {
        bail!(
            "refusing to score {word_count} words from {document}; maximum is {max_words}. \
             Use --max-words {word_count} to authorize this submission"
        );
    }

    Ok(())
}

fn color_enabled() -> bool {
    io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none()
}

fn format_article(article: &article::Article, word_count: usize, color: bool) -> String {
    format!(
        "{}\n  {}\n  {word_count} words · Matter: {}",
        render_title(&article.title, color),
        article.url,
        article.source_id,
    )
}

fn format_document(document: &ScoreDocument, word_count: usize, color: bool) -> String {
    match document {
        ScoreDocument::Matter(article) => format_article(article, word_count, color),
        ScoreDocument::ArticleSet(article) => format!(
            "{}\n  {}\n  {word_count} words · Article set: {}",
            render_title(&article.title, color),
            article.url,
            article.id,
        ),
        ScoreDocument::File { path, .. } => format!(
            "{}\n  {word_count} words · File",
            render_title(&path.display().to_string(), color)
        ),
        ScoreDocument::Stdin { .. } => format!(
            "{}\n  {word_count} words",
            render_title("Standard input", color)
        ),
    }
}

fn format_scored_document(
    document: &ScoreDocument,
    word_count: usize,
    score: &pangram::Score,
    color: bool,
) -> String {
    use std::fmt::Write as _;

    let mut output = format_document(document, word_count, color);
    write!(
        output,
        "\n  Pangram: {} / {} · {:.1}% AI · {:.1}% AI-assisted · {:.1}% human\n  {}",
        score.model,
        score.version,
        score.fractions.ai * 100.0,
        score.fractions.ai_assisted * 100.0,
        score.fractions.human * 100.0,
        score.prediction.headline,
    )
    .expect("writing to a String cannot fail");
    output
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
    use super::{ScoreDocument, enforce_word_limit};

    #[test]
    fn scoring_above_the_word_ceiling_requires_an_explicit_override() {
        let document = ScoreDocument::Stdin {
            text: "word ".repeat(2_001),
        };
        let word_count = document.word_count();

        let error = enforce_word_limit(&document, word_count, 2_000).unwrap_err();
        assert!(error.to_string().contains("--max-words 2001"));
        enforce_word_limit(&document, word_count, 2_001).unwrap();
    }
}
