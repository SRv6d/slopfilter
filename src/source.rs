//! Source-independent content passed between pipeline stages.
pub mod feed;
pub mod matter;

use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    /// A plain-text display title.
    pub title: Option<String>,
    pub url: Option<Url>,
    /// Scoreable plain text or Markdown; sources normalize declared HTML content.
    pub text: String,
}

impl Document {
    #[must_use]
    pub fn word_count(&self) -> usize {
        self.text.split_whitespace().count()
    }
}
