//! Prowlarr pull-sync merge, ported from `backend/Services/ProwlarrIndexerSync.cs` (v1 fork).
//! Pure function over the indexer/profile config: reconciles remote Prowlarr indexers with
//! locally managed entries (owned via `prowlarr_indexer_id`), preserving local tuning fields.

use std::collections::{HashMap, HashSet};

use infinidysk_search::profile::ProfileConfig;

use crate::models::{ConnectionDetails, IndexerConfig};
use crate::prowlarr::{ProwlarrClient, ProwlarrIndexer};

/// Ported from `ProwlarrSyncMergeResult`.
#[derive(Debug, Clone, Default)]
pub struct MergeResult {
    pub remote_indexer_count: usize,
    pub managed_indexer_count: usize,
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub skipped: usize,
    pub skipped_details: Vec<String>,
    pub profiles_changed: bool,
}

impl MergeResult {
    #[must_use]
    pub fn indexers_changed(&self) -> bool {
        self.added > 0 || self.updated > 0 || self.removed > 0
    }
}

fn is_supported(indexer: &ProwlarrIndexer) -> bool {
    indexer
        .protocol
        .as_deref()
        .is_some_and(|p| p.eq_ignore_ascii_case("usenet"))
        && indexer.supports_search
}

/// Ported from `ProwlarrIndexerSync.Merge`. Mutates `indexer_config`/`profile_config` in place.
pub fn merge(
    indexer_config: &mut IndexerConfig,
    profile_config: &mut ProfileConfig,
    remote_indexers: &[ProwlarrIndexer],
    prowlarr_url: &str,
    prowlarr_api_key: &str,
) -> MergeResult {
    let mut result = MergeResult {
        remote_indexer_count: remote_indexers.len(),
        ..Default::default()
    };

    // A copied config can contain duplicate ownership metadata; the first entry wins.
    let mut managed_by_id: HashMap<i32, usize> = HashMap::new();
    for (i, managed) in indexer_config.indexers.iter().enumerate() {
        if let Some(id) = managed.prowlarr_indexer_id {
            managed_by_id.entry(id).or_insert(i);
        }
    }

    let reserved_names: HashSet<String> = indexer_config
        .indexers
        .iter()
        .filter(|x| x.prowlarr_indexer_id.is_none())
        .map(|x| x.name.trim().to_lowercase())
        .collect();
    let mut used_managed_names: HashSet<String> = HashSet::new();
    let mut kept_managed_ids: HashSet<i32> = HashSet::new();
    let mut seen_remote_ids: HashSet<i32> = HashSet::new();
    let mut renames: HashMap<String, String> = HashMap::new();

    let mut remotes: Vec<&ProwlarrIndexer> = remote_indexers.iter().collect();
    remotes.sort_by(|a, b| {
        let ka = (a.name.clone().unwrap_or_default().to_lowercase(), a.id);
        let kb = (b.name.clone().unwrap_or_default().to_lowercase(), b.id);
        ka.cmp(&kb)
    });

    for remote in remotes {
        if !seen_remote_ids.insert(remote.id) {
            result.skipped += 1;
            result.skipped_details.push(format!(
                "{}: duplicate Prowlarr indexer ID {}",
                remote.name.clone().unwrap_or_default(),
                remote.id
            ));
            continue;
        }

        let existing = managed_by_id.get(&remote.id).copied();
        if !is_supported(remote) {
            // Existing managed entries are removed below; new unsupported ones are skipped.
            if existing.is_none() {
                result.skipped += 1;
                result.skipped_details.push(format!(
                    "{}: not a searchable Usenet indexer",
                    remote.name.clone().unwrap_or_default()
                ));
            }
            continue;
        }

        // A disabled Prowlarr indexer updates an existing managed entry to disabled, but is
        // not newly imported until enabled upstream.
        if !remote.enable && existing.is_none() {
            result.skipped += 1;
            result.skipped_details.push(format!(
                "{}: disabled in Prowlarr",
                remote.name.clone().unwrap_or_default()
            ));
            continue;
        }

        let remote_name = remote.name.clone().unwrap_or_default().trim().to_owned();
        let mut final_name = remote_name.clone();
        let mut name_skipped = false;
        if reserved_names.contains(&remote_name.to_lowercase())
            || used_managed_names.contains(&remote_name.to_lowercase())
        {
            if existing.is_none() {
                result.skipped += 1;
                result.skipped_details.push(format!(
                    "{remote_name}: conflicts with an existing indexer name"
                ));
                continue;
            }
            // Keep the prior local name rather than introducing an ambiguous name-based
            // search-profile reference; other owned fields still sync.
            if let Some(idx) = existing {
                final_name.clone_from(&indexer_config.indexers[idx].name);
            }
            name_skipped = true;
        }

        kept_managed_ids.insert(remote.id);
        used_managed_names.insert(final_name.to_lowercase());

        let next_url = ProwlarrClient::build_indexer_api_url(prowlarr_url, remote.id);
        if let Some(idx) = existing {
            let entry = &mut indexer_config.indexers[idx];
            let mut changed = false;
            if entry.name != final_name {
                renames.insert(entry.name.clone(), final_name.clone());
                entry.name.clone_from(&final_name);
                changed = true;
            }
            if entry.url != next_url {
                entry.url = next_url;
                changed = true;
            }
            if entry.api_key != prowlarr_api_key {
                prowlarr_api_key.clone_into(&mut entry.api_key);
                changed = true;
            }
            if entry.enabled != remote.enable {
                entry.enabled = remote.enable;
                changed = true;
            }
            if name_skipped {
                result.skipped += 1;
                result.skipped_details.push(format!(
                    "{remote_name}: name conflicts with an existing indexer; kept {final_name}"
                ));
            }
            if changed {
                result.updated += 1;
            }
        } else {
            indexer_config.indexers.push(ConnectionDetails {
                name: final_name,
                url: next_url,
                api_key: prowlarr_api_key.to_owned(),
                enabled: remote.enable,
                prowlarr_indexer_id: Some(remote.id),
                ..Default::default()
            });
            result.added += 1;
        }
    }

    let removed_names: Vec<String> = indexer_config
        .indexers
        .iter()
        .enumerate()
        .filter(|(_, x)| {
            x.prowlarr_indexer_id
                .is_some_and(|id| !kept_managed_ids.contains(&id))
        })
        .map(|(i, x)| (i, x.name.clone()))
        .collect::<Vec<_>>()
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    // remove in descending index order to keep indices valid
    let mut removed_idx: Vec<usize> = indexer_config
        .indexers
        .iter()
        .enumerate()
        .filter(|(_, x)| {
            x.prowlarr_indexer_id
                .is_some_and(|id| !kept_managed_ids.contains(&id))
        })
        .map(|(i, _)| i)
        .collect();
    removed_idx.sort_unstable_by(|a, b| b.cmp(a));
    for i in removed_idx {
        indexer_config.indexers.remove(i);
        result.removed += 1;
    }

    apply_profile_changes(profile_config, &renames, &removed_names, &mut result);
    result.managed_indexer_count = indexer_config
        .indexers
        .iter()
        .filter(|x| x.prowlarr_indexer_id.is_some())
        .count();
    result
}

fn apply_profile_changes(
    profile_config: &mut ProfileConfig,
    renames: &HashMap<String, String>,
    removed_names: &[String],
    result: &mut MergeResult,
) {
    if renames.is_empty() && removed_names.is_empty() {
        return;
    }
    let removed: HashSet<&str> = removed_names.iter().map(String::as_str).collect();

    for profile in &mut profile_config.profiles {
        let mut next: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for old_name in &profile.indexer_names {
            if removed.contains(old_name.as_str()) {
                continue;
            }
            let name = renames
                .get(old_name)
                .cloned()
                .unwrap_or_else(|| old_name.clone());
            if seen.insert(name.clone()) {
                next.push(name);
            }
        }
        if next != profile.indexer_names {
            profile.indexer_names = next;
            result.profiles_changed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use infinidysk_search::profile::Profile;

    fn remote(
        id: i32,
        name: &str,
        enable: bool,
        supports_search: bool,
        protocol: &str,
    ) -> ProwlarrIndexer {
        ProwlarrIndexer {
            id,
            name: Some(name.to_owned()),
            enable,
            supports_search,
            protocol: Some(protocol.to_owned()),
        }
    }

    fn managed(id: i32, name: &str) -> ConnectionDetails {
        ConnectionDetails {
            name: name.to_owned(),
            url: format!("http://prowlarr/{id}/api"),
            api_key: "key".into(),
            prowlarr_indexer_id: Some(id),
            ..Default::default()
        }
    }

    // Ported from ProwlarrIndexerSyncTests.Merge_ImportsEnabledSearchableUsenetIndexersThroughProwlarrProxy.
    #[test]
    fn imports_enabled_searchable_usenet_indexers_through_prowlarr_proxy() {
        let mut indexers = IndexerConfig::default();
        let mut profiles = ProfileConfig::default();
        let result = merge(
            &mut indexers,
            &mut profiles,
            &[
                remote(7, "Usenet One", true, true, "usenet"),
                remote(8, "Torrent", true, true, "torrent"),
                remote(9, "Disabled", false, true, "usenet"),
                remote(10, "No Search", true, false, "usenet"),
            ],
            "http://prowlarr:9696/prowlarr/",
            "prowlarr-key",
        );
        assert_eq!(indexers.indexers.len(), 1);
        let imported = &indexers.indexers[0];
        assert_eq!(imported.name, "Usenet One");
        assert_eq!(imported.url, "http://prowlarr:9696/prowlarr/7/api");
        assert_eq!(imported.api_key, "prowlarr-key");
        assert!(imported.enabled);
        assert_eq!(imported.prowlarr_indexer_id, Some(7));
        assert_eq!(result.added, 1);
        assert_eq!(result.skipped, 3);
        assert_eq!(result.remote_indexer_count, 4);
    }

    // Ported from Merge_UpdatesOwnedFieldsAndPreservesLocalTuning.
    #[test]
    fn updates_owned_fields_and_preserves_local_tuning() {
        let mut entry = managed(7, "Old Name");
        entry.url = "http://old/7/api".into();
        entry.api_key = "old-key".into();
        entry.max_requests_per_minute = 12;
        entry.hit_limit = Some(100);
        entry.proxy_url = Some("http://proxy:8888".into());
        entry.enable_strict_matching = true;
        entry.filter = Some(crate::models::ResultFilter {
            enabled: true,
            min_grabs: 5,
            ..Default::default()
        });
        let mut indexers = IndexerConfig {
            indexers: vec![entry],
            ..Default::default()
        };

        let result = merge(
            &mut indexers,
            &mut ProfileConfig::default(),
            &[remote(7, "New Name", false, true, "usenet")],
            "http://prowlarr:9696",
            "new-key",
        );

        let updated = &indexers.indexers[0];
        assert_eq!(updated.name, "New Name");
        assert_eq!(updated.url, "http://prowlarr:9696/7/api");
        assert_eq!(updated.api_key, "new-key");
        assert!(!updated.enabled);
        assert_eq!(updated.max_requests_per_minute, 12);
        assert_eq!(updated.hit_limit, Some(100));
        assert_eq!(updated.proxy_url.as_deref(), Some("http://proxy:8888"));
        assert!(updated.enable_strict_matching);
        assert_eq!(updated.filter.as_ref().map(|f| f.min_grabs), Some(5));
        assert_eq!(result.updated, 1);
    }

    // Ported from Merge_RenamesProfileReferencesAndRemovesStaleManagedIndexers.
    #[test]
    fn renames_profile_references_and_removes_stale_managed_indexers() {
        let mut indexers = IndexerConfig {
            indexers: vec![
                managed(7, "Before"),
                managed(8, "Stale"),
                ConnectionDetails {
                    name: "Manual".into(),
                    url: "https://manual.example/api".into(),
                    api_key: "manual-key".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let mut profiles = ProfileConfig {
            profiles: vec![Profile {
                token: "token".into(),
                name: "profile".into(),
                indexer_names: vec!["Before".into(), "Stale".into(), "Manual".into()],
                ..Default::default()
            }],
        };

        let result = merge(
            &mut indexers,
            &mut profiles,
            &[remote(7, "After", true, true, "usenet")],
            "http://prowlarr:9696",
            "key",
        );

        let names: Vec<&str> = indexers.indexers.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["After", "Manual"]);
        assert_eq!(
            profiles.profiles[0].indexer_names,
            vec!["After".to_owned(), "Manual".to_owned()]
        );
        assert_eq!(result.updated, 1);
        assert_eq!(result.removed, 1);
        assert!(result.profiles_changed);
    }

    // Ported from Merge_IsIdempotentAndDoesNotModifyManualNameCollision.
    #[test]
    fn idempotent_and_preserves_manual_name_collision() {
        let mut indexers = IndexerConfig {
            indexers: vec![ConnectionDetails {
                name: "Shared".into(),
                url: "https://manual.example/api".into(),
                api_key: "manual-key".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut profiles = ProfileConfig::default();
        let remotes = [remote(7, "Shared", true, true, "usenet")];

        let first = merge(
            &mut indexers,
            &mut profiles,
            &remotes,
            "http://prowlarr:9696",
            "key",
        );
        let second = merge(
            &mut indexers,
            &mut profiles,
            &remotes,
            "http://prowlarr:9696",
            "key",
        );

        assert_eq!(indexers.indexers.len(), 1);
        assert_eq!(indexers.indexers[0].prowlarr_indexer_id, None);
        assert_eq!(indexers.indexers[0].api_key, "manual-key");
        assert_eq!(first.skipped, 1);
        assert_eq!(second.skipped, 1);
        assert!(!second.indexers_changed());
    }

    // Ported from Merge_DuplicateRemoteNamesImportOnlyTheFirstDeterministically.
    #[test]
    fn duplicate_remote_names_import_only_the_first_deterministically() {
        let mut indexers = IndexerConfig::default();
        let result = merge(
            &mut indexers,
            &mut ProfileConfig::default(),
            &[
                remote(9, "Same", true, true, "usenet"),
                remote(7, "Same", true, true, "usenet"),
            ],
            "http://prowlarr:9696",
            "key",
        );
        assert_eq!(indexers.indexers.len(), 1);
        assert_eq!(indexers.indexers[0].prowlarr_indexer_id, Some(7));
        assert_eq!(result.added, 1);
        assert_eq!(result.skipped, 1);
    }
}
