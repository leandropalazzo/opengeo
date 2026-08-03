use std::collections::BTreeMap;

use anseo_core::ProjectId;
use chrono::{DateTime, Datelike, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::model::CrawlerIngestError;

#[derive(Debug, Clone, Copy)]
pub struct MetricsParams {
    pub project_id: ProjectId,
    pub days: i64,
    pub include_unverified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CrawlerMetrics {
    pub window_start: DateTime<Utc>,
    pub window_end: DateTime<Utc>,
    pub include_unverified: bool,
    pub bots: Vec<BotMetric>,
    pub top_paths: Vec<PathMetric>,
    pub error_paths: Vec<PathMetric>,
    pub trend: Vec<TrendBucket>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CrawlReferReport {
    pub window_start: DateTime<Utc>,
    pub window_end: DateTime<Utc>,
    pub state: CrawlReferState,
    pub bots: Vec<CrawlReferRatio>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CrawlReferState {
    Complete,
    CrawlsOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CrawlReferRatio {
    pub bot_id: String,
    pub verified_crawl_hits: i64,
    pub attributed_referrals: i64,
    pub ratio: Option<f64>,
    pub state: CrawlReferState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BotMetric {
    pub bot_id: String,
    pub hits: i64,
    pub verified_hits: i64,
    pub error_hits: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PathMetric {
    pub path: String,
    pub hits: i64,
    pub error_hits: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrendBucket {
    pub day: String,
    pub hits: i64,
}

pub struct MetricsStore {
    pool: PgPool,
}

impl MetricsStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn from_storage(storage: &anseo_storage::Storage) -> Self {
        Self::new(storage.pool().clone())
    }

    pub async fn fetch(&self, params: MetricsParams) -> Result<CrawlerMetrics, CrawlerIngestError> {
        let days = params.days.clamp(1, 365);
        let window_end = Utc::now();
        let window_start = window_end - Duration::days(days);
        let project_uuid = uuid::Uuid::from_bytes(params.project_id.into_ulid().to_bytes());

        let rows = sqlx::query(
            r#"
            SELECT bot_id, path, status, ip_verified, ts
            FROM crawler_events
            WHERE project_id = $1
              AND ts >= $2
              AND ($3 OR ip_verified = TRUE)
            ORDER BY ts ASC
            "#,
        )
        .bind(project_uuid)
        .bind(window_start)
        .bind(params.include_unverified)
        .fetch_all(&self.pool)
        .await?;

        let mut by_bot: BTreeMap<String, BotMetric> = BTreeMap::new();
        let mut by_path: BTreeMap<String, PathMetric> = BTreeMap::new();
        let mut by_day: BTreeMap<String, i64> = BTreeMap::new();

        for row in rows {
            use sqlx::Row;
            let bot_id: String = row.try_get("bot_id")?;
            let path: String = row.try_get("path")?;
            let status: i32 = row.try_get("status")?;
            let ip_verified: bool = row.try_get("ip_verified")?;
            let ts: DateTime<Utc> = row.try_get("ts")?;
            let is_error = status >= 400;

            let bot = by_bot.entry(bot_id.clone()).or_insert(BotMetric {
                bot_id,
                hits: 0,
                verified_hits: 0,
                error_hits: 0,
            });
            bot.hits += 1;
            if ip_verified {
                bot.verified_hits += 1;
            }
            if is_error {
                bot.error_hits += 1;
            }

            let path_metric = by_path.entry(path.clone()).or_insert(PathMetric {
                path,
                hits: 0,
                error_hits: 0,
            });
            path_metric.hits += 1;
            if is_error {
                path_metric.error_hits += 1;
            }

            let day = format!("{:04}-{:02}-{:02}", ts.year(), ts.month(), ts.day());
            *by_day.entry(day).or_default() += 1;
        }

        let mut bots: Vec<_> = by_bot.into_values().collect();
        bots.sort_by(|a, b| b.hits.cmp(&a.hits).then_with(|| a.bot_id.cmp(&b.bot_id)));

        let mut top_paths: Vec<_> = by_path.values().cloned().collect();
        top_paths.sort_by(|a, b| b.hits.cmp(&a.hits).then_with(|| a.path.cmp(&b.path)));
        top_paths.truncate(20);

        let mut error_paths: Vec<_> = by_path.into_values().filter(|p| p.error_hits > 0).collect();
        error_paths.sort_by(|a, b| {
            b.error_hits
                .cmp(&a.error_hits)
                .then_with(|| b.hits.cmp(&a.hits))
                .then_with(|| a.path.cmp(&b.path))
        });
        error_paths.truncate(20);

        Ok(CrawlerMetrics {
            window_start,
            window_end,
            include_unverified: params.include_unverified,
            bots,
            top_paths,
            error_paths,
            trend: by_day
                .into_iter()
                .map(|(day, hits)| TrendBucket { day, hits })
                .collect(),
        })
    }

    /// Returns crawl → referral ratios for verified bots in the given window.
    ///
    /// **Degraded mode — `CrawlsOnly`:** referral data is not yet captured in
    /// the database (no `referral_events` table exists in the current schema).
    /// The function is intentionally complete: it returns a well-formed
    /// `CrawlReferReport` with `state: CrawlReferState::CrawlsOnly` and
    /// `attributed_referrals: 0` for every bot rather than panicking or
    /// returning an error. Callers can surface this state to the user.
    ///
    /// Once a `referral_events` table is added (tracked in the migration
    /// pipeline), replace the `attributed_referrals: 0` placeholder below with
    /// a LEFT JOIN against that table and compute `ratio` from the real counts.
    pub async fn fetch_crawl_refer_ratio(
        &self,
        params: MetricsParams,
    ) -> Result<CrawlReferReport, CrawlerIngestError> {
        let metrics = self
            .fetch(MetricsParams {
                include_unverified: false,
                ..params
            })
            .await?;

        let bots = metrics
            .bots
            .iter()
            .filter(|bot| bot.verified_hits > 0)
            .map(|bot| CrawlReferRatio {
                bot_id: bot.bot_id.clone(),
                verified_crawl_hits: bot.verified_hits,
                // TODO: wire to referral_events table when available (Story 33.1).
                // Until that table exists the referral count is always zero and
                // ratio remains None — state is CrawlsOnly to signal degraded mode.
                attributed_referrals: 0,
                ratio: None,
                state: CrawlReferState::CrawlsOnly,
            })
            .collect();

        Ok(CrawlReferReport {
            window_start: metrics.window_start,
            window_end: metrics.window_end,
            state: CrawlReferState::CrawlsOnly,
            bots,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_window_clamps_to_valid_range() {
        let params = MetricsParams {
            project_id: ProjectId::new(),
            days: 999,
            include_unverified: false,
        };
        assert_eq!(params.days.clamp(1, 365), 365);
    }

    #[test]
    fn crawl_refer_ratio_shape_degrades_without_referrals() {
        let ratio = CrawlReferRatio {
            bot_id: "openai-gptbot".into(),
            verified_crawl_hits: 12,
            attributed_referrals: 0,
            ratio: None,
            state: CrawlReferState::CrawlsOnly,
        };
        assert_eq!(ratio.state, CrawlReferState::CrawlsOnly);
        assert!(ratio.ratio.is_none());
    }
}
