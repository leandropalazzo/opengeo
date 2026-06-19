//! Story 34.4 — Entitlement guard for hallucination evaluation endpoints.
//!
//! OSS deployments: the `/v1/brands/:brand_id/accuracy/evaluate` endpoint
//! returns **HTTP 402 Payment Required** so that clients receive a clear
//! upgrade prompt rather than a 404.
//!
//! Claim *storage* endpoints (write extracted claims, write ground-truth facts)
//! are available on all plans and are intentionally kept out of this module —
//! they live in the brand-accuracy storage layer consumed directly by the
//! worker pipeline.
//!
//! When the commercial hallucination crate is wired in, replace `evaluate_gate`
//! with a real handler that reads `AppState::hallucination_evaluator`.

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::AppState;

pub fn v1_router() -> Router<AppState> {
    Router::new().route("/v1/brands/:brand_id/accuracy/evaluate", get(evaluate_gate))
}

/// Gate handler — returns 402 for all OSS deployments.
///
/// Pro / Enterprise: replace this handler (or add a layer that checks
/// `AppState::hallucination_evaluator` and forwards to the real impl).
async fn evaluate_gate() -> impl IntoResponse {
    (
        StatusCode::PAYMENT_REQUIRED,
        Json(json!({
            "error": "upgrade_required",
            "detail": "Hallucination monitoring evaluation requires a Pro or Enterprise plan. \
                       Claim storage is available on all plans."
        })),
    )
}
