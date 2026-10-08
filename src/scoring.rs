//! Source-independent Pangram scoring policy.

use anyhow::{Context, Result};

use crate::pangram;

pub(crate) struct Scorer {
    mode: Mode,
    remaining_words: usize,
}

enum Mode {
    DryRun,
    Live {
        client: pangram::Client,
        model: Option<String>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BudgetExhausted {
    pub(crate) document_words: usize,
    pub(crate) remaining_words: usize,
}

pub(crate) type Admission = std::result::Result<Option<pangram::ScoreOutcome>, BudgetExhausted>;

impl Scorer {
    pub(crate) fn new(
        max_words: usize,
        dry_run: bool,
        pangram_api_key: Option<String>,
    ) -> Result<Self> {
        let mode = if dry_run {
            Mode::DryRun
        } else {
            let api_key =
                pangram_api_key.context("Pangram API key is required unless --dry-run is set")?;
            Mode::Live {
                client: pangram::Client::new(api_key),
                model: None,
            }
        };

        Ok(Self {
            mode,
            remaining_words: max_words,
        })
    }

    pub(crate) async fn score(&mut self, text: &str, word_count: usize) -> Result<Admission> {
        if word_count > self.remaining_words {
            return Ok(Err(BudgetExhausted {
                document_words: word_count,
                remaining_words: self.remaining_words,
            }));
        }

        match &mut self.mode {
            Mode::DryRun => {
                self.remaining_words -= word_count;
                Ok(Ok(None))
            }
            Mode::Live { client, model } => {
                if model.is_none() {
                    *model = Some(
                        client
                            .discover_model()
                            .await
                            .context("failed to discover an available Pangram model")?,
                    );
                }

                self.remaining_words -= word_count;
                let outcome = client
                    .score(
                        text,
                        model.as_deref().expect("the model was just initialized"),
                    )
                    .await?;
                Ok(Ok(Some(outcome)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BudgetExhausted, Scorer};

    #[tokio::test]
    async fn cumulative_budget_stops_before_the_first_document_that_does_not_fit() {
        let mut scorer = Scorer::new(10, true, None).unwrap();

        assert!(scorer.score("first", 6).await.unwrap().unwrap().is_none());
        assert!(scorer.score("second", 4).await.unwrap().unwrap().is_none());
        assert_eq!(
            scorer.score("third", 1).await.unwrap().unwrap_err(),
            BudgetExhausted {
                document_words: 1,
                remaining_words: 0,
            }
        );
    }
}
