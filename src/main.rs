use std::{
    env, fs,
    io::{self, IsTerminal, Read},
    path::PathBuf,
};

use anstyle::Style;
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueHint};
use futures_util::{StreamExt, pin_mut};
use source::{Document, matter};

pub mod pangram;
pub mod scoring;
pub mod source;
pub mod store;

const MAX_BUFFERED_MATTER_ITEMS: usize = 4;
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
    /// The maximum total number of words authorized for this invocation.
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
    /// Score Matter articles.
    Matter {
        #[command(flatten)]
        list_options: matter::ListOptions,

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

enum InputDocument {
    Matter { item_id: String, document: Document },
    File { path: PathBuf, document: Document },
    Stdin { document: Document },
}

impl InputDocument {
    fn document(&self) -> &Document {
        match self {
            Self::Matter { document, .. }
            | Self::File { document, .. }
            | Self::Stdin { document } => document,
        }
    }
}

impl std::fmt::Display for InputDocument {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Matter { item_id, .. } => write!(formatter, "Matter item {item_id}"),
            Self::File { path, .. } => write!(formatter, "file {}", path.display()),
            Self::Stdin { .. } => formatter.write_str("standard input"),
        }
    }
}

struct PreparedDocument {
    document: InputDocument,
    word_count: usize,
}

impl PreparedDocument {
    fn new(document: InputDocument) -> Result<Self> {
        if document.document().text.trim().is_empty() {
            bail!("{document} contains no text");
        }

        let word_count = document.document().word_count();
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
            list_options,
            matter_api_token,
            global,
        } => score_matter(matter_api_token, list_options, global).await,
        ScoreInput::File { path, global } => score_file(path, global).await,
        ScoreInput::Stdin { global } => score_stdin(global).await,
    }
}

async fn score_matter(
    matter_api_token: String,
    list_options: matter::ListOptions,
    global: GlobalArgs,
) -> Result<()> {
    let color = color_enabled();
    let mut scorer =
        scoring::Scorer::new(global.max_words, global.dry_run, global.pangram_api_key)?;
    let client = matter::Client::new(matter_api_token);
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<
        std::result::Result<
            (matter::Item, std::result::Result<(), matter::ItemError>),
            matter::Error,
        >,
    >(MAX_BUFFERED_MATTER_ITEMS);
    let producer = tokio::spawn(async move {
        let items = client.list_items_with_markdown(list_options);
        pin_mut!(items);

        while let Some(item) = items.next().await {
            if sender.send(item).await.is_err() {
                break;
            }
        }
    });

    let mut total_words = 0usize;
    let mut available = 0usize;
    let mut failures = 0usize;
    let mut list_error = None;

    let processing = async {
        while let Some(listed) = receiver.recv().await {
            let (item, outcome) = match listed {
                Ok(listed) => listed,
                Err(error) => {
                    list_error = Some(error);
                    break;
                }
            };

            match outcome {
                Ok(()) => {
                    let (item_id, document) = match item.into_document() {
                        Ok(document) => document,
                        Err(error) => {
                            failures += 1;
                            eprintln!("Failed: {error}");
                            continue;
                        }
                    };
                    let document =
                        PreparedDocument::new(InputDocument::Matter { item_id, document })?;

                    if !score_prepared_document(&mut scorer, &document, color).await? {
                        break;
                    }

                    total_words = total_words
                        .checked_add(document.word_count)
                        .context("total word count overflowed")?;
                    available += 1;
                }
                Err(matter::ItemError::Unavailable { reason }) => {
                    eprintln!("{}", format_unavailable(&item, &reason, color));
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

        Ok::<(), anyhow::Error>(())
    }
    .await;

    drop(receiver);
    if !producer.is_finished() {
        producer.abort();
    }
    match producer.await {
        Ok(()) => {}
        Err(error) if error.is_cancelled() => {}
        Err(error) => return Err(error).context("Matter producer task failed"),
    }
    processing?;

    if let Some(error) = list_error {
        return Err(error).context("failed to list Matter items");
    }
    if failures > 0 {
        bail!("failed to fetch {failures} Matter item(s)");
    }

    let noun = if available == 1 {
        "article"
    } else {
        "articles"
    };
    println!("Total: {total_words} words across {available} eligible {noun}.");
    Ok(())
}

async fn score_file(path: PathBuf, global: GlobalArgs) -> Result<()> {
    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read UTF-8 text from {}", path.display()))?;
    let document = PreparedDocument::new(InputDocument::File {
        path,
        document: Document {
            title: None,
            url: None,
            text,
        },
    })?;

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
    let document = PreparedDocument::new(InputDocument::Stdin {
        document: Document {
            title: None,
            url: None,
            text,
        },
    })?;

    score_documents(std::slice::from_ref(&document), global).await
}

async fn score_documents(documents: &[PreparedDocument], global: GlobalArgs) -> Result<()> {
    let color = color_enabled();
    let mut scorer =
        scoring::Scorer::new(global.max_words, global.dry_run, global.pangram_api_key)?;

    for document in documents {
        if !score_prepared_document(&mut scorer, document, color).await? {
            break;
        }
    }

    Ok(())
}

async fn score_prepared_document(
    scorer: &mut scoring::Scorer,
    document: &PreparedDocument,
    color: bool,
) -> Result<bool> {
    let outcome = scorer
        .score(&document.document.document().text, document.word_count)
        .await
        .with_context(|| format!("failed to score {}", document.document))?;

    match outcome {
        Err(scoring::BudgetExhausted {
            document_words,
            remaining_words,
        }) => {
            println!(
                "{}",
                format_document(&document.document, document.word_count, color)
            );
            let word_noun = if remaining_words == 1 {
                "word"
            } else {
                "words"
            };
            println!(
                "  Pangram: word budget exhausted; {remaining_words} {word_noun} remaining, \
                 but this document has {document_words}"
            );
            Ok(false)
        }
        Ok(None) => {
            println!(
                "{}",
                format_document(&document.document, document.word_count, color)
            );
            println!("  Pangram: dry run; would submit");
            Ok(true)
        }
        Ok(Some(pangram::ScoreOutcome::Complete(score))) => {
            println!(
                "{}",
                format_scored_document(&document.document, document.word_count, &score, color)
            );
            Ok(true)
        }
        Ok(Some(pangram::ScoreOutcome::Pending { task, stage })) => {
            println!(
                "{}",
                format_document(&document.document, document.word_count, color)
            );
            println!(
                "  Pangram task {} ({}) remains {stage} and can be resumed.",
                task.task_id, task.model
            );
            Ok(true)
        }
    }
}

fn color_enabled() -> bool {
    io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none()
}

fn format_matter_document(
    item_id: &str,
    document: &Document,
    word_count: usize,
    color: bool,
) -> String {
    let title = document
        .title
        .as_deref()
        .filter(|title| !title.is_empty())
        .unwrap_or("Untitled");
    let mut output = render_title(title, color);
    if let Some(url) = &document.url {
        use std::fmt::Write as _;
        write!(output, "\n  {url}").expect("writing to a String cannot fail");
    }
    use std::fmt::Write as _;
    write!(output, "\n  {word_count} words · Matter: {item_id}")
        .expect("writing to a String cannot fail");
    output
}

fn format_document(document: &InputDocument, word_count: usize, color: bool) -> String {
    match document {
        InputDocument::Matter { item_id, document } => {
            format_matter_document(item_id, document, word_count, color)
        }
        InputDocument::File { path, .. } => format!(
            "{}\n  {word_count} words · File",
            render_title(&path.display().to_string(), color)
        ),
        InputDocument::Stdin { .. } => format!(
            "{}\n  {word_count} words",
            render_title("Standard input", color)
        ),
    }
}

fn format_scored_document(
    document: &InputDocument,
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

fn format_unavailable(item: &matter::Item, reason: &str, color: bool) -> String {
    format!(
        "Unavailable: {}\n  {} · Matter: {}",
        render_title(&item.title, color),
        reason,
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
    use super::{Cli, ScoreInput, matter};
    use clap::Parser;

    #[test]
    fn matter_options_follow_the_source() {
        let cli = Cli::try_parse_from([
            "slopfilter",
            "matter",
            "--status",
            "inbox",
            "--status",
            "queue",
            "--tag",
            "tag_one",
            "--tag",
            "tag_two",
            "--limit",
            "101",
            "--dry-run",
            "--matter-api-token",
            "token",
        ])
        .unwrap();
        let ScoreInput::Matter {
            list_options,
            global,
            ..
        } = cli.input
        else {
            panic!("expected Matter input");
        };

        assert_eq!(list_options.limit, Some(101));
        assert_eq!(
            list_options.status,
            [matter::Status::Inbox, matter::Status::Queue]
        );
        assert_eq!(list_options.tag, ["tag_one", "tag_two"]);
        assert!(global.dry_run);
    }

    #[test]
    fn matter_status_defaults_to_queue() {
        let cli = Cli::try_parse_from([
            "slopfilter",
            "matter",
            "--dry-run",
            "--matter-api-token",
            "token",
        ])
        .unwrap();
        let ScoreInput::Matter { list_options, .. } = cli.input else {
            panic!("expected Matter input");
        };

        assert_eq!(list_options.status, [matter::Status::Queue]);
        assert_eq!(list_options.limit, None);
    }

    #[test]
    fn matter_status_does_not_accept_all() {
        assert!(
            Cli::try_parse_from([
                "slopfilter",
                "matter",
                "--status",
                "all",
                "--dry-run",
                "--matter-api-token",
                "token",
            ])
            .is_err()
        );
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
}
