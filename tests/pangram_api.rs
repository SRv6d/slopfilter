mod support;

use std::{
    io::Write as _,
    process::{Command, Output, Stdio},
};

use support::pangram::{Response, ScriptedServer, assert_api_key, assert_request};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const API_KEY: &str = "secret-key";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdin_uses_the_realtime_api() {
    let pangram = ScriptedServer::start([
        Response::ok(r#"{"models":["pangram-4"]}"#),
        Response::ok(r#"{"task_id":"task-123"}"#),
        Response::ok(score_response("# An article")),
    ])
    .await;

    let output = run_with_stdin(
        &["stdin", "--pangram-api-key", API_KEY],
        "# An article",
        &pangram,
    );
    let requests = pangram.finish().await;

    assert!(output.status.success(), "{}", output_text(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Human Written"));
    assert_api_key(&requests, API_KEY);
    assert_request(&requests[0], "GET", "/models");
    assert_request(&requests[1], "POST", "/task");
    assert_request(&requests[2], "GET", "/task/task-123");
    let submission: serde_json::Value = requests[1].body_json().unwrap();
    assert_eq!(submission["text"], "# An article");
    assert_eq!(submission["model"], "pangram-4");
    assert_eq!(submission["public_dashboard_link"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn feed_runs_sequential_bulk_jobs_and_maps_item_results_in_source_order() {
    let feed = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/feed.xml"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/atom+xml")
                .set_body_string(feed_document()),
        )
        .expect(1)
        .mount(&feed)
        .await;

    let pangram = ScriptedServer::start(bulk_responses()).await;
    let feed_url = format!("{}/feed.xml", feed.uri());
    let api_url = pangram.base_url().to_string();
    let output = Command::new(env!("CARGO_BIN_EXE_slopfilter"))
        .args([
            "feed",
            &feed_url,
            "--max-words",
            "100001",
            "--pangram-api-key",
            API_KEY,
        ])
        .env("SLOPFILTER_PANGRAM_API_BASE_URL", api_url)
        .env("NO_COLOR", "1")
        .output()
        .expect("slopfilter runs");
    let requests = pangram.finish().await;

    assert!(!output.status.success(), "the fourth item should fail");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let diagnostic = output_text(&output);

    let first = stdout.find("First").expect(&diagnostic);
    let second = stdout.find("Second").expect(&diagnostic);
    let third = stdout.find("Third").expect(&diagnostic);
    assert!(first < second && second < third, "{diagnostic}");
    assert!(
        stderr.contains("Failed: Fourth · Feed: item-fourth"),
        "{diagnostic}"
    );
    assert!(
        stderr.contains("failed to process 1 source item"),
        "{diagnostic}"
    );

    let first_awaiting_submission = stderr.find("Awaiting submission:").expect(&diagnostic);
    for title in ["First", "Second", "Third", "Fourth"] {
        let pending = stderr
            .find(&format!("Pending: {title}"))
            .unwrap_or_else(|| panic!("missing pending state for {title}\n{diagnostic}"));
        assert!(
            pending < first_awaiting_submission,
            "{title} was not displayed before any item awaited submission\n{diagnostic}"
        );
    }

    assert_api_key(&requests, API_KEY);
    let expected = [
        ("GET", "/models"),
        ("POST", "/bulk"),
        ("GET", "/bulk/bulk-first"),
        ("GET", "/bulk/bulk-first/results"),
        ("POST", "/bulk"),
        ("GET", "/bulk/bulk-second"),
        ("GET", "/bulk/bulk-second/results"),
    ];
    for (request, (method, path)) in requests.iter().zip(expected) {
        assert_request(request, method, path);
    }

    let first_job: serde_json::Value = requests[1].body_json().unwrap();
    let second_job: serde_json::Value = requests[4].body_json().unwrap();
    assert_eq!(
        first_job["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["item-first", "item-second", "item-third"]
    );
    assert_eq!(second_job["items"][0]["id"], "item-fourth");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_bulk_submission_does_not_start_the_next_job() {
    let feed = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/feed.xml"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/atom+xml")
                .set_body_string(feed_document()),
        )
        .expect(1)
        .mount(&feed)
        .await;

    let pangram = ScriptedServer::start([
        Response::ok(r#"{"models":["pangram-4"]}"#),
        Response::json(500, r#"{"detail":"submission outcome is unknown"}"#),
    ])
    .await;
    let feed_url = format!("{}/feed.xml", feed.uri());
    let output = Command::new(env!("CARGO_BIN_EXE_slopfilter"))
        .args([
            "feed",
            &feed_url,
            "--max-words",
            "100001",
            "--pangram-api-key",
            API_KEY,
        ])
        .env(
            "SLOPFILTER_PANGRAM_API_BASE_URL",
            pangram.base_url().as_str(),
        )
        .env("NO_COLOR", "1")
        .output()
        .expect("slopfilter runs");
    let requests = pangram.finish().await;

    assert!(!output.status.success(), "{}", output_text(&output));
    assert_request(&requests[0], "GET", "/models");
    assert_request(&requests[1], "POST", "/bulk");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("Bulk job 2:"), "{}", output_text(&output));
}

fn run_with_stdin(args: &[&str], input: &str, pangram: &ScriptedServer) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_slopfilter"))
        .args(args)
        .env(
            "SLOPFILTER_PANGRAM_API_BASE_URL",
            pangram.base_url().as_str(),
        )
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("slopfilter starts");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(input.as_bytes())
        .expect("test input is written");
    child.wait_with_output().expect("slopfilter exits")
}

fn output_text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn feed_document() -> String {
    let entries = [
        ("item-first", "First", repeated_word("first", 99_800)),
        ("item-second", "Second", repeated_word("second", 100)),
        ("item-third", "Third", repeated_word("third", 100)),
        ("item-fourth", "Fourth", "fourth".to_owned()),
    ];
    let entries = entries
        .into_iter()
        .map(|(id, title, content)| {
            format!(
                "<entry><title>{title}</title><id>{id}</id><updated>2025-01-01T00:00:00Z</updated><content type=\"text\">{content}</content></entry>"
            )
        })
        .collect::<String>();

    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><feed xmlns=\"http://www.w3.org/2005/Atom\"><title>Test feed</title><id>test-feed</id><updated>2025-01-01T00:00:00Z</updated>{entries}</feed>"
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

fn bulk_responses() -> Vec<Response> {
    vec![
        Response::ok(r#"{"models":["pangram-4"]}"#),
        Response::ok(
            r#"{
                "bulk_id":"bulk-first",
                "status":"queued",
                "total_items":3,
                "accepted_items":[
                    {"index":0,"id":"item-first","task_id":"task-first"},
                    {"index":1,"id":"item-second","task_id":"task-second"},
                    {"index":2,"id":"item-third","task_id":"task-third"}
                ],
                "failed_items":[]
            }"#,
        ),
        Response::ok(
            r#"{
                "bulk_id":"bulk-first",
                "status":"succeeded",
                "total_items":3,
                "accepted":3,
                "succeeded":3,
                "failed":0
            }"#,
        ),
        Response::ok(format!(
            r#"{{
                "bulk_id":"bulk-first",
                "offset":0,
                "limit":1000,
                "total_items":3,
                "items":[
                    {{"index":2,"id":"item-third","task_id":"task-third","stage":"STAGE_SUCCESS","error":null,"result":{}}},
                    {{"index":0,"id":"item-first","task_id":"task-first","stage":"STAGE_SUCCESS","error":null,"result":{}}},
                    {{"index":1,"id":"item-second","task_id":"task-second","stage":"STAGE_SUCCESS","error":null,"result":{}}}
                ],
                "failed_items":[]
            }}"#,
            score_response("Third"),
            score_response("First"),
            score_response("Second")
        )),
        Response::ok(
            r#"{
                "bulk_id":"bulk-second",
                "status":"queued",
                "total_items":1,
                "accepted_items":[
                    {"index":0,"id":"item-fourth","task_id":"task-fourth"}
                ],
                "failed_items":[]
            }"#,
        ),
        Response::ok(
            r#"{
                "bulk_id":"bulk-second",
                "status":"failed",
                "total_items":1,
                "accepted":1,
                "succeeded":0,
                "failed":1
            }"#,
        ),
        Response::ok(
            r#"{
                "bulk_id":"bulk-second",
                "offset":0,
                "limit":1000,
                "total_items":1,
                "items":[],
                "failed_items":[{
                    "index":0,
                    "id":"item-fourth",
                    "task_id":"task-fourth",
                    "stage":"STAGE_FAILED",
                    "error":"invalid text"
                }]
            }"#,
        ),
    ]
}

fn score_response(text: &str) -> String {
    serde_json::json!({
        "stage": "STAGE_SUCCESS",
        "text": text,
        "version": "4.0",
        "headline": "Human Written",
        "prediction": "This text appears human-written.",
        "prediction_short": "Human",
        "fraction_ai": 0.0,
        "fraction_ai_assisted": 0.0,
        "fraction_human": 1.0,
        "num_ai_segments": 0,
        "num_ai_assisted_segments": 0,
        "num_human_segments": 1,
        "windows": []
    })
    .to_string()
}
