use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use anyhow::{Context, Result, anyhow, bail};
use url::Url;

use crate::{
    InputDocument, PreparedDocument, display_title, format_document, format_scored_document,
    render_title,
    scoring::{self, pangram},
    source::Document,
};

const PANGRAM_UNIT_WORDS: usize = 100;
const MAX_PANGRAM_BULK_UNITS: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemState {
    Pending,
    Fetching,
    AwaitingSubmission,
    Unavailable,
    Failed,
    BudgetSkipped,
    Submitted,
    Completed,
    DryRun,
    TimedOut,
}

impl fmt::Display for ItemState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Pending => "Pending",
            Self::Fetching => "Fetching",
            Self::AwaitingSubmission => "Awaiting submission",
            Self::Unavailable => "Unavailable",
            Self::Failed => "Failed",
            Self::BudgetSkipped => "Skipped by word budget",
            Self::Submitted => "Submitted",
            Self::Completed => "Completed",
            Self::DryRun => "Dry run",
            Self::TimedOut => "Pangram timed out",
        })
    }
}

pub(crate) struct ScoringItem {
    source: &'static str,
    id: String,
    title: Option<String>,
    state: ItemState,
    document: Option<PreparedDocument>,
    word_count: Option<usize>,
    units: Option<usize>,
}

impl ScoringItem {
    pub(crate) fn new(source: &'static str, id: String, title: Option<String>) -> Self {
        Self {
            source,
            id,
            title,
            state: ItemState::Pending,
            document: None,
            word_count: None,
            units: None,
        }
    }

    fn transition(&mut self, state: ItemState, detail: Option<&dyn fmt::Display>, color: bool) {
        self.state = state;
        eprintln!(
            "{state}: {} · {}: {}",
            render_title(display_title(self.title.as_deref()), color),
            self.source,
            self.id
        );
        if let Some(detail) = detail {
            eprintln!("  {detail}");
        }
    }

    pub(crate) fn pending(&mut self, color: bool) {
        self.transition(ItemState::Pending, None, color);
    }

    pub(crate) fn fetching(&mut self, color: bool) {
        self.transition(ItemState::Fetching, None, color);
    }

    pub(crate) fn unavailable(&mut self, reason: impl fmt::Display, color: bool) {
        self.transition(ItemState::Unavailable, Some(&reason), color);
    }

    pub(crate) fn failed(&mut self, error: impl fmt::Display, color: bool) {
        self.transition(ItemState::Failed, Some(&error), color);
    }

    pub(crate) fn budget_skipped(&mut self, exhausted: scoring::BudgetExhausted, color: bool) {
        self.document = None;
        let word_noun = if exhausted.remaining_words == 1 {
            "word"
        } else {
            "words"
        };
        let detail = format!(
            "{} {word_noun} remaining, but this document requires {}",
            exhausted.remaining_words, exhausted.document_words
        );
        self.transition(ItemState::BudgetSkipped, Some(&detail), color);
    }

    pub(crate) fn set_document(&mut self, document: Document) -> Result<usize> {
        let prepared = PreparedDocument::new(InputDocument::Item {
            source: self.source,
            item_id: self.id.clone(),
            document,
        })?;
        let word_count = prepared.word_count;
        self.word_count = Some(word_count);
        self.document = Some(prepared);
        Ok(word_count)
    }

    pub(crate) fn mark_awaiting_submission(&mut self, color: bool) {
        let words = self
            .word_count
            .expect("an admitted source item has a word count");
        let units = billable_units(words);
        self.units = Some(units);
        let detail = format!("{words} words · {units} Pangram units");
        self.transition(ItemState::AwaitingSubmission, Some(&detail), color);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Admission {
    pub(crate) admitted: Vec<usize>,
    pub(crate) skipped: Vec<(usize, scoring::BudgetExhausted)>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct BulkJob {
    indices: Vec<usize>,
    units: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobDisposition {
    Complete,
    Stop,
}

pub(crate) fn admit_in_order(
    counts: impl IntoIterator<Item = (usize, usize)>,
    max_words: usize,
) -> Admission {
    let mut budget = scoring::WordBudget::new(max_words);
    let mut admission = Admission {
        admitted: Vec::new(),
        skipped: Vec::new(),
    };

    for (index, words) in counts {
        match budget.admit(words) {
            Ok(()) => admission.admitted.push(index),
            Err(exhausted) => admission.skipped.push((index, exhausted)),
        }
    }
    admission
}

fn billable_units(words: usize) -> usize {
    words.div_ceil(PANGRAM_UNIT_WORDS).max(1)
}

fn partition(items: &mut [ScoringItem], color: bool) -> Vec<BulkJob> {
    for item in items
        .iter_mut()
        .filter(|item| item.state == ItemState::AwaitingSubmission)
    {
        let units = item
            .units
            .expect("items awaiting submission have Pangram units");
        if units > MAX_PANGRAM_BULK_UNITS {
            let detail = format!(
                "requires {units} Pangram units; one bulk item cannot exceed {MAX_PANGRAM_BULK_UNITS}"
            );
            item.failed(detail, color);
        }
    }

    let mut jobs = Vec::new();
    let mut current = BulkJob::default();
    let mut current_ids = HashSet::<&str>::new();
    for (index, item) in items.iter().enumerate() {
        if item.state != ItemState::AwaitingSubmission {
            continue;
        }
        let units = item
            .units
            .expect("items awaiting submission have Pangram units");
        let duplicate_id = current_ids.contains(item.id.as_str());
        if !current.indices.is_empty()
            && (current.units + units > MAX_PANGRAM_BULK_UNITS || duplicate_id)
        {
            jobs.push(std::mem::take(&mut current));
            current_ids.clear();
        }
        current.units += units;
        current.indices.push(index);
        current_ids.insert(item.id.as_str());
    }
    if !current.indices.is_empty() {
        jobs.push(current);
    }
    jobs
}

pub(crate) async fn execute_source_items(
    items: &mut [ScoringItem],
    dry_run: bool,
    pangram_api_key: Option<String>,
    pangram_api_base_url: Option<Url>,
    color: bool,
) -> Result<()> {
    let jobs = partition(items, color);
    if dry_run {
        for (job_index, job) in jobs.iter().enumerate() {
            println!(
                "Bulk job {}: {} item(s) · {} Pangram units · dry run; would submit",
                job_index + 1,
                job.indices.len(),
                job.units
            );
            for &index in &job.indices {
                items[index].transition(ItemState::DryRun, None, color);
                let document = items[index]
                    .document
                    .as_ref()
                    .expect("dry-run jobs contain prepared documents");
                println!(
                    "{}",
                    format_document(&document.document, document.word_count, color)
                );
                println!(
                    "  Pangram: dry run; would submit in bulk job {}",
                    job_index + 1
                );
            }
        }
        return finish(items);
    }

    if !jobs.is_empty() {
        let api_key =
            pangram_api_key.context("Pangram API key is required unless --dry-run is set")?;
        let client = pangram::Client::new(api_key, pangram_api_base_url);
        let model = client
            .discover_model()
            .await
            .context("failed to discover an available Pangram model")?;

        execute_jobs(&client, &model, &jobs, items, color).await?;
    }

    finish(items)
}
async fn execute_jobs(
    client: &pangram::Client,
    model: &str,
    jobs: &[BulkJob],
    items: &mut [ScoringItem],
    color: bool,
) -> Result<()> {
    for (job_index, job) in jobs.iter().enumerate() {
        println!(
            "Bulk job {}: {} item(s) · {} Pangram units",
            job_index + 1,
            job.indices.len(),
            job.units
        );
        if execute_job(client, model, job, items, color).await? == JobDisposition::Stop {
            break;
        }
    }
    Ok(())
}

async fn execute_job(
    client: &pangram::Client,
    model: &str,
    job: &BulkJob,
    items: &mut [ScoringItem],
    color: bool,
) -> Result<JobDisposition> {
    let inputs = job
        .indices
        .iter()
        .map(|&index| {
            let document = items[index]
                .document
                .as_ref()
                .expect("bulk jobs contain prepared documents");
            pangram::BulkInput {
                id: &items[index].id,
                text: &document.document.document().text,
            }
        })
        .collect::<Vec<_>>();
    let submission = client.submit_bulk(&inputs, model).await;
    drop(inputs);

    let submission = match submission {
        Ok(submission) => submission,
        Err(error) => {
            for &index in &job.indices {
                items[index].failed(&error, color);
            }
            return Ok(JobDisposition::Stop);
        }
    };
    let bulk_id = submission.handle.bulk_id.clone();
    let index_by_id = job
        .indices
        .iter()
        .map(|&index| (items[index].id.as_str(), index))
        .collect::<HashMap<_, _>>();

    let mut failed_updates = submission
        .failed
        .into_iter()
        .map(|failure| {
            let index = *index_by_id.get(failure.id.as_str()).ok_or_else(|| {
                anyhow!(
                    "Pangram bulk {bulk_id} rejected unknown item {}",
                    failure.id
                )
            })?;
            Ok((index, failure.message))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    let accepted_updates = submission
        .handle
        .accepted
        .iter()
        .map(|accepted| {
            let index = index_by_id
                .get(accepted.id.as_str())
                .copied()
                .ok_or_else(|| {
                    anyhow!(
                        "Pangram bulk {bulk_id} accepted unknown item {}",
                        accepted.id
                    )
                })?;
            Ok((index, accepted))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    drop(index_by_id);

    let mut accepted_indices = Vec::with_capacity(accepted_updates.len());
    for &index in &job.indices {
        if let Some(message) = failed_updates.remove(&index) {
            items[index].failed(message, color);
        } else if let Some(accepted) = accepted_updates.get(&index) {
            let detail = format!("Pangram bulk {bulk_id} · task {}", accepted.task_id);
            items[index].transition(ItemState::Submitted, Some(&detail), color);
            accepted_indices.push(index);
        } else {
            bail!(
                "Pangram bulk {bulk_id} omitted submitted item {}",
                items[index].id
            );
        }
    }

    let outcomes = match client.wait_for_bulk(&submission.handle).await {
        Ok(outcomes) => outcomes,
        Err(error) => {
            for index in accepted_indices {
                items[index].failed(
                    format!("Pangram bulk {bulk_id} failed while polling: {error}"),
                    color,
                );
            }
            return Ok(JobDisposition::Stop);
        }
    };
    let ordered = order_outcomes(&accepted_indices, items, outcomes, &bulk_id)?;
    let mut pending = false;
    for (index, outcome) in ordered {
        match outcome {
            pangram::BulkItemOutcome::Complete { score, .. } => {
                items[index].transition(ItemState::Completed, None, color);
                let document = items[index]
                    .document
                    .as_ref()
                    .expect("completed items retain their document");
                println!(
                    "{}",
                    format_scored_document(&document.document, document.word_count, &score, color)
                );
            }
            pangram::BulkItemOutcome::Failed { message, .. } => {
                items[index].failed(message, color);
            }
            pangram::BulkItemOutcome::Pending {
                task_id, status, ..
            } => {
                pending = true;
                let detail = format!(
                    "Pangram bulk {bulk_id} task {task_id} remains {status} and can be resumed"
                );
                items[index].transition(ItemState::TimedOut, Some(&detail), color);
            }
        }
    }
    Ok(if pending {
        JobDisposition::Stop
    } else {
        JobDisposition::Complete
    })
}

fn order_outcomes(
    indices: &[usize],
    items: &[ScoringItem],
    outcomes: Vec<pangram::BulkItemOutcome>,
    bulk_id: &str,
) -> Result<Vec<(usize, pangram::BulkItemOutcome)>> {
    let mut by_id = outcomes
        .into_iter()
        .map(|outcome| (outcome.id().to_owned(), outcome))
        .collect::<HashMap<_, _>>();
    if by_id.len() != indices.len() {
        bail!(
            "Pangram bulk {bulk_id} returned {} outcomes for {} accepted items",
            by_id.len(),
            indices.len()
        );
    }

    let mut ordered = Vec::with_capacity(indices.len());
    for &index in indices {
        let outcome = by_id.remove(&items[index].id).ok_or_else(|| {
            anyhow!(
                "Pangram bulk {bulk_id} omitted accepted item {}",
                items[index].id
            )
        })?;
        ordered.push((index, outcome));
    }
    if let Some(id) = by_id.into_keys().next() {
        bail!("Pangram bulk {bulk_id} returned unknown item {id}");
    }
    Ok(ordered)
}

fn finish(items: &[ScoringItem]) -> Result<()> {
    let failures = items
        .iter()
        .filter(|item| item.state == ItemState::Failed)
        .count();
    if failures > 0 {
        bail!("failed to process {failures} source item(s)");
    }

    let eligible = items
        .iter()
        .filter(|item| {
            matches!(
                item.state,
                ItemState::Completed | ItemState::DryRun | ItemState::TimedOut
            )
        })
        .collect::<Vec<_>>();
    let total_words = eligible.iter().try_fold(0usize, |total, item| {
        total
            .checked_add(item.word_count.expect("eligible items have word counts"))
            .context("total word count overflowed")
    })?;
    let noun = if eligible.len() == 1 {
        "article"
    } else {
        "articles"
    };
    println!(
        "Total: {total_words} words across {} eligible {noun}.",
        eligible.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matter_admission_uses_estimates_then_rechecks_actual_counts() {
        let initial = admit_in_order([(0, 6), (1, 5), (2, 4)], 10);
        assert_eq!(initial.admitted, [0, 2]);
        assert_eq!(initial.skipped[0].0, 1);
        assert_eq!(initial.skipped[0].1.remaining_words, 4);

        let actual = admit_in_order([(0, 8), (2, 4)], 10);
        assert_eq!(actual.admitted, [0]);
        assert_eq!(actual.skipped[0].0, 2);
        assert_eq!(actual.skipped[0].1.remaining_words, 2);
    }

    #[test]
    fn partition_respects_billable_unit_boundaries_and_source_order() {
        assert_eq!(billable_units(0), 1);
        assert_eq!(billable_units(100), 1);
        assert_eq!(billable_units(101), 2);

        let mut items = vec![
            prepared_item("first", 99_900),
            prepared_item("second", 100),
            prepared_item("third", 1),
        ];
        let jobs = partition(&mut items, false);

        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].units, 1_000);
        assert_eq!(jobs[0].indices, [0, 1]);
        assert_eq!(jobs[1].units, 1);
        assert_eq!(jobs[1].indices, [2]);
    }

    #[test]
    fn response_mapping_restores_source_order_and_keeps_item_failures_isolated() {
        let items = vec![
            prepared_item("first", 1),
            prepared_item("second", 1),
            prepared_item("third", 1),
        ];
        let outcomes = vec![
            pangram::BulkItemOutcome::Failed {
                id: "third".to_owned(),
                message: "third failed".to_owned(),
            },
            pangram::BulkItemOutcome::Pending {
                id: "first".to_owned(),
                task_id: "task-first".to_owned(),
                status: "running".to_owned(),
            },
            pangram::BulkItemOutcome::Failed {
                id: "second".to_owned(),
                message: "second failed".to_owned(),
            },
        ];

        let ordered = order_outcomes(&[0, 1, 2], &items, outcomes, "bulk-test").unwrap();
        assert_eq!(
            ordered
                .iter()
                .map(|(_, outcome)| outcome.id())
                .collect::<Vec<_>>(),
            ["first", "second", "third"]
        );
        assert!(matches!(
            &ordered[0].1,
            pangram::BulkItemOutcome::Pending { task_id, .. } if task_id == "task-first"
        ));
        assert!(matches!(
            &ordered[1].1,
            pangram::BulkItemOutcome::Failed { message, .. } if message == "second failed"
        ));
        assert!(matches!(
            &ordered[2].1,
            pangram::BulkItemOutcome::Failed { message, .. } if message == "third failed"
        ));
    }

    fn prepared_item(id: &str, words: usize) -> ScoringItem {
        ScoringItem {
            source: "Feed",
            id: id.to_owned(),
            title: Some(id.to_owned()),
            state: ItemState::AwaitingSubmission,
            document: Some(PreparedDocument {
                document: InputDocument::Item {
                    source: "Feed",
                    item_id: id.to_owned(),
                    document: Document {
                        title: Some(id.to_owned()),
                        url: None,
                        text: id.to_owned(),
                    },
                },
                word_count: words,
            }),
            word_count: Some(words),
            units: Some(billable_units(words)),
        }
    }
}
