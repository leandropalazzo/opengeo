//! Story 26.3 — Observability sink stub for CloudWatch / central-log forwarding.
//!
//! In production, replace [`StdoutLogSink`] with a real CloudWatch client
//! (e.g. `aws-sdk-cloudwatchlogs`). The trait keeps the dependency optional so
//! OSS builds compile without AWS credentials.
//!
//! See `docs/runbooks/dr-rto-rpo-drill.md` for wiring instructions and the
//! CloudWatch filter pattern used during DR drills.

/// Trait for emitting structured events to a central log sink.
///
/// Implementations must be `Send + Sync` so they can live behind `Arc<dyn CentralLogSink>`.
pub trait CentralLogSink: Send + Sync {
    /// Emit a named event with a structured JSON payload.
    fn emit(&self, event: &str, payload: &serde_json::Value);
}

/// Development / OSS sink — writes events to stdout via `tracing`.
///
/// Swap for a CloudWatch sink in production by implementing [`CentralLogSink`]
/// against `aws-sdk-cloudwatchlogs` and injecting it at startup.
pub struct StdoutLogSink;

impl CentralLogSink for StdoutLogSink {
    fn emit(&self, event: &str, payload: &serde_json::Value) {
        tracing::info!(event = event, payload = %payload, "central_log");
    }
}
