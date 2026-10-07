//! Source-independent content passed between pipeline stages.
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub title: Option<String>,
    pub url: Option<Url>,
    pub text: String,
}

impl Document {
    pub fn word_count(&self) -> usize {
        self.text.split_whitespace().count()
    }
}
