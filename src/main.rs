use std::{
    env,
    fmt::{self, Write as _},
    fs,
    io::{self, IsTerminal, Read},
    path::PathBuf,
};

use anstyle::Style;
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueHint};
use futures_util::{StreamExt, pin_mut};
use source::{Document, feed, matter};
use url::Url;

pub mod pangram;
pub mod scoring;
pub mod source;
pub mod store;

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

    /// Score entries of an Atom or RSS feed.
    Feed {
        /// The Atom or RSS feed URL.
        #[arg(value_name = "URL", value_hint = ValueHint::Url)]
        url: Url,

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
    /// An item listed by a source such as Matter or a feed.
    Item {
        source: &'static str,
        item_id: String,
        document: Document,
    },
    File {
        path: PathBuf,
        document: Document,
    },
    Stdin {
        document: Document,
    },
}

impl InputDocument {
    fn document(&self) -> &Document {
        match self {
            Self::Item { document, .. }
            | Self::File { document, .. }
            | Self::Stdin { document } => document,
        }
    }
}

impl fmt::Display for InputDocument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Item {
                source, item_id, ..
            } => write!(formatter, "{source} item {item_id}"),
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
        ScoreInput::Feed { url, global } => score_feed(url, global).await,
        ScoreInput::File { path, global } => score_file(path, global).await,
        ScoreInput::Stdin { global } => score_stdin(global).await,
    }
}

/// Scores a source's items in order under one word budget and reports
/// per-item outcomes the same way for every source.
struct ItemScoring {
    source: &'static str,
    scorer: scoring::Scorer,
    color: bool,
    total_words: usize,
    scored: usize,
    failures: usize,
}

impl ItemScoring {
    fn new(source: &'static str, global: GlobalArgs) -> Result<Self> {
        Ok(Self {
            source,
            scorer: scoring::Scorer::new(global.max_words, global.dry_run, global.pangram_api_key)?,
            color: color_enabled(),
            total_words: 0,
            scored: 0,
            failures: 0,
        })
    }

    /// Returns `false` once the word budget is exhausted.
    async fn score(&mut self, item_id: String, document: Document) -> Result<bool> {
        let document = PreparedDocument::new(InputDocument::Item {
            source: self.source,
            item_id,
            document,
        })?;

        if !score_prepared_document(&mut self.scorer, &document, self.color).await? {
            return Ok(false);
        }

        self.total_words = self
            .total_words
            .checked_add(document.word_count)
            .context("total word count overflowed")?;
        self.scored += 1;
        Ok(true)
    }

    fn unavailable(&self, item_id: &str, title: Option<&str>, reason: impl fmt::Display) {
        eprintln!(
            "{}",
            format_item_issue(
                "Unavailable",
                item_id,
                title,
                reason,
                self.source,
                self.color
            )
        );
    }

    fn failed(&mut self, item_id: &str, title: Option<&str>, error: impl fmt::Display) {
        self.failures += 1;
        eprintln!(
            "{}",
            format_item_issue("Failed", item_id, title, error, self.source, self.color)
        );
    }

    fn finish(self) -> Result<()> {
        if self.failures > 0 {
            bail!(
                "failed to process {} {} item(s)",
                self.failures,
                self.source
            );
        }

        let noun = if self.scored == 1 {
            "article"
        } else {
            "articles"
        };
        println!(
            "Total: {} words across {} eligible {noun}.",
            self.total_words, self.scored
        );
        Ok(())
    }
}

async fn score_matter(
    matter_api_token: String,
    list_options: matter::ListOptions,
    global: GlobalArgs,
) -> Result<()> {
    let mut scoring = ItemScoring::new("Matter", global)?;
    let client = matter::Client::new(matter_api_token);
    let documents = client.list_documents(list_options);
    pin_mut!(documents);

    while let Some(listed) = documents.next().await {
        let (item, document) = listed.context("failed to list Matter items")?;

        match document {
            Ok(document) => {
                if !scoring.score(item.id, document).await? {
                    break;
                }
            }
            Err(matter::ItemError::Unavailable { reason }) => {
                scoring.unavailable(&item.id, Some(&item.title), reason);
            }
            Err(error) => scoring.failed(&item.id, Some(&item.title), error),
        }
    }

    scoring.finish()
}

async fn score_feed(feed_url: Url, global: GlobalArgs) -> Result<()> {
    let mut scoring = ItemScoring::new("Feed", global)?;
    let documents = feed::Client::new().fetch_documents(feed_url).await?;

    for (item, document) in documents {
        match document {
            Ok(document) => {
                if !scoring.score(item.id, document).await? {
                    break;
                }
            }
            Err(error @ feed::ItemError::Unavailable) => {
                scoring.unavailable(&item.id, item.title.as_deref(), error);
            }
            Err(error) => scoring.failed(&item.id, item.title.as_deref(), error),
        }
    }

    scoring.finish()
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

fn format_item_document(
    source: &str,
    item_id: &str,
    document: &Document,
    word_count: usize,
    color: bool,
) -> String {
    let mut output = render_title(display_title(document.title.as_deref()), color);
    if let Some(url) = &document.url {
        write!(output, "\n  {url}").expect("writing to a String cannot fail");
    }
    write!(output, "\n  {word_count} words · {source}: {item_id}")
        .expect("writing to a String cannot fail");
    output
}

fn format_document(document: &InputDocument, word_count: usize, color: bool) -> String {
    match document {
        InputDocument::Item {
            source,
            item_id,
            document,
        } => format_item_document(source, item_id, document, word_count, color),
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

fn format_item_issue(
    kind: &str,
    item_id: &str,
    title: Option<&str>,
    detail: impl fmt::Display,
    source: &str,
    color: bool,
) -> String {
    format!(
        "{kind}: {}\n  {detail} · {source}: {item_id}",
        render_title(display_title(title), color),
    )
}

fn display_title(title: Option<&str>) -> &str {
    title
        .filter(|title| !title.is_empty())
        .unwrap_or("Untitled")
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
