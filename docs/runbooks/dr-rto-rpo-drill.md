# DR Drill Runbook — RTO/RPO Verification

**Story 26.3** | Last updated: 2026-06-19 | Owner: Platform / SRE

---

## Overview

This runbook guides the operator through a game-day disaster-recovery drill for
Anseo's production Postgres database (AWS RDS Multi-AZ). The drill verifies two
commitments:

| SLO | Target | Measured by |
|-----|--------|-------------|
| RPO (Recovery Point Objective) | ≤ 5 minutes | `pg_dump` diff before/after PITR restore |
| RTO (Recovery Time Objective) | ≤ 1 hour | wall-clock time from kill to first HTTP 200 |

The drill also verifies that the cross-tenant isolation alert fires within
5 minutes of a simulated RLS bypass event (see Phase 4).

**This runbook is reviewed and updated after every drill. Any divergence
between the procedure and observed system behaviour is a P1 finding.**

---

## Prerequisites

### Tooling

- AWS CLI v2 configured with `AdministratorAccess` (or a scoped DR policy)
  that covers RDS, CloudWatch, GuardDuty, and S3.
- `pg_dump` / `psql` (Postgres 16 client) available on the operator's machine.
- `curl` or `httpie` to probe the API health endpoint.
- Access to the Anseo production `anseo.yaml` and its secrets (via AWS Secrets
  Manager or the local `age` keyring).

### Environment

- Anseo deployed in Multi-AZ mode (see `infra/terraform/rds_dr.tf`).
- Automated backups enabled with `backup_retention_period = 7` days.
- CloudWatch Log Group `anseo/rls-events` active (see Observability section).
- GuardDuty threat detection enabled in the primary region.

### Coordination

- Notify the on-call engineer and product lead at least 24 hours before.
- Schedule the drill during a low-traffic window (weekday 03:00–06:00 UTC).
- Confirm a rollback path: you must be able to restore from the pre-drill
  snapshot within 15 minutes if production is involved.

---

## Drill Procedure

### Phase 1 — Pre-flight

1. **Record drill start time** (UTC):

   ```
   DRILL_START=$(date -u +%Y-%m-%dT%H:%M:%SZ)
   echo "Drill start: $DRILL_START"
   ```

2. **Take a baseline dump** (this is your RPO reference point):

   ```bash
   pg_dump \
     --host="$ANSEO_DB_HOST" \
     --port=5432 \
     --username="$ANSEO_DB_USER" \
     --dbname="$ANSEO_DB_NAME" \
     --format=custom \
     --file="/tmp/anseo-baseline-${DRILL_START}.dump"
   ```

   Record the dump timestamp in seconds since epoch:

   ```bash
   BASELINE_EPOCH=$(date +%s)
   ```

3. **Confirm replication lag is zero** before proceeding:

   ```sql
   SELECT client_addr, state, sent_lsn, write_lsn, flush_lsn, replay_lsn,
          (sent_lsn - replay_lsn) AS replication_lag_bytes
   FROM pg_stat_replication;
   ```

   Expected: `replication_lag_bytes = 0` for the standby. If lag > 0, abort
   the drill and investigate before rescheduling.

4. **Record the current LSN**:

   ```sql
   SELECT pg_current_wal_lsn();
   ```

5. **Smoke-test the API** to confirm a healthy baseline:

   ```bash
   curl -sf https://api.anseo.io/v1/health | jq .
   ```

   Expected: `{"status": "ok"}`.

---

### Phase 2 — Kill-During-Write (Fault Injection)

> **Warning:** This phase terminates the RDS primary. If performed in
> production, writes will be interrupted for the duration of the failover.
> Use a staging environment for rehearsals; production drills require explicit
> approval from the CTO.

1. **Trigger a background write workload** to create in-flight transactions
   during the kill:

   ```bash
   # Run a prompt batch so the worker is actively writing prompt_runs rows.
   curl -X POST https://api.anseo.io/v1/prompt-runs/batch \
     -H "Authorization: Bearer $ANSEO_API_KEY" \
     -H "Content-Type: application/json" \
     -d '{"count": 20, "async": true}'
   ```

2. **Record the pre-kill timestamp** (this bounds the RPO window):

   ```bash
   KILL_EPOCH=$(date +%s)
   KILL_TS=$(date -u +%Y-%m-%dT%H:%M:%SZ)
   echo "Kill timestamp: $KILL_TS"
   ```

3. **Force a Multi-AZ failover** via the AWS CLI:

   ```bash
   aws rds reboot-db-instance \
     --db-instance-identifier anseo-primary \
     --force-failover \
     --region us-east-1
   ```

   Alternatively, use the RDS console: DB Instances → anseo-primary →
   Actions → Reboot → Reboot with Failover.

4. **Monitor failover progress**:

   ```bash
   watch -n 5 "aws rds describe-db-instances \
     --db-instance-identifier anseo-primary \
     --query 'DBInstances[0].DBInstanceStatus' \
     --output text"
   ```

   Expected sequence: `rebooting` → `modifying` → `available`.
   Typical duration: 60–120 seconds.

5. **Note the time the instance returns to `available`**:

   ```bash
   FAILOVER_AVAILABLE_TS=$(date -u +%Y-%m-%dT%H:%M:%SZ)
   ```

---

### Phase 3 — Restore and Measure

#### 3a. PITR Restore (if required by drill scope)

For drills that require a full point-in-time restore (not just Multi-AZ
failover), use:

```bash
aws rds restore-db-instance-to-point-in-time \
  --source-db-instance-identifier anseo-primary \
  --target-db-instance-identifier anseo-pitr-restored \
  --restore-time "$KILL_TS" \
  --db-instance-class db.r7g.large \
  --multi-az \
  --region us-east-1
```

Wait for the restored instance to reach `available` (15–45 minutes for a
full restore). For Multi-AZ failover drills where the standby has already
promoted, skip this step.

#### 3b. Measure RPO

Take a post-restore dump and diff against the baseline:

```bash
RESTORE_EPOCH=$(date +%s)

pg_dump \
  --host="$RESTORED_DB_HOST" \
  --port=5432 \
  --username="$ANSEO_DB_USER" \
  --dbname="$ANSEO_DB_NAME" \
  --format=plain \
  --file="/tmp/anseo-restored.sql"

# Compare row counts for write-heavy tables
psql -h "$RESTORED_DB_HOST" -U "$ANSEO_DB_USER" -d "$ANSEO_DB_NAME" <<SQL
SELECT 'prompt_runs' AS tbl, COUNT(*) FROM prompt_runs
UNION ALL
SELECT 'extracted_claims', COUNT(*) FROM extracted_claims
UNION ALL
SELECT 'brand_ground_truth_facts', COUNT(*) FROM brand_ground_truth_facts;
SQL
```

RPO = `RESTORE_EPOCH - KILL_EPOCH` seconds. Must be ≤ 300 (5 minutes).

If RPO > 300 seconds: **FAIL** — open a P1 incident and do not proceed.

#### 3c. Measure RTO

Probe the API health endpoint in a loop from the moment of kill:

```bash
RTO_START=$KILL_EPOCH
until curl -sf https://api.anseo.io/v1/health | grep -q '"status":"ok"'; do
  echo "$(date -u) — waiting for API..."
  sleep 10
done
RTO_END=$(date +%s)
RTO_SECONDS=$((RTO_END - RTO_START))
echo "RTO: ${RTO_SECONDS}s (target ≤ 3600s)"
```

RTO must be ≤ 3600 seconds (1 hour). If Anseo's connection pool reconnects
automatically (the default with `sqlx` + `min_connections > 0`), RTO is
typically equal to the RDS failover time (60–120 s).

---

### Phase 4 — Cross-Tenant Alert Verification

This phase verifies that the CloudWatch alarm for cross-tenant RLS events
fires within 5 minutes of a simulated violation.

1. **Simulate a cross-tenant access event** by setting a mismatched `app.org`
   in a direct DB session (requires DBA access; never done via the API):

   ```sql
   -- Connect as the anseo application user
   SET app.org = 'ffffffff-ffff-ffff-ffff-ffffffffffff'; -- wrong org UUID
   SELECT COUNT(*) FROM extracted_claims; -- should return 0 due to RLS
   -- The attempt itself is logged to anseo/rls-events
   ```

2. **Check CloudWatch for the event**:

   ```bash
   aws logs filter-log-events \
     --log-group-name "anseo/rls-events" \
     --filter-pattern '"cross_tenant_attempt"' \
     --start-time "$(python3 -c "import time; print(int(time.time() - 600) * 1000)")" \
     --region us-east-1
   ```

3. **Verify the CloudWatch alarm transitions to ALARM state** within 5 minutes:

   ```bash
   aws cloudwatch describe-alarm-history \
     --alarm-name "anseo-cross-tenant-rls-violation" \
     --history-item-type StateUpdate \
     --region us-east-1 \
     --query 'AlarmHistoryItems[0]'
   ```

   Expected: `"newState": {"stateValue": "ALARM"}` within 5 minutes of the
   simulated event.

   If the alarm does not fire: **FAIL** — escalate to the security team.

**CloudWatch metric filter pattern** (applied to `anseo/rls-events`):

```
{ $.event = "cross_tenant_attempt" }
```

**Alarm configuration:**
- Metric: custom metric `CrossTenantAttempts` in namespace `Anseo/Security`
- Threshold: ≥ 1 within 5 minutes (1 data point)
- Alarm action: SNS → on-call PagerDuty integration

---

### Phase 5 — Post-Drill Cleanup

1. If a PITR instance was created, delete it to avoid cost:

   ```bash
   aws rds delete-db-instance \
     --db-instance-identifier anseo-pitr-restored \
     --skip-final-snapshot \
     --region us-east-1
   ```

2. Remove the baseline dump from `/tmp`.

3. Reset any modified DB settings in the application connection.

4. Confirm API is healthy and processing normal traffic:

   ```bash
   curl -sf https://api.anseo.io/v1/health | jq .
   ```

5. File a drill report in the `#platform-drills` Slack channel with:
   - Drill start/end times
   - Measured RPO and RTO
   - Cross-tenant alert result (fired / did not fire)
   - Any deviations from this runbook

---

## iso-3 Pool-Race Replay

The RLS matrix integration test exercises all RLS-protected tables in parallel
transactions to catch pool-race conditions (iso-3 isolation level). Run this
after every migration that touches RLS policies:

```bash
cargo test -p anseo-storage --test rls_matrix -- --test-threads=1
```

Expected output: all tests pass. If any test fails with a
`ERROR: new row violates row-level security policy` that should not have
failed, or returns rows that belong to another org, this is a P0 security
incident.

---

## Pass/Fail Criteria

| Criterion | Target | Result |
|-----------|--------|--------|
| RPO | ≤ 5 minutes | |
| RTO | ≤ 1 hour | |
| Cross-tenant alert fires | ≤ 5 minutes | |
| RLS matrix tests pass | 100% | |
| API returns 200 post-restore | Yes | |

Fill in the **Result** column during the drill and attach to the post-drill
report.

---

## Terraform Reference

The RDS Multi-AZ configuration is stubbed in
`docs/runbooks/terraform-stubs/rds_dr.tf`. Apply only after a maintainer
game-day review; it is not auto-applied by CI.

---

## Observability Wiring

The `CentralLogSink` trait in `crates/audit/src/central_log.rs` defines the
interface used to emit events to CloudWatch. In production:

1. Implement `CentralLogSink` against `aws-sdk-cloudwatchlogs`.
2. Inject the implementation at startup into `AppState`.
3. Emit `cross_tenant_attempt` events from the RLS middleware when
   `current_setting('app.org')` does not match the row's org column.

For OSS deployments, `StdoutLogSink` writes events to stdout; connect stdout
to a CloudWatch Logs agent for centralized aggregation.

---

## References

- AWS RDS PITR docs: https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/USER_PitrRestore.html
- `infra/terraform/rds_dr.tf` — Terraform stub for Multi-AZ RDS
- `crates/audit/src/central_log.rs` — observability sink trait
- `crates/storage/migrations/20260619190000_rls_accuracy_claims.sql` — RLS for accuracy tables
- Story 26.3, Story 34.1, Story 34.4 in the sprint backlog
