//! Stateless client for Pangram's asynchronous text-detection API.
use std::{ops::ControlFlow, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::time::{Instant, sleep_until};
use url::Url;

const API_BASE_URL: &str = "https://text.external-api.pangram.com/";
const API_KEY_HEADER: &str = "x-api-key";
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const TASK_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub(crate) struct Client {
    http: reqwest::Client,
    api_key: Arc<str>,
    base_url: Url,
}

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error("Pangram API request failed")]
    Request(#[from] reqwest::Error),

    #[error("Pangram API request failed while polling task {}", task.task_id)]
    PollRequest {
        task: TaskHandle,
        #[source]
        source: reqwest::Error,
    },

    #[error("Pangram returned no available models")]
    NoAvailableModels,

    #[error("Pangram returned an invalid response: {0}")]
    InvalidResponse(String),

    #[error("Pangram task {task_id} failed: {message}")]
    TaskFailed { task_id: String, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskHandle {
    pub(crate) task_id: String,
    pub(crate) model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TaskStage {
    Preprocessing,
    Other(String),
}

impl std::fmt::Display for TaskStage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Preprocessing => formatter.write_str("preprocessing"),
            Self::Other(stage) => formatter.write_str(stage),
        }
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum ScoreOutcome {
    Complete(Score),
    Pending { task: TaskHandle, stage: TaskStage },
}

#[derive(Debug, PartialEq)]
pub(crate) struct Score {
    pub(crate) task_id: String,
    pub(crate) model: String,
    pub(crate) version: String,
    pub(crate) analyzed_text: String,
    pub(crate) prediction: Prediction,
    pub(crate) fractions: Fractions,
    pub(crate) segment_counts: SegmentCounts,
    pub(crate) windows: Vec<Window>,
    pub(crate) dashboard_link: Option<String>,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Prediction {
    pub(crate) headline: String,
    pub(crate) short: String,
    pub(crate) explanation: String,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Fractions {
    pub(crate) ai: f64,
    pub(crate) ai_assisted: f64,
    pub(crate) human: f64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SegmentCounts {
    pub(crate) ai: u64,
    pub(crate) ai_assisted: u64,
    pub(crate) human: u64,
}

#[derive(Debug, Deserialize, PartialEq)]
pub(crate) struct Window {
    pub(crate) text: String,
    pub(crate) label: String,
    pub(crate) ai_assistance_score: f64,
    pub(crate) confidence: String,
    pub(crate) start_index: usize,
    pub(crate) end_index: usize,
    pub(crate) word_count: u64,
    pub(crate) token_length: u64,
    pub(crate) is_humanized: Option<bool>,
    pub(crate) humanizer_score: Option<f64>,
}

#[derive(Deserialize)]
struct ModelsResponse {
    models: Vec<String>,
}

#[derive(Serialize)]
struct SubmitRequest<'a> {
    text: &'a str,
    model: &'a str,
    public_dashboard_link: bool,
}

#[derive(Deserialize)]
struct SubmitResponse {
    task_id: String,
}

#[derive(Deserialize)]
struct TaskResponse {
    stage: String,
    task_id: Option<String>,
    text: Option<String>,
    version: Option<String>,
    headline: Option<String>,
    prediction: Option<String>,
    prediction_short: Option<String>,
    fraction_ai: Option<f64>,
    fraction_ai_assisted: Option<f64>,
    fraction_human: Option<f64>,
    num_ai_segments: Option<u64>,
    num_ai_assisted_segments: Option<u64>,
    num_human_segments: Option<u64>,
    windows: Option<Vec<Window>>,
    dashboard_link: Option<String>,
    detail: Option<String>,
}

impl Client {
    pub(crate) fn new(api_key: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: api_key.into(),
            base_url: Url::parse(API_BASE_URL).expect("the Pangram API URL is valid"),
        }
    }

    pub(crate) async fn discover_model(&self) -> Result<String, Error> {
        let response: ModelsResponse = self
            .http
            .get(self.endpoint("models"))
            .timeout(REQUEST_TIMEOUT)
            .header(API_KEY_HEADER, self.api_key.as_ref())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let model = response
            .models
            .into_iter()
            .next()
            .ok_or(Error::NoAvailableModels)?;

        if model.is_empty() || model.trim() != model {
            return Err(Error::InvalidResponse(
                "model catalog started with an invalid selector".to_owned(),
            ));
        }

        Ok(model)
    }

    pub(crate) async fn score(&self, text: &str, model: &str) -> Result<ScoreOutcome, Error> {
        let task = self.submit(text, model).await?;
        self.resume(task).await
    }

    pub(crate) async fn resume(&self, task: TaskHandle) -> Result<ScoreOutcome, Error> {
        self.wait_for_task(task).await
    }

    async fn submit(&self, text: &str, model: &str) -> Result<TaskHandle, Error> {
        let response: SubmitResponse = self
            .http
            .post(self.endpoint("task"))
            .timeout(REQUEST_TIMEOUT)
            .header(API_KEY_HEADER, self.api_key.as_ref())
            .json(&SubmitRequest {
                text,
                model,
                public_dashboard_link: false,
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        if response.task_id.is_empty() {
            return Err(Error::InvalidResponse(
                "task submission omitted task_id".to_owned(),
            ));
        }

        Ok(TaskHandle {
            task_id: response.task_id,
            model: model.to_owned(),
        })
    }

    async fn wait_for_task(&self, task: TaskHandle) -> Result<ScoreOutcome, Error> {
        let deadline = Instant::now() + TASK_TIMEOUT;

        loop {
            match self.poll(&task).await? {
                ControlFlow::Break(score) => return Ok(ScoreOutcome::Complete(score)),
                ControlFlow::Continue(stage) => {
                    sleep_until((Instant::now() + POLL_INTERVAL).min(deadline)).await;

                    if Instant::now() >= deadline {
                        return Ok(ScoreOutcome::Pending { task, stage });
                    }
                }
            }
        }
    }

    async fn poll(&self, task: &TaskHandle) -> Result<ControlFlow<Score, TaskStage>, Error> {
        let response: TaskResponse = self
            .http
            .get(self.task_endpoint(&task.task_id))
            .timeout(REQUEST_TIMEOUT)
            .header(API_KEY_HEADER, self.api_key.as_ref())
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|source| Error::PollRequest {
                task: task.clone(),
                source,
            })?
            .json()
            .await
            .map_err(|source| Error::PollRequest {
                task: task.clone(),
                source,
            })?;

        response.into_poll(task)
    }

    fn endpoint(&self, path: &str) -> Url {
        self.base_url
            .join(path)
            .expect("static Pangram endpoint paths are valid")
    }

    fn task_endpoint(&self, task_id: &str) -> Url {
        let mut endpoint = self.endpoint("task");
        endpoint
            .path_segments_mut()
            .expect("the Pangram API URL can contain path segments")
            .push(task_id);
        endpoint
    }
}

impl TaskResponse {
    fn into_poll(self, task: &TaskHandle) -> Result<ControlFlow<Score, TaskStage>, Error> {
        let Self {
            stage,
            task_id,
            text,
            version,
            headline,
            prediction,
            prediction_short,
            fraction_ai,
            fraction_ai_assisted,
            fraction_human,
            num_ai_segments,
            num_ai_assisted_segments,
            num_human_segments,
            windows,
            dashboard_link,
            detail,
        } = self;

        match stage.as_str() {
            "STAGE_SUCCESS" => Ok(ControlFlow::Break(Score {
                task_id: task.task_id.clone(),
                model: task.model.clone(),
                version: required(&stage, version, "version")?,
                analyzed_text: required(&stage, text, "text")?,
                prediction: Prediction {
                    headline: required(&stage, headline, "headline")?,
                    short: required(&stage, prediction_short, "prediction_short")?,
                    explanation: required(&stage, prediction, "prediction")?,
                },
                fractions: Fractions {
                    ai: required(&stage, fraction_ai, "fraction_ai")?,
                    ai_assisted: required(&stage, fraction_ai_assisted, "fraction_ai_assisted")?,
                    human: required(&stage, fraction_human, "fraction_human")?,
                },
                segment_counts: SegmentCounts {
                    ai: required(&stage, num_ai_segments, "num_ai_segments")?,
                    ai_assisted: required(
                        &stage,
                        num_ai_assisted_segments,
                        "num_ai_assisted_segments",
                    )?,
                    human: required(&stage, num_human_segments, "num_human_segments")?,
                },
                windows: required(&stage, windows, "windows")?,
                dashboard_link,
            })),
            "STAGE_FAILED" => Err(Error::TaskFailed {
                task_id: task.task_id.clone(),
                message: headline
                    .or(detail)
                    .filter(|message| !message.is_empty())
                    .unwrap_or_else(|| "task failed without an explanation".to_owned()),
            }),
            "STAGE_PREPROCESSING" => {
                validate_pending_task_id(&stage, task_id.as_deref(), task)?;
                Ok(ControlFlow::Continue(TaskStage::Preprocessing))
            }
            _ => {
                validate_pending_task_id(&stage, task_id.as_deref(), task)?;
                Ok(ControlFlow::Continue(TaskStage::Other(stage)))
            }
        }
    }
}

fn required<T>(stage: &str, value: Option<T>, field: &str) -> Result<T, Error> {
    value.ok_or_else(|| Error::InvalidResponse(format!("{stage} response omitted {field}")))
}

fn validate_pending_task_id(
    stage: &str,
    response_task_id: Option<&str>,
    task: &TaskHandle,
) -> Result<(), Error> {
    match response_task_id {
        Some(task_id) if task_id == task.task_id => Ok(()),
        Some(task_id) => Err(Error::InvalidResponse(format!(
            "poll for task {} returned task_id {task_id}",
            task.task_id
        ))),
        None => Err(Error::InvalidResponse(format!(
            "{stage} response omitted task_id"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        ops::ControlFlow,
        sync::Arc,
        thread,
    };

    use super::{Client, Error, ScoreOutcome, TaskHandle, TaskResponse, TaskStage};
    use url::Url;

    fn task() -> TaskHandle {
        TaskHandle {
            task_id: "task-123".to_owned(),
            model: "pangram-4".to_owned(),
        }
    }

    #[tokio::test]
    async fn client_discovers_submits_and_polls_to_success() {
        let (base_url, server) = lifecycle_server();
        let client = Client {
            http: reqwest::Client::new(),
            api_key: Arc::from("secret-key"),
            base_url,
        };

        let model = client.discover_model().await.unwrap();
        let outcome = client.score("# An article", &model).await.unwrap();
        server.join().unwrap();

        let ScoreOutcome::Complete(score) = outcome else {
            panic!("expected a completed score");
        };
        assert_eq!(model, "pangram-4");
        assert_eq!(score.task_id, "task-123");
        assert_eq!(score.model, "pangram-4");
        assert_eq!(score.version, "4.0");
        assert_eq!(score.prediction.short, "Human");
    }

    fn lifecycle_server() -> (Url, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let server = thread::spawn(move || {
            let responses = [
                r#"{"models":["pangram-4"]}"#,
                r#"{"task_id":"task-123"}"#,
                r#"{"task_id":"task-123","stage":"STAGE_PREPROCESSING"}"#,
                r##"{
                    "stage":"STAGE_SUCCESS",
                    "text":"# An article",
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
                }"##,
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
                    0 => assert!(request.starts_with("GET /models HTTP/1.1")),
                    1 => {
                        assert!(request.starts_with("POST /task HTTP/1.1"));
                        let (_, body) = request.split_once("\r\n\r\n").unwrap();
                        let body: serde_json::Value = serde_json::from_str(body).unwrap();
                        assert_eq!(body["text"], "# An article");
                        assert_eq!(body["model"], "pangram-4");
                        assert_eq!(body["public_dashboard_link"], false);
                    }
                    _ => assert!(request.starts_with("GET /task/task-123 HTTP/1.1")),
                }

                write_response(&mut stream, response);
            }
        });

        (base_url, server)
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

    #[test]
    fn preprocessing_response_remains_resumable() {
        let response: TaskResponse =
            serde_json::from_str(r#"{"task_id":"task-123","stage":"STAGE_PREPROCESSING"}"#)
                .unwrap();

        assert!(matches!(
            response.into_poll(&task()).unwrap(),
            ControlFlow::Continue(TaskStage::Preprocessing)
        ));
    }

    #[test]
    fn successful_response_preserves_detection_details() {
        let response: TaskResponse = serde_json::from_str(
            r#"{
                "stage": "STAGE_SUCCESS",
                "text": "AI-assisted passage. Human passage.",
                "version": "4.0",
                "headline": "AI Assisted",
                "prediction": "A mix of assisted and human content.",
                "prediction_short": "Mixed",
                "fraction_ai": 0.0,
                "fraction_ai_assisted": 0.6,
                "fraction_human": 0.4,
                "num_ai_segments": 0,
                "num_ai_assisted_segments": 1,
                "num_human_segments": 1,
                "windows": [{
                    "text": "AI-assisted passage. ",
                    "label": "AI-Assisted",
                    "ai_assistance_score": 0.55,
                    "confidence": "High",
                    "start_index": 0,
                    "end_index": 21,
                    "word_count": 2,
                    "token_length": 5,
                    "is_humanized": true,
                    "humanizer_score": 0.91
                }]
            }"#,
        )
        .unwrap();

        let ControlFlow::Break(score) = response.into_poll(&task()).unwrap() else {
            panic!("expected a completed score");
        };

        assert_eq!(score.task_id, "task-123");
        assert_eq!(score.model, "pangram-4");
        assert_eq!(score.version, "4.0");
        assert_eq!(score.prediction.short, "Mixed");
        assert_eq!(score.fractions.ai_assisted, 0.6);
        assert_eq!(score.segment_counts.ai_assisted, 1);
        assert_eq!(score.windows[0].confidence, "High");
        assert_eq!(score.windows[0].is_humanized, Some(true));
    }

    #[test]
    fn failed_response_is_an_item_failure() {
        let response: TaskResponse = serde_json::from_str(
            r#"{
                "stage": "STAGE_FAILED",
                "headline": "preprocessing: input contained no valid text"
            }"#,
        )
        .unwrap();

        assert!(matches!(
            response.into_poll(&task()),
            Err(Error::TaskFailed { task_id, message })
                if task_id == "task-123" && message.contains("no valid text")
        ));
    }

    #[test]
    fn unknown_nonterminal_stage_remains_resumable() {
        let response: TaskResponse =
            serde_json::from_str(r#"{"task_id":"task-123","stage":"STAGE_QUEUED"}"#).unwrap();

        assert!(matches!(
            response.into_poll(&task()).unwrap(),
            ControlFlow::Continue(TaskStage::Other(stage)) if stage == "STAGE_QUEUED"
        ));
    }
}
