//! Checks GitHub for a newer Raja release, downloads its installer and hands
//! over to it.
//!
//! The version check follows the `releases/latest` redirect on github.com
//! instead of calling the GitHub API, so it is not subject to the API's
//! hourly limit. The API is only asked (best effort) for the installer's
//! SHA-256, which is then verified after downloading.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::header::{LOCATION, USER_AGENT};
use reqwest::redirect::Policy;
use sha2::{Digest, Sha256};

pub const REPOSITORY: &str = "Aurobindoji/Raja-Download-Manager";
/// The automatic check runs at most this often.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const AGENT: &str = concat!("Raja-Download-Manager/", env!("CARGO_PKG_VERSION"));

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A newer release that can be installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    pub version: String,
    pub release_page: String,
    pub setup_url: String,
    pub setup_name: String,
    /// Expected SHA-256 of the installer, when GitHub reported one.
    pub sha256: Option<String>,
}

/// `"v0.2.4"` / `"0.2.4"` -> `(0, 2, 4)`.
pub fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim().trim_start_matches(['v', 'V']);
    let mut parts = text.split('.').map(|part| part.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    matches!(
        (parse_version(candidate), parse_version(current)),
        (Some(candidate), Some(current)) if candidate > current
    )
}

/// `https://github.com/o/r/releases/tag/v0.2.4` -> `v0.2.4`.
pub fn tag_from_release_url(url: &str) -> Option<&str> {
    let tag = url.trim().rsplit_once("/releases/tag/")?.1;
    let tag = tag.split(['?', '#', '/']).next()?;
    parse_version(tag).map(|_| tag)
}

pub fn setup_asset_name(version: &str) -> String {
    format!("Raja-{version}-Setup-x64.exe")
}

/// Finds the installer's `"digest": "sha256:<hex>"` in a GitHub release JSON.
pub fn setup_digest(release_json: &serde_json::Value, setup_name: &str) -> Option<String> {
    release_json
        .get("assets")?
        .as_array()?
        .iter()
        .find(|asset| asset.get("name").and_then(|name| name.as_str()) == Some(setup_name))?
        .get("digest")?
        .as_str()?
        .strip_prefix("sha256:")
        .filter(|hex| hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
}

fn client(redirects: bool) -> Result<Client, String> {
    Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(if redirects {
            Duration::from_secs(30 * 60)
        } else {
            Duration::from_secs(30)
        })
        .redirect(if redirects {
            Policy::limited(10)
        } else {
            Policy::none()
        })
        .build()
        .map_err(|error| format!("Could not prepare the update check: {error}"))
}

/// Asks GitHub for the latest release. `Ok(None)` means Raja is up to date.
pub fn check_latest() -> Result<Option<UpdateInfo>, String> {
    let latest = format!("https://github.com/{REPOSITORY}/releases/latest");
    let response = client(false)?
        .get(&latest)
        .header(USER_AGENT, AGENT)
        .send()
        .map_err(|error| format!("Could not reach GitHub: {}", error.without_url()))?;
    let location = response
        .headers()
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            format!(
                "GitHub did not report a latest release (HTTP {}).",
                response.status()
            )
        })?;
    let tag = tag_from_release_url(location)
        .ok_or_else(|| "GitHub returned an unexpected release address.".to_owned())?
        .to_owned();
    let version = tag.trim_start_matches(['v', 'V']).to_owned();
    if !is_newer(&version, current_version()) {
        return Ok(None);
    }
    let setup_name = setup_asset_name(&version);
    let sha256 = fetch_setup_digest(&tag, &setup_name);
    Ok(Some(UpdateInfo {
        release_page: format!("https://github.com/{REPOSITORY}/releases/tag/{tag}"),
        setup_url: format!("https://github.com/{REPOSITORY}/releases/download/{tag}/{setup_name}"),
        setup_name,
        version,
        sha256,
    }))
}

/// Best effort: the API may be rate limited; the download then relies on
/// HTTPS from github.com alone.
fn fetch_setup_digest(tag: &str, setup_name: &str) -> Option<String> {
    let url = format!("https://api.github.com/repos/{REPOSITORY}/releases/tags/{tag}");
    let response = client(true)
        .ok()?
        .get(url)
        .header(USER_AGENT, AGENT)
        .header("Accept", "application/vnd.github+json")
        .send()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&response.bytes().ok()?).ok()?;
    setup_digest(&json, setup_name)
}

/// Downloads the installer into `directory`, reporting `(done, total)` bytes,
/// and checks its SHA-256 when one is known. Returns the installer's path.
pub fn download_setup(
    info: &UpdateInfo,
    directory: &Path,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf, String> {
    fs::create_dir_all(directory)
        .map_err(|error| format!("Could not create the update folder: {error}"))?;
    let target = directory.join(&info.setup_name);
    let partial = target.with_extension("exe.part");
    let mut response = client(true)?
        .get(&info.setup_url)
        .header(USER_AGENT, AGENT)
        .send()
        .map_err(|error| format!("Could not download the update: {}", error.without_url()))?;
    if !response.status().is_success() {
        return Err(format!(
            "Could not download the update (HTTP {}).",
            response.status()
        ));
    }
    let total = response.content_length();
    let mut file =
        File::create(&partial).map_err(|error| format!("Could not save the update: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 256 * 1024];
    let mut done = 0_u64;
    loop {
        let read = response
            .read(&mut buffer)
            .map_err(|error| format!("The update download was interrupted: {error}"))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .map_err(|error| format!("Could not save the update: {error}"))?;
        hasher.update(&buffer[..read]);
        done += read as u64;
        progress(done, total);
    }
    file.sync_all()
        .map_err(|error| format!("Could not save the update: {error}"))?;
    drop(file);
    if total.is_some_and(|total| total != done) {
        let _ = fs::remove_file(&partial);
        return Err("The update download was incomplete. Please try again.".to_owned());
    }
    if let Some(expected) = &info.sha256 {
        let actual: String = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if &actual != expected {
            let _ = fs::remove_file(&partial);
            return Err(
                "The downloaded update failed its SHA-256 check and was deleted.".to_owned(),
            );
        }
    }
    let _ = fs::remove_file(&target);
    fs::rename(&partial, &target).map_err(|error| format!("Could not save the update: {error}"))?;
    Ok(target)
}

/// The Raja to start again after updating: this copy when it is the
/// installed one, otherwise the default install location.
pub fn relaunch_path() -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    let installed = current
        .parent()
        .is_some_and(|dir| dir.join("unins000.exe").is_file());
    if installed {
        return Some(current);
    }
    let default = PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
        .join("Programs")
        .join("Raja")
        .join("raja-ui.exe");
    Some(default)
}

/// The helper script: wait for Raja to close, install silently, start Raja.
pub fn handover_script(setup: &Path, relaunch: &Path) -> String {
    format!(
        "@echo off\r\n\
         rem Raja update: wait for Raja to close, install, start Raja again.\r\n\
         ping -n 4 127.0.0.1 >nul\r\n\
         \"{setup}\" /SILENT /SUPPRESSMSGBOXES /NORESTART\r\n\
         start \"\" \"{relaunch}\"\r\n",
        setup = setup.display(),
        relaunch = relaunch.display(),
    )
}

/// Starts the helper in the background. The caller must then exit Raja, so
/// the installer can replace its files.
pub fn start_handover(setup: &Path, relaunch: &Path) -> Result<(), String> {
    let script = setup.with_file_name("raja-update.cmd");
    fs::write(&script, handover_script(setup, relaunch))
        .map_err(|error| format!("Could not prepare the update: {error}"))?;
    let mut command = std::process::Command::new("cmd.exe");
    command.arg("/C").arg(&script);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW | DETACHED_PROCESS is not allowed together;
        // a hidden console that outlives Raja is enough.
        command.creation_flags(0x0800_0000);
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Could not start the update: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_numerically() {
        assert_eq!(parse_version("v0.2.10"), Some((0, 2, 10)));
        assert_eq!(parse_version("0.2"), None);
        assert_eq!(parse_version("0.2.4-beta"), None);
        assert!(is_newer("0.2.10", "0.2.9"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(!is_newer("0.2.4", "0.2.4"));
        assert!(!is_newer("0.2.3", "0.2.4"));
        assert!(!is_newer("garbage", "0.2.4"));
    }

    #[test]
    fn reads_the_tag_from_the_latest_release_redirect() {
        assert_eq!(
            tag_from_release_url(
                "https://github.com/Aurobindoji/Raja-Download-Manager/releases/tag/v0.2.4"
            ),
            Some("v0.2.4")
        );
        assert_eq!(
            tag_from_release_url("https://github.com/o/r/releases"),
            None
        );
        assert_eq!(
            tag_from_release_url("https://github.com/o/r/releases/tag/nightly"),
            None
        );
    }

    #[test]
    fn finds_the_installer_digest_in_release_json() {
        let hex = "ab".repeat(32);
        let json = serde_json::json!({ "assets": [
            { "name": "Raja-0.2.5-windows-x64.zip", "digest": format!("sha256:{}", "cd".repeat(32)) },
            { "name": "Raja-0.2.5-Setup-x64.exe", "digest": format!("sha256:{hex}") }
        ]});
        assert_eq!(setup_digest(&json, "Raja-0.2.5-Setup-x64.exe"), Some(hex));
        assert_eq!(setup_digest(&json, "missing.exe"), None);
    }

    #[test]
    fn handover_waits_installs_silently_then_relaunches() {
        let script = handover_script(
            Path::new(r"C:\Temp\Raja-0.2.5-Setup-x64.exe"),
            Path::new(r"C:\Raja\raja-ui.exe"),
        );
        let wait = script.find("ping").unwrap();
        let install = script
            .find(r#""C:\Temp\Raja-0.2.5-Setup-x64.exe" /SILENT"#)
            .unwrap();
        let relaunch = script.find(r#"start "" "C:\Raja\raja-ui.exe""#).unwrap();
        assert!(wait < install && install < relaunch);
    }
}
