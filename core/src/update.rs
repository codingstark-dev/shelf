use anyhow::Result;
use serde::Deserialize;
use std::{path::Path, time::{SystemTime, UNIX_EPOCH}};

#[derive(Debug, Clone)]
pub struct Release {
    pub tag_name: String,
    pub html_url: String,
    pub body: String,
    /// First `.dmg` asset's download URL, if any.
    pub dmg_url: Option<String>,
}

#[derive(Deserialize)]
struct Raw {
    tag_name: String,
    html_url: String,
    body: Option<String>,
    #[serde(default)]
    assets: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

fn is_newer(tag: &str, current: &str) -> Result<bool> {
    let v = |s: &str| semver::Version::parse(s.trim_start_matches('v'));
    Ok(v(tag)? > v(current)?)
}

/// `repo` is "owner/name". Returns the latest release only if it is newer than `current`.
pub async fn check(repo: &str, current: &str) -> Result<Option<Release>> {
    let raw: Raw = reqwest::Client::new()
        .get(format!("https://api.github.com/repos/{repo}/releases/latest"))
        .header("User-Agent", "shelf")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if !is_newer(&raw.tag_name, current)? {
        return Ok(None);
    }
    let dmg_url = raw.assets.into_iter().find(|a| a.name.ends_with(".dmg")).map(|a| a.browser_download_url);
    Ok(Some(Release { tag_name: raw.tag_name, html_url: raw.html_url, body: raw.body.unwrap_or_default(), dmg_url }))
}

/// True at most once per 24h; records the time in `file` when it returns true.
pub fn due(file: &Path) -> bool {
    due_at(file, SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()))
}

fn due_at(file: &Path, now: u64) -> bool {
    let last = std::fs::read_to_string(file).ok().and_then(|s| s.trim().parse::<u64>().ok());
    if last.is_some_and(|l| now.saturating_sub(l) < 86_400) {
        return false;
    }
    let _ = std::fs::write(file, now.to_string());
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(is_newer("v0.2.0", "0.1.0").unwrap());
        assert!(!is_newer("v0.1.0", "0.1.0").unwrap());
        assert!(!is_newer("0.0.9", "v0.1.0").unwrap());
        assert!(is_newer("v0.10.0", "0.9.0").unwrap()); // not lexical
        assert!(is_newer("nope", "0.1.0").is_err());
    }

    #[test]
    fn throttle() {
        let f = tempfile::tempdir().unwrap().keep().join("last");
        assert!(due_at(&f, 1_000_000)); // first run
        assert!(!due_at(&f, 1_000_000 + 86_399));
        assert!(due_at(&f, 1_000_000 + 86_400));
        assert!(!due_at(&f, 1_000_000 + 86_401)); // timestamp was refreshed
    }
}
