//! Warden dead-release store (v1 fork Phase 3), ported from `backend/Services/WardenStore.cs`.
//! Lives in the main database instead of v1's separate `warden.db` (documented deviation:
//! one database per instance also covers PostgreSQL).

use sqlx::FromRow;

use crate::{Db, Error, Pool, on};

pub const LOCAL_SOURCE_ID: &str = "local";
pub const TRUST_FULL: &str = "full";
pub const TRUST_CORROBORATE: &str = "corroborate";
pub const TRUST_OBSERVE: &str = "observe";

/// v1 `WardenSourceInfo`.
#[derive(Debug, Clone, FromRow)]
pub struct WardenSource {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub url: Option<String>,
    pub enabled: bool,
    pub trust: String,
    pub refresh_hours: i32,
    pub last_checked: i64,
    pub last_updated: i64,
    pub status: Option<String>,
    pub count: i64,
}

/// One dead fingerprint row: (`source_id`, `fp`, `dead_at`, `n`, `backbones`).
#[derive(Debug, Clone, FromRow)]
pub struct WardenEntry {
    pub source_id: String,
    pub fp: String,
    pub dead_at: i64,
    pub n: i32,
    pub backbones: String,
}

/// A remote-source bundle entry for import: (`fp`, `dead_at`, `n`, `backbones`).
pub type RemoteEntrySpec = (String, i64, i32, String);

impl Db {
    pub async fn warden_count(&self) -> Result<i64, Error> {
        Ok(on!(self, pool => sqlx::query_scalar("SELECT COUNT(*) FROM warden_entries")
            .fetch_one(pool)
            .await?))
    }

    pub async fn warden_local_count(&self) -> Result<i64, Error> {
        Ok(on!(
            self,
            pool => sqlx::query_scalar("SELECT COUNT(*) FROM warden_entries WHERE source_id = 'local'")
                .fetch_one(pool)
                .await?
        ))
    }

    /// v1 `EffectiveCount`: fingerprints covered by an enabled full-trust source, or by
    /// `quorum` enabled full/corroborate sources.
    pub async fn warden_effective_count(&self, quorum: i64) -> Result<i64, Error> {
        let q = quorum.max(1);
        Ok(on!(
            self,
            pool => sqlx::query_scalar(
                "SELECT COUNT(*) FROM ( \
                    SELECT e.fp FROM warden_entries e JOIN warden_sources s ON s.id = e.source_id \
                    WHERE s.enabled = 1 AND s.trust IN ('full','corroborate') \
                    GROUP BY e.fp \
                    HAVING MAX(s.trust = 'full') = 1 OR COUNT(*) >= $1 \
                )"
            )
            .bind(q)
            .fetch_one(pool)
            .await?)
        )
    }

    /// v1 `MarkDead` for the local source: upsert with n+1 and merged backbones.
    pub async fn warden_mark_dead(&self, fp: &str, backbones: &str) -> Result<(), Error> {
        let now = jiff::Timestamp::now().as_second();
        on!(
            self,
            pool => {
                let existing: Option<String> =
                    sqlx::query_scalar("SELECT backbones FROM warden_entries WHERE source_id = 'local' AND fp = $1")
                        .bind(fp)
                        .fetch_optional(pool)
                        .await?;
                let merged = merge_backbones(existing.as_deref().unwrap_or(""), backbones);
                sqlx::query(
                    "INSERT INTO warden_entries (source_id, fp, dead_at, n, backbones) \
                     VALUES ('local', $1, $2, 1, $3) \
                     ON CONFLICT(source_id, fp) DO UPDATE SET dead_at = $2, n = n + 1, backbones = $3",
                )
                .bind(fp)
                .bind(now)
                .bind(merged)
                .execute(pool)
                .await?;
                Ok(())
            }
        )
    }

    /// Rows behind one fingerprint from enabled full/corroborate sources:
    /// (`trust`, `backbones`, `source_id`). The app layer applies quorum and backbone scope
    /// (v1 `IsDeadAnywhere`).
    pub async fn warden_dead_rows(&self, fp: &str) -> Result<Vec<(String, String, String)>, Error> {
        if fp.is_empty() {
            return Ok(Vec::new());
        }
        Ok(on!(
            self,
            pool => sqlx::query_as(
                "SELECT s.trust, e.backbones, e.source_id FROM warden_entries e \
                 JOIN warden_sources s ON s.id = e.source_id \
                 WHERE e.fp = $1 AND s.enabled = 1 AND s.trust IN ('full','corroborate')"
            )
            .bind(fp)
            .fetch_all(pool)
            .await?
        ))
    }

    /// Fingerprints that currently count as dead (full-trust hit or quorum), for the
    /// in-memory hook mirror.
    pub async fn warden_effective_fps(&self) -> Result<Vec<String>, Error> {
        Ok(on!(
            self,
            pool => sqlx::query_scalar(
                "SELECT DISTINCT e.fp FROM warden_entries e JOIN warden_sources s ON s.id = e.source_id                  WHERE s.enabled = 1 AND s.trust = 'full'"
            )
            .fetch_all(pool)
            .await?
        ))
    }

    pub async fn warden_get_sources(&self) -> Result<Vec<WardenSource>, Error> {
        Ok(on!(
            self,
            pool => sqlx::query_as(
                "SELECT s.id, s.kind, s.name, s.url, s.enabled, s.trust, s.refresh_hours, \
                    s.last_checked, s.last_updated, s.status, \
                    (SELECT COUNT(*) FROM warden_entries e WHERE e.source_id = s.id) AS count \
                 FROM warden_sources s \
                 ORDER BY CASE WHEN s.id = 'local' THEN 0 ELSE 1 END, s.sort, s.name"
            )
            .fetch_all(pool)
            .await?)
        )
    }

    /// v1 `AddSource`.
    pub async fn warden_add_source(
        &self,
        id: &str,
        kind: &str,
        name: &str,
        url: Option<&str>,
        trust: &str,
        refresh_hours: i32,
    ) -> Result<(), Error> {
        on!(
            self,
            pool => {
                sqlx::query(
                    "INSERT INTO warden_sources (id, kind, name, url, enabled, trust, refresh_hours, last_checked, last_updated, status, sort) \
                     VALUES ($1, $2, $3, $4, 1, $5, $6, 0, 0, NULL, (SELECT COALESCE(MAX(sort),0)+1 FROM warden_sources))"
                )
                .bind(id)
                .bind(kind)
                .bind(name)
                .bind(url)
                .bind(trust)
                .bind(refresh_hours)
                .execute(pool)
                .await?;
                Ok(())
            }
        )
    }

    /// v1 `ImportRemoteSources`: add specs whose URL is not yet present. Returns (added, skipped).
    pub async fn warden_import_remote_sources(
        &self,
        specs: &[(String, String, Option<String>, String, i32)],
    ) -> Result<(i64, i64), Error> {
        if specs.is_empty() {
            return Ok((0, 0));
        }
        on!(
            self,
            pool => {
                let mut tx = pool.begin().await?;
                let existing: Vec<String> =
                    sqlx::query_scalar("SELECT url FROM warden_sources WHERE url IS NOT NULL")
                        .fetch_all(&mut *tx)
                        .await?;
                let mut known: std::collections::HashSet<String> =
                    existing.into_iter().collect();
                let mut added = 0i64;
                let mut skipped = 0i64;
                for (id, name, url, trust, refresh_hours) in specs {
                    let Some(url) = url.as_deref().map(str::trim).filter(|u| !u.is_empty()) else {
                        skipped += 1;
                        continue;
                    };
                    if !known.insert(url.to_ascii_lowercase()) {
                        skipped += 1;
                        continue;
                    }
                    let name = if name.trim().is_empty() {
                        url.split("://")
                            .nth(1)
                            .and_then(|rest| rest.split('/').next())
                            .and_then(|a| a.rsplit('@').next())
                            .map(str::trim)
                            .filter(|h| !h.is_empty())
                            .map_or_else(|| "Untitled".to_owned(), str::to_owned)
                    } else {
                        name.trim().to_owned()
                    };
                    sqlx::query(
                        "INSERT INTO warden_sources (id, kind, name, url, enabled, trust, refresh_hours, last_checked, last_updated, status, sort) \
                         VALUES ($1, 'remote', $2, $3, 1, $4, $5, 0, 0, NULL, (SELECT COALESCE(MAX(sort),0)+1 FROM warden_sources))"
                    )
                    .bind(id)
                    .bind(name)
                    .bind(url)
                    .bind(trust)
                    .bind(*refresh_hours)
                    .execute(&mut *tx)
                    .await?;
                    added += 1;
                }
                tx.commit().await?;
                Ok((added, skipped))
            }
        )
    }

    pub async fn warden_update_source(
        &self,
        id: &str,
        name: Option<&str>,
        url: Option<&str>,
        enabled: Option<bool>,
        trust: Option<&str>,
        refresh_hours: Option<i32>,
    ) -> Result<bool, Error> {
        Ok(on!(
            self,
            pool => {
                let res = sqlx::query(
                    "UPDATE warden_sources SET \
                        name = COALESCE($2, name), url = COALESCE($3, url), \
                        enabled = COALESCE($4, enabled), trust = COALESCE($5, trust), \
                        refresh_hours = COALESCE($6, refresh_hours) \
                     WHERE id = $1"
                )
                .bind(id)
                .bind(name)
                .bind(url)
                .bind(enabled)
                .bind(trust)
                .bind(refresh_hours)
                .execute(pool)
                .await?;
                res.rows_affected() > 0
            }
        ))
    }

    /// Deletes a source and its entries (v1 `DeleteSource`); the local source cannot be deleted.
    pub async fn warden_delete_source(&self, id: &str) -> Result<bool, Error> {
        if id == LOCAL_SOURCE_ID {
            return Ok(false);
        }
        on!(
            self,
            pool => {
                let mut tx = pool.begin().await?;
                sqlx::query("DELETE FROM warden_entries WHERE source_id = $1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                let res = sqlx::query("DELETE FROM warden_sources WHERE id = $1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok(res.rows_affected() > 0)
            }
        )
    }

    /// Replaces one remote source's entries with the bundle's (v1 `SetSourceEntries`),
    /// capped at `max_entries`.
    pub async fn warden_set_source_entries(
        &self,
        source_id: &str,
        entries: &[RemoteEntrySpec],
        max_entries: i64,
        now: i64,
    ) -> Result<i64, Error> {
        on!(
            self,
            pool => {
                let mut tx = pool.begin().await?;
                sqlx::query("DELETE FROM warden_entries WHERE source_id = $1")
                    .bind(source_id)
                    .execute(&mut *tx)
                    .await?;
                let mut inserted = 0i64;
                for (fp, dead_at, n, backbones) in entries.iter().take(usize::try_from(max_entries.max(0)).unwrap_or(usize::MAX)) {
                    sqlx::query(
                        "INSERT INTO warden_entries (source_id, fp, dead_at, n, backbones) VALUES ($1, $2, $3, $4, $5) \
                         ON CONFLICT(source_id, fp) DO NOTHING"
                    )
                    .bind(source_id)
                    .bind(fp)
                    .bind(*dead_at)
                    .bind(*n)
                    .bind(backbones)
                    .execute(&mut *tx)
                    .await?;
                    inserted += 1;
                }
                sqlx::query("UPDATE warden_sources SET last_checked = $2, last_updated = $2, status = 'ok' WHERE id = $1")
                    .bind(source_id)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok(inserted)
            }
        )
    }

    pub async fn warden_source_status(
        &self,
        source_id: &str,
        status: &str,
        etag: Option<&str>,
        now: i64,
    ) -> Result<(), Error> {
        on!(
            self,
            pool => {
                sqlx::query(
                    "UPDATE warden_sources SET last_checked = $2, status = $3, etag = COALESCE($4, etag) WHERE id = $1"
                )
                .bind(source_id)
                .bind(now)
                .bind(status)
                .bind(etag)
                .execute(pool)
                .await?;
                Ok(())
            }
        )
    }

    /// All entries of one source, for backup/export (v1 `GetEntries`).
    pub async fn warden_entries_of(&self, source_id: &str) -> Result<Vec<WardenEntry>, Error> {
        Ok(on!(
            self,
            pool => sqlx::query_as(
                "SELECT source_id, fp, dead_at, n, backbones FROM warden_entries WHERE source_id = $1 ORDER BY dead_at DESC"
            )
            .bind(source_id)
            .fetch_all(pool)
            .await?)
        )
    }

    /// Replaces the whole store (import): delete all non-local sources + entries, insert the given ones.
    pub async fn warden_replace_all(
        &self,
        sources: &[WardenSource],
        entries: &[WardenEntry],
    ) -> Result<(), Error> {
        on!(
            self,
            pool => {
                let mut tx = pool.begin().await?;
                sqlx::query("DELETE FROM warden_entries WHERE source_id <> 'local'").execute(&mut *tx).await?;
                sqlx::query("DELETE FROM warden_sources WHERE id <> 'local'").execute(&mut *tx).await?;
                for s in sources {
                    if s.id == LOCAL_SOURCE_ID {
                        continue;
                    }
                    sqlx::query(
                        "INSERT INTO warden_sources (id, kind, name, url, enabled, trust, refresh_hours, last_checked, last_updated, etag, status, sort) \
                         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) ON CONFLICT (id) DO NOTHING"
                    )
                    .bind(&s.id)
                    .bind(&s.kind)
                    .bind(&s.name)
                    .bind(&s.url)
                    .bind(s.enabled)
                    .bind(&s.trust)
                    .bind(s.refresh_hours)
                    .bind(s.last_checked)
                    .bind(s.last_updated)
                    .bind(None::<String>)
                    .bind(&s.status)
                    .bind(0i32)
                    .execute(&mut *tx)
                    .await?;
                }
                for e in entries {
                    sqlx::query(
                        "INSERT INTO warden_entries (source_id, fp, dead_at, n, backbones) VALUES ($1,$2,$3,$4,$5) \
                         ON CONFLICT(source_id, fp) DO UPDATE SET dead_at = $3, n = $4, backbones = $5"
                    )
                    .bind(&e.source_id)
                    .bind(&e.fp)
                    .bind(e.dead_at)
                    .bind(e.n)
                    .bind(&e.backbones)
                    .execute(&mut *tx)
                    .await?;
                }
                tx.commit().await?;
                Ok(())
            }
        )
    }

    /// Deletes entries of a source beyond the cap, oldest first (v1 `PruneSourceEntries`).
    pub async fn warden_prune_source_entries(&self, source_id: &str, max_entries: i64) -> Result<i64, Error> {
        let max_entries = max_entries.max(0);
        Ok(on!(
            self,
            pool => sqlx::query(
                "DELETE FROM warden_entries WHERE source_id = $1 AND fp NOT IN ( \
                    SELECT fp FROM warden_entries WHERE source_id = $1 ORDER BY dead_at DESC LIMIT $2 \
                )"
            )
            .bind(source_id)
            .bind(max_entries)
            .execute(pool)
            .await?
            .rows_affected().cast_signed()
        ))
    }
}

/// v1 `MergeBackbones`: union of comma-separated hosts, sorted, deduped.
#[must_use]
pub fn merge_backbones(existing: &str, current: &str) -> String {
    let set: std::collections::BTreeSet<String> = existing
        .split(',')
        .chain(current.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    if set.is_empty() {
        return String::new();
    }
    set.into_iter().collect::<Vec<_>>().join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backbones_merge_sorted_deduped() {
        assert_eq!(merge_backbones("b.com,a.com", "c.com,a.com"), "a.com,b.com,c.com");
        assert_eq!(merge_backbones("", ""), "");
    }
}
