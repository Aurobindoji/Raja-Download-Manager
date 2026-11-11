use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_MESSAGE_SIZE: usize = 1024 * 1024;

fn main() {
    if let Err(error) = run() {
        eprintln!("RAJA browser host: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let inbox = inbox_directory()?;
    fs::create_dir_all(&inbox)?;
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();

    let mut length_bytes = [0; 4];
    match input.read(&mut length_bytes[..1])? {
        0 => return Ok(()),
        _ => input.read_exact(&mut length_bytes[1..])?,
    }
    let length = u32::from_le_bytes(length_bytes) as usize;
    if length == 0 || length > MAX_MESSAGE_SIZE {
        write_response(&mut output, false, "Invalid native message size.")?;
        return Ok(());
    }

    let mut message = vec![0; length];
    input.read_exact(&mut message)?;
    match handle_request(&inbox, &message, true) {
        Ok(response) => write_json_response(&mut output, &response)?,
        Err(error) => write_response(&mut output, false, &error)?,
    }
    Ok(())
}

fn inbox_directory() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let database = raja_core::app::default_database_path()?;
    let directory = database
        .parent()
        .ok_or("RAJA database path has no parent directory")?
        .join("browser-inbox");
    Ok(directory)
}

fn handle_request(
    inbox: &std::path::Path,
    message: &[u8],
    start_ui: bool,
) -> Result<serde_json::Value, String> {
    let value: serde_json::Value =
        serde_json::from_slice(message).map_err(|error| error.to_string())?;
    let url = value
        .get("url")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Missing URL.".to_owned())?;
    let parsed = validate_url(url)?;
    let action = value
        .get("action")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("download");

    if action == "list_formats" {
        let heights = available_video_heights(parsed.as_str())?;
        return Ok(serde_json::json!({ "ok": true, "heights": heights }));
    }
    if action != "download" {
        return Err("Unsupported browser request.".to_owned());
    }
    let kind = value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("video");
    if !matches!(kind, "http" | "video") {
        return Err("Unsupported download type.".to_owned());
    }
    let max_height = value
        .get("max_height")
        .and_then(serde_json::Value::as_u64)
        .map(u32::try_from)
        .transpose()
        .map_err(|_| "Invalid video quality.".to_owned())?;
    if let Some(height) = max_height
        && !(144..=1080).contains(&height)
    {
        return Err("Unsupported video quality.".to_owned());
    }

    let title = value
        .get("title")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let container = value
        .get("container")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let description = value
        .get("description")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let file_size = value
        .get("file_size")
        .and_then(serde_json::Value::as_u64)
        .filter(|&sz| sz > 0);
    // Name the browser chose (from the server's Content-Disposition) for a
    // download it handed over; at most 255 characters.
    let file_name = value
        .get("file_name")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.chars().count() <= 255);

    let request = serde_json::json!({
        "url": parsed.as_str(),
        "kind": kind,
        "max_height": max_height,
        "title": title,
        "container": container,
        "description": description,
        "file_size": file_size,
        "file_name": file_name,
    });
    let bytes = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let filename = format!("request-{}-{timestamp}.json", std::process::id());
    let path = inbox.join(filename);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.write_all(&bytes).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;

    if start_ui {
        start_raja_ui().map_err(|error| error.to_string())?;
    }
    Ok(serde_json::json!({ "ok": true, "message": "Queued in RAJA." }))
}

fn validate_url(url: &str) -> Result<reqwest::Url, String> {
    let parsed = reqwest::Url::parse(url).map_err(|error| error.to_string())?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(
            "Only HTTP or HTTPS URLs without embedded credentials are accepted.".to_owned(),
        );
    }
    Ok(parsed)
}

fn available_video_heights(url: &str) -> Result<Vec<u32>, String> {
    let mut command = Command::new(raja_core::video_tools::yt_dlp_executable());
    command.args([
        "--ignore-config",
        "--no-warnings",
        "--no-playlist",
        "--no-progress",
        "--socket-timeout",
        "10",
        "--retries",
        "1",
        "--skip-download",
        "--dump-single-json",
        url,
    ]);
    raja_core::video_tools::configure_video_tool_path(&mut command);
    let output = command
        .output()
        .map_err(|error| format!("Could not query yt-dlp for available video formats: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        return Err(if detail.is_empty() {
            format!("yt-dlp could not inspect this video ({}).", output.status)
        } else {
            format!("yt-dlp could not inspect this video: {detail}")
        });
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("yt-dlp returned invalid format information: {error}"))?;
    let heights = parse_video_heights(&metadata);
    if heights.is_empty() {
        return Err("No downloadable video formats up to 1080p were found.".to_owned());
    }
    // Keep the full information: when the user then starts the download,
    // Raja hands it to yt-dlp instead of asking YouTube a second time.
    // Saving is only an optimisation, so a failure is ignored.
    if let Ok(directory) = raja_core::video_tools::video_info_directory() {
        let _ = raja_core::video_tools::save_video_info(&directory, url, &output.stdout);
    }
    Ok(heights)
}

fn parse_video_heights(metadata: &serde_json::Value) -> Vec<u32> {
    let heights = metadata
        .get("formats")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|format| {
            format
                .get("vcodec")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|codec| codec != "none")
        })
        .filter_map(|format| format.get("height").and_then(serde_json::Value::as_u64))
        .filter(|height| (144..=1080).contains(height))
        .map(|height| height as u32)
        .collect::<BTreeSet<_>>();
    heights.into_iter().rev().collect()
}

fn start_raja_ui() -> io::Result<()> {
    let executable = std::env::current_exe()?;
    let ui_executable = executable
        .parent()
        .ok_or_else(|| io::Error::other("RAJA browser host has no installation directory"))?
        .join(if cfg!(windows) {
            "raja-ui.exe"
        } else {
            "raja-ui"
        });
    // Start hidden: RAJA then shows only the "Download File Info" box for the
    // queued browser request instead of the whole main window.
    Command::new(ui_executable)
        .arg("--background")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

fn write_response(output: &mut impl Write, ok: bool, message: &str) -> io::Result<()> {
    let response = serde_json::json!({
        "ok": ok,
        "message": message,
    });
    write_json_response(output, &response)
}

fn write_json_response(output: &mut impl Write, response: &serde_json::Value) -> io::Result<()> {
    let response = serde_json::to_vec(response).map_err(io::Error::other)?;
    let length = u32::try_from(response.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Response is too large."))?;
    output.write_all(&length.to_le_bytes())?;
    output.write_all(&response)?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::{MAX_MESSAGE_SIZE, handle_request, write_response};
    use std::fs;

    #[test]
    fn queues_only_valid_http_or_https_requests() {
        let inbox = std::env::temp_dir().join(format!(
            "raja-browser-host-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&inbox).unwrap();
        let message = br#"{"url":"https://example.test/video","kind":"video"}"#;
        let response = handle_request(&inbox, message, false).unwrap();
        let files = fs::read_dir(&inbox)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(files.len(), 1);
        let request: serde_json::Value =
            serde_json::from_slice(&fs::read(files[0].path()).unwrap()).unwrap();
        assert_eq!(request["url"], "https://example.test/video");
        assert_eq!(request["kind"], "video");
        assert_eq!(request["max_height"], serde_json::Value::Null);
        assert_eq!(response["ok"], true);
        fs::remove_dir_all(inbox).unwrap();
    }

    #[test]
    fn rejects_non_http_urls() {
        let inbox = std::env::temp_dir();
        let message = br#"{"url":"file:///secret","kind":"http"}"#;
        assert!(handle_request(&inbox, message, false).is_err());
    }

    #[test]
    fn rejects_out_of_range_video_quality() {
        let inbox = std::env::temp_dir();
        let message = br#"{"url":"https://example.test/video","kind":"video","max_height":4320}"#;
        assert!(handle_request(&inbox, message, false).is_err());
    }

    #[test]
    fn parses_available_video_heights_from_extractor_metadata() {
        let metadata = serde_json::json!({
            "formats": [
                {"height": 720, "vcodec": "avc1"},
                {"height": 1080, "vcodec": "vp9"},
                {"height": 2160, "vcodec": "av1"},
                {"height": 1080, "vcodec": "none"}
            ]
        });
        assert_eq!(super::parse_video_heights(&metadata), vec![1080, 720]);
    }

    #[test]
    fn documents_native_message_size_limit() {
        assert!(MAX_MESSAGE_SIZE < u32::MAX as usize);
    }

    #[test]
    fn writes_native_messaging_response_with_little_endian_length() {
        let mut output = Vec::new();
        write_response(&mut output, true, "Queued in RAJA.").unwrap();
        let length = u32::from_le_bytes(output[..4].try_into().unwrap()) as usize;
        assert_eq!(length, output.len() - 4);
        let response: serde_json::Value = serde_json::from_slice(&output[4..]).unwrap();
        assert_eq!(response["ok"], true);
    }
}
