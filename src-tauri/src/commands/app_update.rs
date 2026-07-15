//! Lightweight FlipperUI release checks.
//!
//! The app never creates releases or downloads installers here. It asks the
//! public GitHub API for the latest published stable release, compares its
//! semantic version with the running application, and returns a trusted GitHub
//! Releases URL for the frontend to open in the user's browser.

use std::io::Read;
use std::time::Duration;

use semver::Version;
use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::error::{FlipperError, Result};

const LATEST_RELEASE_API: &str = "https://api.github.com/repos/fuckmaz/FlipperUI/releases/latest";
const LATEST_RELEASE_PAGE: &str = "https://github.com/fuckmaz/FlipperUI/releases/latest";
const MAX_RELEASE_RESPONSE_BYTES: u64 = 512 * 1024;

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    name: Option<String>,
    html_url: String,
    body: Option<String>,
    published_at: Option<String>,
    draft: bool,
    prerelease: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdateCheck {
    current_version: String,
    latest_version: String,
    available: bool,
    release_name: String,
    release_url: String,
    notes: Option<String>,
    published_at: Option<String>,
}

#[tauri::command]
pub async fn app_update_check(app: AppHandle) -> Result<AppUpdateCheck> {
    let current_version = app.package_info().version.to_string();
    tauri::async_runtime::spawn_blocking(move || check_latest_release(&current_version))
        .await
        .map_err(|error| FlipperError::Internal(error.to_string()))?
}

fn check_latest_release(current_version: &str) -> Result<AppUpdateCheck> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(10))
        .timeout_write(Duration::from_secs(10))
        .redirects(3)
        .user_agent(concat!("FlipperUI/", env!("CARGO_PKG_VERSION")))
        .build();

    let response = agent
        .get(LATEST_RELEASE_API)
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .timeout(Duration::from_secs(15))
        .call()
        .map_err(|error| {
            FlipperError::Internal(format!("could not check GitHub Releases: {error}"))
        })?;

    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_RELEASE_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            FlipperError::Internal(format!("could not read GitHub's release response: {error}"))
        })?;
    if bytes.len() as u64 > MAX_RELEASE_RESPONSE_BYTES {
        return Err(FlipperError::Internal(
            "GitHub's release response was unexpectedly large".into(),
        ));
    }

    let release: GitHubRelease = serde_json::from_slice(&bytes).map_err(|error| {
        FlipperError::Internal(format!("GitHub returned invalid release data: {error}"))
    })?;
    release_check(current_version, release)
}

fn release_check(current_version: &str, release: GitHubRelease) -> Result<AppUpdateCheck> {
    if release.draft || release.prerelease {
        return Err(FlipperError::Internal(
            "GitHub returned a non-stable release from the latest-release endpoint".into(),
        ));
    }

    let current = Version::parse(current_version).map_err(|error| {
        FlipperError::Internal(format!("the running app version is invalid: {error}"))
    })?;
    let latest_version = release.tag_name.trim_start_matches('v');
    let latest = Version::parse(latest_version).map_err(|error| {
        FlipperError::Internal(format!(
            "the latest GitHub release version is invalid: {error}"
        ))
    })?;

    let release_url =
        trusted_release_url(&release.html_url).unwrap_or_else(|| LATEST_RELEASE_PAGE.to_string());
    let release_name = release
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| format!("FlipperUI v{latest_version}"));

    Ok(AppUpdateCheck {
        current_version: current.to_string(),
        latest_version: latest.to_string(),
        available: latest > current,
        release_name,
        release_url,
        notes: release.body.filter(|body| !body.trim().is_empty()),
        published_at: release.published_at,
    })
}

fn trusted_release_url(candidate: &str) -> Option<String> {
    let url = url::Url::parse(candidate).ok()?;
    let trusted = url.scheme() == "https"
        && url.host_str() == Some("github.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path().starts_with("/fuckmaz/FlipperUI/releases/");
    trusted.then(|| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::{release_check, trusted_release_url, GitHubRelease};

    fn release(tag: &str) -> GitHubRelease {
        GitHubRelease {
            tag_name: tag.into(),
            name: None,
            html_url: format!("https://github.com/fuckmaz/FlipperUI/releases/tag/{tag}"),
            body: Some("Release notes".into()),
            published_at: Some("2026-07-15T12:00:00Z".into()),
            draft: false,
            prerelease: false,
        }
    }

    #[test]
    fn semantic_version_comparison_only_offers_newer_releases() {
        assert!(release_check("0.4.5", release("v0.4.6")).unwrap().available);
        assert!(!release_check("0.4.6", release("v0.4.6")).unwrap().available);
        assert!(!release_check("0.5.0", release("v0.4.6")).unwrap().available);
    }

    #[test]
    fn only_the_flipperui_github_release_path_is_opened() {
        assert!(
            trusted_release_url("https://github.com/fuckmaz/FlipperUI/releases/tag/v0.4.6")
                .is_some()
        );
        assert!(trusted_release_url("https://evil.example/releases/tag/v0.4.6").is_none());
        assert!(trusted_release_url(
            "https://github.com/someone-else/FlipperUI/releases/tag/v0.4.6"
        )
        .is_none());
    }

    #[test]
    fn draft_and_prerelease_results_are_rejected() {
        let mut draft = release("v0.4.6");
        draft.draft = true;
        assert!(release_check("0.4.5", draft).is_err());

        let mut prerelease = release("v0.4.6-rc.1");
        prerelease.prerelease = true;
        assert!(release_check("0.4.5", prerelease).is_err());
    }
}
