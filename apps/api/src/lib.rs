//! OpenGEO API surface — read-only endpoints powering the local Dashboard
//! (FR-17..FR-20). Phase 1 keeps the API tightly scoped to what `apps/web`
//! consumes; the public REST surface (Phase 2) builds on these handlers.

pub mod boot;
pub mod color_validator;
pub mod extractors;
pub mod middleware;
pub mod routes;
pub mod setup_probe;

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use anseo_core::{Config, ProjectId};
use anseo_providers::ProviderRegistry;
use anseo_scheduler::events::LifecycleEvent;
use anseo_storage::Storage;
use axum::http::Method;
use axum::Router;
use tokio::sync::{broadcast, RwLock};
use tower_http::cors::{Any, CorsLayer};
use ulid::Ulid;

use crate::routes::serve_status::ServeInfo;
use crate::routes::setup::InstallState;

use crate::middleware::auth::{require_api_key, require_operator_key};
use crate::middleware::geo_gate::geo_gate_middleware;
use crate::middleware::rate_limit::RateLimitStore;

#[derive(Clone)]
pub struct AppState {
    pub storage: Arc<Storage>,
    /// The "current project" the dashboard is reading. Phase 1 single-project
    /// deployments derive this from the `anseo.yaml`'s brand name.
    pub project_id: ProjectId,
    /// Phase 2 ARCH-16 broadcast — the worker, webhook dispatcher, and
    /// notification channels each publish here; the SSE route subscribes
    /// per-client.
    pub events: broadcast::Sender<LifecycleEvent>,
    /// Project `Config` loaded at boot from `anseo.yaml`. Carries the
    /// prompt + provider declarations needed by the orchestrator's API-
    /// mode entry point in `routes::prompt_runs`. Optional so dev binds
    /// (where the YAML may be absent) still boot for the read-only
    /// surface; the write path returns a clear 503 if missing.
    pub config: Option<Arc<Config>>,
    /// Live `ProviderRegistry` constructed from configured secrets via
    /// `anseo_providers::registry::build_real_registry`. Providers
    /// without a secret are absent here; the orchestrator synthesises a
    /// `failed` record for them on dispatch.
    pub provider_registry: Option<Arc<ProviderRegistry>>,
    /// Boot-derived project wire name (`brand.name` from `anseo.yaml`).
    ///
    /// As of Epic 36 (Story 36.2) the `/v1/*` surface no longer pins to this
    /// value — `extractors::project::project_header_guard` performs real
    /// per-request resolution over the `projects` table (ADR-004 precedence)
    /// and stamps a `ProjectScope` into request extensions. This field now
    /// only serves the legacy single-project `/api/*` dashboard surface (which
    /// carries no project header) as the `EffectiveProject` fallback, plus
    /// boot-time seeding in `main`.
    pub configured_project: Arc<String>,
    /// Story 15.1 — in-memory progress map for `POST /v1/setup/clickhouse/install`.
    /// Keyed by the install `ulid` returned in the 202 response; populated by
    /// the background mock state machine in `routes::setup`. Lives in
    /// `AppState` so the SSE stream handler can find the same install across
    /// requests. The map is intentionally process-local — the install flow
    /// is operator-driven, not multi-instance.
    pub setup_install_state: Arc<RwLock<HashMap<Ulid, InstallState>>>,
    /// Story 37.1 — supervisor metadata injected by `ogeo serve` when running
    /// the API and worker in the same process. `None` when the API binary is
    /// used standalone (the `/v1/serve/status` endpoint returns `mode:
    /// "standalone"` in that case).
    pub serve_info: Option<Arc<ServeInfo>>,
    /// Story 41.2 — runtime plugin load report, computed once at boot by
    /// `anseo_plugin_host::loader::scan_and_load`. Each installed plugin carries
    /// its activation status (`loaded | skipped | load_error`); `GET /v1/plugins`
    /// serves this verbatim and `anseo plugin list` renders the same data.
    pub loaded_plugins: Arc<Vec<anseo_plugin_host::loader::LoadedPlugin>>,
    /// Story 27.4 — per-org token-bucket rate limiter. Shared across all
    /// requests; keyed by org_id extracted from the path. Process-local
    /// (restarts reset all buckets) — acceptable for the current single-node
    /// deployment. Multi-node would need a shared store (e.g. Redis).
    pub rate_limit: RateLimitStore,
}

pub fn router(state: AppState) -> Router {
    // Two coexisting surfaces, both gated by `require_api_key` when active
    // keys exist (operators can still run un-keyed against `127.0.0.1` for
    // dev; the boot-time bind refusal at `apps/api/src/main` covers the
    // public-interface case).
    //
    // - **Root paths** `/api/runs`, `/api/citations`, `/api/visibility`,
    //   `/healthz`. The Phase 1 dashboard at `apps/web` consumes these.
    //   Now auth-gated (Decision 3 of the Story 12.1 review): the
    //   dashboard will need to forward an `X-OpenGEO-API-Key` header.
    // - **`/v1/*`** — Phase 2 public REST surface. Same handlers,
    //   path-stripped of the legacy `/api/` segment, plus the SSE events
    //   route at `/v1/projects/:project_id/events`. Same auth gate.
    //
    // Phase 4 multi-project will re-scope the reads under
    // `/v1/projects/:project_id/runs` etc — Phase 2 is single-project so
    // the flat shape works. The deviation from architecture-phase2.md
    // §4.1's project-scoped surface is codified as the
    // `AD-Phase2-PathScoping` architecture-delta entry in the same doc
    // (immediately after the §4.1 tables); see that entry for rationale,
    // SDK-consumer impact, and the forward-path / deprecation plan.
    let phase_1_reads_at_root = Router::new()
        .merge(routes::runs::router())
        .merge(routes::run_detail::router())
        .merge(routes::citations::router())
        .merge(routes::visibility::router())
        .merge(routes::health::router());

    let phase_1_reads_under_v1 = Router::new()
        .merge(routes::runs::v1_router())
        .merge(routes::run_detail::v1_router())
        .merge(routes::citations::v1_router())
        .merge(routes::visibility::v1_router())
        .merge(routes::health::v1_router());

    let v1_routes = Router::new()
        .merge(phase_1_reads_under_v1)
        .merge(routes::prompt_runs::v1_router())
        .merge(routes::ingest::v1_router())
        .merge(routes::contributions::v1_router())
        .merge(routes::prompts::v1_router())
        .merge(routes::brands::v1_router())
        .merge(routes::claims::v1_router())
        .merge(routes::brand::v1_router())
        .merge(routes::analytics::v1_router())
        .merge(routes::anomalies::v1_router())
        .merge(routes::comparisons::v1_router())
        .merge(routes::crawlers::v1_router())
        .merge(routes::audit::v1_router())
        .merge(routes::schedules::v1_router())
        .merge(routes::alert_rules::v1_router())
        .merge(routes::setup::v1_router())
        .merge(routes::prompts_similarity::v1_router())
        .merge(routes::providers::v1_router())
        .merge(routes::recommendations::v1_router())
        .merge(routes::mcp::v1_router())
        .merge(routes::entities::v1_router())
        .merge(routes::verification::v1_router())
        .merge(routes::density_check::v1_router())
        .merge(routes::serve_status::v1_router())
        .merge(routes::notifications::v1_router())
        .merge(routes::impersonation::v1_router())
        .merge(routes::bundle_import::v1_router())
        .merge(routes::account::v1_router())
        .merge(routes::dsar::v1_router())
        .merge(routes::portal_grants::v1_router())
        // Story 21.2 — per-org OIDC SSO connector config store.
        .merge(routes::sso::v1_router())
        .merge(routes::events::router_under_v1_relative());

    // Premium surface — only compiled into the `pro` build. The default OSS
    // build never references the entitlement-gated hallucination evaluator.
    let v1_surface = v1_routes
        // Story 27.4 — Per-org token-bucket rate limiter. Applied FIRST (outermost)
        // so requests from a flooded org are rejected before any downstream work.
        // Only org-scoped paths (/orgs/:org_id/…) are limited; global paths pass through.
        .route_layer(axum::middleware::from_fn_with_state(
            state.rate_limit.clone(),
            middleware::rate_limit::org_rate_limit,
        ))
        // Story 22.2 — RBAC capability check (single policy point).
        // Runs AFTER auth (has OrgContext); BEFORE the project-header guard.
        // Routes that need a specific capability inject `RequiredCapability`
        // into extensions; routes without it pass through (API-key self-host).
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::authz::check_authz,
        ))
        // Story 44.4 — Geo-gating for identified-tier (class-c) endpoints.
        // Applied BEFORE auth so jurisdiction rejections are visible without
        // a valid API key. Only identified-tier paths are blocked; the
        // middleware fast-paths anonymous/aggregate endpoints.
        .route_layer(axum::middleware::from_fn(geo_gate_middleware))
        // Story 0.11 — X-Anseo-Project header substrate. Layered
        // INSIDE the auth gate so unauthenticated callers still get a
        // 401 before any project-header consideration. Each layer is
        // applied bottom-up by Axum, so this guard runs AFTER
        // `require_api_key` on each request.
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            extractors::project_header_guard,
        ))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_api_key,
        ));

    // Story 36.3 — operator-scoped project registry. These endpoints manage
    // the *set* of projects, so they are project-agnostic: gated by
    // `require_api_key` but NOT by the `X-Anseo-Project` guard (you can't
    // select a project before listing/creating one). Nested at `/v1` so they
    // share the prefix without inheriting the per-request resolution layer.
    //
    // Story 41.2 — `GET /v1/plugins` lists the plugins loaded at serve boot,
    // which is global/operator state (not project data). Story 41.3 adds the
    // plugin/marketplace surface (browse + install/remove/upgrade). Both are
    // global, operator-scoped resources (not per-project), so they ride the
    // operator surface alongside the project registry: `require_api_key` but
    // NOT the `X-Anseo-Project` guard.
    let v1_operator_surface = routes::projects::v1_router()
        .merge(routes::plugins::v1_router())
        .merge(routes::suite::v1_router())
        // Story 47.4 — operator site-analytics dashboard reads. Global operator
        // state (public-site traffic aggregates), not per-project data, so it
        // rides the operator surface: `require_api_key`, no `X-Anseo-Project`
        // guard — same scoping as `/v1/plugins`.
        .merge(routes::site_analytics::v1_router())
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_api_key,
        ));

    // Story 48.4 — operator entity-admin surface (brands & verification admin).
    // Gated by `require_operator_key` (constant-time compare against
    // ANSEO_OPERATOR_API_KEY), a DISTINCT credential from tenant project API
    // keys: a tenant key never satisfies this gate, so tenant projects cannot
    // reach `/v1/operator/entities/*`. NOT under `require_api_key` nor the
    // `X-Anseo-Project` guard — this is global, single-operator state.
    let v1_operator_admin_surface = routes::operator_entities::v1_router()
        // Story 49.0 — Plane-1 OSS substrate (consent/density reads +
        // terms-finalize gate). Same require_operator_key gate as 48.4.
        .merge(routes::operator_plane1::v1_router())
        // Story 48.10 — disputes operator review queue + lifecycle actions.
        // Uses operator key (not project API key) so the admin BFF can reach
        // it with ANSEO_OPERATOR_API_KEY; no X-Anseo-Project header needed.
        .merge(routes::disputes::v1_router())
        // Admin BFF surfaces — reached with ANSEO_OPERATOR_API_KEY,
        // no X-Anseo-Project header needed (global operator state).
        .merge(routes::orgs::v1_router())
        .merge(routes::org_audit::v1_router())
        .merge(routes::org_branding::v1_router())
        .merge(routes::billing::v1_router())
        .merge(routes::cost::v1_router())
        .route_layer(axum::middleware::from_fn(require_operator_key));

    let phase_1_at_root_gated = phase_1_reads_at_root.route_layer(
        axum::middleware::from_fn_with_state(state.clone(), require_api_key),
    );

    // PUBLIC, unauthenticated `/v1` surface — no `require_api_key`, no project
    // guard, no geo-gate. Read-only/anonymous endpoints third parties hit
    // directly:
    //   * Story 43.5 — verified-badge embeds (`<img src=".../v1/badge/...">`
    //     can't send an API key or project header).
    //   * Story 43.6 — dispute submission (`POST /v1/disputes`) + public reads
    //     (`GET /v1/disputes/:id`, `.../events`) per AC-5. The gated operator
    //     surface (review queue + lifecycle actions) stays in `v1_surface`.
    let v1_public_surface = routes::badge::v1_router()
        .merge(routes::disputes::public_router())
        .merge(routes::verification::public_router())
        .merge(routes::site_events::v1_router())
        .merge(routes::billing::public_router())
        // Story 27.1 — public signup + email-verify (no API key required).
        .merge(routes::signup::public_router())
        // Story 21.2 — public OAuth stub (redirect + callback, no API key).
        .merge(routes::sso::public_router())
        // Leaderboard is public aggregate data — no API key required.
        // Client-side fetches from the web app hit this unauthenticated.
        .merge(routes::leaderboard::v1_router());

    let mut base = Router::new()
        .merge(phase_1_at_root_gated)
        .nest("/v1", v1_public_surface)
        .nest("/v1", v1_operator_surface)
        .nest("/v1", v1_operator_admin_surface)
        .nest("/v1", v1_surface)
        // Story 43.7 — public, unauthenticated comms preference center +
        // one-click unsubscribe. No API key: authority is the opaque token in
        // the URL (resolves to one recipient). NOT under the /v1 auth gate.
        .merge(routes::comms::public_router());

    // `POST /test/seed` is registered only when ANSEO_TEST_MODE=1. The
    // env-var gate lives at router build time so production binaries never
    // expose the route, regardless of which HTTP layer middleware applies.
    if routes::test_seed::is_enabled_via_env() {
        tracing::warn!(
            event = "service.test_mode_enabled",
            "ANSEO_TEST_MODE=1 detected — mounting POST /test/seed. This MUST NOT be set in production."
        );
        base = base.merge(routes::test_seed::router());
    }

    base.with_state(state).layer(
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods([Method::GET, Method::POST])
            .allow_headers(Any),
    )
}

/// Parse a hex/ULID string into a `ProjectId`. Helper used by main + tests.
pub fn parse_project_id(s: &str) -> anyhow::Result<ProjectId> {
    ProjectId::from_str(s).map_err(|e| anyhow::anyhow!("invalid project id: {e}"))
}

/// Pure boot-time bind-refusal logic (Story 12.1 NFR-AuthHardenedByDefault).
/// Returns `Ok(())` if the bind is acceptable, `Err(message)` otherwise.
/// Extracted from `apps/api/src/main.rs` so the policy is unit-testable
/// without standing up a server.
///
/// Rules:
/// 1. Bind address must parse as a `SocketAddr` (IP literal + port).
/// 2. If the address is a loopback IP, accept (no auth gate needed).
/// 3. If non-loopback AND `ANSEO_TEST_MODE=1`, refuse — `/test/seed` is
///    unauthenticated by design and must never reach the open internet.
/// 4. If non-loopback AND zero active keys exist for this project, refuse —
///    a public bind with no keys is unauthenticated by definition.
pub fn check_bind_acceptable(
    bind_addr: &str,
    test_mode_enabled: bool,
    active_keys_for_project: i64,
) -> Result<std::net::SocketAddr, String> {
    let socket = std::net::SocketAddr::from_str(bind_addr)
        .map_err(|e| format!("invalid ANSEO_API_BIND `{bind_addr}`: {e}"))?;
    if socket.ip().is_loopback() {
        return Ok(socket);
    }
    if test_mode_enabled {
        return Err(format!(
            "ANSEO_API_BIND=`{bind_addr}` is non-loopback AND ANSEO_TEST_MODE=1 — \
             refusing to start. The /test/seed surface is unauthenticated by design; \
             it must never be reachable on a public interface."
        ));
    }
    if active_keys_for_project == 0 {
        return Err(format!(
            "ANSEO_API_BIND=`{bind_addr}` is non-loopback but no active API keys exist \
             for this project. Generate one with `ogeo api key create --name <slug>` \
             before binding to a public interface, set ANSEO_BOOTSTRAP_API_KEY for \
             trusted private-network stacks, or bind to 127.0.0.1 / ::1 for local-only \
             access."
        ));
    }
    Ok(socket)
}

/// Derive the persisted `(sha256_hash, display_prefix)` for a bootstrap API
/// key supplied verbatim via `ANSEO_BOOTSTRAP_API_KEY`.
///
/// The boot path (`apps/api/src/main.rs`) seeds this key when a project has
/// zero active keys, so a trusted private-network deployment — the Docker
/// Compose stack, where the api binds `0.0.0.0` so the sibling `web`
/// container can reach it but no operator has run `ogeo api key create` —
/// satisfies [`check_bind_acceptable`] without hand-seeding the database.
///
/// Returns `Err` when the plaintext doesn't match the canonical
/// `ogeo_<32 base62>` wire shape that the auth middleware's `looks_like_key`
/// gate requires; seeding a malformed value would persist a key that can
/// never authenticate a request, so we fail loudly at boot instead. Keeping
/// the derivation pure (no DB) mirrors `check_bind_acceptable` so the policy
/// is unit-testable without a live Postgres.
pub fn bootstrap_key_material(plaintext: &str) -> Result<(String, String), String> {
    use anseo_core::api_key::{looks_like_key, sha256_hex, DISPLAY_PREFIX_LEN, KEY_PREFIX};
    if !looks_like_key(plaintext) {
        return Err(
            "ANSEO_BOOTSTRAP_API_KEY does not match the required `ogeo_<32 base62>` \
             shape; generate a valid value with `ogeo api key create` (or omit the env \
             var to keep the keyless-bind refusal)."
                .to_string(),
        );
    }
    let random = &plaintext[KEY_PREFIX.len()..];
    let display_prefix: String = random.chars().take(DISPLAY_PREFIX_LEN).collect();
    Ok((sha256_hex(plaintext), display_prefix))
}
