use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{StreamExt, pin_mut};
use url::Url;

use crate::{
    GlobalArgs, InputDocument, PreparedDocument, color_enabled, display_title, format_document,
    format_scored_document, pangram, render_title, scoring, source::Document, source::feed,
    source::matter,
};

const PANGRAM_UNIT_WORDS: usize = 100;
const MAX_PANGRAM_BULK_UNITS: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemState {
    Pending,
    Selected,
    Prepared,
    Unavailable,
    Failed,
    BudgetSkipped,
    Submitted,
    Completed,
    DryRun,
    PangramPending,
}

impl ItemState {
    fn label(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Selected => "Selected",
            Self::Prepared => "Prepared",
            Self::Unavailable => "Unavailable",
            Self::Failed => "Failed",
            Self::BudgetSkipped => "Skipped by word budget",
            Self::Submitted => "Submitted",
            Self::Completed => "Completed",
            Self::DryRun => "Dry run",
            Self::PangramPending => "Pangram pending",
        }
    }
}

struct ItemRun {
    source: &'static str,
    id: String,
    title: Option<String>,
    state: ItemState,
    document: Option<PreparedDocument>,
    word_count: Option<usize>,
    units: Option<usize>,
}

impl ItemRun {
    fn new(source: &'static str, id: String, title: Option<String>) -> Self {
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
            "{}: {} · {}: {}",
            state.label(),
            render_title(display_title(self.title.as_deref()), color),
            self.source,
            self.id
        );
        if let Some(detail) = detail {
            eprintln!("  {detail}");
        }
    }

    fn pending(&mut self, color: bool) {
        self.transition(ItemState::Pending, None, color);
    }

    fn unavailable(&mut self, reason: impl fmt::Display, color: bool) {
        self.transition(ItemState::Unavailable, Some(&reason), color);
    }

    fn failed(&mut self, error: impl fmt::Display, color: bool) {
        self.transition(ItemState::Failed, Some(&error), color);
    }

    fn budget_skipped(&mut self, exhausted: scoring::BudgetExhausted, color: bool) {
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

    fn set_document(&mut self, document: Document) -> Result<(), anyhow::Error> {
        let prepared = PreparedDocument::new(InputDocument::Item {
            source: self.source,
            item_id: self.id.clone(),
            document,
        })?;
        self.word_count = Some(prepared.word_count);
        self.document = Some(prepared);
        Ok(())
    }

    fn prepare(&mut self, color: bool) {
        let words = self
            .word_count
            .expect("an admitted source item has a word count");
        let units = billable_units(words);
        self.units = Some(units);
        let detail = format!("{words} words · {units} Pangram units");
        self.transition(ItemState::Prepared, Some(&detail), color);
    }
}

struct MatterCandidate {
    item: matter::Item,
    run: ItemRun,
}

struct FeedCandidate {
    document: Option<Result<Document, feed::ItemError>>,
    run: ItemRun,
}

#[derive(Debug, PartialEq, Eq)]
struct Admission {
    admitted: Vec<usize>,
    skipped: Vec<(usize, scoring::BudgetExhausted)>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct BulkJob {
    indices: Vec<usize>,
    units: usize,
}

pub(crate) async fn score_matter(
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
        let run = ItemRun::new("Matter", item.id.clone(), Some(item.title.clone()));
        candidates.push(MatterCandidate { item, run });
    }

    for candidate in &mut candidates {
        candidate.run.pending(color);
    }

    let mut estimates = Vec::new();
    for (index, candidate) in candidates.iter_mut().enumerate() {
        match candidate.item.estimated_word_count() {
            Ok(words) => estimates.push((index, words)),
            Err(matter::ItemError::Unavailable { reason }) => {
                candidate.run.unavailable(reason, color);
            }
            Err(error) => candidate.run.failed(error, color),
        }
    }
    let initial = admit_in_order(estimates, global.max_words);
    for (index, exhausted) in initial.skipped {
        candidates[index].run.budget_skipped(exhausted, color);
    }
    for &index in &initial.admitted {
        candidates[index]
            .run
            .transition(ItemState::Selected, None, color);
    }

    let selected_items = initial
        .admitted
        .iter()
        .map(|&index| &candidates[index].item)
        .collect::<Vec<_>>();
    let fetched = client.fetch_documents(&selected_items).await;
    let mut actual_counts = Vec::new();
    for (index, document) in initial.admitted.into_iter().zip(fetched) {
        let run = &mut candidates[index].run;
        match document {
            Ok(document) => match run.set_document(document) {
                Ok(()) => actual_counts.push((index, run.word_count.expect("just prepared"))),
                Err(error) => run.failed(error, color),
            },
            Err(matter::ItemError::Unavailable { reason }) => run.unavailable(reason, color),
            Err(error) => run.failed(error, color),
        }
    }

    let actual = admit_in_order(actual_counts, global.max_words);
    for (index, exhausted) in actual.skipped {
        candidates[index].run.budget_skipped(exhausted, color);
    }
    for index in actual.admitted {
        candidates[index].run.prepare(color);
    }

    let mut items = candidates
        .into_iter()
        .map(|candidate| candidate.run)
        .collect::<Vec<_>>();
    execute(&mut items, global.dry_run, global.pangram_api_key, color).await
}

pub(crate) async fn score_feed(feed_url: Url, global: GlobalArgs) -> Result<()> {
    let color = color_enabled();
    let documents = feed::Client::new().fetch_documents(feed_url).await?;
    let mut candidates = documents
        .into_iter()
        .map(|(item, document)| FeedCandidate {
            run: ItemRun::new("Feed", item.id, item.title),
            document: Some(document),
        })
        .collect::<Vec<_>>();

    for candidate in &mut candidates {
        candidate.run.pending(color);
    }

    let mut counts = Vec::new();
    for (index, candidate) in candidates.iter_mut().enumerate() {
        let document = candidate
            .document
            .take()
            .expect("feed documents are prepared once");
        match document {
            Ok(document) => match candidate.run.set_document(document) {
                Ok(()) => counts.push((
                    index,
                    candidate
                        .run
                        .word_count
                        .expect("document was just prepared"),
                )),
                Err(error) => candidate.run.failed(error, color),
            },
            Err(error @ feed::ItemError::Unavailable) => {
                candidate.run.unavailable(error, color);
            }
            Err(error) => candidate.run.failed(error, color),
        }
    }

    let admission = admit_in_order(counts, global.max_words);
    for (index, exhausted) in admission.skipped {
        candidates[index].run.budget_skipped(exhausted, color);
    }
    for index in admission.admitted {
        candidates[index].run.prepare(color);
    }

    let mut items = candidates
        .into_iter()
        .map(|candidate| candidate.run)
        .collect::<Vec<_>>();
    execute(&mut items, global.dry_run, global.pangram_api_key, color).await
}

fn admit_in_order(counts: impl IntoIterator<Item = (usize, usize)>, max_words: usize) -> Admission {
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

fn partition(items: &mut [ItemRun], color: bool) -> Vec<BulkJob> {
    for item in items
        .iter_mut()
        .filter(|item| item.state == ItemState::Prepared)
    {
        let units = item
            .units
            .expect("prepared source items have Pangram units");
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
        if item.state != ItemState::Prepared {
            continue;
        }
        let units = item
            .units
            .expect("prepared source items have Pangram units");
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

async fn execute(
    items: &mut [ItemRun],
    dry_run: bool,
    pangram_api_key: Option<String>,
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
        let client = pangram::Client::new(api_key);
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
    items: &mut [ItemRun],
    color: bool,
) -> Result<()> {
    for (job_index, job) in jobs.iter().enumerate() {
        println!(
            "Bulk job {}: {} item(s) · {} Pangram units",
            job_index + 1,
            job.indices.len(),
            job.units
        );
        execute_job(client, model, job, items, color).await?;
    }
    Ok(())
}

async fn execute_job(
    client: &pangram::Client,
    model: &str,
    job: &BulkJob,
    items: &mut [ItemRun],
    color: bool,
) -> Result<()> {
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
            return Ok(());
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
            return Ok(());
        }
    };
    let ordered = order_outcomes(&accepted_indices, items, outcomes, &bulk_id)?;
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
                let detail = format!(
                    "Pangram bulk {bulk_id} task {task_id} remains {status} and can be resumed"
                );
                items[index].transition(ItemState::PangramPending, Some(&detail), color);
            }
        }
    }
    Ok(())
}

fn order_outcomes(
    indices: &[usize],
    items: &[ItemRun],
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

fn finish(items: &[ItemRun]) -> Result<()> {
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
                ItemState::Completed | ItemState::DryRun | ItemState::PangramPending
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
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        thread,
    };

    use super::*;
    use url::Url;

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

    #[tokio::test]
    async fn sequential_jobs_submit_poll_and_apply_item_results() {
        let (base_url, server) = sequential_bulk_server();
        let client = pangram::Client::with_base_url("secret-key".to_owned(), base_url);
        let mut items = vec![prepared_item("first", 100_000), prepared_item("second", 1)];
        let jobs = partition(&mut items, false);

        execute_jobs(&client, "pangram-4", &jobs, &mut items, false)
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(jobs.len(), 2);
        assert_eq!(items[0].state, ItemState::Completed);
        assert_eq!(items[1].state, ItemState::Failed);
    }

    fn sequential_bulk_server() -> (Url, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let server = thread::spawn(move || {
            let responses = vec![
                r#"{
                    "bulk_id":"bulk-first",
                    "status":"queued",
                    "total_items":1,
                    "accepted_items":[
                        {"index":0,"id":"first","task_id":"task-first"}
                    ],
                    "failed_items":[]
                }"#
                .to_owned(),
                r#"{
                    "bulk_id":"bulk-first",
                    "status":"succeeded",
                    "total_items":1,
                    "accepted":1,
                    "succeeded":1,
                    "failed":0
                }"#
                .to_owned(),
                format!(
                    r#"{{
                        "bulk_id":"bulk-first",
                        "offset":0,
                        "limit":1000,
                        "total_items":1,
                        "items":[{{
                            "index":0,
                            "id":"first",
                            "task_id":"task-first",
                            "stage":"STAGE_SUCCESS",
                            "error":null,
                            "result":{}
                        }}],
                        "failed_items":[]
                    }}"#,
                    score_response("first")
                ),
                r#"{
                    "bulk_id":"bulk-second",
                    "status":"queued",
                    "total_items":1,
                    "accepted_items":[
                        {"index":0,"id":"second","task_id":"task-second"}
                    ],
                    "failed_items":[]
                }"#
                .to_owned(),
                r#"{
                    "bulk_id":"bulk-second",
                    "status":"failed",
                    "total_items":1,
                    "accepted":1,
                    "succeeded":0,
                    "failed":1
                }"#
                .to_owned(),
                r#"{
                    "bulk_id":"bulk-second",
                    "offset":0,
                    "limit":1000,
                    "total_items":1,
                    "items":[],
                    "failed_items":[{
                        "index":0,
                        "id":"second",
                        "task_id":"task-second",
                        "stage":"STAGE_FAILED",
                        "error":"invalid text"
                    }]
                }"#
                .to_owned(),
            ];

            for (index, response) in responses.into_iter().enumerate() {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                match index {
                    0 => {
                        assert!(request.starts_with("POST /bulk HTTP/1.1"));
                        assert!(request.contains(r#""id":"first""#));
                    }
                    1 => assert!(request.starts_with("GET /bulk/bulk-first HTTP/1.1")),
                    2 => {
                        assert!(request.starts_with("GET /bulk/bulk-first/results?"));
                    }
                    3 => {
                        assert!(request.starts_with("POST /bulk HTTP/1.1"));
                        assert!(request.contains(r#""id":"second""#));
                    }
                    4 => assert!(request.starts_with("GET /bulk/bulk-second HTTP/1.1")),
                    5 => {
                        assert!(request.starts_with("GET /bulk/bulk-second/results?"));
                    }
                    _ => unreachable!(),
                }
                write_response(&mut stream, &response);
            }
        });
        (base_url, server)
    }

    fn score_response(text: &str) -> String {
        format!(
            r#"{{
                "stage":"STAGE_SUCCESS",
                "text":"{text}",
                "version":"4.0",
                "headline":"Human Written",
                "prediction":"This text appears human-written.",
                "prediction_short":"Human",
                "fraction_ai":0.0,
                "fraction_ai_assisted":0.0,
                "fraction_human":1.0,
                "num_ai_segments":0,
                "num_ai_assisted_segments":0,
                "num_human_segments":1,
                "windows":[]
            }}"#
        )
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let bytes_read = stream.read(&mut buffer).unwrap();
            assert!(bytes_read > 0, "connection closed before request completed");
            request.extend_from_slice(&buffer[..bytes_read]);
            let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                continue;
            };
            let body_start = header_end + 4;
            let headers = std::str::from_utf8(&request[..header_end]).unwrap();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if request.len() >= body_start + content_length {
                request.truncate(body_start + content_length);
                return String::from_utf8(request).unwrap();
            }
        }
    }

    fn write_response(stream: &mut TcpStream, body: &str) {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    }

    fn prepared_item(id: &str, words: usize) -> ItemRun {
        ItemRun {
            source: "Feed",
            id: id.to_owned(),
            title: Some(id.to_owned()),
            state: ItemState::Prepared,
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
