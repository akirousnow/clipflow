use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Default GitHub repository used for online update checks and binary downloads.
/// Can be overridden via `CLIPFLOW_REPO="owner/repo"`.
pub const DEFAULT_GITHUB_REPO: &str = "akirousnow/clipflow";

/// Current crate version embedded at compile time.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Metadata describing an available newer GitHub release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub tag_name: String,
    pub asset_name: String,
    pub asset_url: String,
    pub checksums_url: Option<String>,
    pub release_url: String,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

pub fn configured_repo() -> String {
    std::env::var("CLIPFLOW_REPO")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_GITHUB_REPO.to_string())
}

pub fn platform_asset_name() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "clipflow-linux-aarch64.tar.gz",
        _ => "clipflow-linux-x86_64.tar.gz",
    }
}

/// Parses a semver string such as `"v0.2.1"` or `"0.2.1"` into `(major, minor, patch)`.
pub fn parse_semver(raw: &str) -> Option<(u64, u64, u64)> {
    let trimmed = raw.trim().strip_prefix('v').unwrap_or(raw.trim());
    let core = trimmed.split('-').next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Returns `true` when `candidate` is strictly newer than `current`.
pub fn is_newer_version(current: &str, candidate: &str) -> bool {
    match (parse_semver(current), parse_semver(candidate)) {
        (Some(cur), Some(cand)) => cand > cur,
        _ => false,
    }
}

/// Parses a standard `SHA256SUMS.txt` file and extracts the hex digest for `target_filename`.
pub fn find_expected_sha256(checksums_text: &str, target_filename: &str) -> Option<String> {
    for line in checksums_text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let hash = parts.next()?;
        let file = parts.next()?.trim_start_matches('*');
        if file == target_filename && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

/// Computes the lowercase hex SHA-256 digest of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// Queries GitHub Releases for a newer version of ClipFlow.
/// Returns `Ok(None)` if already up to date (or if no stable release exists yet).
pub fn check_for_update() -> Result<Option<UpdateInfo>> {
    let repo = configured_repo();
    let api_url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let body = http_get_bytes(&api_url, 10)?;
    let release: GithubRelease =
        serde_json::from_slice(&body).context("failed to parse GitHub release JSON")?;

    if release.draft || release.prerelease {
        return Ok(None);
    }
    if !is_newer_version(CURRENT_VERSION, &release.tag_name) {
        return Ok(None);
    }

    let wanted = platform_asset_name();
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == wanted)
        .ok_or_else(|| anyhow!("release {} has no asset named {wanted}", release.tag_name))?;

    let checksums_url = release
        .assets
        .iter()
        .find(|a| a.name == "SHA256SUMS.txt")
        .map(|a| a.browser_download_url.clone());

    let latest_version = release
        .tag_name
        .trim_start_matches('v')
        .to_string();

    Ok(Some(UpdateInfo {
        current_version: CURRENT_VERSION.to_string(),
        latest_version,
        tag_name: release.tag_name,
        asset_name: asset.name.clone(),
        asset_url: asset.browser_download_url.clone(),
        checksums_url,
        release_url: release.html_url,
    }))
}

/// Downloads the release tarball, verifies SHA-256 against `SHA256SUMS.txt` when provided,
/// extracts the binary, and atomically replaces the current executable on disk.
pub fn perform_self_update(info: &UpdateInfo) -> Result<PathBuf> {
    let current_exe = std::env::current_exe().context("failed to resolve current executable")?;
    let exe_dir = current_exe
        .parent()
        .context("current executable has no parent directory")?
        .to_path_buf();

    let stage_dir = std::env::temp_dir().join(format!("clipflow-update-{}", std::process::id()));
    let _ = fs::remove_dir_all(&stage_dir);
    fs::create_dir_all(&stage_dir)?;

    let result = (|| -> Result<PathBuf> {
        let archive_path = stage_dir.join(&info.asset_name);
        let archive_bytes = http_get_bytes(&info.asset_url, 120)
            .with_context(|| format!("failed to download {}", info.asset_url))?;

        if let Some(sums_url) = &info.checksums_url {
            let sums_bytes = http_get_bytes(sums_url, 15)
                .with_context(|| format!("failed to download {sums_url}"))?;
            let sums_text = String::from_utf8_lossy(&sums_bytes);
            let expected = find_expected_sha256(&sums_text, &info.asset_name)
                .ok_or_else(|| anyhow!("SHA256SUMS.txt does not list {}", info.asset_name))?;
            let actual = sha256_hex(&archive_bytes);
            if actual != expected {
                bail!(
                    "SHA-256 verification failed for {} (expected {expected}, got {actual})",
                    info.asset_name
                );
            }
        }

        fs::write(&archive_path, &archive_bytes)?;

        let status = Command::new("tar")
            .args(["-xzf"])
            .arg(&archive_path)
            .arg("-C")
            .arg(&stage_dir)
            .status()
            .context("failed to run tar to extract update archive")?;
        if !status.success() {
            bail!("tar extraction failed with status {status}");
        }

        let extracted_bin = find_binary_in_dir(&stage_dir, "clipflow")
            .context("extracted archive does not contain `clipflow` binary")?;

        let staged_target = exe_dir.join(format!(".clipflow.update.{}.tmp", std::process::id()));
        fs::copy(&extracted_bin, &staged_target).with_context(|| {
            format!(
                "failed to stage new binary into {} (check write permissions)",
                staged_target.display()
            )
        })?;
        fs::set_permissions(&staged_target, fs::Permissions::from_mode(0o755))?;

        fs::rename(&staged_target, &current_exe).with_context(|| {
            format!(
                "failed to atomically replace {}",
                current_exe.display()
            )
        })?;

        Ok(current_exe)
    })();

    let _ = fs::remove_dir_all(&stage_dir);
    result
}

fn find_binary_in_dir(dir: &Path, name: &str) -> Option<PathBuf> {
    let direct = dir.join(name);
    if direct.is_file() {
        return Some(direct);
    }
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if p.is_dir()
            && let Some(found) = find_binary_in_dir(&p, name)
        {
            return Some(found);
        }
    }
    None
}

fn http_get_bytes(url: &str, timeout_secs: u64) -> Result<Vec<u8>> {
    let out = Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            &timeout_secs.to_string(),
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            &format!("User-Agent: clipflow/{CURRENT_VERSION}"),
            url,
        ])
        .output()
        .context("failed to execute curl")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("HTTP GET {url} failed: {}", stderr.trim());
    }
    Ok(out.stdout)
}
