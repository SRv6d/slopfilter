//! Scoring policy, workflows, and provider implementations.

pub(crate) mod pangram;
mod workflow;

pub(crate) use workflow::{ScoringItem, admit_in_order, execute_source_items};

use anyhow::{Context, Result};

pub(crate) struct Scorer {
    mode: Mode,
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

// Keep completed scores inline; boxing every live result adds an avoidable allocation.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq)]
pub(crate) enum RealtimeOutcome {
    DryRun,
    Pangram(pangram::ScoreOutcome),
}

pub(crate) struct WordBudget {
    remaining_words: usize,
}

impl Scorer {
    pub(crate) fn new(
        dry_run: bool,
        pangram_api_key: Option<String>,
        pangram_api_base_url: Option<pangram::ApiBaseUrl>,
    ) -> Result<Self> {
        let mode = if dry_run {
            Mode::DryRun
        } else {
            let api_key =
                pangram_api_key.context("Pangram API key is required unless --dry-run is set")?;
            Mode::Live {
                client: pangram::Client::new(api_key, pangram_api_base_url),
                model: None,
            }
        };

        Ok(Self { mode })
    }

    pub(crate) async fn score(&mut self, text: &str) -> Result<RealtimeOutcome> {
        match &mut self.mode {
            Mode::DryRun => Ok(RealtimeOutcome::DryRun),
            Mode::Live { client, model } => {
                if model.is_none() {
                    *model = Some(
                        client
                            .discover_model()
                            .await
                            .context("failed to discover an available Pangram model")?,
                    );
                }

                let outcome = client
                    .score(
                        text,
                        model.as_deref().expect("the model was just initialized"),
                    )
                    .await?;
                Ok(RealtimeOutcome::Pangram(outcome))
            }
        }
    }
}

impl WordBudget {
    pub(crate) fn new(max_words: usize) -> Self {
        Self {
            remaining_words: max_words,
        }
    }

    pub(crate) fn admit(
        &mut self,
        document_words: usize,
    ) -> std::result::Result<(), BudgetExhausted> {
        if document_words > self.remaining_words {
            return Err(BudgetExhausted {
                document_words,
                remaining_words: self.remaining_words,
            });
        }

        self.remaining_words -= document_words;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{BudgetExhausted, WordBudget};

    #[test]
    fn cumulative_budget_rejects_only_documents_that_do_not_fit() {
        let mut budget = WordBudget::new(10);

        assert_eq!(budget.admit(6), Ok(()));
        assert_eq!(
            budget.admit(5),
            Err(BudgetExhausted {
                document_words: 5,
                remaining_words: 4,
            })
        );
        assert_eq!(budget.admit(4), Ok(()));
        assert_eq!(budget.remaining_words, 0);
    }
}
