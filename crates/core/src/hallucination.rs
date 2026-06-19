//! Open-core boundary for hallucination / brand-accuracy monitoring.
//!
//! The OSS tier stores claims and ground-truth facts (see the `extracted_claims`
//! and `brand_ground_truth_facts` tables) but does **not** evaluate them.
//! The commercial tier provides a real implementation via `crates/hallucination`
//! and wires it into the API via dependency-injection at startup.
//!
//! # Wiring guide
//!
//! OSS deployments: use [`NoopEvaluator`] (the default). The API returns 402
//! for any evaluation request.
//!
//! Pro / Enterprise deployments: construct the real evaluator from
//! `crates/hallucination` and place it behind `Arc<dyn HallucinationEvaluator>`
//! in `AppState`.

/// Evaluation outcome for a single extracted claim.
#[derive(Debug, Clone, PartialEq)]
pub enum ClaimVerdict {
    Accurate,
    Inaccurate { rationale: String },
    Unverifiable { rationale: String },
}

/// Error returned when hallucination evaluation is attempted without entitlement.
#[derive(Debug, thiserror::Error)]
pub enum EvaluationError {
    #[error("Hallucination monitoring requires a Pro or Enterprise plan")]
    UpgradeRequired,
    #[error("Evaluation failed: {0}")]
    Internal(String),
}

/// Trait for the hallucination evaluation engine.
///
/// OSS ships [`NoopEvaluator`]; Pro wires in the real engine from
/// `crates/hallucination`.  Implementations must be `Send + Sync` because they
/// live inside `Arc<dyn HallucinationEvaluator>` in `AppState`.
pub trait HallucinationEvaluator: Send + Sync {
    /// Evaluate a single `claim` against a set of `ground_truth` fact strings.
    ///
    /// # Errors
    ///
    /// Returns [`EvaluationError::UpgradeRequired`] for OSS deployments.
    /// Returns [`EvaluationError::Internal`] for transient backend failures.
    fn evaluate(
        &self,
        claim: &str,
        ground_truth: &[String],
    ) -> Result<ClaimVerdict, EvaluationError>;
}

/// No-op evaluator shipped with OSS deployments.
///
/// Every call returns [`EvaluationError::UpgradeRequired`]. The API layer
/// translates this to HTTP 402 Payment Required with an upgrade prompt.
pub struct NoopEvaluator;

impl HallucinationEvaluator for NoopEvaluator {
    fn evaluate(
        &self,
        _claim: &str,
        _ground_truth: &[String],
    ) -> Result<ClaimVerdict, EvaluationError> {
        Err(EvaluationError::UpgradeRequired)
    }
}
