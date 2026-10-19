use std::{
    collections::HashMap,
    future,
    io::{self, Write as _},
    sync::{Arc, Mutex},
};

use serde::Deserialize;
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

#[derive(Clone, Default)]
struct MockPangram {
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    next_task: usize,
    next_bulk: usize,
    tasks: HashMap<String, ScoredText>,
    bulks: HashMap<String, BulkJob>,
}

struct ScoredText {
    model: String,
    text: String,
}

struct BulkJob {
    model: String,
    items: Vec<BulkItem>,
}

#[derive(Deserialize)]
struct TaskRequest {
    text: String,
    model: String,
}

#[derive(Deserialize)]
struct BulkRequest {
    items: Vec<BulkItem>,
    model: String,
}

#[derive(Deserialize)]
struct BulkItem {
    id: String,
    text: String,
}

impl Respond for MockPangram {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        match (request.method.as_str(), request.url.path()) {
            ("GET", "/") => text_response(200, "slopfilter Pangram mock\n"),
            ("GET", "/demo-feed.xml") => feed_response(),
            ("GET", "/models") => json_response(200, json!({"models": ["pangram-mock"]})),
            ("POST", "/task") => self.submit_task(request),
            ("POST", "/bulk") => self.submit_bulk(request),
            ("GET", path) if path.starts_with("/task/") => self.task_result(path),
            ("GET", path) if path.starts_with("/bulk/") && path.ends_with("/results") => {
                self.bulk_results(path)
            }
            ("GET", path) if path.starts_with("/bulk/") => self.bulk_status(path),
            _ => text_response(404, "unknown mock endpoint\n"),
        }
    }
}

impl MockPangram {
    fn submit_task(&self, request: &Request) -> ResponseTemplate {
        let request: TaskRequest = match serde_json::from_slice(&request.body) {
            Ok(request) => request,
            Err(error) => return text_response(400, &format!("invalid task request: {error}\n")),
        };
        let mut state = self.state.lock().expect("mock state is not poisoned");
        state.next_task += 1;
        let task_id = format!("mock-task-{}", state.next_task);
        state.tasks.insert(
            task_id.clone(),
            ScoredText {
                model: request.model,
                text: request.text,
            },
        );

        json_response(200, json!({"task_id": task_id}))
    }

    fn task_result(&self, path: &str) -> ResponseTemplate {
        let task_id = path.trim_start_matches("/task/");
        let state = self.state.lock().expect("mock state is not poisoned");
        let Some(task) = state.tasks.get(task_id) else {
            return text_response(404, "unknown mock task\n");
        };

        json_response(200, score(task_id, &task.model, &task.text))
    }

    fn submit_bulk(&self, request: &Request) -> ResponseTemplate {
        let request: BulkRequest = match serde_json::from_slice(&request.body) {
            Ok(request) => request,
            Err(error) => return text_response(400, &format!("invalid bulk request: {error}\n")),
        };
        if request.items.is_empty() {
            return text_response(400, "bulk requests require at least one item\n");
        }

        let mut state = self.state.lock().expect("mock state is not poisoned");
        state.next_bulk += 1;
        let bulk_id = format!("mock-bulk-{}", state.next_bulk);
        let accepted_items = request
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                json!({
                    "index": index,
                    "id": item.id,
                    "task_id": format!("{bulk_id}-task-{index}")
                })
            })
            .collect::<Vec<_>>();
        let total_items = request.items.len();
        state.bulks.insert(
            bulk_id.clone(),
            BulkJob {
                model: request.model,
                items: request.items,
            },
        );

        json_response(
            200,
            json!({
                "bulk_id": bulk_id,
                "status": "queued",
                "total_items": total_items,
                "accepted_items": accepted_items,
                "failed_items": []
            }),
        )
    }

    fn bulk_status(&self, path: &str) -> ResponseTemplate {
        let bulk_id = path.trim_start_matches("/bulk/");
        let state = self.state.lock().expect("mock state is not poisoned");
        let Some(job) = state.bulks.get(bulk_id) else {
            return text_response(404, "unknown mock bulk job\n");
        };

        json_response(
            200,
            json!({
                "bulk_id": bulk_id,
                "status": "succeeded",
                "total_items": job.items.len(),
                "accepted": job.items.len(),
                "succeeded": job.items.len(),
                "failed": 0
            }),
        )
    }

    fn bulk_results(&self, path: &str) -> ResponseTemplate {
        let bulk_id = path
            .trim_start_matches("/bulk/")
            .trim_end_matches("/results");
        let state = self.state.lock().expect("mock state is not poisoned");
        let Some(job) = state.bulks.get(bulk_id) else {
            return text_response(404, "unknown mock bulk job\n");
        };
        let items = job
            .items
            .iter()
            .enumerate()
            .rev()
            .map(|(index, item)| {
                let task_id = format!("{bulk_id}-task-{index}");
                json!({
                    "index": index,
                    "id": item.id,
                    "task_id": task_id,
                    "stage": "STAGE_SUCCESS",
                    "error": null,
                    "result": score(&task_id, &job.model, &item.text)
                })
            })
            .collect::<Vec<_>>();

        json_response(
            200,
            json!({
                "bulk_id": bulk_id,
                "offset": 0,
                "limit": 1000,
                "total_items": job.items.len(),
                "items": items,
                "failed_items": []
            }),
        )
    }
}

#[tokio::main]
async fn main() {
    let server = MockServer::start().await;
    Mock::given(|_request: &Request| true)
        .respond_with(MockPangram::default())
        .mount(&server)
        .await;

    let base_url = format!("{}/", server.uri());
    println!("Mock Pangram API listening at {base_url}");
    println!("Run this in another terminal:\n");
    println!(
        "SLOPFILTER_PANGRAM_API_BASE_URL={base_url} PANGRAM_API_KEY=mock \\\n  cargo run -- feed {base_url}demo-feed.xml --max-words 100001"
    );
    println!("\nPress Ctrl-C to stop the mock server.");
    io::stdout().flush().expect("stdout flush succeeds");

    future::pending::<()>().await;
}

fn score(task_id: &str, _model: &str, text: &str) -> Value {
    json!({
        "stage": "STAGE_SUCCESS",
        "task_id": task_id,
        "text": text,
        "version": "mock-1",
        "headline": "Human Written (mock)",
        "prediction": "Mock Pangram response; no detection was performed.",
        "prediction_short": "Human",
        "fraction_ai": 0.0,
        "fraction_ai_assisted": 0.0,
        "fraction_human": 1.0,
        "num_ai_segments": 0,
        "num_ai_assisted_segments": 0,
        "num_human_segments": 1,
        "windows": []
    })
}

fn feed_response() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/atom+xml")
        .set_body_string(demo_feed())
}

fn json_response(status: u16, body: Value) -> ResponseTemplate {
    ResponseTemplate::new(status)
        .insert_header("content-type", "application/json")
        .set_body_json(body)
}

fn text_response(status: u16, body: &str) -> ResponseTemplate {
    ResponseTemplate::new(status)
        .insert_header("content-type", "text/plain")
        .set_body_string(body)
}

fn demo_feed() -> String {
    let entries = [
        ("mock-first", "First mock article", repeated_word("first", 99_800)),
        ("mock-second", "Second mock article", repeated_word("second", 100)),
        ("mock-third", "Third mock article", repeated_word("third", 100)),
        ("mock-fourth", "Fourth mock article", "fourth".to_owned()),
    ]
    .into_iter()
    .map(|(id, title, content)| {
        format!(
            "<entry><title>{title}</title><id>{id}</id><updated>2025-01-01T00:00:00Z</updated><content type=\"text\">{content}</content></entry>"
        )
    })
    .collect::<String>();

    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><feed xmlns=\"http://www.w3.org/2005/Atom\"><title>Mock feed</title><id>mock-feed</id><updated>2025-01-01T00:00:00Z</updated>{entries}</feed>"
    )
}

fn repeated_word(word: &str, count: usize) -> String {
    let mut text = String::with_capacity((word.len() + 1) * count);
    for index in 0..count {
        if index > 0 {
            text.push(' ');
        }
        text.push_str(word);
    }
    text
}
