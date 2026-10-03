use crate::article::Article;
use reqwest::Client as HttpClient;
use serde::Deserialize;
use thiserror::Error;
use url::Url;

const API_BASE_URL: &str = "https://api.getmatter.com/public/v1";

pub(crate) struct Client {
    http: HttpClient,
    token: String,
}

pub(crate) struct QueueArticles {
    pub(crate) articles: Vec<Article>,
    pub(crate) unavailable: Vec<UnavailableItem>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct UnavailableItem {
    pub(crate) id: String,
    pub(crate) reason: String,
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
}

#[derive(Debug, PartialEq, Eq)]
enum ItemOutcome {
    Available(Article),
    Unavailable(UnavailableItem),
}

impl Client {
    pub(crate) fn new(token: String) -> Self {
        Self {
            http: HttpClient::new(),
            token,
        }
    }

    pub(crate) async fn queue_articles(&self, limit: u8) -> Result<QueueArticles, Error> {
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

        let mut articles = Vec::new();
        let mut unavailable = Vec::new();

        for summary in listed.results {
            match self.item(&summary.id).await?.into_availability()? {
                ItemOutcome::Available(article) => articles.push(article),
                ItemOutcome::Unavailable(item) => unavailable.push(item),
            }
        }

        Ok(QueueArticles {
            articles,
            unavailable,
        })
    }

    async fn item(&self, id: &str) -> Result<Item, Error> {
        Ok(self
            .http
            .get(format!("{API_BASE_URL}/items/{id}"))
            .bearer_auth(&self.token)
            .query(&[("include", "markdown")])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
}

#[derive(Deserialize)]
struct ItemList {
    results: Vec<ItemSummary>,
}

#[derive(Deserialize)]
struct ItemSummary {
    id: String,
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

        if processing_status != "completed" {
            return Ok(ItemOutcome::Unavailable(UnavailableItem {
                id,
                reason: format!("content extraction is {processing_status}"),
            }));
        }

        let Some(markdown) = markdown.filter(|markdown| !markdown.trim().is_empty()) else {
            return Ok(ItemOutcome::Unavailable(UnavailableItem {
                id,
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
    use super::{Error, Item, ItemOutcome, UnavailableItem};

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
                reason: "Matter returned no extracted Markdown".to_owned(),
            })
        );
    }
}
