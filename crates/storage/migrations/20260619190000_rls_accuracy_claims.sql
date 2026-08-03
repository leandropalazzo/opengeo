-- Story 34.1: Add RLS policies to accuracy/claims tables.
-- These tables were created in 20260602110000_brand_accuracy_claims.sql without
-- row-level security. Both tables use `organization_id` (nullable) and
-- `tenant_id` (nullable) rather than a single `org_id` column — the policy
-- therefore falls back to `tenant_id` when `organization_id` is NULL, which
-- covers the legacy single-tenant deployment path.
--
-- The rls_matrix integration test enforces ENABLE + FORCE + at least one
-- SELECT policy on every table touched by RLS. This migration satisfies that
-- contract for `extracted_claims` and `brand_ground_truth_facts`.

ALTER TABLE extracted_claims ENABLE ROW LEVEL SECURITY;
ALTER TABLE extracted_claims FORCE ROW LEVEL SECURITY;

CREATE POLICY extracted_claims_select ON extracted_claims
    FOR SELECT USING (
        COALESCE(organization_id, tenant_id) =
            current_setting('app.org', true)::uuid
    );
CREATE POLICY extracted_claims_insert ON extracted_claims
    FOR INSERT WITH CHECK (
        COALESCE(organization_id, tenant_id) =
            current_setting('app.org', true)::uuid
    );
CREATE POLICY extracted_claims_update ON extracted_claims
    FOR UPDATE USING (
        COALESCE(organization_id, tenant_id) =
            current_setting('app.org', true)::uuid
    );

ALTER TABLE brand_ground_truth_facts ENABLE ROW LEVEL SECURITY;
ALTER TABLE brand_ground_truth_facts FORCE ROW LEVEL SECURITY;

CREATE POLICY brand_truth_select ON brand_ground_truth_facts
    FOR SELECT USING (
        COALESCE(organization_id, tenant_id) =
            current_setting('app.org', true)::uuid
    );
CREATE POLICY brand_truth_insert ON brand_ground_truth_facts
    FOR INSERT WITH CHECK (
        COALESCE(organization_id, tenant_id) =
            current_setting('app.org', true)::uuid
    );
CREATE POLICY brand_truth_update ON brand_ground_truth_facts
    FOR UPDATE USING (
        COALESCE(organization_id, tenant_id) =
            current_setting('app.org', true)::uuid
    );
