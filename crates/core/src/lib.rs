//! Shared foundation crate for OpenGEO: errors, IDs, telemetry, secrets.
//!
//! # Stable contracts
//!
//! - **CLI exit codes** — see [`error::ExitCode`]. PRD §11.4. Stable within a major version.
//! - **Provider error taxonomy** — see [`error::ProviderErrorKind`]. PRD §11.5. Closed enum in Phase 1.
//! - **Telemetry field names** — see [`telemetry::fields`]. NFR-Observability. Renaming is breaking.
//! - **Stable IDs** — see [`ids`]. ULID-backed newtypes for Project, Prompt, PromptRun,
//!   Mention, Citation, Claim, GroundTruthFact, and per-request correlation.
//!
//! # Secret handling
//!
//! API keys and other in-memory secrets MUST be held in [`secret::Secret`], which redacts
//! in `Debug`/`Display` and refuses to `Serialize`. See NFR-6 (Privacy-by-default).

pub mod config;
pub mod egress;
pub mod error;
pub mod ids;
pub mod secret;
pub mod secret_store;
pub mod telemetry;

pub mod api_key;
pub mod hallucination;
pub mod similarity;

pub use api_key::{GeneratedApiKey, API_KEY_HEADER, KEY_PREFIX as API_KEY_PREFIX};
pub use config::{
    project_id_for_name, prompt_id_for, AnomalySensitivity, BrandConfig, CompetitorConfig, Config,
    ConfigError, PromptConfig, ProviderConfig, ProviderName, ScheduleConfig,
    DEFAULT_ANTHROPIC_MODEL, DEFAULT_GEMINI_MODEL, DEFAULT_GROK_MODEL, DEFAULT_MISTRAL_MODEL,
    DEFAULT_OPENAI_MODEL, DEFAULT_OPENROUTER_MODEL, DEFAULT_PERPLEXITY_MODEL,
    DEFAULT_SCHEDULE_DEBOUNCE_MINUTES, SCHEMA_VERSION_V0_1, SCHEMA_VERSION_V0_2,
};
pub use egress::{
    is_forbidden_ip, parse_ip_literal, validate_host_literal, validate_resolved_ip,
    EgressPolicyError,
};
pub use error::{ExitCode, OpenGeoError, ProviderErrorKind};
pub use ids::{
    CitationId, ClaimId, GroundTruthFactId, MentionId, ProjectId, PromptId, PromptRunId, RequestId,
};
pub use secret::Secret;
pub use secret_store::{
    default_chain, get_provider_secret, provider_secret_key, remove_provider_secret,
    set_provider_secret, AgeFileStore, ChainedStore, InMemoryStore, KeyringStore, KmsClient,
    KmsOrgStore, RuntimeSecretResolver, SecretStore, SecretStoreError, WrappedDataKey,
    WriteOnlyOrgSecretStore, AGE_PASSPHRASE_ENV, BENCHMARK_KEK_KEY_PREFIX, KEYRING_SERVICE,
};
