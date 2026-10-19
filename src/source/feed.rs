//! Atom and RSS source adapter.

use std::time::Duration;

use feed_rs::{
    model::{Entry as ParsedEntry, Text},
    parser,
};
use reqwest::Client as HttpClient;
use thiserror::Error;
use url::Url;

use super::Document;

const HTML_TEXT_WIDTH: usize = 120;
/// Bounds the feed request through the end of its response body.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct Client {
    http: HttpClient,
}

/// Display metadata for a feed entry, kept alongside its document outcome.
#[derive(Debug)]
pub(crate) struct Item {
    pub(crate) id: String,
    pub(crate) title: Option<String>,
}

/// A feed entry with its document, or the item-scoped reason it has none.
type EntryDocument = (Item, Result<Document, ItemError>);

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error("failed to fetch feed")]
    Fetch(#[from] reqwest::Error),

    #[error("feed is invalid")]
    InvalidFeed(#[from] parser::ParseFeedError),
}

#[derive(Debug, Error)]
pub(crate) enum ItemError {
    #[error("no inline full-text content; summaries are not scored")]
    Unavailable,

    #[error("content could not be normalized")]
    InvalidContent(#[source] html2text::Error),
}

impl Client {
    pub(crate) fn new() -> Self {
        Self {
            http: HttpClient::new(),
        }
    }

    pub(crate) async fn fetch_documents(&self, feed_url: Url) -> Result<Vec<EntryDocument>, Error> {
        let bytes = self
            .http
            .get(feed_url.clone())
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;

        parse_documents(&feed_url, &bytes)
    }
}

fn parse_documents(feed_url: &Url, bytes: &[u8]) -> Result<Vec<EntryDocument>, Error> {
    let feed = parser::Builder::new()
        .base_uri(Some(feed_url.as_str()))
        .build()
        .parse(bytes)?;

    Ok(feed
        .entries
        .into_iter()
        .map(|entry| entry_document(feed_url, entry))
        .collect())
}

fn entry_document(feed_url: &Url, entry: ParsedEntry) -> EntryDocument {
    // Titles are display metadata: one that cannot be normalized is absent, like a malformed URL.
    let title = entry
        .title
        .as_ref()
        .and_then(|title| display_text(title).ok().flatten());
    let document = match item_text(&entry) {
        Ok(Some(text)) => Ok(Document {
            title: title.clone(),
            url: item_url(feed_url, &entry),
            text,
        }),
        Ok(None) => Err(ItemError::Unavailable),
        Err(source) => Err(ItemError::InvalidContent(source)),
    };

    (
        Item {
            id: entry.id,
            title,
        },
        document,
    )
}

fn item_url(feed_url: &Url, entry: &ParsedEntry) -> Option<Url> {
    entry
        .links
        .iter()
        .filter(|link| link.rel.as_deref() == Some("alternate"))
        .chain(entry.links.iter().filter(|link| link.rel.is_none()))
        .find_map(|link| {
            let href = link.href.trim();
            if href.is_empty() {
                None
            } else {
                feed_url.join(href).ok()
            }
        })
}

/// Scores only full inline content; summaries are too short to score reliably.
fn item_text(entry: &ParsedEntry) -> Result<Option<String>, html2text::Error> {
    let Some(content) = entry.content.as_ref() else {
        return Ok(None);
    };
    let Some(body) = content.body.as_deref() else {
        return Ok(None);
    };

    scoreable_text(body, content.content_type.as_str())
}

fn display_text(text: &Text) -> Result<Option<String>, html2text::Error> {
    let text = scoreable_text(&text.content, text.content_type.as_str())?;

    Ok(text.map(|text| collapse_whitespace(&text)))
}

fn scoreable_text(content: &str, content_type: &str) -> Result<Option<String>, html2text::Error> {
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim();
    let rendered = match media_type {
        "text/html" | "application/xhtml+xml" => html2text::config::plain_no_decorate()
            .link_footnotes(false)
            .string_from_read(content.as_bytes(), HTML_TEXT_WIDTH)?,
        media_type if media_type.starts_with("text/") => content.to_owned(),
        _ => return Ok(None),
    };
    let rendered = rendered.trim();

    Ok((!rendered.is_empty()).then(|| rendered.to_owned()))
}

fn collapse_whitespace(text: &str) -> String {
    let mut words = text.split_whitespace();
    let Some(first) = words.next() else {
        return String::new();
    };
    let mut collapsed = String::with_capacity(text.len());
    collapsed.push_str(first);

    for word in words {
        collapsed.push(' ');
        collapsed.push_str(word);
    }

    collapsed
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED_URL: &str = "https://example.com/feeds/main.xml";

    #[test]
    fn atom_entries_map_in_feed_order() {
        let documents = parse_documents(
            &Url::parse(FEED_URL).unwrap(),
            br#"<?xml version="1.0" encoding="utf-8"?>
                <feed xmlns="http://www.w3.org/2005/Atom">
                  <title>Example</title>
                  <id>https://example.com/</id>
                  <updated>2025-01-01T00:00:00Z</updated>
                  <entry>
                    <title type="html">&lt;strong&gt;First&lt;/strong&gt;</title>
                    <id>atom-first</id>
                    <updated>2025-01-02T00:00:00Z</updated>
                    <link rel="alternate" href="/articles/first" />
                    <content type="html">&lt;h1&gt;Heading&lt;/h1&gt;&lt;p&gt;Full &lt;strong&gt;article&lt;/strong&gt;.&lt;/p&gt;</content>
                  </entry>
                  <entry>
                    <title>Second</title>
                    <id>atom-second</id>
                    <updated>2025-01-01T00:00:00Z</updated>
                    <link href="https://example.com/articles/second" />
                    <summary>Summary only</summary>
                  </entry>
                </feed>"#,
        )
        .unwrap();

        assert_eq!(
            documents
                .iter()
                .map(|(item, _)| item.id.as_str())
                .collect::<Vec<_>>(),
            ["atom-first", "atom-second"]
        );

        let (item, first) = documents.into_iter().next().unwrap();
        let first = first.unwrap();
        assert_eq!(item.title.as_deref(), Some("First"));
        assert_eq!(first.title.as_deref(), Some("First"));
        assert_eq!(
            first.url.as_ref().map(Url::as_str),
            Some("https://example.com/articles/first")
        );
        assert_eq!(first.text, "# Heading\n\nFull article.");
    }

    #[test]
    fn rss_encoded_content_is_scored_and_description_only_is_unavailable() {
        let documents = parse_documents(
            &Url::parse(FEED_URL).unwrap(),
            br#"<?xml version="1.0"?>
                <rss version="2.0" xmlns:content="http://purl.org/rss/1.0/modules/content/">
                  <channel>
                    <title>Example</title>
                    <link>https://example.com/</link>
                    <description>Example feed</description>
                    <item>
                      <guid>rss-full</guid>
                      <title>Full entry</title>
                      <link>https://example.com/full</link>
                      <description>Short summary</description>
                      <content:encoded><![CDATA[<p>Full <em>entry</em>.</p>]]></content:encoded>
                    </item>
                    <item>
                      <guid>rss-summary</guid>
                      <title>Summary entry</title>
                      <link>https://example.com/summary</link>
                      <description><![CDATA[<p>Summary <strong>entry</strong>.</p>]]></description>
                    </item>
                  </channel>
                </rss>"#,
        )
        .unwrap();

        let mut documents = documents.into_iter();

        assert_eq!(documents.next().unwrap().1.unwrap().text, "Full entry.");
        let (item, summary_only) = documents.next().unwrap();
        assert_eq!(item.title.as_deref(), Some("Summary entry"));
        assert!(matches!(summary_only, Err(ItemError::Unavailable)));
    }

    #[test]
    fn unsupported_content_does_not_fall_back_to_the_summary() {
        let mut documents = parse_documents(
            &Url::parse(FEED_URL).unwrap(),
            br#"<feed xmlns="http://www.w3.org/2005/Atom">
                  <title>Example</title>
                  <id>https://example.com/</id>
                  <updated>2025-01-01T00:00:00Z</updated>
                  <entry>
                    <title>Attachment</title>
                    <id>unsupported-content</id>
                    <updated>2025-01-01T00:00:00Z</updated>
                    <content type="application/pdf">JVBERi0=</content>
                    <summary>Scoreable summary</summary>
                  </entry>
                </feed>"#,
        )
        .unwrap();

        assert!(matches!(
            documents.pop().unwrap().1,
            Err(ItemError::Unavailable)
        ));
    }

    #[test]
    fn linked_content_is_unavailable() {
        let mut documents = parse_documents(
            &Url::parse(FEED_URL).unwrap(),
            br#"<feed xmlns="http://www.w3.org/2005/Atom">
                  <title>Example</title>
                  <id>https://example.com/</id>
                  <updated>2025-01-01T00:00:00Z</updated>
                  <entry>
                    <title>Linked content</title>
                    <id>linked-content</id>
                    <updated>2025-01-01T00:00:00Z</updated>
                    <content type="text/html" src="https://example.com/article" />
                  </entry>
                </feed>"#,
        )
        .unwrap();

        assert!(matches!(
            documents.pop().unwrap().1,
            Err(ItemError::Unavailable)
        ));
    }

    #[test]
    fn invalid_entry_url_is_ignored() {
        let mut documents = parse_documents(
            &Url::parse(FEED_URL).unwrap(),
            br#"<rss version="2.0" xmlns:content="http://purl.org/rss/1.0/modules/content/">
                  <channel>
                    <title>Example</title>
                    <link>https://example.com/</link>
                    <description>Example feed</description>
                    <item>
                      <guid>bad-url</guid>
                      <title>Bad URL</title>
                      <link>http://[::1</link>
                      <content:encoded>Article body</content:encoded>
                    </item>
                  </channel>
                </rss>"#,
        )
        .unwrap();

        let document = documents.pop().unwrap().1.unwrap();

        assert_eq!(document.url, None);
        assert_eq!(document.text, "Article body");
    }

    #[test]
    fn invalid_feed_fails_before_producing_items() {
        let error = parse_documents(&Url::parse(FEED_URL).unwrap(), b"not a feed").unwrap_err();

        assert!(matches!(error, Error::InvalidFeed(_)));
    }
}
