use std::{
    collections::{HashMap, HashSet},
    ops::ControlFlow,
};

use serde::{Deserialize, Serialize};
use tokio::time::{Instant, sleep_until};

use super::{
    API_KEY_HEADER, Client, Error, POLL_INTERVAL, REQUEST_TIMEOUT, Score, TASK_TIMEOUT, TaskHandle,
    TaskResponse, require_success,
};

const MAX_RESULTS_PER_PAGE: usize = 1_000;

#[derive(Debug, Serialize)]
pub(crate) struct BulkInput<'a> {
    pub(crate) id: &'a str,
    pub(crate) text: &'a str,
}

#[derive(Debug, Clone)]
pub(crate) struct BulkAccepted {
    pub(crate) id: String,
    pub(crate) task_id: String,
}

#[derive(Debug)]
pub(crate) struct BulkFailure {
    pub(crate) id: String,
    pub(crate) message: String,
}

#[derive(Debug, Clone)]
pub(crate) struct BulkHandle {
    pub(crate) bulk_id: String,
    pub(crate) model: String,
    pub(crate) accepted: Vec<BulkAccepted>,
    total_items: usize,
    rejected_ids: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct BulkSubmission {
    pub(crate) handle: BulkHandle,
    pub(crate) failed: Vec<BulkFailure>,
}

// Successful scores are kept inline to avoid allocating every normal result.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub(crate) enum BulkItemOutcome {
    Complete {
        id: String,
        score: Score,
    },
    Failed {
        id: String,
        message: String,
    },
    Pending {
        id: String,
        task_id: String,
        status: String,
    },
}

impl BulkItemOutcome {
    pub(crate) fn id(&self) -> &str {
        match self {
            Self::Complete { id, .. } | Self::Failed { id, .. } | Self::Pending { id, .. } => id,
        }
    }
}

#[derive(Serialize)]
struct SubmitBulkRequest<'a> {
    items: &'a [BulkInput<'a>],
    model: &'a str,
}

#[derive(Deserialize)]
struct SubmitBulkResponse {
    bulk_id: String,
    status: String,
    total_items: usize,
    accepted_items: Vec<BulkItemMetadata>,
    failed_items: Vec<BulkItemMetadata>,
}

#[derive(Deserialize)]
struct BulkStatusResponse {
    bulk_id: String,
    status: String,
    total_items: usize,
    accepted: usize,
    succeeded: usize,
    failed: usize,
}

#[derive(Deserialize)]
struct BulkResultsResponse {
    bulk_id: String,
    total_items: usize,
    items: Vec<BulkResultItem>,
    failed_items: Vec<BulkItemMetadata>,
}

#[derive(Deserialize)]
struct BulkItemMetadata {
    index: usize,
    id: Option<String>,
    task_id: Option<String>,
    stage: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct BulkResultItem {
    index: usize,
    id: Option<String>,
    task_id: Option<String>,
    stage: String,
    error: Option<String>,
    result: Option<TaskResponse>,
}

impl Client {
    pub(crate) async fn submit_bulk(
        &self,
        items: &[BulkInput<'_>],
        model: &str,
    ) -> Result<BulkSubmission, Error> {
        if items.is_empty() {
            return Err(Error::InvalidResponse(
                "cannot submit an empty Pangram bulk job".to_owned(),
            ));
        }

        let response = self
            .http
            .post(self.endpoint("bulk"))
            .timeout(REQUEST_TIMEOUT)
            .header(API_KEY_HEADER, self.api_key.as_ref())
            .json(&SubmitBulkRequest { items, model })
            .send()
            .await?;
        let response: SubmitBulkResponse = require_success(response).await?.json().await?;

        if response.bulk_id.is_empty() {
            return Err(Error::InvalidResponse(
                "bulk submission omitted bulk_id".to_owned(),
            ));
        }
        if !matches!(response.status.as_str(), "queued" | "running" | "failed") {
            return Err(Error::InvalidResponse(format!(
                "bulk submission returned unknown status {}",
                response.status
            )));
        }
        if response.total_items != items.len() {
            return Err(Error::InvalidResponse(format!(
                "bulk submission reported {} items for a {}-item request",
                response.total_items,
                items.len()
            )));
        }

        let requested_ids = items.iter().map(|item| item.id).collect::<HashSet<_>>();
        if requested_ids.len() != items.len() {
            return Err(Error::InvalidResponse(
                "bulk request contained duplicate item IDs".to_owned(),
            ));
        }

        let accepted = response
            .accepted_items
            .into_iter()
            .map(BulkItemMetadata::into_accepted)
            .collect::<Result<Vec<_>, _>>()?;
        let failed = response
            .failed_items
            .into_iter()
            .map(BulkItemMetadata::into_failure)
            .collect::<Result<Vec<_>, _>>()?;
        if accepted.len() + failed.len() != items.len() {
            return Err(Error::InvalidResponse(format!(
                "bulk submission accounted for {} of {} items",
                accepted.len() + failed.len(),
                items.len()
            )));
        }

        let mut returned_ids = HashSet::with_capacity(items.len());
        for id in accepted
            .iter()
            .map(|item| item.id.as_str())
            .chain(failed.iter().map(|item| item.id.as_str()))
        {
            if !requested_ids.contains(id) {
                return Err(Error::InvalidResponse(format!(
                    "bulk submission returned unknown item {id}"
                )));
            }
            if !returned_ids.insert(id) {
                return Err(Error::InvalidResponse(format!(
                    "bulk submission returned item {id} more than once"
                )));
            }
        }

        let rejected_ids = failed.iter().map(|item| item.id.clone()).collect();
        Ok(BulkSubmission {
            handle: BulkHandle {
                bulk_id: response.bulk_id,
                model: model.to_owned(),
                accepted,
                total_items: response.total_items,
                rejected_ids,
            },
            failed,
        })
    }

    pub(crate) async fn wait_for_bulk(
        &self,
        handle: &BulkHandle,
    ) -> Result<Vec<BulkItemOutcome>, Error> {
        if handle.accepted.is_empty() {
            return Ok(Vec::new());
        }

        let deadline = Instant::now() + TASK_TIMEOUT;
        loop {
            let status = self.bulk_status(handle).await?;
            if status.is_terminal()? {
                return self.bulk_results(handle).await;
            }

            sleep_until((Instant::now() + POLL_INTERVAL).min(deadline)).await;
            if Instant::now() >= deadline {
                return Ok(handle
                    .accepted
                    .iter()
                    .map(|item| BulkItemOutcome::Pending {
                        id: item.id.clone(),
                        task_id: item.task_id.clone(),
                        status: status.status.clone(),
                    })
                    .collect());
            }
        }
    }

    async fn bulk_status(&self, handle: &BulkHandle) -> Result<BulkStatusResponse, Error> {
        let response = self
            .http
            .get(self.bulk_endpoint(&handle.bulk_id, None))
            .timeout(REQUEST_TIMEOUT)
            .header(API_KEY_HEADER, self.api_key.as_ref())
            .send()
            .await?;
        let response: BulkStatusResponse = require_success(response).await?.json().await?;
        response.validate(handle)?;
        Ok(response)
    }

    async fn bulk_results(&self, handle: &BulkHandle) -> Result<Vec<BulkItemOutcome>, Error> {
        let response = self
            .http
            .get(self.bulk_endpoint(&handle.bulk_id, Some("results")))
            .timeout(REQUEST_TIMEOUT)
            .header(API_KEY_HEADER, self.api_key.as_ref())
            .query(&[("offset", 0usize), ("limit", MAX_RESULTS_PER_PAGE)])
            .send()
            .await?;
        let response: BulkResultsResponse = require_success(response).await?.json().await?;
        response.into_outcomes(handle)
    }

    fn bulk_endpoint(&self, bulk_id: &str, suffix: Option<&str>) -> url::Url {
        let mut endpoint = self.endpoint("bulk");
        let mut segments = endpoint
            .path_segments_mut()
            .expect("the Pangram API URL can contain path segments");
        segments.push(bulk_id);
        if let Some(suffix) = suffix {
            segments.push(suffix);
        }
        drop(segments);
        endpoint
    }
}

impl BulkItemMetadata {
    fn into_accepted(self) -> Result<BulkAccepted, Error> {
        let context = format!("accepted bulk item at index {}", self.index);
        let id = required_string(self.id, &context, "id")?;
        let task_id = required_string(self.task_id, &context, "task_id")?;
        Ok(BulkAccepted { id, task_id })
    }

    fn into_failure(self) -> Result<BulkFailure, Error> {
        let context = format!("failed bulk item at index {}", self.index);
        let id = required_string(self.id, &context, "id")?;
        let message = self
            .error
            .filter(|message| !message.is_empty())
            .unwrap_or_else(|| {
                format!(
                    "Pangram item failed during {}",
                    self.stage.as_deref().unwrap_or("an unknown stage")
                )
            });
        Ok(BulkFailure { id, message })
    }
}

impl BulkStatusResponse {
    fn validate(&self, handle: &BulkHandle) -> Result<(), Error> {
        if self.bulk_id != handle.bulk_id {
            return Err(Error::InvalidResponse(format!(
                "poll for bulk {} returned bulk_id {}",
                handle.bulk_id, self.bulk_id
            )));
        }
        if self.total_items != handle.total_items {
            return Err(Error::InvalidResponse(format!(
                "bulk {} reported {} total items after submission reported {}",
                handle.bulk_id, self.total_items, handle.total_items
            )));
        }
        if self.accepted != handle.accepted.len() {
            return Err(Error::InvalidResponse(format!(
                "bulk {} reported {} accepted items after submission reported {}",
                handle.bulk_id,
                self.accepted,
                handle.accepted.len()
            )));
        }
        let terminal = self.succeeded.checked_add(self.failed).ok_or_else(|| {
            Error::InvalidResponse("bulk terminal-item count overflowed".to_owned())
        })?;
        if terminal > self.total_items {
            return Err(Error::InvalidResponse(format!(
                "bulk {} reported more terminal items than total items",
                handle.bulk_id
            )));
        }
        Ok(())
    }

    fn is_terminal(&self) -> Result<bool, Error> {
        match self.status.as_str() {
            "queued" | "running" => Ok(false),
            "succeeded" | "failed" | "partial" => Ok(true),
            status => Err(Error::InvalidResponse(format!(
                "bulk {} returned unknown status {status}",
                self.bulk_id
            ))),
        }
    }
}

impl BulkResultsResponse {
    fn into_outcomes(self, handle: &BulkHandle) -> Result<Vec<BulkItemOutcome>, Error> {
        if self.bulk_id != handle.bulk_id {
            return Err(Error::InvalidResponse(format!(
                "results for bulk {} returned bulk_id {}",
                handle.bulk_id, self.bulk_id
            )));
        }
        if self.total_items != handle.total_items {
            return Err(Error::InvalidResponse(format!(
                "bulk {} results reported {} items after submission reported {}",
                handle.bulk_id, self.total_items, handle.total_items
            )));
        }

        let accepted_indices = handle
            .accepted
            .iter()
            .enumerate()
            .map(|(index, item)| (item.id.as_str(), index))
            .collect::<HashMap<_, _>>();
        let rejected_ids = handle
            .rejected_ids
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let mut returned_indices = HashSet::with_capacity(handle.accepted.len());
        let mut outcomes = Vec::with_capacity(handle.accepted.len());

        for item in self.items {
            let context = format!("bulk result item at index {}", item.index);
            let id = required_string(item.id, &context, "id")?;
            if rejected_ids.contains(id.as_str()) {
                continue;
            }
            validate_result_id(&id, &accepted_indices, &mut returned_indices, handle)?;

            match item.stage.as_str() {
                "STAGE_SUCCESS" => {
                    let task_id = required_string(item.task_id, &context, "task_id")?;
                    let result = item.result.ok_or_else(|| {
                        Error::InvalidResponse(format!("successful bulk item {id} omitted result"))
                    })?;
                    let task = TaskHandle {
                        task_id,
                        model: handle.model.clone(),
                    };
                    let ControlFlow::Break(score) = result.into_poll(&task)? else {
                        return Err(Error::InvalidResponse(format!(
                            "successful bulk item {id} contained a nonterminal result"
                        )));
                    };
                    outcomes.push(BulkItemOutcome::Complete { id, score });
                }
                "STAGE_FAILED" => outcomes.push(BulkItemOutcome::Failed {
                    id,
                    message: item
                        .error
                        .filter(|message| !message.is_empty())
                        .unwrap_or_else(|| {
                            "Pangram bulk item failed without an explanation".to_owned()
                        }),
                }),
                stage => {
                    return Err(Error::InvalidResponse(format!(
                        "terminal bulk {} returned nonterminal item {id} at {stage}",
                        handle.bulk_id
                    )));
                }
            }
        }

        for item in self.failed_items {
            let failure = item.into_failure()?;
            if rejected_ids.contains(failure.id.as_str()) {
                continue;
            }
            validate_result_id(
                &failure.id,
                &accepted_indices,
                &mut returned_indices,
                handle,
            )?;
            outcomes.push(BulkItemOutcome::Failed {
                id: failure.id,
                message: failure.message,
            });
        }

        if returned_indices.len() != handle.accepted.len() {
            return Err(Error::InvalidResponse(format!(
                "terminal bulk {} returned {} of {} accepted item results",
                handle.bulk_id,
                returned_indices.len(),
                handle.accepted.len()
            )));
        }
        Ok(outcomes)
    }
}

fn validate_result_id(
    id: &str,
    accepted_indices: &HashMap<&str, usize>,
    returned_indices: &mut HashSet<usize>,
    handle: &BulkHandle,
) -> Result<(), Error> {
    let Some(index) = accepted_indices.get(id) else {
        return Err(Error::InvalidResponse(format!(
            "bulk {} returned unknown item {id}",
            handle.bulk_id
        )));
    };
    if !returned_indices.insert(*index) {
        return Err(Error::InvalidResponse(format!(
            "bulk {} returned item {id} more than once",
            handle.bulk_id
        )));
    }
    Ok(())
}

fn required_string(value: Option<String>, context: &str, field: &str) -> Result<String, Error> {
    value
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::InvalidResponse(format!("{context} omitted {field}")))
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        thread,
    };

    use super::{BulkInput, BulkItemOutcome};
    use crate::pangram::Client;
    use url::Url;

    #[tokio::test]
    async fn client_submits_polls_and_returns_individual_results() {
        let (base_url, server) = bulk_lifecycle_server();
        let client = Client::with_base_url("secret-key".to_owned(), base_url);
        let inputs = [
            BulkInput {
                id: "item-first",
                text: "First article",
            },
            BulkInput {
                id: "item-second",
                text: "Second article",
            },
        ];

        let submission = client.submit_bulk(&inputs, "pangram-4").await.unwrap();
        assert!(submission.failed.is_empty());
        let outcomes = client.wait_for_bulk(&submission.handle).await.unwrap();
        server.join().unwrap();

        assert!(outcomes.iter().any(|outcome| matches!(
            outcome,
            BulkItemOutcome::Failed { id, message }
                if id == "item-second" && message == "invalid text"
        )));
        assert!(outcomes.iter().any(|outcome| matches!(
            outcome,
            BulkItemOutcome::Complete { id, score }
                if id == "item-first" && score.task_id == "task-first"
        )));
    }

    fn bulk_lifecycle_server() -> (Url, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let server = thread::spawn(move || {
            let responses = [
                r#"{
                    "bulk_id":"bulk-123",
                    "status":"queued",
                    "total_items":2,
                    "accepted_items":[
                        {"index":0,"id":"item-first","task_id":"task-first"},
                        {"index":1,"id":"item-second","task_id":"task-second"}
                    ],
                    "failed_items":[]
                }"#,
                r#"{
                    "bulk_id":"bulk-123",
                    "status":"running",
                    "total_items":2,
                    "accepted":2,
                    "succeeded":0,
                    "failed":0
                }"#,
                r#"{
                    "bulk_id":"bulk-123",
                    "status":"partial",
                    "total_items":2,
                    "accepted":2,
                    "succeeded":1,
                    "failed":1
                }"#,
                &format!(
                    r#"{{
                        "bulk_id":"bulk-123",
                        "offset":0,
                        "limit":1000,
                        "total_items":2,
                        "items":[{{
                            "index":0,
                            "id":"item-first",
                            "task_id":"task-first",
                            "stage":"STAGE_SUCCESS",
                            "error":null,
                            "result":{}
                        }}],
                        "failed_items":[{{
                            "index":1,
                            "id":"item-second",
                            "task_id":"task-second",
                            "stage":"STAGE_FAILED",
                            "error":"invalid text"
                        }}]
                    }}"#,
                    score_response("First article")
                ),
            ];

            for (index, response) in responses.into_iter().enumerate() {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("x-api-key: secret-key")
                );
                match index {
                    0 => {
                        assert!(request.starts_with("POST /bulk HTTP/1.1"));
                        let (_, body) = request.split_once("\r\n\r\n").unwrap();
                        let body: serde_json::Value = serde_json::from_str(body).unwrap();
                        assert_eq!(body["model"], "pangram-4");
                        assert_eq!(body["items"][0]["id"], "item-first");
                        assert_eq!(body["items"][1]["text"], "Second article");
                    }
                    1 | 2 => assert!(request.starts_with("GET /bulk/bulk-123 HTTP/1.1")),
                    3 => {
                        assert!(request.starts_with("GET /bulk/bulk-123/results?"));
                        assert!(request.contains("offset=0"));
                        assert!(request.contains("limit=1000"));
                    }
                    _ => unreachable!(),
                }
                write_response(&mut stream, response);
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
}
