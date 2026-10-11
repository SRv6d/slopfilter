//! Matter source adapter for the Items API.
use std::{
    fmt::{self, Write as _},
    num::NonZeroU32,
    sync::Arc,
    time::Duration,
};

use super::Document;
use async_stream::try_stream;
use futures_util::{Stream, StreamExt};
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use reqwest::{
    Client as HttpClient, StatusCode,
    header::{HeaderMap, RETRY_AFTER},
};
use serde::{Deserialize, Deserializer};
use thiserror::Error;
use tokio::{
    sync::Mutex,
    time::{Instant, sleep_until},
};
use url::Url;

const API_BASE_URL: &str = "https://api.getmatter.com/public/v1/";
const MAX_BURST_REQUESTS_PER_SECOND: u32 = 5;
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(10);
const MAX_RATE_LIMIT_RETRIES: u8 = 3;
const MAX_ITEMS_PER_PAGE: usize = 100;
const MAX_CONCURRENT_CONTENT_REQUESTS: usize = 4;

#[derive(Clone)]
pub(crate) struct Client {
    http: HttpClient,
    token: String,
    base_url: Url,
    content_gate: Arc<ContentRequestGate>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Status {
    Inbox,
    #[default]
    Queue,
    Archive,
}

impl fmt::Display for Status {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Inbox => "inbox",
            Self::Queue => "queue",
            Self::Archive => "archive",
        })
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct Item {
    pub(crate) id: String,
    pub(crate) title: String,
    #[serde(default, deserialize_with = "deserialize_url")]
    url: Option<Url>,
    processing_status: String,
    #[serde(default)]
    markdown: Option<String>,
}

#[derive(Debug, clap::Args)]
pub(crate) struct ListOptions {
    /// Filter items by status.
    #[arg(
        long,
        value_enum,
        default_values_t = [Status::default()],
    )]
    pub(crate) status: Vec<Status>,

    /// Filter items by their tag.
    #[arg(
        long,
        value_hint = clap::ValueHint::Other,
    )]
    pub(crate) tag: Vec<String>,

    /// The maximum number of items to score.
    #[arg(long, value_hint = clap::ValueHint::Other)]
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error("at least one Matter item status is required")]
    MissingStatus,

    #[error("Matter indicated more items without a pagination cursor")]
    MissingCursor,

    #[error("failed to fetch Matter items")]
    Fetch(#[from] reqwest::Error),
}

#[derive(Debug, Error)]
pub(crate) enum ItemError {
    #[error("Matter document is unavailable: {reason}")]
    Unavailable { reason: String },

    #[error("failed to fetch Matter item {id}")]
    Fetch {
        id: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("Matter rate limit persisted while fetching {id} after {retries} retries")]
    RateLimited { id: String, retries: u8 },
}

struct ContentRequestGate {
    burst_limiter: DefaultDirectRateLimiter,
    deferred_until: Mutex<Instant>,
}

impl ContentRequestGate {
    fn new() -> Self {
        Self {
            burst_limiter: RateLimiter::direct(Quota::per_second(
                NonZeroU32::new(MAX_BURST_REQUESTS_PER_SECOND)
                    .expect("the Matter burst limit is nonzero"),
            )),
            deferred_until: Mutex::new(Instant::now()),
        }
    }

    async fn acquire(&self) {
        loop {
            let deferred_until = *self.deferred_until.lock().await;
            sleep_until(deferred_until.max(Instant::now())).await;
            self.burst_limiter.until_ready().await;

            if *self.deferred_until.lock().await <= Instant::now() {
                return;
            }
        }
    }

    async fn defer(&self, delay: Duration) {
        let mut deferred_until = self.deferred_until.lock().await;
        *deferred_until = (*deferred_until).max(Instant::now() + delay);
    }
}

impl Client {
    pub(crate) fn new(token: String) -> Self {
        Self {
            http: HttpClient::new(),
            token,
            base_url: Url::parse(API_BASE_URL).expect("the Matter API base URL is valid"),
            content_gate: Arc::new(ContentRequestGate::new()),
        }
    }

    pub(crate) fn list_items(
        &self,
        options: ListOptions,
    ) -> impl Stream<Item = Result<Item, Error>> + '_ {
        try_stream! {
            let statuses =
                comma_separated(options.status).ok_or(Error::MissingStatus)?;
            let tags = comma_separated(options.tag);
            let mut remaining = options.limit;
            let mut cursor = None;

            while remaining != Some(0) {
                let page_limit = remaining
                    .unwrap_or(MAX_ITEMS_PER_PAGE)
                    .min(MAX_ITEMS_PER_PAGE)
                    .to_string();
                let mut request = self
                    .http
                    .get(self.endpoint("items"))
                    .bearer_auth(&self.token)
                    .query(&[
                        ("status", statuses.as_str()),
                        ("content_type", "article"),
                        ("limit", page_limit.as_str()),
                    ]);

                if let Some(tags) = tags.as_deref() {
                    request = request.query(&[("tag", tags)]);
                }
                if let Some(cursor) = cursor.as_deref() {
                    request = request.query(&[("cursor", cursor)]);
                }

                let page: ItemList = request.send().await?.error_for_status()?.json().await?;

                for item in page
                    .results
                    .into_iter()
                    .take(remaining.unwrap_or(usize::MAX))
                {
                    if let Some(remaining) = remaining.as_mut() {
                        *remaining -= 1;
                    }
                    yield item;
                }

                if remaining == Some(0) || !page.has_more {
                    break;
                }

                cursor = Some(page.next_cursor.ok_or(Error::MissingCursor)?);
            }
        }
    }

    pub(crate) fn list_items_with_markdown(
        &self,
        options: ListOptions,
    ) -> impl Stream<Item = Result<(Item, Result<(), ItemError>), Error>> + '_ {
        let client = self.clone();

        self.list_items(options)
            .map(move |listed| {
                let client = client.clone();

                async move {
                    let mut item = listed?;
                    let outcome = client.include_markdown(&mut item).await;
                    Ok::<_, Error>((item, outcome))
                }
            })
            .buffered(MAX_CONCURRENT_CONTENT_REQUESTS)
    }

    async fn include_markdown(&self, item: &mut Item) -> Result<(), ItemError> {
        item.ensure_processed()?;
        *item = self.item(&item.id).await?;
        item.ensure_markdown_available()
    }

    async fn item(&self, id: &str) -> Result<Item, ItemError> {
        for attempt in 0..=MAX_RATE_LIMIT_RETRIES {
            self.content_gate.acquire().await;

            let response = self
                .http
                .get(self.endpoint(&format!("items/{id}")))
                .bearer_auth(&self.token)
                .query(&[("include", "markdown")])
                .send()
                .await
                .map_err(|source| ItemError::Fetch {
                    id: id.to_owned(),
                    source,
                })?;

            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                self.content_gate
                    .defer(retry_after(response.headers()))
                    .await;

                if attempt == MAX_RATE_LIMIT_RETRIES {
                    return Err(ItemError::RateLimited {
                        id: id.to_owned(),
                        retries: MAX_RATE_LIMIT_RETRIES,
                    });
                }

                continue;
            }

            let response = response
                .error_for_status()
                .map_err(|source| ItemError::Fetch {
                    id: id.to_owned(),
                    source,
                })?;
            return response.json().await.map_err(|source| ItemError::Fetch {
                id: id.to_owned(),
                source,
            });
        }

        unreachable!("a bounded retry loop always returns")
    }

    fn endpoint(&self, path: &str) -> Url {
        self.base_url
            .join(path)
            .expect("Matter API paths are valid URL references")
    }
}

fn comma_separated<T: fmt::Display>(values: impl IntoIterator<Item = T>) -> Option<String> {
    let mut values = values.into_iter();
    let mut filter = values.next()?.to_string();

    for value in values {
        write!(&mut filter, ",{value}").unwrap();
    }

    Some(filter)
}

fn retry_after(headers: &HeaderMap) -> Duration {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_RETRY_AFTER)
}

fn deserialize_url<'de, D>(deserializer: D) -> Result<Option<Url>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(value.and_then(|value| Url::parse(&value).ok()))
}

#[derive(Deserialize)]
struct ItemList {
    results: Vec<Item>,
    has_more: bool,
    next_cursor: Option<String>,
}

impl Item {
    fn ensure_processed(&self) -> Result<(), ItemError> {
        if self.processing_status != "completed" {
            return Err(ItemError::Unavailable {
                reason: format!("content extraction is {}", self.processing_status),
            });
        }

        Ok(())
    }

    fn ensure_markdown_available(&self) -> Result<(), ItemError> {
        self.ensure_processed()?;

        if self
            .markdown
            .as_ref()
            .is_none_or(|markdown| markdown.trim().is_empty())
        {
            return Err(ItemError::Unavailable {
                reason: "Matter returned no extracted Markdown".to_owned(),
            });
        }

        Ok(())
    }

    pub(crate) fn into_document(self) -> Result<(String, Document), ItemError> {
        let Self {
            id,
            title,
            url,
            processing_status,
            markdown,
        } = self;
        let title = title.trim().to_owned();

        if processing_status != "completed" {
            return Err(ItemError::Unavailable {
                reason: format!("content extraction is {processing_status}"),
            });
        }

        let Some(markdown) = markdown.filter(|markdown| !markdown.trim().is_empty()) else {
            return Err(ItemError::Unavailable {
                reason: "Matter returned no extracted Markdown".to_owned(),
            });
        };

        Ok((
            id,
            Document {
                title: Some(title),
                url,
                text: markdown,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::Arc,
        thread,
        time::Duration,
    };

    use futures_util::{StreamExt, pin_mut};
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
    use url::Url;

    use super::{
        Client, ContentRequestGate, DEFAULT_RETRY_AFTER, Item, ItemError, ListOptions, Status,
        comma_separated, retry_after,
    };

    #[test]
    fn filter_values_are_serialized_for_api_queries() {
        assert_eq!(
            comma_separated([Status::Inbox, Status::Queue, Status::Archive]).as_deref(),
            Some("inbox,queue,archive")
        );
        assert_eq!(
            comma_separated(["tag_one", "tag_two"]).as_deref(),
            Some("tag_one,tag_two")
        );
        assert_eq!(comma_separated::<Status>([]).as_deref(), None);
    }

    #[tokio::test]
    async fn list_items_follows_pagination_until_the_requested_limit() {
        let (base_url, server) = pagination_server(["limit=2", "limit=1"]);
        let client = Client {
            http: reqwest::Client::new(),
            token: "secret-token".to_owned(),
            base_url,
            content_gate: Arc::new(ContentRequestGate::new()),
        };
        let items = client.list_items(ListOptions {
            status: vec![Status::Queue],
            tag: Vec::new(),
            limit: Some(2),
        });
        pin_mut!(items);

        let first = items.next().await.unwrap().unwrap();
        let second = items.next().await.unwrap().unwrap();

        assert_eq!(first.id, "itm_first");
        assert_eq!(second.id, "itm_second");
        assert!(items.next().await.is_none());
        server.join().unwrap();
    }

    #[tokio::test]
    async fn list_items_without_a_limit_exhausts_all_pages() {
        let (base_url, server) = pagination_server(["limit=100", "limit=100"]);
        let client = Client {
            http: reqwest::Client::new(),
            token: "secret-token".to_owned(),
            base_url,
            content_gate: Arc::new(ContentRequestGate::new()),
        };
        let items = client.list_items(ListOptions {
            status: vec![Status::Queue],
            tag: Vec::new(),
            limit: None,
        });
        pin_mut!(items);

        assert_eq!(items.next().await.unwrap().unwrap().id, "itm_first");
        assert_eq!(items.next().await.unwrap().unwrap().id, "itm_second");
        assert!(items.next().await.is_none());
        server.join().unwrap();
    }

    #[tokio::test]
    async fn zero_limit_makes_no_request() {
        let client = Client::new("secret-token".to_owned());
        let items = client.list_items(ListOptions {
            status: vec![Status::Queue],
            tag: Vec::new(),
            limit: Some(0),
        });
        pin_mut!(items);

        assert!(items.next().await.is_none());
    }

    #[tokio::test]
    async fn content_fetch_failure_remains_item_scoped() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).starts_with("GET /items?"));
            write_response(
                &mut stream,
                r#"{
                    "results": [{
                        "id": "itm_first",
                        "title": "First",
                        "url": "https://example.com/first",
                        "processing_status": "completed"
                    }],
                    "has_more": false,
                    "next_cursor": null
                }"#,
            );

            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request(&mut stream).starts_with("GET /items/itm_first?include=markdown"));
            write_status_response(&mut stream, "500 Internal Server Error", "{}");
        });
        let client = Client {
            http: reqwest::Client::new(),
            token: "secret-token".to_owned(),
            base_url: Url::parse(&format!("http://{address}/")).unwrap(),
            content_gate: Arc::new(ContentRequestGate::new()),
        };
        let items = client.list_items_with_markdown(ListOptions {
            status: vec![Status::Queue],
            tag: Vec::new(),
            limit: None,
        });
        pin_mut!(items);

        let (item, outcome) = items.next().await.unwrap().unwrap();

        assert_eq!(item.id, "itm_first");
        assert!(matches!(
            outcome,
            Err(ItemError::Fetch { id, .. }) if id == "itm_first"
        ));
        assert!(items.next().await.is_none());
        server.join().unwrap();
    }

    fn pagination_server(expected_limits: [&'static str; 2]) -> (Url, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let responses = [
                r#"{
                    "results": [{
                        "id": "itm_first",
                        "title": "First",
                        "url": "https://example.com/first",
                        "processing_status": "completed"
                    }],
                    "has_more": true,
                    "next_cursor": "next"
                }"#,
                r#"{
                    "results": [{
                        "id": "itm_second",
                        "title": "Second",
                        "url": "https://example.com/second",
                        "processing_status": "completed"
                    }],
                    "has_more": false,
                    "next_cursor": null
                }"#,
            ];

            for (index, response) in responses.into_iter().enumerate() {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                assert!(request.starts_with("GET /items?"));
                assert!(request.contains(expected_limits[index]));
                if index == 1 {
                    assert!(request.contains("cursor=next"));
                }
                write_response(&mut stream, response);
            }
        });

        (Url::parse(&format!("http://{address}/")).unwrap(), server)
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0; 4096];

        loop {
            let bytes_read = stream.read(&mut buffer).unwrap();
            assert!(bytes_read > 0, "connection closed before request completed");
            request.extend_from_slice(&buffer[..bytes_read]);

            if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                return String::from_utf8(request).unwrap();
            }
        }
    }

    fn write_response(stream: &mut TcpStream, body: &str) {
        write_status_response(stream, "200 OK", body);
    }

    fn write_status_response(stream: &mut TcpStream, status: &str, body: &str) {
        write!(
            stream,
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    }

    #[test]
    fn listed_item_can_omit_markdown() {
        let item: Item = serde_json::from_str(
            r#"{
                "id": "itm_123",
                "title": "A saved article",
                "url": "https://example.com/article",
                "processing_status": "completed"
            }"#,
        )
        .unwrap();

        assert!(item.markdown.is_none());
    }

    #[test]
    fn completed_item_maps_to_a_document() {
        let item: Item = serde_json::from_str(
            r##"{
                "id": "itm_123",
                "title": "  A saved article  ",
                "url": "https://example.com/article",
                "processing_status": "completed",
                "markdown": "# Heading\n\nArticle body"
            }"##,
        )
        .unwrap();

        let (_, document) = item.into_document().unwrap();

        assert_eq!(document.title.as_deref(), Some("A saved article"));
        assert_eq!(
            document.url.as_ref().map(url::Url::as_str),
            Some("https://example.com/article")
        );
        assert_eq!(document.text, "# Heading\n\nArticle body");
        assert_eq!(document.word_count(), 4);
    }

    #[test]
    fn item_with_an_invalid_url_keeps_its_document() {
        let item: Item = serde_json::from_str(
            r#"{
                "id": "itm_123",
                "title": "A saved article",
                "url": "not a URL",
                "processing_status": "completed",
                "markdown": "Article body"
            }"#,
        )
        .unwrap();

        let (_, document) = item.into_document().unwrap();

        assert_eq!(document.url, None);
        assert_eq!(document.text, "Article body");
    }

    #[test]
    fn item_without_a_url_keeps_its_document() {
        let item: Item = serde_json::from_str(
            r#"{
                "id": "itm_123",
                "title": "A saved article",
                "processing_status": "completed",
                "markdown": "Article body"
            }"#,
        )
        .unwrap();

        let (_, document) = item.into_document().unwrap();

        assert_eq!(document.url, None);
        assert_eq!(document.text, "Article body");
    }

    #[test]
    fn processing_item_is_unavailable() {
        let item: Item = serde_json::from_str(
            r#"{
                "id": "itm_123",
                "title": "A saved article",
                "url": "https://example.com/article",
                "processing_status": "processing",
                "markdown": null
            }"#,
        )
        .unwrap();

        assert!(matches!(
            item.into_document(),
            Err(ItemError::Unavailable { reason }) if reason == "content extraction is processing"
        ));
    }

    #[test]
    fn completed_item_without_markdown_is_unavailable() {
        let item: Item = serde_json::from_str(
            r#"{
                "id": "itm_123",
                "title": "A saved article",
                "url": "https://example.com/article",
                "processing_status": "completed",
                "markdown": "   "
            }"#,
        )
        .unwrap();

        assert!(matches!(
            item.into_document(),
            Err(ItemError::Unavailable { reason })
                if reason == "Matter returned no extracted Markdown"
        ));
    }

    #[test]
    fn retry_after_uses_the_server_delay() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("12"));

        assert_eq!(retry_after(&headers), Duration::from_secs(12));
    }

    #[test]
    fn retry_after_defaults_to_the_documented_fallback() {
        assert_eq!(retry_after(&HeaderMap::new()), DEFAULT_RETRY_AFTER);
    }
}
