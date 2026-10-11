//! Atom and RSS source adapter.

use feed_rs::{
    model::{Entry as ParsedEntry, Text},
    parser,
};
use reqwest::Client as HttpClient;
use thiserror::Error;
use url::Url;

use super::Document;

const HTML_TEXT_WIDTH: usize = 120;

#[derive(Clone, Default)]
pub struct Client {
    http: HttpClient,
}

#[derive(Debug)]
pub struct Item {
    entry: ParsedEntry,
    url: Option<Url>,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to fetch feed")]
    Fetch(#[from] reqwest::Error),

    #[error("feed is invalid")]
    InvalidFeed(#[from] parser::ParseFeedError),
}

#[derive(Debug, Error)]
pub enum ItemError {
    #[error("feed item {id} has no inline text content or summary")]
    Unavailable { id: String },

    #[error("feed item {id} content could not be normalized")]
    InvalidContent {
        id: String,
        #[source]
        source: html2text::Error,
    },
}

impl Client {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn fetch_items(&self, feed_url: Url) -> Result<Vec<Item>, Error> {
        let bytes = self
            .http
            .get(feed_url.clone())
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;

        parse_items(&feed_url, &bytes)
    }
}

impl Item {
    pub fn id(&self) -> &str {
        &self.entry.id
    }

    pub fn into_document(self) -> Result<(String, Document), ItemError> {
        let Self { entry, url } = self;
        let title = entry
            .title
            .as_ref()
            .map(display_text)
            .transpose()
            .map_err(|source| ItemError::InvalidContent {
                id: entry.id.clone(),
                source,
            })?
            .flatten();
        let text = item_text(&entry)
            .map_err(|source| ItemError::InvalidContent {
                id: entry.id.clone(),
                source,
            })?
            .ok_or_else(|| ItemError::Unavailable {
                id: entry.id.clone(),
            })?;

        Ok((entry.id, Document { title, url, text }))
    }
}

pub fn parse_items(feed_url: &Url, bytes: &[u8]) -> Result<Vec<Item>, Error> {
    let feed = parser::Builder::new()
        .base_uri(Some(feed_url.as_str()))
        .build()
        .parse(bytes)?;

    Ok(feed
        .entries
        .into_iter()
        .map(|entry| {
            let url = item_url(feed_url, &entry);
            Item { entry, url }
        })
        .collect())
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

fn item_text(entry: &ParsedEntry) -> Result<Option<String>, html2text::Error> {
    if let Some(content) = entry.content.as_ref()
        && let Some(body) = content.body.as_deref()
        && let Some(text) = scoreable_text(body, content.content_type.as_str())?
    {
        return Ok(Some(text));
    }

    entry
        .summary
        .as_ref()
        .map(|summary| scoreable_text(&summary.content, summary.content_type.as_str()))
        .transpose()
        .map(Option::flatten)
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
        let items = parse_items(
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
            items.iter().map(Item::id).collect::<Vec<_>>(),
            ["atom-first", "atom-second"]
        );

        let (_, first) = items.into_iter().next().unwrap().into_document().unwrap();
        assert_eq!(first.title.as_deref(), Some("First"));
        assert_eq!(
            first.url.as_ref().map(Url::as_str),
            Some("https://example.com/articles/first")
        );
        assert_eq!(first.text, "# Heading\n\nFull article.");
    }

    #[test]
    fn rss_content_and_description_map_to_documents() {
        let items = parse_items(
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

        let documents = items
            .into_iter()
            .map(Item::into_document)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(documents[0].1.text, "Full entry.");
        assert_eq!(documents[1].1.text, "Summary entry.");
    }

    #[test]
    fn unsupported_full_content_falls_back_to_the_summary() {
        let mut items = parse_items(
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

        let (_, document) = items.pop().unwrap().into_document().unwrap();

        assert_eq!(document.text, "Scoreable summary");
    }

    #[test]
    fn entry_without_inline_content_or_summary_is_unavailable() {
        let mut items = parse_items(
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
            items.pop().unwrap().into_document(),
            Err(ItemError::Unavailable { id }) if id == "linked-content"
        ));
    }

    #[test]
    fn invalid_entry_url_is_ignored() {
        let mut items = parse_items(
            &Url::parse(FEED_URL).unwrap(),
            br#"<rss version="2.0">
                  <channel>
                    <title>Example</title>
                    <link>https://example.com/</link>
                    <description>Example feed</description>
                    <item>
                      <guid>bad-url</guid>
                      <title>Bad URL</title>
                      <link>http://[::1</link>
                      <description>Article body</description>
                    </item>
                  </channel>
                </rss>"#,
        )
        .unwrap();

        let (_, document) = items.pop().unwrap().into_document().unwrap();

        assert_eq!(document.url, None);
        assert_eq!(document.text, "Article body");
    }

    #[test]
    fn invalid_feed_fails_before_producing_items() {
        let error = parse_items(&Url::parse(FEED_URL).unwrap(), b"not a feed").unwrap_err();

        assert!(matches!(error, Error::InvalidFeed(_)));
    }
}
