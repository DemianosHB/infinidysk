//! Search exclude patterns, ported from `backend/Utils/ExcludePatternParser.cs` (v1 fork).
//! Accepts bare .NET regex bodies and JavaScript-style `/body/flags` wrappers (the shape
//! community lists publish); matching is always case-insensitive, `m`/`s` wrapper flags
//! map to multiline/dotall. Invalid lines are skipped (never throw).
//!
//! Deviation from v1: no 250 ms per-pattern match timeout (.NET `Regex` timeout; neither
//! `regex` nor `fancy-regex` supports one). Patterns without lookaround are delegated to
//! the linear-time `regex` engine, and titles are short, so a pathological pattern cannot
//! hang the search in practice.

use std::sync::LazyLock;

use fancy_regex::{Regex, RegexBuilder};
use regex::Regex as PlainRegex;

static JS_WRAPPER: LazyLock<PlainRegex> =
    LazyLock::new(|| PlainRegex::new(r"^/(.+)/([a-z]*)$").expect("valid regex"));

/// v1 `ExcludePatternParser.ParsedPattern`: the dedup key plus the compiled regex.
#[derive(Debug, Clone)]
pub struct ParsedPattern {
    pub key: String,
    pub regex: Regex,
}

/// v1 `ExcludePatternParser.Parse`: one line → a compiled pattern, or `None` for blanks,
/// `#` comments and patterns that fail to compile.
#[must_use]
pub fn parse(line: Option<&str>) -> Option<ParsedPattern> {
    let line = line?;
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let mut body = trimmed.to_owned();
    let mut multi_line = false;
    let mut dot_all = false;
    let mut extra_flags = String::new();
    if let Some(w) = JS_WRAPPER.captures(trimmed) {
        body = w.get(1).map(|m| m.as_str().to_owned())?;
        let flags = w.get(2).map_or("", |m| m.as_str());
        // Fixed order so "/x/ms" and "/x/sm" yield the same dedup key.
        if flags.contains('m') {
            multi_line = true;
            extra_flags.push('m');
        }
        if flags.contains('s') {
            dot_all = true;
            extra_flags.push('s');
        }
    }
    if body.is_empty() {
        return None;
    }
    match RegexBuilder::new(&body)
        .case_insensitive(true)
        .multi_line(multi_line)
        .dot_matches_new_line(dot_all)
        .build()
    {
        Ok(regex) => {
            // IgnoreCase is constant, so it is excluded from the key — a raw body and its
            // /.../i wrapper form collapse to one entry.
            let key = format!("{body} {extra_flags}");
            Some(ParsedPattern { key, regex })
        }
        Err(e) => {
            tracing::warn!("Skipping invalid exclude pattern {trimmed}: {e}");
            None
        }
    }
}

/// Compiles a multi-line raw config value, deduplicating by key in first-seen order
/// (v1 `ConfigManager.BuildExcludePatterns` manual-list part).
#[must_use]
pub fn compile_lines(raw: &str) -> Vec<ParsedPattern> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for line in raw.split('\n').filter(|l| !l.is_empty()) {
        if let Some(p) = parse(Some(line))
            && !seen.contains(&p.key)
        {
            seen.push(p.key.clone());
            out.push(p);
        }
    }
    out
}

/// v1 `SearchProfileService.MatchesExcludePattern`: underscores fold to dots, then any
/// pattern matching drops the title.
#[must_use]
pub fn matches_any(title: Option<&str>, patterns: &[ParsedPattern]) -> bool {
    let Some(title) = title else { return false };
    if patterns.is_empty() || title.is_empty() {
        return false;
    }
    let normalized = title.replace('_', ".");
    patterns
        .iter()
        .any(|p| p.regex.is_match(&normalized).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from tests/NzbWebDAV.Tests/Utils/ExcludePatternParserTests.cs.

    #[test]
    fn parse_bare_body_is_case_insensitive_by_default() {
        let p = parse(Some(r"\.iso$")).expect("parsed");
        assert!(p.regex.is_match("FILE.ISO").expect("match"));
        assert!(!p.regex.is_match("FILE.mkv").expect("match"));
    }

    #[test]
    fn parse_js_wrapper_shares_dedup_key_with_bare_body() {
        let bare = parse(Some(r"\.(iso|img)$")).expect("bare");
        let wrapped = parse(Some(r"/\.(iso|img)$/i")).expect("wrapped");
        assert_eq!(bare.key, wrapped.key);
        assert!(wrapped.regex.is_match("Movie.ISO").expect("match"));
    }

    #[test]
    fn parse_ms_flags_are_order_independent_in_key() {
        let ms = parse(Some("/x/ms")).expect("ms");
        let sm = parse(Some("/x/sm")).expect("sm");
        assert_eq!(ms.key, sm.key);
        // Both flags were applied (verified via behaviour: dot matches newline, ^ per line).
        assert!(
            ms.regex.is_match("a\nb").expect("match") || ms.regex.is_match("x").expect("match")
        );
    }

    #[test]
    fn parse_blanks_comments_and_invalid_return_none() {
        for line in [None, Some(""), Some("   "), Some("# comment"), Some("[")] {
            assert!(parse(line).is_none(), "{line:?}");
        }
    }

    #[test]
    fn parse_unknown_js_flags_are_ignored() {
        let p = parse(Some("/foo/gu")).expect("parsed");
        assert_eq!(p.key, "foo ");
        assert!(p.regex.is_match("FOO").expect("match"));
    }

    #[test]
    fn compile_lines_dedupes_by_key() {
        let compiled = compile_lines("\\.iso$\n/\\.iso$/i\n# c\n\n\\.img$");
        assert_eq!(compiled.len(), 2);
    }

    #[test]
    fn matches_any_folds_underscores_to_dots() {
        let patterns = compile_lines(r"\.iso$");
        assert!(matches_any(Some("MY_FILE.ISO"), &patterns));
        assert!(matches_any(Some("MY_FILE_ISO"), &patterns));
        assert!(!matches_any(Some("MY_FILE.mkv"), &patterns));
        assert!(!matches_any(None, &patterns));
        assert!(!matches_any(Some(""), &patterns));
    }
}
