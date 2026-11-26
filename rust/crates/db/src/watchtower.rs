//! v1-fork Watchtower tables (migration 0014), ported from
//! `backend/Database/Models/WantedItem.cs`, `ListSource.cs` and the
//! `WatchtowerService`/`WtReconcile` queries: list sources and wanted items
//! with their provenance/shortlist JSON columns.

use uuid::Uuid;

use crate::{Db, Error, Pool, on};

/// v1 `ListSource` row. Unix-second timestamps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListSourceRow {
    pub id: Uuid,
    pub kind: String,
    pub name: String,
    pub url: Option<String>,
    pub enabled: bool,
    pub cap: i32,
    pub series_scope: Option<String>,
    pub created_at: i64,
    pub last_synced_at: Option<i64>,
    pub last_sync_error: Option<String>,
}

/// v1 `ListSource.Kind*`.
pub const KIND_MANUAL: &str = "manual";
pub const KIND_STREMIO_CATALOG: &str = "stremio-catalog";
pub const KIND_URL_LIST: &str = "url-list";

/// v1 `WantedItem` row. Unix-second timestamps; `provenance`/`shortlist` are JSON text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WantedItemRow {
    pub id: Uuid,
    pub key: String,
    pub r#type: String,
    pub content_id: String,
    pub title: String,
    pub state: String,
    pub provenance: String,
    pub shortlist: String,
    pub winner_nzb: Option<Vec<u8>>,
    pub responder_host: Option<String>,
    pub fail_reason: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_resolved_at: Option<i64>,
    pub last_verified_at: Option<i64>,
    pub next_check_at: Option<i64>,
}

/// v1 `WantedItem.State*`.
pub const STATE_SCOUTING: &str = "scouting";
pub const STATE_READY: &str = "ready";
pub const STATE_UNAVAILABLE: &str = "unavailable";
pub const STATE_PARKED: &str = "parked";
pub const STATE_EXPANDER: &str = "expander";

/// v1 `WantedItem.IsBareSeries`.
#[must_use]
pub fn is_bare_series(r#type: &str, content_id: &str) -> bool {
    r#type == "series" && content_id.split(':').count() < 3
}

const SOURCE_COLUMNS: &str =
    "id, kind, name, url, enabled, cap, series_scope, created_at, last_synced_at, last_sync_error";
const WANTED_COLUMNS: &str = "id, key, type, content_id, title, state, provenance, shortlist, \
     winner_nzb, responder_host, fail_reason, created_at, updated_at, last_resolved_at, \
     last_verified_at, next_check_at";

#[derive(sqlx::FromRow)]
struct SourceRow {
    id: Uuid,
    kind: String,
    name: String,
    url: Option<String>,
    enabled: bool,
    cap: i32,
    series_scope: Option<String>,
    created_at: i64,
    last_synced_at: Option<i64>,
    last_sync_error: Option<String>,
}

impl From<SourceRow> for ListSourceRow {
    fn from(r: SourceRow) -> Self {
        Self {
            id: r.id,
            kind: r.kind,
            name: r.name,
            url: r.url,
            enabled: r.enabled,
            cap: r.cap,
            series_scope: r.series_scope,
            created_at: r.created_at,
            last_synced_at: r.last_synced_at,
            last_sync_error: r.last_sync_error,
        }
    }
}

#[derive(sqlx::FromRow)]
struct WantedRow {
    id: Uuid,
    key: String,
    r#type: String,
    content_id: String,
    title: String,
    state: String,
    provenance: String,
    shortlist: String,
    winner_nzb: Option<Vec<u8>>,
    responder_host: Option<String>,
    fail_reason: Option<String>,
    created_at: i64,
    updated_at: i64,
    last_resolved_at: Option<i64>,
    last_verified_at: Option<i64>,
    next_check_at: Option<i64>,
}

impl From<WantedRow> for WantedItemRow {
    fn from(r: WantedRow) -> Self {
        Self {
            id: r.id,
            key: r.key,
            r#type: r.r#type,
            content_id: r.content_id,
            title: r.title,
            state: r.state,
            provenance: r.provenance,
            shortlist: r.shortlist,
            winner_nzb: r.winner_nzb,
            responder_host: r.responder_host,
            fail_reason: r.fail_reason,
            created_at: r.created_at,
            updated_at: r.updated_at,
            last_resolved_at: r.last_resolved_at,
            last_verified_at: r.last_verified_at,
            next_check_at: r.next_check_at,
        }
    }
}

impl Db {
    /// All list sources, oldest first (v1 `GetWatchtowerController` ordering).
    pub async fn list_sources_all(&self) -> Result<Vec<ListSourceRow>, Error> {
        let sql = format!("SELECT {SOURCE_COLUMNS} FROM list_sources ORDER BY created_at");
        let rows = on!(self, pool => sqlx::query_as::<_, SourceRow>(sqlx::AssertSqlSafe(sql))
            .fetch_all(pool)
            .await?);
        Ok(rows.into_iter().map(ListSourceRow::from).collect())
    }

    /// Enabled non-manual sources (v1 `SyncDueSourcesAsync`).
    pub async fn enabled_remote_sources(&self) -> Result<Vec<ListSourceRow>, Error> {
        let sql =
            format!("SELECT {SOURCE_COLUMNS} FROM list_sources WHERE enabled = 1 AND kind <> $1");
        let rows = on!(self, pool => sqlx::query_as::<_, SourceRow>(sqlx::AssertSqlSafe(sql))
            .bind(KIND_MANUAL)
            .fetch_all(pool)
            .await?);
        Ok(rows.into_iter().map(ListSourceRow::from).collect())
    }

    /// Updates a source's sync bookkeeping (`last_synced_at`, `last_sync_error`).
    pub async fn update_source_sync(
        &self,
        id: Uuid,
        last_synced_at: i64,
        last_sync_error: Option<&str>,
    ) -> Result<(), Error> {
        on!(self, pool => {
            sqlx::query(
                "UPDATE list_sources SET last_synced_at = $1, last_sync_error = $2 WHERE id = $3",
            )
            .bind(last_synced_at)
            .bind(last_sync_error)
            .bind(id)
            .execute(pool)
            .await?;
            Ok::<_, Error>(())
        })
    }

    /// Clears `last_synced_at` so the next cycle re-syncs the source (v1 `SyncSourceAsync`).
    pub async fn clear_source_sync(&self, id: Uuid) -> Result<(), Error> {
        on!(self, pool => {
            sqlx::query("UPDATE list_sources SET last_synced_at = NULL WHERE id = $1")
                .bind(id)
                .execute(pool)
                .await?;
            Ok::<_, Error>(())
        })
    }

    /// Inserts or replaces a list source.
    pub async fn upsert_source(&self, s: &ListSourceRow) -> Result<(), Error> {
        on!(self, pool => {
            sqlx::query(
                "INSERT INTO list_sources (id, kind, name, url, enabled, cap, series_scope, \
                 created_at, last_synced_at, last_sync_error) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) \
                 ON CONFLICT (id) DO UPDATE SET kind=$2, name=$3, url=$4, enabled=$5, cap=$6, \
                 series_scope=$7, last_synced_at=$9, last_sync_error=$10",
            )
            .bind(s.id)
            .bind(&s.kind)
            .bind(&s.name)
            .bind(&s.url)
            .bind(s.enabled)
            .bind(s.cap)
            .bind(&s.series_scope)
            .bind(s.created_at)
            .bind(s.last_synced_at)
            .bind(&s.last_sync_error)
            .execute(pool)
            .await?;
            Ok::<_, Error>(())
        })
    }

    /// Deletes a list source by id.
    pub async fn delete_source(&self, id: Uuid) -> Result<u64, Error> {
        let n = on!(self, pool => sqlx::query("DELETE FROM list_sources WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await?
            .rows_affected());
        Ok(n)
    }

    /// Wanted-item counts grouped by state (v1 stats).
    pub async fn wanted_counts_by_state(&self) -> Result<Vec<(String, i64)>, Error> {
        Ok(on!(self, pool => sqlx::query_as::<_, (String, i64)>(
            "SELECT state, COUNT(*) FROM wanted_items GROUP BY state"
        )
        .fetch_all(pool)
        .await?))
    }

    /// One wanted item by its unique key.
    pub async fn wanted_by_key(&self, key: &str) -> Result<Option<WantedItemRow>, Error> {
        let sql = format!("SELECT {WANTED_COLUMNS} FROM wanted_items WHERE key = $1");
        let row = on!(self, pool => sqlx::query_as::<_, WantedRow>(sqlx::AssertSqlSafe(sql))
            .bind(key)
            .fetch_optional(pool)
            .await?);
        Ok(row.map(WantedItemRow::from))
    }

    /// Wanted items whose key is in `keys` (chunked like v1's 400-row slices).
    pub async fn wanted_by_keys(&self, keys: &[String]) -> Result<Vec<WantedItemRow>, Error> {
        let mut out = Vec::new();
        for chunk in keys.chunks(400) {
            if chunk.is_empty() {
                continue;
            }
            let placeholders: Vec<String> = (1..=chunk.len()).map(|i| format!("${i}")).collect();
            let sql = format!(
                "SELECT {WANTED_COLUMNS} FROM wanted_items WHERE key IN ({})",
                placeholders.join(",")
            );
            let rows = on!(self, pool => {
                let mut q = sqlx::query_as::<_, WantedRow>(sqlx::AssertSqlSafe(sql));
                for k in chunk {
                    q = q.bind(k);
                }
                q.fetch_all(pool).await?
            });
            out.extend(rows.into_iter().map(WantedItemRow::from));
        }
        Ok(out)
    }

    /// Due expander rows (v1 `ExpandDueExpandersAsync`).
    pub async fn wanted_due_expanders(
        &self,
        now: i64,
        limit: i64,
    ) -> Result<Vec<WantedItemRow>, Error> {
        let sql = format!(
            "SELECT {WANTED_COLUMNS} FROM wanted_items \
             WHERE state = $1 AND (next_check_at IS NULL OR next_check_at <= $2) \
             ORDER BY next_check_at LIMIT $3"
        );
        let rows = on!(self, pool => sqlx::query_as::<_, WantedRow>(sqlx::AssertSqlSafe(sql))
            .bind(STATE_EXPANDER)
            .bind(now)
            .bind(limit)
            .fetch_all(pool)
            .await?);
        Ok(rows.into_iter().map(WantedItemRow::from).collect())
    }

    /// Due resolve rows: scouting, or unavailable with `next_check` due
    /// (v1 `ResolveDueItemsAsync`), newest first.
    pub async fn wanted_due_resolve(
        &self,
        now: i64,
        limit: i64,
    ) -> Result<Vec<WantedItemRow>, Error> {
        let sql = format!(
            "SELECT {WANTED_COLUMNS} FROM wanted_items \
             WHERE state = $1 OR (state = $2 AND next_check_at IS NOT NULL AND next_check_at <= $3) \
             ORDER BY created_at DESC LIMIT $4"
        );
        let rows = on!(self, pool => sqlx::query_as::<_, WantedRow>(sqlx::AssertSqlSafe(sql))
            .bind(STATE_SCOUTING)
            .bind(STATE_UNAVAILABLE)
            .bind(now)
            .bind(limit)
            .fetch_all(pool)
            .await?);
        Ok(rows.into_iter().map(WantedItemRow::from).collect())
    }

    /// Due keep-fresh rows (v1 `KeepFreshDueItemsAsync`), soonest first.
    pub async fn wanted_due_keepfresh(
        &self,
        now: i64,
        limit: i64,
    ) -> Result<Vec<WantedItemRow>, Error> {
        let sql = format!(
            "SELECT {WANTED_COLUMNS} FROM wanted_items \
             WHERE state = $1 AND next_check_at IS NOT NULL AND next_check_at <= $2 \
             ORDER BY next_check_at LIMIT $3"
        );
        let rows = on!(self, pool => sqlx::query_as::<_, WantedRow>(sqlx::AssertSqlSafe(sql))
            .bind(STATE_READY)
            .bind(now)
            .bind(limit)
            .fetch_all(pool)
            .await?);
        Ok(rows.into_iter().map(WantedItemRow::from).collect())
    }

    /// Rows whose provenance JSON mentions `needle` (v1 `Provenance.Contains(...)`).
    pub async fn wanted_claimed_by(&self, needle: &str) -> Result<Vec<WantedItemRow>, Error> {
        let sql = format!(
            "SELECT {WANTED_COLUMNS} FROM wanted_items WHERE provenance LIKE $1 ESCAPE '\\'"
        );
        let pattern = format!(
            "%{}%",
            needle
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let rows = on!(self, pool => sqlx::query_as::<_, WantedRow>(sqlx::AssertSqlSafe(sql))
            .bind(pattern)
            .fetch_all(pool)
            .await?);
        Ok(rows.into_iter().map(WantedItemRow::from).collect())
    }

    /// Rows of type `season` whose `content_id` starts with `prefix` (parked-fallback lookups).
    pub async fn season_ids_with_prefix(
        &self,
        prefix: &str,
        state: Option<&str>,
    ) -> Result<Vec<String>, Error> {
        let sql = match state {
            Some(_) => "SELECT content_id FROM wanted_items WHERE type = 'season' \
                        AND content_id LIKE $1 ESCAPE '\\' AND state = $2"
                .to_owned(),
            None => "SELECT content_id FROM wanted_items WHERE type = 'season' \
                     AND content_id LIKE $1 ESCAPE '\\'"
                .to_owned(),
        };
        let pattern = format!(
            "{}%",
            prefix
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let rows = on!(self, pool => {
            let mut q = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql)).bind(pattern);
            if let Some(st) = state {
                q = q.bind(st);
            }
            q.fetch_all(pool).await?
        });
        Ok(rows)
    }

    /// Inserts a wanted item (fails on duplicate key, like v1's unique index).
    pub async fn insert_wanted(&self, w: &WantedItemRow) -> Result<(), Error> {
        on!(self, pool => {
            sqlx::query(
                "INSERT INTO wanted_items (id, key, type, content_id, title, state, provenance, \
                 shortlist, winner_nzb, responder_host, fail_reason, created_at, updated_at, \
                 last_resolved_at, last_verified_at, next_check_at) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)"
            )
            .bind(w.id)
            .bind(&w.key)
            .bind(&w.r#type)
            .bind(&w.content_id)
            .bind(&w.title)
            .bind(&w.state)
            .bind(&w.provenance)
            .bind(&w.shortlist)
            .bind(&w.winner_nzb)
            .bind(&w.responder_host)
            .bind(&w.fail_reason)
            .bind(w.created_at)
            .bind(w.updated_at)
            .bind(w.last_resolved_at)
            .bind(w.last_verified_at)
            .bind(w.next_check_at)
            .execute(pool)
            .await?;
            Ok::<_, Error>(())
        })
    }

    /// Rewrites a wanted item's mutable columns (read-modify-write under a write permit).
    pub async fn update_wanted(&self, w: &WantedItemRow) -> Result<(), Error> {
        on!(self, pool => {
            sqlx::query(
                "UPDATE wanted_items SET key=$2, type=$3, content_id=$4, title=$5, state=$6, \
                 provenance=$7, shortlist=$8, winner_nzb=$9, responder_host=$10, fail_reason=$11, \
                 created_at=$12, updated_at=$13, last_resolved_at=$14, last_verified_at=$15, \
                 next_check_at=$16 WHERE id=$1"
            )
            .bind(w.id)
            .bind(&w.key)
            .bind(&w.r#type)
            .bind(&w.content_id)
            .bind(&w.title)
            .bind(&w.state)
            .bind(&w.provenance)
            .bind(&w.shortlist)
            .bind(&w.winner_nzb)
            .bind(&w.responder_host)
            .bind(&w.fail_reason)
            .bind(w.created_at)
            .bind(w.updated_at)
            .bind(w.last_resolved_at)
            .bind(w.last_verified_at)
            .bind(w.next_check_at)
            .execute(pool)
            .await?;
            Ok::<_, Error>(())
        })
    }

    /// Deletes wanted items by id (chunked like v1 `WtReconcile.DeleteByIdsAsync`).
    pub async fn delete_wanted_by_ids(&self, ids: &[Uuid]) -> Result<u64, Error> {
        let mut removed = 0u64;
        for chunk in ids.chunks(400) {
            if chunk.is_empty() {
                continue;
            }
            let placeholders: Vec<String> = (1..=chunk.len()).map(|i| format!("${i}")).collect();
            let sql = format!(
                "DELETE FROM wanted_items WHERE id IN ({})",
                placeholders.join(",")
            );
            let n = on!(self, pool => {
                let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
                for id in chunk {
                    q = q.bind(*id);
                }
                q.execute(pool).await?.rows_affected()
            });
            removed += n;
        }
        Ok(removed)
    }

    /// Page of leaf items for the get route: optional state filter, substring search on
    /// `title/content_id`, optional provenance tag (expander children). Returns rows + total.
    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors the v1 GetWatchtowerController query surface"
    )]
    pub async fn wanted_page(
        &self,
        state: Option<&str>,
        search: Option<&str>,
        provenance_tag: Option<&str>,
        include_expanders: bool,
        sort: &str,
        offset: i64,
        limit: i64,
    ) -> Result<(Vec<WantedItemRow>, i64), Error> {
        let mut conds = Vec::new();
        let mut binds: Vec<String> = Vec::new();
        if !include_expanders {
            conds.push("state <> 'expander'".to_owned());
        }
        if let Some(s) = state {
            conds.push(format!("state = ${}", binds.len() + 1));
            binds.push(s.to_owned());
        }
        if let Some(tag) = provenance_tag {
            conds.push(format!("provenance LIKE ${} ESCAPE '\\'", binds.len() + 1));
            binds.push(format!(
                "%{}%",
                tag.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            ));
        }
        if let Some(q) = search.filter(|q| !q.trim().is_empty()) {
            conds.push(format!(
                "(LOWER(title) LIKE ${} ESCAPE '\\' OR LOWER(content_id) LIKE ${} ESCAPE '\\')",
                binds.len() + 1,
                binds.len() + 2
            ));
            let like = format!(
                "%{}%",
                q.to_lowercase()
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            );
            binds.push(like.clone());
            binds.push(like);
        }
        let where_clause = if conds.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conds.join(" AND "))
        };
        let order_by = match sort {
            "title" => "ORDER BY title ASC, key ASC",
            "recheck" => "ORDER BY (next_check_at IS NULL) ASC, next_check_at ASC, key ASC",
            "status" => {
                "ORDER BY CASE state WHEN 'unavailable' THEN 0 WHEN 'scouting' THEN 1 \
                          WHEN 'parked' THEN 2 WHEN 'ready' THEN 3 ELSE 4 END ASC, \
                          updated_at DESC, key ASC"
            }
            _ => "ORDER BY updated_at DESC, key ASC",
        };
        let count_sql = format!("SELECT COUNT(*) FROM wanted_items {where_clause}");
        let page_sql = format!(
            "SELECT {WANTED_COLUMNS} FROM wanted_items {where_clause} \
             {order_by} LIMIT ${} OFFSET ${}",
            binds.len() + 1,
            binds.len() + 2
        );
        let result = on!(self, pool => {
            let mut qc = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(count_sql));
            for b in &binds {
                qc = qc.bind(b);
            }
            let total = qc.fetch_one(pool).await?;
            let mut qp = sqlx::query_as::<_, WantedRow>(sqlx::AssertSqlSafe(page_sql));
            for b in &binds {
                qp = qp.bind(b);
            }
            let rows = qp
                .bind(limit)
                .bind(offset)
                .fetch_all(pool)
                .await?;
            Ok::<_, Error>((total, rows))
        })?;
        let (total, rows) = result;
        Ok((rows.into_iter().map(WantedItemRow::from).collect(), total))
    }

    /// Keys of wanted items matching the `remove-by-filter` / `recheck-by-filter` form
    /// (v1 `FilteredKeysAsync`).
    pub async fn wanted_keys_filtered(
        &self,
        state: Option<&str>,
        search: Option<&str>,
    ) -> Result<Vec<String>, Error> {
        let mut conds = Vec::new();
        let mut binds: Vec<String> = Vec::new();
        if let Some(s) = state.filter(|s| {
            matches!(
                *s,
                "ready" | "scouting" | "unavailable" | "parked" | "expander"
            )
        }) {
            conds.push(format!("state = ${}", binds.len() + 1));
            binds.push(s.to_owned());
        }
        if let Some(q) = search.filter(|q| !q.trim().is_empty()) {
            conds.push(format!(
                "(LOWER(title) LIKE ${} ESCAPE '\\' OR LOWER(content_id) LIKE ${} ESCAPE '\\')",
                binds.len() + 1,
                binds.len() + 2
            ));
            let like = format!(
                "%{}%",
                q.to_lowercase()
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            );
            binds.push(like.clone());
            binds.push(like);
        }
        let where_clause = if conds.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conds.join(" AND "))
        };
        let sql = format!("SELECT key FROM wanted_items {where_clause}");
        let keys = on!(self, pool => {
            let mut q = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql));
            for b in &binds {
                q = q.bind(b);
            }
            q.fetch_all(pool).await
        })?;
        Ok(keys)
    }

    /// Lightweight scan of every non-expander row (state, title, `content_id`, provenance),
    /// used for the shows overview tallies (v1 `BuildShowsAsync`).
    pub async fn wanted_leaf_scan(&self) -> Result<Vec<(String, String, String, String)>, Error> {
        let rows = on!(self, pool => sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT state, title, content_id, provenance FROM wanted_items WHERE state <> 'expander'",
        )
        .fetch_all(pool)
        .await)?;
        Ok(rows)
    }
}
