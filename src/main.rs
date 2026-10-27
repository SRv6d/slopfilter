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

pub mod scoring;
pub mod source;
pub mod store;

const DEFAULT_MAX_SCORE_WORDS: usize = 2_000;

/// Inspect and score text for likely AI authorship.
#[derive(Debug, Parser)]
#[command(about, version)]
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

    /// Override the Pangram API endpoint.
    #[arg(
        long,
        env = "SLOPFILTER_PANGRAM_API_BASE_URL",
        hide = true,
        value_name = "URL"
    )]
    pangram_api_base_url: Option<Url>,
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

async fn score_matter(
    matter_api_token: String,
    list_options: matter::ListOptions,
    global: GlobalArgs,
) -> Result<()> {
    let color = color_enabled();
    let client = matter::Client::new(matter_api_token);
    let listed = client.list_items(list_options);
    pin_mut!(listed);
    let mut candidates = Vec::new();

    while let Some(item) = listed.next().await {
        let item = item.context("failed to list Matter items")?;
        let scoring_item =
            scoring::ScoringItem::new("Matter", item.id.clone(), Some(item.title.clone()));
        candidates.push((item, scoring_item));
    }

    for (_, item) in &mut candidates {
        item.pending(color);
    }

    let mut estimates = Vec::new();
    for (index, (matter_item, scoring_item)) in candidates.iter_mut().enumerate() {
        match matter_item.estimated_word_count() {
            Ok(words) => estimates.push((index, words)),
            Err(matter::ItemError::Unavailable { reason }) => {
                scoring_item.unavailable(reason, color);
            }
            Err(error) => scoring_item.failed(error, color),
        }
    }
    let initial = scoring::admit_in_order(estimates, global.max_words);
    for (index, exhausted) in initial.skipped {
        candidates[index].1.budget_skipped(exhausted, color);
    }
    for &index in &initial.admitted {
        candidates[index].1.fetching(color);
    }

    let selected_items = initial
        .admitted
        .iter()
        .map(|&index| &candidates[index].0)
        .collect::<Vec<_>>();
    let fetched = client.fetch_documents(&selected_items).await;
    let mut actual_counts = Vec::new();
    for (index, document) in initial.admitted.into_iter().zip(fetched) {
        let item = &mut candidates[index].1;
        match document {
            Ok(document) => match item.set_document(document) {
                Ok(words) => actual_counts.push((index, words)),
                Err(error) => item.failed(error, color),
            },
            Err(matter::ItemError::Unavailable { reason }) => item.unavailable(reason, color),
            Err(error) => item.failed(error, color),
        }
    }

    let actual = scoring::admit_in_order(actual_counts, global.max_words);
    for (index, exhausted) in actual.skipped {
        candidates[index].1.budget_skipped(exhausted, color);
    }
    for index in actual.admitted {
        candidates[index].1.mark_awaiting_submission(color);
    }

    let mut items = candidates
        .into_iter()
        .map(|(_, scoring_item)| scoring_item)
        .collect::<Vec<_>>();
    scoring::execute_source_items(
        &mut items,
        global.dry_run,
        global.pangram_api_key,
        global.pangram_api_base_url,
        color,
    )
    .await
}

async fn score_feed(feed_url: Url, global: GlobalArgs) -> Result<()> {
    let color = color_enabled();
    let (mut items, documents): (Vec<_>, Vec<_>) = feed::Client::new()
        .fetch_documents(feed_url)
        .await?
        .into_iter()
        .map(|(item, document)| {
            (
                scoring::ScoringItem::new("Feed", item.id, item.title),
                document,
            )
        })
        .unzip();

    for item in &mut items {
        item.pending(color);
    }

    let mut counts = Vec::new();
    for (index, (item, document)) in items.iter_mut().zip(documents).enumerate() {
        match document {
            Ok(document) => match item.set_document(document) {
                Ok(words) => counts.push((index, words)),
                Err(error) => item.failed(error, color),
            },
            Err(error @ feed::ItemError::Unavailable) => item.unavailable(error, color),
            Err(error) => item.failed(error, color),
        }
    }

    let admission = scoring::admit_in_order(counts, global.max_words);
    for (index, exhausted) in admission.skipped {
        items[index].budget_skipped(exhausted, color);
    }
    for index in admission.admitted {
        items[index].mark_awaiting_submission(color);
    }

    scoring::execute_source_items(
        &mut items,
        global.dry_run,
        global.pangram_api_key,
        global.pangram_api_base_url,
        color,
    )
    .await
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
    let mut budget = scoring::WordBudget::new(global.max_words);
    let mut scorer = scoring::Scorer::new(
        global.dry_run,
        global.pangram_api_key,
        global.pangram_api_base_url,
    )?;

    for document in documents {
        if !score_prepared_document(&mut scorer, &mut budget, document, color).await? {
            break;
        }
    }

    Ok(())
}

async fn score_prepared_document(
    scorer: &mut scoring::Scorer,
    budget: &mut scoring::WordBudget,
    document: &PreparedDocument,
    color: bool,
) -> Result<bool> {
    if let Err(scoring::BudgetExhausted {
        document_words,
        remaining_words,
    }) = budget.admit(document.word_count)
    {
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
        return Ok(false);
    }

    let outcome = scorer
        .score(&document.document.document().text)
        .await
        .with_context(|| format!("failed to score {}", document.document))?;
    match outcome {
        scoring::RealtimeOutcome::DryRun => {
            println!(
                "{}",
                format_document(&document.document, document.word_count, color)
            );
            println!("  Pangram: dry run; would submit");
        }
        scoring::RealtimeOutcome::Pangram(scoring::pangram::ScoreOutcome::Complete(score)) => {
            println!(
                "{}",
                format_scored_document(&document.document, document.word_count, &score, color)
            );
        }
        scoring::RealtimeOutcome::Pangram(scoring::pangram::ScoreOutcome::Pending {
            task,
            stage,
        }) => {
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
    Ok(true)
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
    score: &scoring::pangram::Score,
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
    use clap::{CommandFactory, Parser};

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
            unreachable!("expected Matter input");
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
            unreachable!("expected Matter input");
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

    #[test]
    fn pangram_api_base_url_is_hidden_and_uses_url_parsing() {
        let help = Cli::command().render_long_help().to_string();
        assert!(!help.contains("pangram-api-base-url"));
        assert!(!help.contains("SLOPFILTER_PANGRAM_API_BASE_URL"));

        assert!(
            Cli::try_parse_from([
                "slopfilter",
                "file",
                "article.md",
                "--dry-run",
                "--pangram-api-base-url",
                "http://127.0.0.1:1234/api",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "slopfilter",
                "file",
                "article.md",
                "--dry-run",
                "--pangram-api-base-url",
                "not a URL",
            ])
            .is_err()
        );
    }
}
