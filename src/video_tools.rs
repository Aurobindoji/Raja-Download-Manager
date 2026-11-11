use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

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
}
