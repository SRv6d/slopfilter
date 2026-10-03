use crate::article::Article;
use reqwest::Client as HttpClient;
use serde::Deserialize;
use url::Url;

const API_BASE_URL: &str = "https://api.getmatter.com/public/v1";

pub(crate) struct Client {
    http: HttpClient,
    token: String,
}

pub(crate) struct QueueArticles {
    pub(crate) articles: Vec<Article>,
    pub(crate) skipped: Vec<SkippedItem>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SkippedItem {
    pub(crate) id: String,
    pub(crate) reason: String,
}

impl Client {
    pub(crate) fn new(token: String) -> Self {
        Self {
            http: HttpClient::new(),
            token,
        }
    }

    pub(crate) async fn queue_articles(&self, limit: u8) -> Result<QueueArticles, reqwest::Error> {
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
        let mut skipped = Vec::new();

        for summary in listed.results {
            match self.item(&summary.id).await?.into_article() {
                Ok(article) => articles.push(article),
                Err(item) => skipped.push(item),
            }
        }

        Ok(QueueArticles { articles, skipped })
    }

    async fn item(&self, id: &str) -> Result<Item, reqwest::Error> {
        self.http
            .get(format!("{API_BASE_URL}/items/{id}"))
            .bearer_auth(&self.token)
            .query(&[("include", "markdown")])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
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
    fn into_article(self) -> Result<Article, SkippedItem> {
        let Self {
            id,
            title,
            url,
            processing_status,
            markdown,
        } = self;

        if processing_status != "completed" {
            return Err(SkippedItem {
                id,
                reason: format!("content extraction is {processing_status}"),
            });
        }

        let url = match Url::parse(&url) {
            Ok(url) => url,
            Err(_) => {
                return Err(SkippedItem {
                    id,
                    reason: "Matter returned an invalid URL".to_owned(),
                });
            }
        };

        let Some(markdown) = markdown.filter(|markdown| !markdown.trim().is_empty()) else {
            return Err(SkippedItem {
                id,
                reason: "Matter returned no extracted Markdown".to_owned(),
            });
        };

        Ok(Article {
            source_id: id,
            title: title.trim().to_owned(),
            url,
            markdown,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Item;

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

        let article = item.into_article().unwrap();

        assert_eq!(article.source_id, "itm_123");
        assert_eq!(article.title, "A saved article");
        assert_eq!(article.url.as_str(), "https://example.com/article");

        assert_eq!(article.word_count(), 4);
    }

    #[test]
    fn item_with_an_invalid_url_is_skipped() {
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

        assert_eq!(
            item.into_article().unwrap_err(),
            super::SkippedItem {
                id: "itm_123".to_owned(),
                reason: "Matter returned an invalid URL".to_owned(),
            }
        );
    }

    #[test]
    fn item_without_completed_markdown_is_skipped() {
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
            item.into_article().unwrap_err(),
            super::SkippedItem {
                id: "itm_123".to_owned(),
                reason: "content extraction is processing".to_owned(),
            }
        );
    }
}
