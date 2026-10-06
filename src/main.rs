//! Command-line entrypoint for inspecting and scoring articles.
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
pub mod matter;
pub mod pangram;
pub mod store;

const MAX_CONCURRENT_FETCHES: usize = 4;
const DEFAULT_MAX_SCORE_WORDS: usize = 2_000;

#[derive(Debug, Parser)]
#[command(about = "Inspect and score text for likely AI authorship")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect or submit inputs to Pangram for billable analysis.
    Score {
        #[arg(
            long,
            global = true,
            value_name = "COUNT",
            help = "Maximum source items to process (1-20)",
            value_parser = parse_limit
        )]
        limit: Option<u8>,

        #[command(flatten)]
        score: ScoreOptions,

        #[command(subcommand)]
        input: ScoreInput,
    },
}

#[derive(Debug, Args)]
struct ScoreOptions {
    #[arg(
        long,
        global = true,
        default_value_t = DEFAULT_MAX_SCORE_WORDS,
        help = "Maximum words authorized per submission",
        value_parser = parse_max_words
    )]
    max_words: usize,

    #[arg(
        long,
        global = true,
        help = "Inspect inputs without contacting Pangram"
    )]
    dry_run: bool,

    #[arg(
        long,
        global = true,
        env = "PANGRAM_API_KEY",
        hide_env_values = true,
        value_name = "KEY"
    )]
    pangram_api_key: Option<String>,
}

impl ScoreOptions {
    fn validate(&self) -> Result<()> {
        if !self.dry_run && self.pangram_api_key.is_none() {
            bail!("Pangram API key is required unless --dry-run is set");
        }

        Ok(())
    }
}

#[derive(Debug, Subcommand)]
enum ScoreInput {
    /// Inspect or score queued Matter articles.
    Matter {
        #[arg(
            long,
            env = "MATTER_API_TOKEN",
            hide_env_values = true,
            value_name = "TOKEN"
        )]
        matter_api_token: String,
    },

    /// Inspect or score a UTF-8 text or Markdown file.
    File {
        #[arg(value_name = "PATH", help = "UTF-8 text or Markdown file")]
        path: PathBuf,
    },

    /// Inspect or score UTF-8 text piped on standard input.
    Stdin,
}

enum ScoreDocument {
    Matter(article::Article),
    File { path: PathBuf, text: String },
    Stdin { text: String },
}

impl ScoreDocument {
    fn text(&self) -> &str {
        match self {
            Self::Matter(article) => &article.markdown,
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
            Self::File { path, .. } => write!(formatter, "file {}", path.display()),
            Self::Stdin { .. } => formatter.write_str("standard input"),
        }
    }
}

struct PreparedDocument {
    document: ScoreDocument,
    word_count: usize,
}

impl PreparedDocument {
    fn new(document: ScoreDocument) -> Result<Self> {
        if document.text().trim().is_empty() {
            bail!("{document} contains no text");
        }

        let word_count = document.word_count();
        Ok(Self {
            document,
            word_count,
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let Command::Score {
        limit,
        score,
        input,
    } = cli.command;
    score.validate()?;

    match input {
        ScoreInput::Matter { matter_api_token } => {
            score_matter(matter_api_token, limit.unwrap_or(1), score).await
        }
        ScoreInput::File { path } => {
            reject_limit(limit)?;
            score_file(path, score).await
        }
        ScoreInput::Stdin => {
            reject_limit(limit)?;
            score_stdin(score).await
        }
    }
}

fn reject_limit(limit: Option<u8>) -> Result<()> {
    if limit.is_some() {
        bail!("--limit is only valid for article sources");
    }

    Ok(())
}

async fn score_matter(matter_api_token: String, limit: u8, score: ScoreOptions) -> Result<()> {
    let documents = load_matter_documents(matter_api_token, limit).await?;
    let total_words = documents
        .iter()
        .map(|document| document.word_count)
        .sum::<usize>();
    let available = documents.len();

    score_documents(&documents, score).await?;

    let noun = if available == 1 {
        "article"
    } else {
        "articles"
    };
    println!("Total: {total_words} words across {available} eligible {noun}.");
    Ok(())
}

async fn load_matter_documents(
    matter_api_token: String,
    limit: u8,
) -> Result<Vec<PreparedDocument>> {
    let client = matter::Client::new(matter_api_token);
    let articles = client
        .queued_articles(limit)
        .await
        .context("failed to list queued Matter articles")?;
    let mut documents = Vec::with_capacity(articles.len());
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

    let mut failures = 0;

    while let Some((item, outcome)) = fetches.next().await {
        match outcome {
            Ok(matter::ItemOutcome::Available(article)) => {
                documents.push(PreparedDocument::new(ScoreDocument::Matter(article))?);
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

    if failures > 0 {
        bail!("failed to fetch {failures} queued Matter article(s)");
    }

    Ok(documents)
}

async fn score_file(path: PathBuf, score: ScoreOptions) -> Result<()> {
    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read UTF-8 text from {}", path.display()))?;
    let document = PreparedDocument::new(ScoreDocument::File { path, text })?;

    score_documents(std::slice::from_ref(&document), score).await
}

async fn score_stdin(score: ScoreOptions) -> Result<()> {
    if io::stdin().is_terminal() {
        bail!("no piped input; pass text on standard input");
    }

    let mut text = String::new();
    io::stdin()
        .read_to_string(&mut text)
        .context("failed to read UTF-8 text from standard input")?;
    let document = PreparedDocument::new(ScoreDocument::Stdin { text })?;

    score_documents(std::slice::from_ref(&document), score).await
}

async fn score_documents(documents: &[PreparedDocument], score: ScoreOptions) -> Result<()> {
    let color = color_enabled();
    if score.dry_run {
        for document in documents {
            println!(
                "{}",
                format_document(&document.document, document.word_count, color)
            );
            if document.word_count > score.max_words {
                println!(
                    "  Pangram: dry run; would refuse above the {}-word maximum",
                    score.max_words
                );
            } else {
                println!("  Pangram: dry run; would submit");
            }
        }
        return Ok(());
    }

    for document in documents {
        enforce_word_limit(&document.document, document.word_count, score.max_words)?;
    }

    if documents.is_empty() {
        return Ok(());
    }

    let pangram_api_key = score
        .pangram_api_key
        .context("Pangram API key is required unless --dry-run is set")?;
    let pangram_client = pangram::Client::new(pangram_api_key);
    let model = pangram_client
        .discover_model()
        .await
        .context("failed to discover an available Pangram model")?;

    for document in documents {
        let outcome = pangram_client
            .score(document.document.text(), &model)
            .await
            .with_context(|| format!("failed to score {}", document.document))?;

        match outcome {
            pangram::ScoreOutcome::Complete(score) => {
                println!(
                    "{}",
                    format_scored_document(&document.document, document.word_count, &score, color)
                );
            }
            pangram::ScoreOutcome::Pending { task, stage } => {
                println!(
                    "{}",
                    format_document(&document.document, document.word_count, color)
                );
                println!(
                    "  Pangram task {} ({}) remains {stage} and can be resumed.",
                    task.task_id, task.model
                );
            }
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
    use super::{
        Cli, Command, ScoreDocument, ScoreInput, ScoreOptions, enforce_word_limit, reject_limit,
    };
    use clap::Parser;

    #[test]
    fn dry_run_limit_can_precede_or_follow_the_source() {
        for arguments in [
            [
                "slopfilter",
                "score",
                "--limit",
                "7",
                "--dry-run",
                "matter",
                "--matter-api-token",
                "token",
            ],
            [
                "slopfilter",
                "score",
                "matter",
                "--limit",
                "7",
                "--dry-run",
                "--matter-api-token",
                "token",
            ],
        ] {
            let cli = Cli::try_parse_from(arguments).unwrap();
            let Command::Score {
                limit,
                score,
                input: ScoreInput::Matter { .. },
            } = cli.command
            else {
                panic!("expected the Matter score command");
            };

            assert_eq!(limit, Some(7));
            assert!(score.dry_run);
        }
    }

    #[test]
    fn pangram_credentials_are_optional_only_for_dry_runs() {
        let mut options = ScoreOptions {
            max_words: 2_000,
            dry_run: true,
            pangram_api_key: None,
        };
        options.validate().unwrap();

        options.dry_run = false;
        assert!(options.validate().is_err());
    }

    #[test]
    fn limit_is_rejected_for_non_source_inputs() {
        reject_limit(None).unwrap();
        assert!(reject_limit(Some(1)).is_err());
    }

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
