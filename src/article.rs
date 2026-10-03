#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Article {
    pub source_id: String,
    pub title: String,
    pub url: String,
    pub markdown: String,
}

impl Article {
    pub fn word_count(&self) -> usize {
        self.markdown.split_whitespace().count()
    }
}
