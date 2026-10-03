//! Matter source adapter for the Items API.
use std::{num::NonZeroU32, sync::Arc, time::Duration};

use crate::article::Article;
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

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct UnavailableItem {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) reason: String,
}

#[derive(Debug)]
pub(crate) struct QueuedArticle {
    pub(crate) id: String,
    pub(crate) title: String,
}

#[derive(Debug, Error)]
pub(crate) enum Error {
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

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ItemOutcome {
    Available(Article),
    Unavailable(UnavailableItem),
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

    pub(crate) async fn queued_articles(&self, limit: u8) -> Result<Vec<QueuedArticle>, Error> {
        let limit = limit.to_string();
        let listed: ItemList = self
            .http
            .get(format!("{API_BASE_URL}/items"))
            .bearer_auth(&self.token)
            .query(&[
                ("status", "queue"),
                ("content_type", "article"),
                ("limit", &limit),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(listed
            .results
            .into_iter()
            .map(|item| QueuedArticle {
                id: item.id,
                title: item.title.trim().to_owned(),
            })
            .collect())
    }

    pub(crate) async fn fetch_article(&self, id: &str) -> Result<ItemOutcome, Error> {
        self.item(id).await?.into_availability()
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
    fn into_availability(self) -> Result<ItemOutcome, Error> {
        let Self {
            id,
            title,
            url,
            processing_status,
            markdown,
        } = self;
        let url = Url::parse(&url).map_err(|source| Error::InvalidUrl {
            id: id.clone(),
            source,
        })?;
        let title = title.trim().to_owned();

        if processing_status != "completed" {
            return Ok(ItemOutcome::Unavailable(UnavailableItem {
                id,
                title: title.clone(),
                reason: format!("content extraction is {processing_status}"),
            }));
        }

        let Some(markdown) = markdown.filter(|markdown| !markdown.trim().is_empty()) else {
            return Ok(ItemOutcome::Unavailable(UnavailableItem {
                id,
                title: title.clone(),
                reason: "Matter returned no extracted Markdown".to_owned(),
            }));
        };

        Ok(ItemOutcome::Available(Article {
            source_id: id,
            title: title.trim().to_owned(),
            url,
            markdown,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_RETRY_AFTER, Error, Item, ItemOutcome, UnavailableItem, retry_after};
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
    use std::time::Duration;

    #[test]
    fn completed_item_maps_to_an_article() {
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

        let ItemOutcome::Available(article) = item.into_availability().unwrap() else {
            panic!("expected an available article");
        };

        assert_eq!(article.source_id, "itm_123");
        assert_eq!(article.title, "A saved article");
        assert_eq!(article.url.as_str(), "https://example.com/article");
        assert_eq!(article.word_count(), 4);
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
            item.into_availability(),
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

        assert_eq!(
            item.into_availability().unwrap(),
            ItemOutcome::Unavailable(UnavailableItem {
                id: "itm_123".to_owned(),
                title: "A saved article".to_owned(),
                reason: "content extraction is processing".to_owned(),
            })
        );
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

        assert_eq!(
            item.into_availability().unwrap(),
            ItemOutcome::Unavailable(UnavailableItem {
                id: "itm_123".to_owned(),
                title: "A saved article".to_owned(),
                reason: "Matter returned no extracted Markdown".to_owned(),
            })
        );
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
