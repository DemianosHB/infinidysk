//! The canonical search-result model, ported from `NewznabClient.NewznabItem` in the
//! C# v1 fork (`backend/Clients/Indexers/NewznabClient.cs`). Pure data; produced by the
//! indexer client (`infinidysk-indexer`) and consumed by the ordering in [`crate::sorter`].

use jiff::Timestamp;

/// One Newznab search result item. `title`/`guid`/`nzb_url` are always present (empty
/// string when the indexer omitted them); everything else is optional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub guid: String,
    pub nzb_url: String,
    pub size: i64,
    pub posted: Option<Timestamp>,
    pub usenet_date: Option<Timestamp>,
    pub grabs: Option<i32>,
    pub comments: Option<i32>,
    pub password: Option<i32>,
    pub files: Option<i32>,
    pub group: Option<String>,
    pub poster: Option<String>,
    pub source_indexer_name: Option<String>,
    pub language: Option<String>,
    pub subs: Option<String>,
    pub info_hash: Option<String>,
}
