//! Matter source adapter for the Items API.
use std::{
    fmt::{self, Write as _},
    num::NonZeroU32,
    sync::Arc,
    time::Duration,
};

use super::Document;
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use reqwest::{
    Client as HttpClient, StatusCode,
    header::{HeaderMap, RETRY_AFTER},
};
use serde::Deserialize;
use thiserror::Error;
use tokio::{
    sync::Mutex,
    time::{Instant, sleep_until},
};
use url::Url;

const API_BASE_URL: &str = "https://api.getmatter.com/public/v1";
const MAX_BURST_REQUESTS_PER_SECOND: u32 = 5;
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(10);
const MAX_RATE_LIMIT_RETRIES: u8 = 3;

#[derive(Clone)]
pub(crate) struct Client {
    http: HttpClient,
    token: String,
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

#[derive(Debug)]
pub(crate) struct Article {
    pub(crate) id: String,
    pub(crate) title: String,
}

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error("Matter document is unavailable: {reason}")]
    Unavailable { reason: String },

    #[error("at least one Matter item status is required")]
    MissingStatus,

    #[error("Matter API request failed")]
    Request(#[from] reqwest::Error),

    #[error("Matter item {id} has an invalid URL")]
    InvalidUrl {
        id: String,
        #[source]
        source: url::ParseError,
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
            content_gate: Arc::new(ContentRequestGate::new()),
        }
    }

    pub(crate) async fn articles(
        &self,
        statuses: impl IntoIterator<Item = Status>,
        tags: impl IntoIterator<Item = String>,
        limit: usize,
    ) -> Result<Vec<Article>, Error> {
        let statuses = comma_separated(statuses).ok_or(Error::MissingStatus)?;
        let tags = comma_separated(tags);
        let limit = limit.to_string();
        let request = self
            .http
            .get(format!("{API_BASE_URL}/items"))
            .bearer_auth(&self.token)
            .query(&[
                ("status", statuses.as_str()),
                ("content_type", "article"),
                ("limit", limit.as_str()),
            ]);
        let request = match tags.as_deref() {
            Some(tags) => request.query(&[("tag", tags)]),
            None => request,
        };
        let listed: ItemList = request.send().await?.error_for_status()?.json().await?;

        Ok(listed
            .results
            .into_iter()
            .map(|item| Article {
                id: item.id,
                title: item.title.trim().to_owned(),
            })
            .collect())
    }

    pub(crate) async fn fetch_document(&self, id: &str) -> Result<Document, Error> {
        self.item(id).await?.into_document()
    }

    async fn item(&self, id: &str) -> Result<Item, Error> {
        for attempt in 0..=MAX_RATE_LIMIT_RETRIES {
            self.content_gate.acquire().await;

            let response = self
                .http
                .get(format!("{API_BASE_URL}/items/{id}"))
                .bearer_auth(&self.token)
                .query(&[("include", "markdown")])
                .send()
                .await?;

            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                self.content_gate
                    .defer(retry_after(response.headers()))
                    .await;

                if attempt == MAX_RATE_LIMIT_RETRIES {
                    return Err(Error::RateLimited {
                        id: id.to_owned(),
                        retries: MAX_RATE_LIMIT_RETRIES,
                    });
                }

                continue;
            }

            return Ok(response.error_for_status()?.json().await?);
        }

        unreachable!("a bounded retry loop always returns")
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

#[derive(Deserialize)]
struct ItemList {
    results: Vec<ItemSummary>,
}

#[derive(Deserialize)]
struct ItemSummary {
    id: String,
    title: String,
}

#[derive(Deserialize)]
struct Item {
    id: String,
    title: String,
    url: String,
    processing_status: String,
    markdown: Option<String>,
}

impl Item {
    fn into_document(self) -> Result<Document, Error> {
        let Self {
            id,
            title,
            url,
            processing_status,
            markdown,
        } = self;
        let url = Url::parse(&url).map_err(|source| Error::InvalidUrl { id, source })?;
        let title = title.trim().to_owned();

        if processing_status != "completed" {
            return Err(Error::Unavailable {
                reason: format!("content extraction is {processing_status}"),
            });
        }

        let Some(markdown) = markdown.filter(|markdown| !markdown.trim().is_empty()) else {
            return Err(Error::Unavailable {
                reason: "Matter returned no extracted Markdown".to_owned(),
            });
        };

        Ok(Document {
            title: Some(title),
            url: Some(url),
            text: markdown,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_RETRY_AFTER, Error, Item, Status, comma_separated, retry_after};
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
    use std::time::Duration;

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

        let document = item.into_document().unwrap();

        assert_eq!(document.title.as_deref(), Some("A saved article"));
        assert_eq!(
            document.url.as_ref().map(url::Url::as_str),
            Some("https://example.com/article")
        );
        assert_eq!(document.text, "# Heading\n\nArticle body");
        assert_eq!(document.word_count(), 4);
    }

    #[test]
    fn item_with_an_invalid_url_fails() {
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

        assert!(matches!(
            item.into_document(),
            Err(Error::InvalidUrl { id, .. }) if id == "itm_123"
        ));
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
            Err(Error::Unavailable { reason }) if reason == "content extraction is processing"
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
            Err(Error::Unavailable { reason })
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
