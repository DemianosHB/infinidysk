//! v1-fork play-route queries, ported from `ProfilePlayController` and
//! `ProfileStreamStateService` (v1): completed-history lookups by release file name,
//! in-flight queue lookups, and the file items below a completed job's folder.

use std::collections::BTreeSet;

use crate::{Db, Error, HistoryEntry, HistoryRow, Item, ItemRow, Pool, on};
use infinidysk_core::{ItemKind, JobId};

/// `SELECT … FROM history …` column list shared with `HistoryRow`.
const HISTORY_COLUMNS: &str = "id, created_at, file_name, job_name, category, status, \
     total_bytes, download_seconds, fail_message, download_dir, nzb_blob, storage";

/// `SELECT … FROM items …` column list shared with `ItemRow`.
const ITEM_COLUMNS: &str = "id, parent_id, name, path, kind, size, meta_blob, nzb_blob, \
     created_at, release_date, health_state, encrypted";

fn is_excluded(exclude: &BTreeSet<String>, id: uuid::Uuid) -> bool {
    let s = id.as_simple().to_string();
    exclude.iter().any(|e| e.eq_ignore_ascii_case(&s))
}

impl Db {
    /// v1 `TryResolveExistingAsync` / `FindCompletedReleaseAsync`: completed history rows
    /// whose file name is one of `file_names` (exact match, like EF's `Contains`), newest
    /// first, excluding broken history ids. `category` optionally narrows to one category.
    pub async fn completed_history_by_file_names(
        &self,
        file_names: &[String],
        category: Option<&str>,
        exclude: &[String],
    ) -> Result<Vec<HistoryEntry>, Error> {
        let excluded: BTreeSet<String> = exclude.iter().cloned().collect();
        let sql = format!(
            "SELECT {HISTORY_COLUMNS} FROM history \
             WHERE status = 1 AND file_name = $1 AND ($2 IS NULL OR category = $2)"
        );
        let rows = on!(self, pool => {
            let mut rows = Vec::new();
            for name in file_names {
                let found: Vec<HistoryRow> = sqlx::query_as::<_, HistoryRow>(sqlx::AssertSqlSafe(sql.clone()))
                    .bind(name)
                    .bind(category)
                    .fetch_all(pool)
                    .await?;
                rows.extend(found);
            }
            rows
        });
        let mut entries: Vec<HistoryEntry> = rows
            .into_iter()
            .filter(|r| !is_excluded(&excluded, r.id))
            .map(HistoryEntry::try_from)
            .collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| std::cmp::Reverse(e.created_at));
        Ok(entries)
    }

    /// v1 `CommitAsync` existing-queue lookup: the newest queued (not yet finished) job with
    /// this file name and category.
    pub async fn queue_item_by_file_name(
        &self,
        file_name: &str,
        category: &str,
    ) -> Result<Option<JobId>, Error> {
        let id = on!(self, pool => sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT id FROM queue WHERE file_name = $1 AND category = $2 \
             ORDER BY created_at DESC LIMIT 1"
        )
        .bind(file_name)
        .bind(category)
        .fetch_optional(pool)
        .await?);
        Ok(id.map(JobId))
    }

    /// v1 `FindInFlightQueueItemAsync`: the newest queued job whose file name is one of
    /// `file_names`. (v1 also matches a content-group key; the rust queue table has no such
    /// column — file-name matching only.)
    pub async fn in_flight_queue_by_file_names(
        &self,
        file_names: &[String],
    ) -> Result<Option<JobId>, Error> {
        let sql = "SELECT id, created_at FROM queue WHERE file_name = $1 ORDER BY created_at DESC LIMIT 1";
        let best = on!(self, pool => {
            let mut best: Option<(i64, uuid::Uuid)> = None;
            for name in file_names {
                let row: Option<(uuid::Uuid, i64)> = sqlx::query_as(sql)
                    .bind(name)
                    .fetch_optional(pool)
                    .await?;
                if let Some((id, created_at)) = row
                    && best.as_ref().is_none_or(|(c, _)| created_at > *c)
                {
                    best = Some((created_at, id));
                }
            }
            best
        });
        Ok(best.map(|(_, id)| JobId(id)))
    }

    /// v1 `ProfileStreamStateService.GetReadyNzbFileNamesAsync`: the `file_names` (matched
    /// case-insensitively, like v1's `ToLower`) that have a completed, non-broken history
    /// entry with at least one video file below the job folder.
    pub async fn ready_nzb_file_names(
        &self,
        file_names: &[String],
        exclude: &[String],
        is_video: impl Fn(&str) -> bool,
    ) -> Result<Vec<String>, Error> {
        let excluded: BTreeSet<String> = exclude.iter().cloned().collect();
        let sql = "SELECT h.id, h.file_name, i.name FROM history h \
             JOIN items d ON d.id = h.download_dir \
             JOIN items i ON substr(i.path, 1, length(d.path) + 1) = d.path || '/' \
             WHERE h.status = 1 AND LOWER(h.file_name) = LOWER($1) AND i.kind <> $2";
        let ready = on!(self, pool => {
            let mut ready = Vec::new();
            for name in file_names {
                let rows: Vec<(uuid::Uuid, String, String)> = sqlx::query_as::<_, (uuid::Uuid, String, String)>(sqlx::AssertSqlSafe(sql))
                    .bind(name)
                    .bind(ItemKind::Directory as i16)
                    .fetch_all(pool)
                    .await?;
                if rows
                    .iter()
                    .any(|(id, _, item_name)| !is_excluded(&excluded, *id) && is_video(item_name))
                {
                    ready.push(name.clone());
                }
            }
            ready
        });
        Ok(ready)
    }

    /// v1 `FindBestVideoAsync`'s items query: all file items below a completed job's
    /// folder (the history entry's `download_dir`), any depth.
    pub async fn history_file_items(&self, id: JobId) -> Result<Vec<Item>, Error> {
        let sql_items = format!(
            "SELECT {ITEM_COLUMNS} FROM items i \
             WHERE i.kind <> $1 AND substr(i.path, 1, length($2) + 1) = $2 || '/' \
             ORDER BY i.path"
        );
        let rows = on!(self, pool => {
            let dir_path: Option<String> = sqlx::query_scalar(
                "SELECT d.path FROM history h JOIN items d ON d.id = h.download_dir WHERE h.id = $1",
            )
            .bind(id.0)
            .fetch_optional(pool)
            .await?;
            match dir_path {
                None => Vec::new(),
                Some(dir_path) => {
                    sqlx::query_as::<_, ItemRow>(sqlx::AssertSqlSafe(sql_items))
                        .bind(ItemKind::Directory as i16)
                        .bind(&dir_path)
                        .fetch_all(pool)
                        .await?
                }
            }
        });
        rows.into_iter().map(Item::try_from).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excluded_ids_match_case_insensitively() {
        let id = uuid::Uuid::new_v4();
        let excluded = BTreeSet::from([id.as_simple().to_string().to_uppercase()]);
        assert!(is_excluded(&excluded, id));
        assert!(!is_excluded(&BTreeSet::from(["nope".to_owned()]), id));
    }
}
