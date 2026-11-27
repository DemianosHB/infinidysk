//! Makes sure `ui/dist` holds a page to embed: the web UI build, else a placeholder. Also
//! copies the release workflow's stamped `release-notes.build.md` into `OUT_DIR` when present,
//! so the binary serves the release notes of the tag it was built from.

use std::error::Error;
use std::path::PathBuf;
use std::{env, fs};

const PLACEHOLDER: &str = "<!doctype html><title>infinidysk</title>\
<p>This build has no web UI. Run <code>npm ci &amp;&amp; npm run build</code> in <code>ui/</code> \
and rebuild.</p>\n";
const NOTES: &str = "release-notes.build.md";

fn main() -> Result<(), Box<dyn Error>> {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let dist = manifest.join("../../ui/dist");
    // ponytail: writes into the gitignored source tree so the embed path stays fixed; a Vite
    // build replaces the placeholder. Watching a missing path would rerun this every build.
    fs::create_dir_all(&dist)?;
    let index = dist.join("index.html");
    if !index.is_file() {
        fs::write(index, PLACEHOLDER)?;
    }
    println!("cargo::rerun-if-changed={}", dist.display());
    let notes = manifest.join("../../").join(NOTES);
    let out = PathBuf::from(env::var("OUT_DIR")?).join(NOTES);
    if notes.is_file() {
        fs::copy(&notes, &out)?;
        println!("cargo::rerun-if-changed={}", notes.display());
    } else {
        fs::write(&out, "// No release notes were stamped into this build.\n")?;
    }
    println!("cargo::rerun-if-env-changed=INFINIDYSK_VERSION");
    if let Ok(version) = env::var("INFINIDYSK_VERSION")
        && !version.is_empty()
        && version.chars().any(char::is_whitespace)
    {
        return Err(
            format!("INFINIDYSK_VERSION must not contain whitespace, not {version:?}").into(),
        );
    }
    println!("cargo::rerun-if-env-changed=INFINIDYSK_COMMIT");
    if let Ok(commit) = env::var("INFINIDYSK_COMMIT")
        && !commit.is_empty()
        && !(commit.len() == 40
            && commit
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
    {
        return Err(format!("INFINIDYSK_COMMIT must be a full commit hash, not {commit:?}").into());
    }
    Ok(())
}
