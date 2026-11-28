//! Search-execution integration tests against a loopback Newznab indexer, characterizing
//! the v1 `SearchProfileService` behavior (paging, per-indexer limits, hit budgets, exclude
//! patterns, dedupe, category overrides, strict matching, fallback queries, play tokens).

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use common::{items, lit, query_param, rss, serve_newznab};
use futures_util::future::BoxFuture;
use infinidysk_indexer::models::{ConnectionDetails, IndexerConfig, ResultFilter};
use infinidysk_profiles::cache::Candidate;
use infinidysk_profiles::resolvers::ResolverConfig;
use infinidysk_profiles::{Resolvers, SearchService, SearchSettings, WarmerHook};
use infinidysk_search::profile::{FallbackMode, Profile, ProfileConfig};
use tokio_util::sync::CancellationToken;

fn indexer(name: &str, url: String) -> ConnectionDetails {
    ConnectionDetails {
        name: name.to_owned(),
        url,
        api_key: "key".into(),
        ..Default::default()
    }
}

fn profile(token: &str, indexer_names: Vec<String>) -> Profile {
    Profile {
        token: token.to_owned(),
        indexer_names,
        ..Default::default()
    }
}

fn service(profiles: ProfileConfig, indexers: IndexerConfig, exclude_raw: &str) -> SearchService {
    let cfg = ResolverConfig {
        enabled: false,
        ..Default::default()
    };
    let settings = SearchSettings {
        profiles,
        indexers,
        exclude_patterns_raw: exclude_raw.to_owned(),
        ..Default::default()
    };
    SearchService::new(settings, Resolvers::new(cfg))
}

fn query(params: &[(&str, &str)]) -> Vec<(String, String)> {
    params
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[tokio::test]
async fn search_pages_in_100_chunks_and_truncates_to_target() {
    // target 150 -> two pages (offset 0 and 100), result truncated to 150.
    let (addr, requests) = serve_newznab(Arc::new(|target| {
        let offset = query_param(target, "offset").unwrap_or_else(|| "0".into());
        assert_eq!(query_param(target, "limit").as_deref(), Some("100"));
        if offset == "0" {
            rss(&items("Show", 100))
        } else {
            rss(&items("Show2", 100))
        }
    }))
    .await;
    let svc = service(
        ProfileConfig {
            profiles: vec![profile("tok", vec![])],
        },
        IndexerConfig {
            search_result_limit: Some(150),
            indexers: vec![indexer("a", format!("http://{addr}/api"))],
            ..Default::default()
        },
        "",
    );
    let out = svc
        .search(
            "tok",
            "series",
            "tt1:1:1",
            &query(&[("t", "tvsearch"), ("q", "Show")]),
            &CancellationToken::new(),
            None,
            false,
            false,
        )
        .await
        .expect("outcome");
    assert_eq!(out.candidates.len(), 150);
    assert_eq!(out.play_tokens.len(), 150);
    let targets = requests.targets();
    assert_eq!(targets.len(), 2);
    assert_eq!(query_param(&targets[0], "offset").as_deref(), Some("0"));
    assert_eq!(query_param(&targets[1], "offset").as_deref(), Some("100"));
    // apikey/extended are prepended by the client.
    assert_eq!(query_param(&targets[0], "apikey").as_deref(), Some("key"));
    assert_eq!(query_param(&targets[0], "extended").as_deref(), Some("1"));
    // Play tokens resolve back to the candidate list.
    let entry = svc.cache().get(&out.play_tokens[0]).expect("entry");
    assert_eq!(entry.candidates.len(), 150);
    assert_eq!(entry.primary().nzb_url, out.candidates[0].nzb_url);
}

#[tokio::test]
async fn search_stops_on_short_page_without_extra_request() {
    let (addr, requests) = serve_newznab(Arc::new(|_| rss(&items("Show", 3)))).await;
    let svc = service(
        ProfileConfig {
            profiles: vec![profile("tok", vec![])],
        },
        IndexerConfig {
            search_result_limit: Some(500),
            indexers: vec![indexer("a", format!("http://{addr}/api"))],
            ..Default::default()
        },
        "",
    );
    let out = svc
        .search(
            "tok",
            "series",
            "tt1:1:1",
            &query(&[("t", "tvsearch")]),
            &CancellationToken::new(),
            None,
            false,
            false,
        )
        .await
        .expect("outcome");
    assert_eq!(out.candidates.len(), 3);
    assert_eq!(requests.count(), 1, "short page ends paging");
}

#[tokio::test]
async fn hit_limit_skips_the_indexer_on_the_next_search() {
    let (addr, requests) = serve_newznab(Arc::new(|_| rss(&items("Show", 2)))).await;
    let mut idx = indexer("a", format!("http://{addr}/api"));
    idx.hit_limit = Some(1);
    let svc = service(
        ProfileConfig {
            profiles: vec![profile("tok", vec![])],
        },
        IndexerConfig {
            indexers: vec![idx],
            ..Default::default()
        },
        "",
    );
    let ct = CancellationToken::new();
    let first = svc
        .search(
            "tok",
            "series",
            "tt1:1:1",
            &query(&[("t", "tvsearch")]),
            &ct,
            None,
            false,
            false,
        )
        .await
        .expect("outcome");
    assert_eq!(first.candidates.len(), 2);
    assert_eq!(requests.count(), 1);
    // The single recorded search hit exhausts the limit of 1.
    assert!(!svc.has_resolve_headroom("tok", &ct).await);
    let second = svc
        .search(
            "tok",
            "series",
            "tt1:1:1",
            &query(&[("t", "tvsearch")]),
            &ct,
            None,
            false,
            false,
        )
        .await
        .expect("outcome");
    assert_eq!(second.candidates.len(), 0);
    assert_eq!(requests.count(), 1, "indexer skipped, no new request");
}

#[tokio::test]
async fn exclude_patterns_drop_matching_titles() {
    let (addr, _requests) = serve_newznab(Arc::new(|_| {
        rss(&lit(&[
            ("Good.Show.1080p.WEB-DL", "http://nzb.example/good.nzb", 1),
            ("Bad.Show.ISO", "http://nzb.example/bad.nzb", 2),
        ]))
    }))
    .await;
    let svc = service(
        ProfileConfig {
            profiles: vec![profile("tok", vec![])],
        },
        IndexerConfig {
            indexers: vec![indexer("a", format!("http://{addr}/api"))],
            ..Default::default()
        },
        r"\.iso$",
    );
    let out = svc
        .search(
            "tok",
            "movie",
            "tt1",
            &query(&[("t", "movie"), ("imdbid", "1")]),
            &CancellationToken::new(),
            None,
            false,
            false,
        )
        .await
        .expect("outcome");
    assert_eq!(out.candidates.len(), 1);
    assert_eq!(out.candidates[0].title, "Good.Show.1080p.WEB-DL");
}

#[tokio::test]
async fn dedupe_by_nzb_url_keeps_first_indexer_and_profile_filters_indexers() {
    let (addr_a, _ra) = serve_newznab(Arc::new(|_| {
        rss(&lit(&[
            ("Dup.Show.1080p", "http://nzb.example/dup.nzb", 1),
            ("Only.A.1080p", "http://nzb.example/a.nzb", 2),
        ]))
    }))
    .await;
    let (addr_b, _rb) = serve_newznab(Arc::new(|_| {
        rss(&lit(&[
            ("Dup.Show.2160p", "http://nzb.example/dup.nzb", 9),
            ("Only.B.1080p", "http://nzb.example/b.nzb", 3),
        ]))
    }))
    .await;
    // Profile selects only indexer "b".
    let svc = service(
        ProfileConfig {
            profiles: vec![profile("tok", vec!["b".into()])],
        },
        IndexerConfig {
            indexers: vec![
                indexer("a", format!("http://{addr_a}/api")),
                indexer("b", format!("http://{addr_b}/api")),
            ],
            ..Default::default()
        },
        "",
    );
    let out = svc
        .search(
            "tok",
            "movie",
            "tt1",
            &query(&[("t", "movie")]),
            &CancellationToken::new(),
            None,
            false,
            false,
        )
        .await
        .expect("outcome");
    let titles: Vec<&str> = out.candidates.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, ["Dup.Show.2160p", "Only.B.1080p"]);

    // Without a filter, the duplicate nzb_url keeps the first indexer's item.
    let svc_all = service(
        ProfileConfig {
            profiles: vec![profile("tok", vec![])],
        },
        IndexerConfig {
            indexers: vec![
                indexer("a", format!("http://{addr_a}/api")),
                indexer("b", format!("http://{addr_b}/api")),
            ],
            ..Default::default()
        },
        "",
    );
    let out_all = svc_all
        .search(
            "tok",
            "movie",
            "tt1",
            &query(&[("t", "movie")]),
            &CancellationToken::new(),
            None,
            false,
            false,
        )
        .await
        .expect("outcome");
    assert_eq!(out_all.candidates.len(), 3);
    let dup = out_all
        .candidates
        .iter()
        .find(|c| c.nzb_url == "http://nzb.example/dup.nzb")
        .expect("dup");
    assert_eq!(dup.title, "Dup.Show.1080p", "first indexer wins the dedupe");
}

#[tokio::test]
async fn category_overrides_merge_extras_and_ignore_drops_cat() {
    let (addr, requests) = serve_newznab(Arc::new(|_| rss(&items("Show", 1)))).await;
    let mut idx = indexer("a", format!("http://{addr}/api"));
    idx.extra_tv_categories = Some("5070,5030".into());
    let svc = service(
        ProfileConfig {
            profiles: vec![profile("tok", vec![])],
        },
        IndexerConfig {
            indexers: vec![idx],
            ..Default::default()
        },
        "",
    );
    svc.search(
        "tok",
        "series",
        "tt1:1:1",
        &query(&[("t", "tvsearch"), ("cat", "5000")]),
        &CancellationToken::new(),
        None,
        false,
        false,
    )
    .await;
    let t = &requests.targets()[0];
    assert_eq!(query_param(t, "cat").as_deref(), Some("5000,5070,5030"));

    let (addr2, requests2) = serve_newznab(Arc::new(|_| rss(&items("Show", 1)))).await;
    let mut idx2 = indexer("a", format!("http://{addr2}/api"));
    idx2.ignore_category_filter = true;
    let svc2 = service(
        ProfileConfig {
            profiles: vec![profile("tok", vec![])],
        },
        IndexerConfig {
            indexers: vec![idx2],
            ..Default::default()
        },
        "",
    );
    svc2.search(
        "tok",
        "series",
        "tt1:1:1",
        &query(&[("t", "tvsearch"), ("cat", "5000")]),
        &CancellationToken::new(),
        None,
        false,
        false,
    )
    .await;
    let t2 = &requests2.targets()[0];
    assert!(query_param(t2, "cat").is_none(), "cat removed: {t2}");
}

#[tokio::test]
async fn strict_matching_consensus_drops_outlier_from_strict_indexer() {
    // No canonical identity resolvable offline -> consensus fallback (needs >= 2 results
    // sharing a head). Two "Dup..." heads from the strict indexer form the consensus.
    let (addr, _requests) = serve_newznab(Arc::new(|_| {
        rss(&lit(&[
            ("Dup.Show.2024.1080p.WEB", "http://nzb.example/d1.nzb", 1),
            ("Dup.Show.2024.2160p.BluRay", "http://nzb.example/d2.nzb", 2),
            ("Something.Else.2020.1080p", "http://nzb.example/x.nzb", 3),
        ]))
    }))
    .await;
    let mut idx = indexer("strict", format!("http://{addr}/api"));
    idx.enable_strict_matching = true;
    let svc = service(
        ProfileConfig {
            profiles: vec![profile("tok", vec![])],
        },
        IndexerConfig {
            indexers: vec![idx],
            ..Default::default()
        },
        "",
    );
    let out = svc
        .search(
            "tok",
            "movie",
            "tt1",
            &query(&[("t", "movie"), ("imdbid", "1")]),
            &CancellationToken::new(),
            None,
            false,
            false,
        )
        .await
        .expect("outcome");
    assert_eq!(
        out.candidates.len(),
        2,
        "consensus drops the outlier: {:?}",
        out.candidates
            .iter()
            .map(|c| c.title.as_str())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn per_indexer_result_filter_applies_before_merge() {
    let (addr, _requests) = serve_newznab(Arc::new(|_| {
        rss(&lit(&[
            ("Show.S01E01.1080p", "http://nzb.example/fresh.nzb", 1),
            ("Show.S01E01.1080p.OLD", "http://nzb.example/old.nzb", 2),
        ]))
    }))
    .await;
    let mut idx = indexer("a", format!("http://{addr}/api"));
    idx.filter = Some(ResultFilter {
        enabled: true,
        min_grabs: 5,
        grabs_grace_hours: 0,
        ..Default::default()
    });
    let svc = service(
        ProfileConfig {
            profiles: vec![profile("tok", vec![])],
        },
        IndexerConfig {
            indexers: vec![idx],
            ..Default::default()
        },
        "",
    );
    let out = svc
        .search(
            "tok",
            "series",
            "tt1:1:1",
            &query(&[("t", "tvsearch")]),
            &CancellationToken::new(),
            None,
            false,
            false,
        )
        .await
        .expect("outcome");
    // grabs grace 0 -> both items (grabs 0 and 1) are below min_grabs 5 and dropped.
    assert_eq!(out.candidates.len(), 0);
}

#[tokio::test]
async fn fallback_runs_title_variants_until_threshold() {
    // Primary tvsearch returns 1 result; fallback (client_query given, resolvers offline)
    // must run targeted then broad title queries and stop once >= 3 distinct results.
    let (addr, requests) = serve_newznab(Arc::new(|target| {
        let q = query_param(target, "q").unwrap_or_default();
        let season = query_param(target, "season");
        if q.is_empty() {
            rss(&items("Show.S01E01", 1))
        } else if season.is_some() {
            rss(&items("Show.Targeted", 1))
        } else {
            rss(&items("Show.Broad", 3))
        }
    }))
    .await;
    let mut p = profile("tok", vec![]);
    p.tv_fallback = FallbackMode::Broad;
    p.tv_fallback_min_results = 3;
    let svc = service(
        ProfileConfig { profiles: vec![p] },
        IndexerConfig {
            indexers: vec![indexer("a", format!("http://{addr}/api"))],
            ..Default::default()
        },
        "",
    );
    let out = svc
        .search(
            "tok",
            "series",
            "tt1:1:1",
            &query(&[("t", "tvsearch"), ("season", "1"), ("ep", "1")]),
            &CancellationToken::new(),
            Some("Show"),
            false,
            false,
        )
        .await
        .expect("outcome");
    assert!(
        out.candidates.len() >= 3,
        "fallback lifted results: {}",
        out.candidates.len()
    );
    let targets = requests.targets();
    let with_q: Vec<&String> = targets
        .iter()
        .filter(|t| query_param(t, "q").is_some())
        .collect();
    assert!(
        with_q.len() >= 2,
        "targeted + broad variants ran: {targets:?}"
    );
    let broad_ran = targets.iter().any(|t| {
        query_param(t, "q").as_deref() == Some("Show") && query_param(t, "season").is_none()
    });
    assert!(broad_ran, "broad variant without season ran: {targets:?}");
}

#[tokio::test]
async fn unknown_profile_returns_none_and_empty_indexers_give_empty_outcome() {
    let svc = service(ProfileConfig::default(), IndexerConfig::default(), "");
    assert!(
        svc.search(
            "nope",
            "movie",
            "tt1",
            &query(&[("t", "movie")]),
            &CancellationToken::new(),
            None,
            false,
            false
        )
        .await
        .is_none()
    );

    let svc2 = service(
        ProfileConfig {
            profiles: vec![profile("tok", vec!["missing".into()])],
        },
        IndexerConfig::default(),
        "",
    );
    let out = svc2
        .search(
            "tok",
            "movie",
            "tt1",
            &query(&[("t", "movie")]),
            &CancellationToken::new(),
            None,
            false,
            false,
        )
        .await
        .expect("outcome");
    assert_eq!(out.candidates.len(), 0);
    assert_eq!(out.play_tokens.len(), 0);
}

#[tokio::test]
async fn build_imdb_query_shapes() {
    let svc = service(ProfileConfig::default(), IndexerConfig::default(), "");
    let ct = CancellationToken::new();

    let movie = svc
        .build_imdb_query("movie", "tt0133093", &ct)
        .await
        .expect("movie");
    assert_eq!(
        movie,
        query(&[("t", "movie"), ("imdbid", "0133093"), ("cat", "2000")])
    );

    // Non-tt ids are rejected for plain movie queries.
    assert!(svc.build_imdb_query("movie", "abc", &ct).await.is_none());

    // Season: imdb:season (tvdb resolution disabled offline -> imdbid fallback).
    let season = svc
        .build_imdb_query("season", "tt0133093:2", &ct)
        .await
        .expect("season");
    assert_eq!(
        season,
        query(&[
            ("t", "tvsearch"),
            ("season", "2"),
            ("cat", "5000"),
            ("imdbid", "0133093")
        ])
    );

    // Series: imdb:season:episode.
    let series = svc
        .build_imdb_query("series", "tt0133093:2:5", &ct)
        .await
        .expect("series");
    assert_eq!(
        series,
        query(&[
            ("t", "tvsearch"),
            ("season", "2"),
            ("ep", "5"),
            ("cat", "5000"),
            ("imdbid", "0133093")
        ])
    );

    // tvdb- ids anchor on tvdbid directly.
    let tvdb = svc
        .build_imdb_query("series", "tvdb-81189:1:1", &ct)
        .await
        .expect("tvdb");
    assert_eq!(
        tvdb,
        query(&[
            ("t", "tvsearch"),
            ("tvdbid", "81189"),
            ("season", "1"),
            ("ep", "1"),
            ("cat", "5000")
        ])
    );
    // tvdb- is series-only.
    assert!(
        svc.build_imdb_query("movie", "tvdb-81189:1:1", &ct)
            .await
            .is_none()
    );

    // search_by_imdb with an unbuildable query yields an empty (not missing) outcome.
    let svc2 = service(
        ProfileConfig {
            profiles: vec![profile("tok", vec![])],
        },
        IndexerConfig::default(),
        "",
    );
    let out = svc2
        .search_by_imdb("tok", "movie", "not-an-id", &ct, None, false, false)
        .await
        .expect("outcome");
    assert_eq!(out.candidates.len(), 0);
}

#[tokio::test]
async fn external_kitsu_id_resolves_through_loopback_kitsu() {
    // kitsu:42 -> kitsu anime 42 with an imdb mapping; series endpoint builds a movie query.
    let (addr, requests) = serve_newznab(Arc::new(|target| {
        if target.contains("/anime/42") {
            br#"{"data":{"attributes":{"subtype":"movie","canonicalTitle":"Some Film"}},"included":[{"type":"mappings","attributes":{"externalSite":"imdb","externalId":"0111111"}}]}"#.to_vec()
        } else {
            rss(&items("Some.Film", 2))
        }
    }))
    .await;
    let cfg = ResolverConfig {
        enabled: true,
        kitsu_base: format!("http://{addr}"),
        anime_lists_url: format!("http://{addr}/nonexistent-dataset.json"),
        ..Default::default()
    };
    let settings = SearchSettings {
        profiles: ProfileConfig {
            profiles: vec![profile("tok", vec![])],
        },
        indexers: IndexerConfig {
            indexers: vec![indexer("a", format!("http://{addr}/api"))],
            ..Default::default()
        },
        ..Default::default()
    };
    let svc = SearchService::new(settings, Resolvers::new(cfg));
    let ct = CancellationToken::new();
    let q = svc
        .build_imdb_query("series", "kitsu:42:1:2", &ct)
        .await
        .expect("external query");
    // The kitsu entry is a movie with an imdb id -> movie query with cat 2000,2070.
    assert_eq!(
        q,
        query(&[("t", "movie"), ("imdbid", "0111111"), ("cat", "2000,2070")])
    );
    let targets = requests.targets();
    assert!(
        targets.iter().any(|t| t.contains("/anime/42?")),
        "{targets:?}"
    );
}

fn warm_candidate(nzb_url: &str) -> Candidate {
    Candidate {
        indexer_name: "watchtower".into(),
        indexer_user_agent: "ua".into(),
        nzb_url: nzb_url.into(),
        title: "Warm.Pick.2026.1080p.WEB-DL-GRP".into(),
        size: 5_000_000,
        posted: None,
        usenet_date: None,
        poster: None,
        grabs: Some(9),
        password: None,
        proxy_url: None,
        source_indexer_name: None,
        language: None,
        subs: None,
        info_hash: None,
        verified_available: false,
    }
}

fn warmer(picks: HashMap<String, Vec<Candidate>>) -> WarmerHook {
    Arc::new(
        move |key: &str, _ct: &CancellationToken| -> BoxFuture<'_, Vec<Candidate>> {
            let picks = picks.clone();
            let key = key.to_owned();
            Box::pin(async move { picks.get(&key).cloned().unwrap_or_default() })
        },
    )
}

fn watchtower_service(addr: SocketAddr, enabled: bool) -> SearchService {
    let mut settings = SearchSettings {
        profiles: ProfileConfig {
            profiles: vec![profile("tok", vec![])],
        },
        indexers: IndexerConfig {
            indexers: vec![indexer("a", format!("http://{addr}/api"))],
            ..Default::default()
        },
        ..Default::default()
    };
    settings.watchtower_enabled = enabled;
    SearchService::new(
        settings,
        Resolvers::new(ResolverConfig {
            enabled: false,
            ..Default::default()
        }),
    )
}

#[tokio::test]
async fn watchtower_warming_boosts_verified_picks_to_top() {
    // Indexer returns a (small) and b (large); the watchtower pick matches a and adds a new
    // one, so both verified picks must lead the list (v1 AddWarmedExactAsync).
    let (addr, _requests) = serve_newznab(Arc::new(|_| {
        rss(&lit(&[
            (
                "Movie.2026.1080p.WEB-DL.A",
                "http://nzb.example/a.nzb",
                1_000_000,
            ),
            (
                "Movie.2026.1080p.WEB-DL.B",
                "http://nzb.example/b.nzb",
                9_000_000,
            ),
        ]))
    }))
    .await;
    let svc = watchtower_service(addr, true);
    let mut picks = HashMap::new();
    picks.insert(
        "movie:tt123".to_owned(),
        vec![
            warm_candidate("http://nzb.example/a.nzb"),
            warm_candidate("http://nzb.example/warm.nzb"),
        ],
    );
    svc.install_warmer(Some(warmer(picks)));
    let out = svc
        .search(
            "tok",
            "movie",
            "tt123",
            &query(&[("t", "movie"), ("q", "Movie")]),
            &CancellationToken::new(),
            None,
            false,
            false,
        )
        .await
        .expect("outcome");
    assert_eq!(out.candidates.len(), 3);
    assert_eq!(out.play_tokens.len(), 3);
    assert_eq!(out.candidates[0].nzb_url, "http://nzb.example/a.nzb");
    assert!(out.candidates[0].verified_available);
    assert_eq!(out.candidates[1].nzb_url, "http://nzb.example/warm.nzb");
    assert!(out.candidates[1].verified_available);
    assert_eq!(out.candidates[2].nzb_url, "http://nzb.example/b.nzb");
    assert!(!out.candidates[2].verified_available);
}

#[tokio::test]
async fn watchtower_warming_marks_season_bundle_picks() {
    // Series episode search tt1:2:3 also warms from the season bundle key season:tt1:2;
    // matching picks are flagged in place, unknown ones appended (v1
    // AddWarmedSeasonBundleAsync, no reordering).
    let (addr, _requests) = serve_newznab(Arc::new(|_| {
        rss(&lit(&[(
            "Show.S02E03.1080p.WEB-DL",
            "http://nzb.example/s.nzb",
            1_000_000,
        )]))
    }))
    .await;
    let svc = watchtower_service(addr, true);
    let mut picks = HashMap::new();
    picks.insert(
        "season:tt1:2".to_owned(),
        vec![
            warm_candidate("http://nzb.example/s.nzb"),
            warm_candidate("http://nzb.example/bundle.nzb"),
        ],
    );
    svc.install_warmer(Some(warmer(picks)));
    let out = svc
        .search(
            "tok",
            "series",
            "tt1:2:3",
            &query(&[
                ("t", "tvsearch"),
                ("q", "Show"),
                ("season", "2"),
                ("ep", "3"),
            ]),
            &CancellationToken::new(),
            None,
            false,
            false,
        )
        .await
        .expect("outcome");
    assert_eq!(out.candidates.len(), 2);
    assert_eq!(out.candidates[0].nzb_url, "http://nzb.example/s.nzb");
    assert!(out.candidates[0].verified_available);
    assert_eq!(out.candidates[1].nzb_url, "http://nzb.example/bundle.nzb");
    assert!(out.candidates[1].verified_available);
}

#[tokio::test]
async fn watchtower_warming_injects_picks_into_empty_results() {
    // No indexer hits at all: the warm pick still becomes the (only) candidate.
    let (addr, _requests) = serve_newznab(Arc::new(|_| rss(&[]))).await;
    let svc = watchtower_service(addr, true);
    let mut picks = HashMap::new();
    picks.insert(
        "movie:tt123".to_owned(),
        vec![warm_candidate("http://nzb.example/warm.nzb")],
    );
    svc.install_warmer(Some(warmer(picks)));
    let out = svc
        .search(
            "tok",
            "movie",
            "tt123",
            &query(&[("t", "movie"), ("q", "Movie")]),
            &CancellationToken::new(),
            None,
            false,
            false,
        )
        .await
        .expect("outcome");
    assert_eq!(out.candidates.len(), 1);
    assert!(out.candidates[0].verified_available);
    assert_eq!(out.candidates[0].nzb_url, "http://nzb.example/warm.nzb");
}

#[tokio::test]
async fn watchtower_warming_skipped_when_disabled_or_hookless() {
    let (addr, _requests) = serve_newznab(Arc::new(|_| {
        rss(&lit(&[(
            "Movie.2026.1080p.WEB-DL.A",
            "http://nzb.example/a.nzb",
            1_000_000,
        )]))
    }))
    .await;
    let mut picks = HashMap::new();
    picks.insert(
        "movie:tt123".to_owned(),
        vec![warm_candidate("http://nzb.example/a.nzb")],
    );
    // Disabled: hook installed but gated off by the setting.
    let svc = watchtower_service(addr, false);
    svc.install_warmer(Some(warmer(picks.clone())));
    let out = svc
        .search(
            "tok",
            "movie",
            "tt123",
            &query(&[("t", "movie"), ("q", "Movie")]),
            &CancellationToken::new(),
            None,
            false,
            false,
        )
        .await
        .expect("outcome");
    assert_eq!(out.candidates.len(), 1);
    assert!(!out.candidates[0].verified_available);
    // Enabled but no hook installed: no-op, no panic.
    let svc = watchtower_service(addr, true);
    let out = svc
        .search(
            "tok",
            "movie",
            "tt123",
            &query(&[("t", "movie"), ("q", "Movie")]),
            &CancellationToken::new(),
            None,
            false,
            false,
        )
        .await
        .expect("outcome");
    assert_eq!(out.candidates.len(), 1);
    assert!(!out.candidates[0].verified_available);
}
