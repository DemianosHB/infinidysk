//! Play Watchdog entries (v1 fork Phase 3), ported from `backend/Database/Models/WatchdogEntry.cs`,
//! `backend/Services/WatchdogLog.cs` and `backend/Services/WatchdogPurgeService.cs`.

use sqlx::FromRow;

use crate::{Db, Error, Pool, on};

/// v1 `WatchdogEntry.Outcome` ordinals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i16)]
pub enum WatchdogOutcome {
    PreVerifyAvailable = 0,
    PreVerifyDead = 1,
    PreVerifyTimeout = 2,
    Cancelled = 3,
    EnqueueFailed = 4,
    QueueFailed = 5,
    QueueCompleted = 6,
    BudgetTimeout = 7,
    ExcludedByPattern = 8,
}

impl WatchdogOutcome {
    #[must_use]
    pub const fn as_i16(self) -> i16 {
        self as i16
    }

    #[must_use]
    pub fn from_i16(v: i16) -> Self {
        match v {
            1 => Self::PreVerifyDead,
            2 => Self::PreVerifyTimeout,
            3 => Self::Cancelled,
            4 => Self::EnqueueFailed,
            5 => Self::QueueFailed,
            6 => Self::QueueCompleted,
            7 => Self::BudgetTimeout,
            8 => Self::ExcludedByPattern,
            _ => Self::PreVerifyAvailable,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::PreVerifyAvailable => "PreVerifyAvailable",
            Self::PreVerifyDead => "PreVerifyDead",
            Self::PreVerifyTimeout => "PreVerifyTimeout",
            Self::Cancelled => "Cancelled",
            Self::EnqueueFailed => "EnqueueFailed",
            Self::QueueFailed => "QueueFailed",
            Self::QueueCompleted => "QueueCompleted",
            Self::BudgetTimeout => "BudgetTimeout",
            Self::ExcludedByPattern => "ExcludedByPattern",
        }
    }
}

/// v1 `WatchdogEntry`.
#[derive(Debug, Clone, FromRow)]
pub struct WatchdogEntry {
    pub id: i64,
    pub click_id: String,
    pub attempted_at: i64,
    pub content_type: String,
    pub requested_title: String,
    pub candidate_title: String,
    pub indexer_name: String,
    pub size: i64,
    pub rank_index: i32,
    pub result: i16,
    pub fail_reason: Option<String>,
    pub duration_ms: i64,
    pub is_winner: bool,
    pub provider_host: Option<String>,
    pub queue_item_id: Option<String>,
    pub content_group_key: Option<String>,
}

/// Input for [`Db::watchdog_record`].
#[derive(Debug, Clone)]
pub struct NewWatchdogEntry {
    pub click_id: String,
    pub attempted_at: i64,
    pub content_type: String,
    pub requested_title: String,
    pub candidate_title: String,
    pub indexer_name: String,
    pub size: i64,
    pub rank_index: i32,
    pub result: WatchdogOutcome,
    pub fail_reason: Option<String>,
    pub duration_ms: i64,
    pub is_winner: bool,
    pub provider_host: Option<String>,
    pub queue_item_id: Option<String>,
    pub content_group_key: Option<String>,
}

impl Db {
    pub async fn watchdog_record(&self, e: &NewWatchdogEntry) -> Result<i64, Error> {
        Ok(on!(
            self,
            pool => sqlx::query_scalar(
                "INSERT INTO watchdog_entries \
                    (click_id, attempted_at, content_type, requested_title, candidate_title, indexer_name, size, \
                     rank_index, result, fail_reason, duration_ms, is_winner, provider_host, queue_item_id, content_group_key) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15) \
                 RETURNING id"
            )
            .bind(&e.click_id)
            .bind(e.attempted_at)
            .bind(&e.content_type)
            .bind(&e.requested_title)
            .bind(&e.candidate_title)
            .bind(&e.indexer_name)
            .bind(e.size)
            .bind(e.rank_index)
            .bind(e.result.as_i16())
            .bind(&e.fail_reason)
            .bind(e.duration_ms)
            .bind(e.is_winner)
            .bind(&e.provider_host)
            .bind(&e.queue_item_id)
            .bind(&e.content_group_key)
            .fetch_one(pool)
            .await?
        ))
    }

    /// v1 `GetWatchdogEntries`: latest first, capped.
    pub async fn watchdog_list(&self, limit: i64) -> Result<Vec<WatchdogEntry>, Error> {
        let limit = limit.clamp(1, 5000);
        Ok(on!(
            self,
            pool => sqlx::query_as(
                "SELECT id, click_id, attempted_at, content_type, requested_title, candidate_title, \
                    indexer_name, size, rank_index, result, fail_reason, duration_ms, is_winner, \
                    provider_host, queue_item_id, content_group_key \
                 FROM watchdog_entries ORDER BY attempted_at DESC, id DESC LIMIT $1"
            )
            .bind(limit)
            .fetch_all(pool)
            .await?)
        )
    }

    /// v1 `ClearWatchdogEntries`: delete all (or up to `limit`) entries.
    pub async fn watchdog_clear(&self) -> Result<i64, Error> {
        Ok(on!(
            self,
            pool => sqlx::query("DELETE FROM watchdog_entries").execute(pool).await?.rows_affected().cast_signed()
        ))
    }

    /// v1 `WatchdogPurgeService`: delete entries older than `max_age_ms` (and cap the table).
    pub async fn watchdog_purge(&self, max_age_ms: i64, cap: i64) -> Result<i64, Error> {
        on!(
            self,
            pool => {
                let cutoff = jiff::Timestamp::now().as_millisecond() - max_age_ms;
                let mut removed = sqlx::query("DELETE FROM watchdog_entries WHERE attempted_at < $1")
                    .bind(cutoff)
                    .execute(pool)
                    .await?
                    .rows_affected().cast_signed();
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM watchdog_entries")
                    .fetch_one(pool)
                    .await?;
                if count > cap {
                    removed += sqlx::query(
                        "DELETE FROM watchdog_entries WHERE id IN ( \
                            SELECT id FROM watchdog_entries ORDER BY attempted_at DESC, id DESC LIMIT -1 OFFSET $1 \
                        )"
                    )
                    .bind(cap)
                    .execute(pool)
                    .await?
                    .rows_affected().cast_signed();
                }
                Ok(removed)
            }
        )
    }
}
