use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use sha2::{Digest, Sha256};

pub fn yt_dlp_executable() -> PathBuf {
    let executable = if cfg!(windows) {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    };
    if let Some(path) = find_in_path(executable) {
        return path;
    }

    #[cfg(windows)]
    if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        let package = PathBuf::from(local_app_data)
            .join("Microsoft")
            .join("WinGet")
            .join("Packages")
            .join("yt-dlp.yt-dlp_Microsoft.Winget.Source_8wekyb3d8bbwe")
            .join(executable);
        if package.is_file() {
            return package;
        }
    }

    PathBuf::from(executable)
}

pub fn aria2c_executable() -> PathBuf {
    let executable = if cfg!(windows) {
        "aria2c.exe"
    } else {
        "aria2c"
    };
    if let Ok(current_exe) = env::current_exe()
        && let Some(dir) = current_exe.parent()
    {
        let sibling = dir.join(executable);
        if sibling.is_file() {
            return sibling;
        }
    }
    if let Some(path) = find_in_path(executable) {
        return path;
    }

    #[cfg(windows)]
    if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        let root = PathBuf::from(local_app_data)
            .join("Microsoft")
            .join("WinGet")
            .join("Packages")
            .join("aria2.aria2_Microsoft.Winget.Source_8wekyb3d8bbwe");
        let direct = root.join(executable);
        if direct.is_file() {
            return direct;
        }
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let candidate = entry.path().join(executable);
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }

    PathBuf::from(executable)
}

pub fn configure_video_tool_path(command: &mut Command) {
    let mut paths = env::var_os("PATH")
        .map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    #[cfg(windows)]
    if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        paths.extend(winget_tool_directories(Path::new(&local_app_data)));
    }
    if let Ok(path) = env::join_paths(paths) {
        command.env("PATH", path);
    }
    // yt-dlp is a Python program: force UTF-8 stdout so non-ASCII titles
    // (e.g. Hindi) in printed file paths are not mangled by the console code page.
    command.env("PYTHONIOENCODING", "utf-8");
    command.env("PYTHONUTF8", "1");
}

fn find_in_path(executable: &str) -> Option<PathBuf> {
    env::var_os("PATH")
        .into_iter()
        .flat_map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(executable))
        .find(|path| path.is_file())
}

#[cfg(windows)]
fn winget_tool_directories(local_app_data: &Path) -> Vec<PathBuf> {
    let packages = local_app_data
        .join("Microsoft")
        .join("WinGet")
        .join("Packages");
    let mut directories = Vec::new();
    for package in [
        "yt-dlp.yt-dlp_Microsoft.Winget.Source_8wekyb3d8bbwe",
        "DenoLand.Deno_Microsoft.Winget.Source_8wekyb3d8bbwe",
    ] {
        let directory = packages.join(package);
        if directory.is_dir() {
            directories.push(directory);
        }
    }
    for package in [
        "yt-dlp.FFmpeg_Microsoft.Winget.Source_8wekyb3d8bbwe",
        "Gyan.FFmpeg_Microsoft.Winget.Source_8wekyb3d8bbwe",
    ] {
        let root = packages.join(package);
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let bin = entry.path().join("bin");
            if bin.is_dir() {
                directories.push(bin);
            }
        }
    }
    directories
}

/// Video information older than this is not reused: the stream links inside
/// it expire after a few hours, so stay well inside that.
pub const VIDEO_INFO_MAX_AGE: Duration = Duration::from_secs(20 * 60);

/// Folder for the video information yt-dlp already produced while the browser
/// extension listed the available qualities (`%LOCALAPPDATA%\Raja\video-info`).
pub fn video_info_directory() -> io::Result<PathBuf> {
    let database = crate::app::default_database_path()?;
    let parent = database
        .parent()
        .ok_or_else(|| io::Error::other("Raja database path has no parent folder"))?;
    Ok(parent.join("video-info"))
}

/// File that holds the saved video information for `url`.
pub fn video_info_path(directory: &Path, url: &str) -> PathBuf {
    let digest = Sha256::digest(url.as_bytes());
    let name: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    directory.join(format!("{name}.json"))
}

/// Saves yt-dlp's `--dump-single-json` output for `url` so starting the
/// download can skip a second extraction. Written to a temporary file and
/// renamed, so a reader never sees half a file. Old entries are removed.
pub fn save_video_info(directory: &Path, url: &str, json: &[u8]) -> io::Result<PathBuf> {
    fs::create_dir_all(directory)?;
    let path = video_info_path(directory, url);
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temporary, json)?;
    fs::rename(&temporary, &path)?;
    prune_video_info(directory, VIDEO_INFO_MAX_AGE * 6);
    Ok(path)
}

/// Saved video information for `url`, if it exists and is younger than
/// `max_age`.
pub fn fresh_video_info(directory: &Path, url: &str, max_age: Duration) -> Option<PathBuf> {
    let path = video_info_path(directory, url);
    let age = fs::metadata(&path)
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .unwrap_or(Duration::ZERO);
    (age <= max_age).then_some(path)
}

fn prune_video_info(directory: &Path, older_than: Duration) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > older_than);
        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::configure_video_tool_path;
    use std::process::Command;

    #[test]
    fn configures_child_processes_with_an_explicit_path() {
        let mut command = Command::new("yt-dlp");
        configure_video_tool_path(&mut command);
        assert!(
            command.get_envs().any(|(key, value)| {
                key == "PATH" && value.is_some_and(|path| !path.is_empty())
            })
        );
    }

    #[test]
    fn saved_video_info_is_found_only_for_the_same_url_while_fresh() {
        let directory =
            std::env::temp_dir().join(format!("raja-video-info-test-{}", std::process::id()));
        let url = "https://www.youtube.com/watch?v=abc";
        let saved = super::save_video_info(&directory, url, b"{\"id\":\"abc\"}").unwrap();
        assert_eq!(std::fs::read(&saved).unwrap(), b"{\"id\":\"abc\"}");
        assert_eq!(
            super::fresh_video_info(&directory, url, super::VIDEO_INFO_MAX_AGE),
            Some(saved.clone())
        );
        assert_eq!(
            super::fresh_video_info(
                &directory,
                "https://www.youtube.com/watch?v=other",
                super::VIDEO_INFO_MAX_AGE
            ),
            None
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(
            super::fresh_video_info(&directory, url, std::time::Duration::from_millis(1)),
            None
        );
        let _ = std::fs::remove_dir_all(&directory);
    }
}
