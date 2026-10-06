use std::{
    env, fs,
    io::{self, IsTerminal, Read},
    path::PathBuf,
};

use anstyle::Style;
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueHint};
use futures_util::{StreamExt, pin_mut, stream};

pub mod article;
pub mod matter;
pub mod pangram;
pub mod store;

const MAX_CONCURRENT_FETCHES: usize = 4;
const DEFAULT_MAX_SCORE_WORDS: usize = 2_000;

/// Inspect and score text for likely AI authorship.
#[derive(Debug, Parser)]
#[command(about)]
struct Cli {
    #[command(subcommand)]
    input: ScoreInput,
}

#[derive(Debug, Args)]
struct GlobalArgs {
    /// The maximum number of words authorized per submission.
    #[arg(
        long,
        default_value_t = DEFAULT_MAX_SCORE_WORDS,
        value_parser = parse_max_words,
        value_hint = ValueHint::Other
    )]
    max_words: usize,

    /// Inspect inputs without submitting to Pangram.
    #[arg(long)]
    dry_run: bool,

    /// The Pangram API key.
    #[arg(
        long,
        env = "PANGRAM_API_KEY",
        hide_env_values = true,
        value_name = "KEY",
        value_hint = ValueHint::Other,
        required_unless_present = "dry_run"
    )]
    pangram_api_key: Option<String>,
}

#[derive(Debug, Subcommand)]
enum ScoreInput {
    /// Score queued Matter articles.
    Matter {
        /// The maximum number of items to score.
        #[arg(long, value_hint = ValueHint::Other)]
        limit: Option<usize>,

        /// The Matter API token.
        #[arg(
            long,
            env = "MATTER_API_TOKEN",
            hide_env_values = true,
            value_name = "TOKEN",
            value_hint = ValueHint::Other
        )]
        matter_api_token: String,

        #[command(flatten)]
        global: GlobalArgs,
    },

    /// Score a UTF-8 text or Markdown file.
    File {
        /// The UTF-8 text or Markdown file to score.
        #[arg(value_name = "PATH", value_hint = ValueHint::FilePath)]
        path: PathBuf,

        #[command(flatten)]
        global: GlobalArgs,
    },

    /// Score UTF-8 text piped on standard input.
    Stdin {
        #[command(flatten)]
        global: GlobalArgs,
    },
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
    let Cli { input } = Cli::parse();

    match input {
        ScoreInput::Matter {
            limit,
            matter_api_token,
            global,
        } => score_matter(matter_api_token, limit.unwrap_or(1), global).await,
        ScoreInput::File { path, global } => score_file(path, global).await,
        ScoreInput::Stdin { global } => score_stdin(global).await,
    }
}

async fn score_matter(matter_api_token: String, limit: usize, global: GlobalArgs) -> Result<()> {
    let documents = load_matter_documents(matter_api_token, limit).await?;
    let total_words = documents
        .iter()
        .map(|document| document.word_count)
        .sum::<usize>();
    let available = documents.len();

    score_documents(&documents, global).await?;

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
    limit: usize,
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

async fn score_file(path: PathBuf, global: GlobalArgs) -> Result<()> {
    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read UTF-8 text from {}", path.display()))?;
    let document = PreparedDocument::new(ScoreDocument::File { path, text })?;

    score_documents(std::slice::from_ref(&document), global).await
}

async fn score_stdin(global: GlobalArgs) -> Result<()> {
    if io::stdin().is_terminal() {
        bail!("no piped input; pass text on standard input");
    }

    let mut text = String::new();
    io::stdin()
        .read_to_string(&mut text)
        .context("failed to read UTF-8 text from standard input")?;
    let document = PreparedDocument::new(ScoreDocument::Stdin { text })?;

    score_documents(std::slice::from_ref(&document), global).await
}

async fn score_documents(documents: &[PreparedDocument], global: GlobalArgs) -> Result<()> {
    let color = color_enabled();
    if global.dry_run {
        for document in documents {
            println!(
                "{}",
                format_document(&document.document, document.word_count, color)
            );
            if document.word_count > global.max_words {
                println!(
                    "  Pangram: dry run; would refuse above the {}-word maximum",
                    global.max_words
                );
            } else {
                println!("  Pangram: dry run; would submit");
            }
        }
        return Ok(());
    }

    for document in documents {
        enforce_word_limit(&document.document, document.word_count, global.max_words)?;
    }

    if documents.is_empty() {
        return Ok(());
    }

    let pangram_api_key = global
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
    use super::{Cli, ScoreDocument, ScoreInput, enforce_word_limit};
    use clap::Parser;

    #[test]
    fn matter_options_follow_the_source() {
        let cli = Cli::try_parse_from([
            "slopfilter",
            "matter",
            "--limit",
            "101",
            "--dry-run",
            "--matter-api-token",
            "token",
        ])
        .unwrap();
        let ScoreInput::Matter { limit, global, .. } = cli.input else {
            panic!("expected Matter input");
        };

        assert_eq!(limit, Some(101));
        assert!(global.dry_run);
    }

    #[test]
    fn pangram_credentials_are_optional_only_for_dry_runs() {
        assert!(Cli::try_parse_from(["slopfilter", "file", "article.md", "--dry-run"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "slopfilter",
                "file",
                "article.md",
                "--pangram-api-key",
                "key",
            ])
            .is_ok()
        );
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
