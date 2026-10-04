//! Portable, versioned article sets produced by source listing commands.
use std::{collections::HashSet, io::Write, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::article::Article;

pub(crate) const FORMAT: &str = "slopfilter/article-set";
const VERSION: u32 = 1;

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ArticleSet {
    format: String,
    version: u32,
    source: Source,
    articles: Vec<StoredArticle>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Source {
    Matter,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct StoredArticle {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) markdown: String,
    pub(crate) word_count: usize,
}

impl ArticleSet {
    pub(crate) fn matter(articles: Vec<StoredArticle>) -> Self {
        Self {
            format: FORMAT.to_owned(),
            version: VERSION,
            source: Source::Matter,
            articles,
        }
    }

    pub(crate) fn parse(text: &str) -> Result<Option<Self>> {
        if !text.contains(FORMAT) {
            return Ok(None);
        }

        let looks_like_article_set = has_format_marker(text);
        let value: serde_json::Value = match serde_json::from_str(text) {
            Ok(value) => value,
            Err(error) if looks_like_article_set => {
                return Err(error).context("invalid slopfilter article set");
            }
            Err(_) => return Ok(None),
        };

        if value.get("format").and_then(serde_json::Value::as_str) != Some(FORMAT) {
            return Ok(None);
        }

        let article_set: Self =
            serde_json::from_value(value).context("invalid slopfilter article set")?;
        article_set.validate()?;
        Ok(Some(article_set))
    }

    pub(crate) fn select(self, id: &str) -> Result<StoredArticle> {
        self.articles
            .into_iter()
            .find(|article| article.id == id)
            .ok_or_else(|| anyhow!("article set contains no item with ID {id}"))
    }

    fn validate(&self) -> Result<()> {
        if self.version != VERSION {
            bail!(
                "unsupported slopfilter article-set version {}; expected {VERSION}",
                self.version
            );
        }

        let mut ids = HashSet::with_capacity(self.articles.len());
        for article in &self.articles {
            if article.id.is_empty() {
                bail!("article set contains an empty item ID");
            }
            if !ids.insert(article.id.as_str()) {
                bail!("article set contains duplicate item ID {}", article.id);
            }
        }

        Ok(())
    }
}

impl StoredArticle {
    pub(crate) fn from_article(article: Article, word_count: usize) -> Self {
        Self {
            id: article.source_id,
            title: article.title,
            url: article.url.into(),
            markdown: article.markdown,
            word_count,
        }
    }
}

fn has_format_marker(text: &str) -> bool {
    let Some(after_name) = text.split_once("\"format\"").map(|(_, rest)| rest) else {
        return false;
    };
    let Some(after_colon) = after_name.trim_start().strip_prefix(':') else {
        return false;
    };
    let Some(value) = after_colon.trim_start().strip_prefix('"') else {
        return false;
    };

    value
        .strip_prefix(FORMAT)
        .is_some_and(|remainder| remainder.starts_with('"'))
}

pub(crate) fn write(path: &Path, article_set: &ArticleSet, force: bool) -> Result<()> {
    article_set.validate()?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create a temporary file in {}", parent.display()))?;

    serde_json::to_writer(&mut temporary, article_set)
        .context("failed to serialize the article set")?;
    temporary
        .write_all(b"\n")
        .context("failed to finish the article set")?;
    temporary
        .as_file()
        .sync_all()
        .context("failed to flush the article set")?;

    let persisted = if force {
        temporary.persist(path)
    } else {
        temporary.persist_noclobber(path)
    };
    persisted
        .map(|_| ())
        .map_err(|error| error.error)
        .with_context(|| {
            if path.exists() && !force {
                format!(
                    "refusing to overwrite {}; pass --force to replace it",
                    path.display()
                )
            } else {
                format!("failed to save article set to {}", path.display())
            }
        })
}

#[cfg(test)]
mod tests {
    use super::{ArticleSet, FORMAT, StoredArticle, write};

    fn article(id: &str) -> StoredArticle {
        StoredArticle {
            id: id.to_owned(),
            title: "An article".to_owned(),
            url: "https://example.com/article".to_owned(),
            markdown: "Article body".to_owned(),
            word_count: 2,
        }
    }

    #[test]
    fn tagged_article_set_round_trips() {
        let serialized =
            serde_json::to_string(&ArticleSet::matter(vec![article("itm-123")])).unwrap();
        let parsed = ArticleSet::parse(&serialized).unwrap().unwrap();

        assert_eq!(parsed.select("itm-123").unwrap().markdown, "Article body");
    }

    #[test]
    fn ordinary_json_remains_plain_text() {
        assert!(
            ArticleSet::parse(r#"{"article":"text"}"#)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn malformed_tagged_input_is_rejected() {
        let malformed = format!(r#"{{ "format" : "{FORMAT}", "version": 1"#);
        assert!(ArticleSet::parse(&malformed).is_err());
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let serialized = serde_json::to_string(&serde_json::json!({
            "format": FORMAT,
            "version": 2,
            "source": {"type": "matter"},
            "articles": []
        }))
        .unwrap();

        assert!(ArticleSet::parse(&serialized).is_err());
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let serialized =
            serde_json::to_string(&ArticleSet::matter(vec![article("same"), article("same")]))
                .unwrap();

        assert!(ArticleSet::parse(&serialized).is_err());
    }

    #[test]
    fn writing_requires_force_to_replace_a_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("articles.slop.json");
        let first = ArticleSet::matter(vec![article("first")]);
        let second = ArticleSet::matter(vec![article("second")]);

        write(&path, &first, false).unwrap();
        assert!(write(&path, &second, false).is_err());
        let unchanged = std::fs::read_to_string(&path).unwrap();
        assert!(
            ArticleSet::parse(&unchanged)
                .unwrap()
                .unwrap()
                .select("first")
                .is_ok()
        );
        write(&path, &second, true).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            ArticleSet::parse(&text)
                .unwrap()
                .unwrap()
                .select("second")
                .unwrap()
                .id,
            "second"
        );
    }
}
