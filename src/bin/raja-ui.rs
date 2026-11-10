#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(windows)]
#[path = "raja_ui/tray.rs"]
mod tray;

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui;
use raja_core::storage::{DownloadSummary, Storage};

struct RajaUi {
    url: String,
    output: String,
    output_suggested: bool,
    video_page_download: bool,
    sha256: String,
    default_download_directory: String,
    database: String,
    core_executable: PathBuf,
    downloads: Vec<DownloadSummary>,
    progress_samples: HashMap<i64, ProgressSample>,
    children: Vec<RunningProcess>,
    queue: VecDeque<i64>,
    queue_running: bool,
    output_rx: Receiver<String>,
    output_tx: Sender<String>,
    status: String,
    last_refresh: Instant,
    category: DownloadCategory,
    selected_download: Option<i64>,
    show_add_dialog: bool,
    show_forget_dialog: bool,
    show_scheduler_dialog: bool,
    show_options_dialog: bool,
    schedule_after_minutes: u32,
    scheduled_queue_at: Option<Instant>,
    inbox_poll_at: Instant,
    #[cfg(windows)]
    tray: tray::TrayHandle,
    #[cfg(windows)]
    exit_requested: bool,
}

struct RunningProcess {
    child: Child,
    queue_download: Option<i64>,
    kind: ProcessKind,
    video_download: Option<i64>,
    stop_requested: bool,
}

#[derive(Clone, Copy)]
enum ProcessKind {
    Core,
    Video,
}

#[derive(Debug, Clone, Copy)]
struct ProgressSample {
    durable_bytes: u64,
    changed_at: Instant,
    bytes_per_second: Option<f64>,
}

impl ProgressSample {
    fn new(durable_bytes: u64, now: Instant) -> Self {
        Self {
            durable_bytes,
            changed_at: now,
            bytes_per_second: None,
        }
    }

    fn observe(&mut self, durable_bytes: u64, now: Instant) {
        if durable_bytes != self.durable_bytes {
            let elapsed = now.duration_since(self.changed_at).as_secs_f64();
            self.bytes_per_second = if durable_bytes > self.durable_bytes && elapsed > 0.0 {
                Some((durable_bytes - self.durable_bytes) as f64 / elapsed)
            } else {
                None
            };
            self.durable_bytes = durable_bytes;
            self.changed_at = now;
        } else if now.duration_since(self.changed_at) >= Duration::from_secs(3) {
            self.bytes_per_second = Some(0.0);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DownloadCategory {
    All,
    Compressed,
    Documents,
    Music,
    Programs,
    Video,
    Unfinished,
    Finished,
    GrabberProjects,
    Queues,
}

impl DownloadCategory {
    fn label(self) -> &'static str {
        match self {
            Self::All => "All Downloads",
            Self::Compressed => "Compressed",
            Self::Documents => "Documents",
            Self::Music => "Music",
            Self::Programs => "Programs",
            Self::Video => "Video",
            Self::Unfinished => "Unfinished",
            Self::Finished => "Finished",
            Self::GrabberProjects => "Video-page jobs",
            Self::Queues => "Queues",
        }
    }

    fn matches(self, download: &DownloadSummary) -> bool {
        let extension = PathBuf::from(&download.file_name)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match self {
            Self::All => true,
            Self::Compressed => matches!(
                extension.as_str(),
                "7z" | "bz" | "bz2" | "gz" | "rar" | "tar" | "xz" | "zip"
            ),
            Self::Documents => matches!(
                extension.as_str(),
                "csv"
                    | "doc"
                    | "docx"
                    | "epub"
                    | "pdf"
                    | "ppt"
                    | "pptx"
                    | "rtf"
                    | "txt"
                    | "xls"
                    | "xlsx"
            ),
            Self::Music => matches!(
                extension.as_str(),
                "aac" | "aiff" | "flac" | "m4a" | "mp3" | "ogg" | "wav" | "wma"
            ),
            Self::Programs => matches!(
                extension.as_str(),
                "appx" | "deb" | "dmg" | "exe" | "msi" | "pkg" | "rpm"
            ),
            Self::Video => {
                download.download_kind == "video"
                    || matches!(
                        extension.as_str(),
                        "avi" | "m4v" | "mkv" | "mov" | "mp4" | "mpeg" | "mpg" | "webm" | "wmv"
                    )
            }
            Self::Unfinished => download.status != "complete",
            Self::Finished => download.status == "complete",
            Self::Queues => {
                matches!(
                    download.status.as_str(),
                    "queued" | "paused" | "failed" | "active"
                )
            }
            Self::GrabberProjects => download.download_kind == "video",
        }
    }
}

impl RajaUi {
    fn new(
        initial_url: Option<String>,
        launch_error: Option<String>,
        #[cfg(windows)] tray: tray::TrayHandle,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let database = raja_core::app::default_database_path()?;
        if let Some(parent) = database.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let default_directory = default_download_directory();
        let saved_directory = Storage::open(&database)?.setting("download_directory")?;
        let default_download_directory = saved_directory
            .map(PathBuf::from)
            .filter(|directory| directory.is_dir())
            .unwrap_or(default_directory);
        let suggested_output = initial_url
            .as_deref()
            .map(|url| default_download_directory.join(suggested_filename(url)));
        let current_executable = std::env::current_exe()?;
        let executable_name = if cfg!(windows) {
            "raja-core.exe"
        } else {
            "raja-core"
        };
        let core_executable = current_executable
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(executable_name);
        let (output_tx, output_rx) = mpsc::channel();
        let mut app = Self {
            url: initial_url.clone().unwrap_or_default(),
            output: suggested_output
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            output_suggested: suggested_output.is_some(),
            video_page_download: false,
            sha256: String::new(),
            default_download_directory: default_download_directory.to_string_lossy().into_owned(),
            database: database.to_string_lossy().into_owned(),
            core_executable,
            downloads: Vec::new(),
            progress_samples: HashMap::new(),
            children: Vec::new(),
            queue: VecDeque::new(),
            queue_running: false,
            output_rx,
            output_tx,
            status: launch_error.unwrap_or_default(),
            last_refresh: Instant::now() - Duration::from_secs(2),
            category: DownloadCategory::All,
            selected_download: None,
            show_add_dialog: initial_url.is_some(),
            show_forget_dialog: false,
            show_scheduler_dialog: false,
            show_options_dialog: false,
            schedule_after_minutes: 15,
            scheduled_queue_at: None,
            inbox_poll_at: Instant::now() - Duration::from_secs(1),
            #[cfg(windows)]
            tray,
            #[cfg(windows)]
            exit_requested: false,
        };
        app.refresh_downloads();
        Ok(app)
    }

    fn refresh_downloads(&mut self) {
        let database_path = PathBuf::from(&self.database);
        if let Some(parent) = database_path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            self.status = format!("Could not create database folder: {error}");
            return;
        }
        match Storage::open(&self.database).and_then(|storage| storage.list_downloads()) {
            Ok(downloads) => {
                let now = Instant::now();
                for download in &downloads {
                    self.progress_samples
                        .entry(download.id)
                        .and_modify(|sample| sample.observe(download.durable_bytes, now))
                        .or_insert_with(|| ProgressSample::new(download.durable_bytes, now));
                }
                self.progress_samples
                    .retain(|id, _| downloads.iter().any(|download| download.id == *id));
                self.downloads = downloads;
                self.last_refresh = now;
            }
            Err(error) => self.status = format!("Could not load downloads: {error}"),
        }
    }

    fn run_core(&mut self, arguments: &[String]) {
        self.run_core_task(arguments, None);
    }

    fn run_core_task(&mut self, arguments: &[String], queue_download: Option<i64>) -> bool {
        let mut command = Command::new(&self.core_executable);
        command
            .args(arguments)
            .arg("--database")
            .arg(&self.database);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        match command.spawn() {
            Ok(mut child) => {
                let stdout = child.stdout.take();
                let stderr = child.stderr.take();
                if let Some(stdout) = stdout {
                    forward_output(stdout, self.output_tx.clone());
                }
                if let Some(stderr) = stderr {
                    forward_output(stderr, self.output_tx.clone());
                }
                self.children.push(RunningProcess {
                    child,
                    queue_download,
                    kind: ProcessKind::Core,
                    video_download: None,
                    stop_requested: false,
                });
                self.status = "Raja command started.".to_owned();
                self.last_refresh = Instant::now() - Duration::from_secs(2);
                true
            }
            Err(error) => {
                self.status = format!(
                    "Could not start {}: {error}. Build both binaries with `cargo build --release --bins`.",
                    self.core_executable.display()
                );
                false
            }
        }
    }

    fn start_queue(&mut self) {
        self.scheduled_queue_at = None;
        self.queue = queueable_download_ids(&self.downloads);
        if self.queue.is_empty() {
            self.status = "No queued, paused, or failed downloads to start.".to_owned();
            return;
        }
        self.queue_running = true;
        self.status = format!("Starting queue ({} download(s)).", self.queue.len());
        self.advance_queue();
    }

    fn stop_queue(&mut self) {
        self.scheduled_queue_at = None;
        self.queue_running = false;
        self.queue.clear();
        if let Some(process) = self
            .children
            .iter()
            .find(|process| process.queue_download.is_some())
        {
            if let Some(video_id) = process.video_download {
                self.stop_video_download(video_id);
            } else if let Some(download_id) = process.queue_download {
                self.run_core(&["pause".to_owned(), download_id.to_string()]);
                self.status = "Queue stopped; the current download is pausing.".to_owned();
            }
        } else {
            self.status = "Queue stopped.".to_owned();
        }
    }

    fn pause_all(&mut self) {
        self.queue_running = false;
        self.queue.clear();
        let mut stopped_video = 0;
        let mut video_stop_error = None;
        let mut paused_video_ids = Vec::new();
        for process in &mut self.children {
            if matches!(process.kind, ProcessKind::Video) {
                process.stop_requested = true;
                match terminate_process_tree(&mut process.child) {
                    Ok(()) => {
                        stopped_video += 1;
                        if let Some(download_id) = process.video_download {
                            paused_video_ids.push(download_id);
                        }
                    }
                    Err(error) => {
                        video_stop_error = Some(error.to_string());
                    }
                }
            }
        }
        for download_id in paused_video_ids {
            if let Err(error) = self.update_video_status(
                download_id,
                "paused",
                None,
                None,
                Some("Video extraction stopped by user"),
            ) {
                video_stop_error = Some(error);
            }
        }
        let active_ids = self
            .downloads
            .iter()
            .filter(|download| download.download_kind == "http" && download.status == "active")
            .map(|download| download.id)
            .collect::<Vec<_>>();
        for id in &active_ids {
            self.run_core(&["pause".to_owned(), id.to_string()]);
        }
        self.status = if let Some(error) = video_stop_error {
            format!("Could not stop video extraction: {error}")
        } else {
            match (active_ids.len(), stopped_video) {
                (0, 0) => "There are no active downloads to pause.".to_owned(),
                (0, 1) => "Stopped video extraction; rerun it to resume.".to_owned(),
                (0, count) => format!("Stopped {count} video extractions; rerun to resume."),
                (count, 0) => format!("Pausing {count} active download(s)."),
                (count, videos) => format!(
                    "Pausing {count} download(s) and stopping {videos} video extraction(s)."
                ),
            }
        };
    }

    fn start_video_extraction(&mut self, url: &str, output: &str) {
        self.start_video_extraction_with_max_height(url, output, None);
    }

    fn start_video_extraction_with_max_height(
        &mut self,
        url: &str,
        output: &str,
        max_height: Option<u32>,
    ) {
        let valid_url = match validate_external_url(url) {
            Ok(url) => url,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        let output = PathBuf::from(output.trim());
        let validation = max_height.map_or_else(
            || video_extraction_arguments(&valid_url, &output),
            |height| video_extraction_arguments_for_height(&valid_url, &output, Some(height)),
        );
        if let Err(error) = validation {
            self.status = error;
            return;
        }
        let parent = output
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        if let Err(error) = std::fs::create_dir_all(parent) {
            self.status = format!("Could not create the video destination folder: {error}");
            return;
        }

        let Some(file_name) = output.file_name().and_then(|name| name.to_str()) else {
            self.status = "Video destination must include a valid file name.".to_owned();
            return;
        };
        let Some(output_dir) = parent.to_str() else {
            self.status = "Video destination folder must be a valid Unicode path.".to_owned();
            return;
        };
        let download_id = match Storage::open(&self.database).and_then(|mut storage| {
            storage.create_video_download(&valid_url, file_name, output_dir)
        }) {
            Ok(id) => id,
            Err(error) => {
                self.status = format!("Could not save video download to Raja history: {error}");
                return;
            }
        };
        self.start_video_job_with_max_height(download_id, &valid_url, &output, None, max_height);
    }

    fn start_video_job(
        &mut self,
        download_id: i64,
        url: &str,
        output: &std::path::Path,
        queue_download: Option<i64>,
    ) -> bool {
        self.start_video_job_with_max_height(download_id, url, output, queue_download, None)
    }

    fn start_video_job_with_max_height(
        &mut self,
        download_id: i64,
        url: &str,
        output: &std::path::Path,
        queue_download: Option<i64>,
        max_height: Option<u32>,
    ) -> bool {
        if self
            .children
            .iter()
            .any(|process| process.video_download == Some(download_id))
        {
            self.status = format!("Video download {download_id} is already running.");
            return false;
        }
        let arguments = match max_height.map_or_else(
            || video_job_arguments(download_id, url, output),
            |height| video_job_arguments_for_height(download_id, url, output, Some(height)),
        ) {
            Ok(arguments) => arguments,
            Err(error) => {
                self.status = error.clone();
                self.fail_video_job(download_id, &error);
                return false;
            }
        };
        let mut command = Command::new(raja_core::video_tools::yt_dlp_executable());
        command
            .args(arguments)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        raja_core::video_tools::configure_video_tool_path(&mut command);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        match command.spawn() {
            Ok(mut child) => {
                if let Some(stdout) = child.stdout.take() {
                    forward_output(stdout, self.output_tx.clone());
                }
                if let Some(stderr) = child.stderr.take() {
                    forward_output(stderr, self.output_tx.clone());
                }
                if let Err(error) =
                    self.update_video_status(download_id, "active", None, None, None)
                {
                    match terminate_process_tree(&mut child) {
                        Ok(()) => {
                            self.status = format!("Could not update video history: {error}");
                        }
                        Err(stop_error) => {
                            self.children.push(RunningProcess {
                                child,
                                queue_download,
                                kind: ProcessKind::Video,
                                video_download: Some(download_id),
                                stop_requested: false,
                            });
                            self.status = format!(
                                "Could not update video history: {error}; also could not stop yt-dlp: {stop_error}"
                            );
                        }
                    }
                    return false;
                }
                self.children.push(RunningProcess {
                    child,
                    queue_download,
                    kind: ProcessKind::Video,
                    video_download: Some(download_id),
                    stop_requested: false,
                });
                self.status =
                    "Video extraction started with yt-dlp. Progress is reported by the extractor."
                        .to_owned();
                self.last_refresh = Instant::now() - Duration::from_secs(2);
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.status =
                    "yt-dlp was not found. Install yt-dlp and add it to PATH to extract video pages."
                        .to_owned();
                let message = self.status.clone();
                self.fail_video_job(download_id, &message);
                false
            }
            Err(error) => {
                self.status = format!("Could not start yt-dlp: {error}");
                let message = self.status.clone();
                self.fail_video_job(download_id, &message);
                false
            }
        }
    }

    fn fail_video_job(&mut self, download_id: i64, message: &str) {
        if let Err(error) =
            self.update_video_status(download_id, "failed", None, None, Some(message))
        {
            self.status = format!("{message} (could not save video status: {error})");
        }
        self.last_refresh = Instant::now() - Duration::from_secs(2);
    }

    fn update_video_status(
        &self,
        download_id: i64,
        status: &str,
        downloaded_bytes: Option<u64>,
        total_bytes: Option<u64>,
        message: Option<&str>,
    ) -> Result<(), String> {
        Storage::open(&self.database)
            .and_then(|mut storage| {
                storage.update_video_download(
                    download_id,
                    status,
                    downloaded_bytes,
                    total_bytes,
                    message,
                )
            })
            .map_err(|error| error.to_string())
    }

    fn update_video_output_path(&self, download_id: i64, output_path: &str) -> Result<(), String> {
        Storage::open(&self.database)
            .and_then(|mut storage| {
                storage.update_video_output_path(download_id, std::path::Path::new(output_path))
            })
            .map_err(|error| error.to_string())
    }

    fn complete_video_download(&self, download_id: i64) -> Result<(), String> {
        let output_path = Storage::open(&self.database)
            .and_then(|storage| {
                storage.list_downloads().map(|downloads| {
                    downloads
                        .into_iter()
                        .find(|download| {
                            download.id == download_id && download.download_kind == "video"
                        })
                        .map(|download| download.output_path)
                })
            })
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("Video download {download_id} is missing from history."))?;
        let metadata = std::fs::metadata(&output_path).map_err(|error| {
            format!(
                "yt-dlp succeeded but the output file is unavailable at {}: {error}",
                output_path.display()
            )
        })?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err(format!(
                "yt-dlp succeeded but produced no non-empty video file at {}.",
                output_path.display()
            ));
        }
        self.update_video_status(
            download_id,
            "complete",
            Some(metadata.len()),
            Some(metadata.len()),
            Some("Video extraction completed"),
        )
    }

    fn resume_download(&mut self, download_id: i64) {
        let Some(download) = self.downloads.iter().find(|item| item.id == download_id) else {
            self.status = format!("Download {download_id} is not in the current history.");
            return;
        };
        if download.download_kind == "video" {
            let url = download.source_url.clone();
            let output = download.output_path.clone();
            if self.start_video_job(download_id, &url, &output, None) {
                self.status = "Video extraction resumed.".to_owned();
            }
        } else {
            self.run_core(&["retry".to_owned(), download_id.to_string()]);
        }
    }

    fn poll_browser_inbox(&mut self) {
        if self.inbox_poll_at.elapsed() < Duration::from_millis(400) {
            return;
        }
        self.inbox_poll_at = Instant::now();
        let Some(inbox) = PathBuf::from(&self.database)
            .parent()
            .map(|parent| parent.join("browser-inbox"))
        else {
            return;
        };
        let entries = match std::fs::read_dir(&inbox) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                self.status = format!("Could not read browser download requests: {error}");
                return;
            }
        };
        let mut requests = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) if entry.path().extension().is_some_and(|ext| ext == "json") => {
                    requests.push(entry.path());
                }
                Ok(_) => {}
                Err(error) => {
                    self.status = format!("Could not enumerate browser download requests: {error}");
                    return;
                }
            }
        }
        requests.sort();
        for path in requests {
            let request = match std::fs::read(&path) {
                Ok(contents) => serde_json::from_slice::<serde_json::Value>(&contents)
                    .map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            if let Err(error) = std::fs::remove_file(&path) {
                self.status = format!(
                    "Could not remove processed browser request {}: {error}",
                    path.display()
                );
                continue;
            }
            let request = match request {
                Ok(request) => request,
                Err(error) => {
                    self.status = format!("Invalid browser download request: {error}");
                    continue;
                }
            };
            let Some(url) = request.get("url").and_then(serde_json::Value::as_str) else {
                self.status = "Browser download request has no URL.".to_owned();
                continue;
            };
            let Some(kind) = request.get("kind").and_then(serde_json::Value::as_str) else {
                self.status = "Browser download request has no download type.".to_owned();
                continue;
            };
            let max_height = match request.get("max_height") {
                None | Some(serde_json::Value::Null) => None,
                Some(value) => match value
                    .as_u64()
                    .and_then(|height| u32::try_from(height).ok())
                    .filter(|height| (144..=1080).contains(height))
                {
                    Some(height) => Some(height),
                    None => {
                        self.status = "Browser request has an invalid video quality.".to_owned();
                        continue;
                    }
                },
            };
            let url = match validate_external_url(url) {
                Ok(url) => url,
                Err(error) => {
                    self.status = error;
                    continue;
                }
            };
            let output = PathBuf::from(&self.default_download_directory)
                .join(browser_suggested_filename(&url, kind == "video"));
            match kind {
                "video" => {
                    let output = output.to_string_lossy().into_owned();
                    self.start_video_extraction_with_max_height(&url, &output, max_height);
                }
                "http" => {
                    let output = output.to_string_lossy().into_owned();
                    self.run_core(&["download".to_owned(), url, output]);
                }
                _ => self.status = "Unsupported browser download type.".to_owned(),
            }
        }
    }

    #[cfg(windows)]
    fn poll_tray(&mut self, context: &egui::Context) {
        while let Ok(command) = self.tray.try_recv() {
            match command {
                tray::TrayCommand::Show => {
                    context.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    context.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                tray::TrayCommand::Exit => {
                    self.exit_requested = true;
                    context.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    fn pause_download(&mut self, download_id: i64) {
        if self
            .downloads
            .iter()
            .any(|download| download.id == download_id && download.download_kind == "video")
        {
            self.stop_video_download(download_id);
        } else {
            self.run_core(&["pause".to_owned(), download_id.to_string()]);
        }
    }

    fn stop_video_download(&mut self, download_id: i64) {
        let termination = {
            let Some(process) = self
                .children
                .iter_mut()
                .find(|process| process.video_download == Some(download_id))
            else {
                self.status = format!("Video download {download_id} is not running.");
                return;
            };
            process.stop_requested = true;
            terminate_process_tree(&mut process.child)
        };
        match termination {
            Ok(()) => match self.update_video_status(
                download_id,
                "paused",
                None,
                None,
                Some("Video extraction stopped by user"),
            ) {
                Ok(()) => {
                    self.status = "Video extraction stopped; resume it to continue.".to_owned()
                }
                Err(error) => {
                    self.status =
                        format!("Stopped video extraction but could not save its status: {error}")
                }
            },
            Err(error) => self.status = format!("Could not stop video extraction: {error}"),
        }
    }

    fn advance_queue(&mut self) {
        if !self.queue_running
            || self
                .children
                .iter()
                .any(|process| process.queue_download.is_some())
        {
            return;
        }
        let Some(download_id) = self.queue.pop_front() else {
            self.queue_running = false;
            self.status = "Download queue completed.".to_owned();
            return;
        };
        if let Some(download) = self.downloads.iter().find(|item| item.id == download_id)
            && download.download_kind == "video"
        {
            let url = download.source_url.clone();
            let output = download.output_path.clone();
            if !self.start_video_job(download_id, &url, &output, Some(download_id)) {
                self.queue_running = false;
            }
            return;
        }
        if !self.run_core_task(
            &["retry".to_owned(), download_id.to_string()],
            Some(download_id),
        ) {
            self.queue.push_front(download_id);
            self.queue_running = false;
        }
    }

    fn open_selected_folder(&mut self) {
        let Some(download) = self
            .downloads
            .iter()
            .find(|download| Some(download.id) == self.selected_download)
        else {
            return;
        };
        if download.status != "complete" {
            self.status = "Only completed downloads can be opened in Explorer.".to_owned();
            return;
        }
        let output_path = download.output_path.clone();
        if !output_path.is_file() {
            self.status = format!("Downloaded file was not found: {}", output_path.display());
            return;
        }

        #[cfg(windows)]
        match Command::new("explorer.exe")
            .arg("/select,")
            .arg(&output_path)
            .spawn()
        {
            Ok(_) => self.status = "Opened the download location in Explorer.".to_owned(),
            Err(error) => self.status = format!("Could not open File Explorer: {error}"),
        }
        #[cfg(not(windows))]
        {
            let _ = output_path;
            self.status = "Opening downloads in Explorer is supported on Windows only.".to_owned();
        }
    }

    fn poll_children_and_output(&mut self) {
        while let Ok(line) = self.output_rx.try_recv() {
            if let Some((download_id, output_path)) = parse_video_output_line(&line) {
                if let Err(error) = self.update_video_output_path(download_id, &output_path) {
                    self.status = format!("Could not save final video path: {error}");
                }
            } else if let Some((download_id, downloaded_bytes, total_bytes)) =
                parse_video_progress_line(&line)
            {
                let is_running = self.children.iter().any(|process| {
                    process.video_download == Some(download_id) && !process.stop_requested
                });
                if is_running
                    && let Err(error) = self.update_video_status(
                        download_id,
                        "active",
                        Some(downloaded_bytes),
                        total_bytes,
                        None,
                    )
                {
                    self.status = format!("Could not save video progress: {error}");
                }
            } else {
                self.status = line;
            }
        }
        let mut queue_failed = None;
        let mut completed = Vec::new();
        let mut process_errors = Vec::new();
        self.children
            .retain_mut(|process| match process.child.try_wait() {
                Ok(Some(status)) => {
                    completed.push((
                        process.queue_download,
                        process.kind,
                        process.video_download,
                        process.stop_requested,
                        status,
                    ));
                    false
                }
                Ok(None) => true,
                Err(error) => {
                    process_errors.push(error.to_string());
                    true
                }
            });
        for error in process_errors {
            self.status = format!("Could not check Raja process: {error}");
        }
        for (queue_download, kind, video_download, stop_requested, status) in completed {
            if let Some(download_id) = video_download
                && !stop_requested
            {
                if status.success() {
                    if let Err(error) = self.complete_video_download(download_id) {
                        if let Err(save_error) = self.update_video_status(
                            download_id,
                            "failed",
                            None,
                            None,
                            Some(&error),
                        ) {
                            self.status =
                                format!("{error} (could not save failed state: {save_error})");
                        } else {
                            self.status = error;
                        }
                    }
                } else {
                    let message = format!("yt-dlp exited unsuccessfully with {status}");
                    if let Err(error) =
                        self.update_video_status(download_id, "failed", None, None, Some(&message))
                    {
                        self.status = format!("{message} (could not save failed state: {error})");
                    }
                }
            }
            if let Some(download_id) = queue_download {
                if !status.success() && !stop_requested {
                    queue_failed = Some(download_id);
                }
            } else {
                self.status = match (kind, status.success(), stop_requested) {
                    (ProcessKind::Core, true, _) => "Raja command completed.".to_owned(),
                    (ProcessKind::Core, false, _) => {
                        format!("Raja command exited with {status}.")
                    }
                    (ProcessKind::Video, true, _) => "Video extraction completed.".to_owned(),
                    (ProcessKind::Video, false, true) => {
                        "Video extraction stopped; resume it to continue.".to_owned()
                    }
                    (ProcessKind::Video, false, false) => {
                        format!("Video extraction failed with {status}: {}", self.status)
                    }
                };
            }
            self.last_refresh = Instant::now() - Duration::from_secs(2);
        }
        if let Some(download_id) = queue_failed {
            self.queue_running = false;
            self.status = format!("Queue paused because download {download_id} did not complete.");
        }
    }
}

impl Drop for RajaUi {
    fn drop(&mut self) {
        let mut paused_downloads = Vec::new();
        for process in &mut self.children {
            if matches!(process.kind, ProcessKind::Video) && !process.stop_requested {
                match terminate_process_tree(&mut process.child) {
                    Ok(()) => {
                        process.stop_requested = true;
                        if let Some(download_id) = process.video_download {
                            paused_downloads.push(download_id);
                        }
                    }
                    Err(error) => {
                        eprintln!("Could not stop video extraction during exit: {error}");
                    }
                }
            }
        }
        for download_id in paused_downloads {
            if let Err(error) = self.update_video_status(
                download_id,
                "paused",
                None,
                None,
                Some("Raja closed; video extraction paused"),
            ) {
                eprintln!("Could not save paused video status during exit: {error}");
            }
        }
    }
}

impl eframe::App for RajaUi {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        #[cfg(windows)]
        self.poll_tray(&context);
        self.poll_browser_inbox();
        self.poll_children_and_output();
        if self
            .scheduled_queue_at
            .is_some_and(|start_at| Instant::now() >= start_at)
        {
            self.scheduled_queue_at = None;
            self.start_queue();
        }
        self.advance_queue();
        if self.last_refresh.elapsed() >= Duration::from_secs(1) {
            self.refresh_downloads();
        }
        if context.input(|input| input.viewport().close_requested()) {
            #[cfg(windows)]
            if !self.exit_requested {
                context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                context.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                self.status = "RAJA is still running in the system tray.".to_owned();
            }
        }

        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::from_rgb(35, 36, 39);
        visuals.window_fill = egui::Color32::from_rgb(39, 40, 43);
        visuals.extreme_bg_color = egui::Color32::from_rgb(30, 31, 34);
        visuals.override_text_color = Some(egui::Color32::from_rgb(225, 228, 233));
        context.set_visuals(visuals);
        draw_menu_bar(self, ui);
        draw_toolbar(self, ui);

        egui::Panel::left("download-categories")
            .default_size(190.0)
            .min_size(150.0)
            .resizable(true)
            .show(ui, |ui| draw_categories(self, ui));

        egui::Panel::bottom("status-bar")
            .exact_size(24.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(&self.status);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(format!("{} download(s)", self.downloads.len()));
                    });
                });
            });
        egui::CentralPanel::default().show(ui, |ui| draw_download_table(self, ui));

        if self.show_add_dialog {
            draw_add_download_dialog(self, &context);
        }
        if self.show_forget_dialog {
            draw_forget_download_dialog(self, &context);
        }
        if self.show_scheduler_dialog {
            draw_scheduler_dialog(self, &context);
        }
        if self.show_options_dialog {
            draw_options_dialog(self, &context);
        }

        context.request_repaint_after(Duration::from_millis(250));
    }
}

fn draw_menu_bar(app: &mut RajaUi, ui: &mut egui::Ui) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(28, 29, 32))
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.menu_button("Tasks", |ui| {
                    ui.label("Download tasks are managed from the toolbar.");
                });
                ui.menu_button("File", |ui| {
                    if ui.button("Add URL...").clicked() {
                        app.show_add_dialog = true;
                        ui.close();
                    }
                    if ui.button("Hide to system tray").clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button("Downloads", |ui| {
                    let selected = app
                        .downloads
                        .iter()
                        .find(|download| Some(download.id) == app.selected_download);
                    let can_retry = selected.is_some_and(|download| {
                        matches!(download.status.as_str(), "failed" | "paused" | "queued")
                    });
                    let can_pause = selected.is_some_and(|download| download.status == "active");
                    if ui
                        .add_enabled(can_pause, egui::Button::new("Pause selected"))
                        .clicked()
                        && let Some(id) = app.selected_download
                    {
                        app.pause_download(id);
                        ui.close();
                    }
                    if ui
                        .add_enabled(can_retry, egui::Button::new("Resume selected"))
                        .clicked()
                        && let Some(id) = app.selected_download
                    {
                        app.resume_download(id);
                        ui.close();
                    }
                    if ui.button("Refresh").clicked() {
                        app.refresh_downloads();
                        ui.close();
                    }
                });
                ui.menu_button("View", |ui| {
                    ui.label("Choose a category in the left pane.");
                });
                ui.menu_button("Help", |ui| {
                    ui.label("Raja Download Manager");
                    ui.label("GPL-3.0-only");
                });
                ui.menu_button("Registration", |ui| {
                    ui.label("Raja is free software under GPL-3.0-only.");
                });
            });
        });
}

fn draw_toolbar(app: &mut RajaUi, ui: &mut egui::Ui) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(43, 44, 47))
        .inner_margin(egui::Margin::symmetric(8, 7))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.add(toolbar_button("＋", "Add URL")).clicked() {
                    app.show_add_dialog = true;
                }
                let selected = app
                    .downloads
                    .iter()
                    .find(|download| Some(download.id) == app.selected_download);
                let can_retry = selected.is_some_and(|download| {
                    matches!(download.status.as_str(), "failed" | "paused" | "queued")
                });
                let can_pause = selected.is_some_and(|download| download.status == "active");
                let can_open_folder = selected.is_some_and(|download| {
                    download.status == "complete" && download.output_path.is_file()
                });
                let can_forget = selected.is_some_and(|download| download.status == "complete");
                let can_pause_all = app
                    .downloads
                    .iter()
                    .any(|download| download.status == "active")
                    || app
                        .children
                        .iter()
                        .any(|process| matches!(process.kind, ProcessKind::Video));
                let can_start_queue = app.downloads.iter().any(|download| {
                    matches!(download.status.as_str(), "queued" | "paused" | "failed")
                });
                let can_schedule_queue =
                    !app.queue_running && (can_start_queue || app.scheduled_queue_at.is_some());
                if ui
                    .add_enabled(can_retry, toolbar_button("↓", "Resume"))
                    .clicked()
                    && let Some(id) = app.selected_download
                {
                    app.resume_download(id);
                }
                if ui
                    .add_enabled(can_pause, toolbar_button("Ⅱ", "Stop"))
                    .clicked()
                    && let Some(id) = app.selected_download
                {
                    app.pause_download(id);
                }
                if ui.add(toolbar_button("↻", "Refresh")).clicked() {
                    app.refresh_downloads();
                }
                if ui
                    .add_enabled(can_open_folder, toolbar_button("▣", "Open folder"))
                    .clicked()
                {
                    app.open_selected_folder();
                }
                ui.separator();
                if ui
                    .add_enabled(
                        can_pause_all,
                        egui::Button::new("▣\nStop All").min_size(egui::vec2(58.0, 48.0)),
                    )
                    .clicked()
                {
                    app.pause_all();
                }
                if ui
                    .add_enabled(
                        can_forget,
                        egui::Button::new("⌫\nForget").min_size(egui::vec2(58.0, 48.0)),
                    )
                    .clicked()
                {
                    app.show_forget_dialog = true;
                }
                if ui
                    .add(egui::Button::new("⚙\nOptions").min_size(egui::vec2(58.0, 48.0)))
                    .clicked()
                {
                    app.show_options_dialog = true;
                }
                let scheduler_label = app.scheduled_queue_at.map_or_else(
                    || "◷\nScheduler".to_owned(),
                    |start_at| {
                        format!(
                            "◷\n{}m",
                            start_at
                                .saturating_duration_since(Instant::now())
                                .as_secs()
                                .div_ceil(60)
                        )
                    },
                );
                if ui
                    .add_enabled(
                        can_schedule_queue,
                        egui::Button::new(scheduler_label).min_size(egui::vec2(68.0, 48.0)),
                    )
                    .clicked()
                {
                    app.show_scheduler_dialog = true;
                }
                let start_queue = if app.queue_running {
                    ui.add_enabled(
                        false,
                        egui::Button::new("⇧\nQueue Running").min_size(egui::vec2(86.0, 48.0)),
                    )
                } else {
                    ui.add_enabled(
                        can_start_queue,
                        egui::Button::new("⇧\nStart Queue").min_size(egui::vec2(78.0, 48.0)),
                    )
                };
                if start_queue.clicked() {
                    app.start_queue();
                }
                if ui
                    .add_enabled(
                        app.queue_running,
                        egui::Button::new("▣\nStop Queue").min_size(egui::vec2(78.0, 48.0)),
                    )
                    .clicked()
                {
                    app.stop_queue();
                }
            });
        });
}

fn toolbar_button<'a>(icon: &'a str, label: &'a str) -> egui::Button<'a> {
    egui::Button::new(
        egui::RichText::new(format!("{icon}\n{label}"))
            .color(egui::Color32::from_rgb(205, 211, 221))
            .size(13.0),
    )
    .min_size(egui::vec2(62.0, 48.0))
}

fn draw_categories(app: &mut RajaUi, ui: &mut egui::Ui) {
    ui.heading("Categories");
    ui.separator();
    category_item(ui, &mut app.category, DownloadCategory::All, "▾");
    ui.indent("all-downloads", |ui| {
        category_item(ui, &mut app.category, DownloadCategory::Compressed, "▣");
        category_item(ui, &mut app.category, DownloadCategory::Documents, "▤");
        category_item(ui, &mut app.category, DownloadCategory::Music, "♫");
        category_item(ui, &mut app.category, DownloadCategory::Programs, "⚙");
        category_item(ui, &mut app.category, DownloadCategory::Video, "▶");
    });
    ui.add_space(6.0);
    category_item(ui, &mut app.category, DownloadCategory::Unfinished, "◌");
    category_item(ui, &mut app.category, DownloadCategory::Finished, "✓");
    category_item(
        ui,
        &mut app.category,
        DownloadCategory::GrabberProjects,
        "◉",
    );
    category_item(ui, &mut app.category, DownloadCategory::Queues, "▤");
    ui.add_space(8.0);
    ui.separator();
    ui.label(
        egui::RichText::new("Categories are inferred from file names.")
            .small()
            .weak(),
    );
}

fn category_item(
    ui: &mut egui::Ui,
    selected: &mut DownloadCategory,
    category: DownloadCategory,
    icon: &str,
) {
    let response = ui.selectable_label(
        *selected == category,
        format!("{icon}  {}", category.label()),
    );
    if response.clicked() {
        *selected = category;
    }
}

fn draw_download_table(app: &mut RajaUi, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.heading(app.category.label());
        ui.label(
            egui::RichText::new(format!(
                "{} item(s)",
                app.downloads
                    .iter()
                    .filter(|download| app.category.matches(download))
                    .count()
            ))
            .small()
            .weak(),
        );
    });
    ui.separator();
    let downloads = app
        .downloads
        .iter()
        .filter(|download| app.category.matches(download))
        .cloned()
        .collect::<Vec<_>>();
    egui::ScrollArea::both()
        .id_salt("downloads-table-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new("downloads-grid")
                .striped(true)
                .min_col_width(46.0)
                .show(ui, |ui| {
                    for header in [
                        "File Name",
                        "Q",
                        "Size",
                        "Status",
                        "Time left",
                        "Transfer rate",
                        "Last Try Date",
                        "Description",
                    ] {
                        ui.strong(header);
                    }
                    ui.end_row();

                    for download in downloads {
                        let selected = app.selected_download == Some(download.id);
                        let response = ui.selectable_label(selected, &download.file_name);
                        if response.clicked() {
                            app.selected_download = Some(download.id);
                        }
                        response.on_hover_text(format!("Download ID {}", download.id));
                        ui.label(download.id.to_string());
                        ui.label(download.total_size.map_or_else(
                            || format!("{} bytes", download.durable_bytes),
                            format_bytes,
                        ));
                        ui.vertical(|ui| {
                            ui.label(status_label(&download.status));
                            if let Some(total_size) = download.total_size.filter(|size| *size > 0) {
                                let progress = download.durable_bytes as f32 / total_size as f32;
                                ui.add(
                                    egui::ProgressBar::new(progress.clamp(0.0, 1.0))
                                        .desired_width(120.0)
                                        .show_percentage(),
                                );
                            }
                        });
                        let sample = app.progress_samples.get(&download.id);
                        let is_active = download.status == "active";
                        let rate = sample
                            .filter(|_| is_active)
                            .and_then(|sample| sample.bytes_per_second);
                        ui.label(time_left_label(
                            download.total_size,
                            download.durable_bytes,
                            rate,
                            is_active,
                        ));
                        ui.label(transfer_rate_label(rate));
                        ui.label(last_attempt_label(download.last_attempt_at));
                        ui.label(
                            if download.status == "complete" && download.download_kind == "video" {
                                "Saved by yt-dlp"
                            } else if download.status == "complete" {
                                "Verified and saved"
                            } else if download.download_kind == "video" {
                                "Video page (yt-dlp)"
                            } else {
                                "HTTP/HTTPS download"
                            },
                        );
                        ui.end_row();
                    }
                });
        });
}

fn draw_add_download_dialog(app: &mut RajaUi, context: &egui::Context) {
    let mut open = true;
    let mut start_download = false;
    egui::Window::new("Add URL")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(520.0)
        .show(context, |ui| {
            ui.label("Download URL");
            let url_changed = ui
                .add(
                    egui::TextEdit::singleline(&mut app.url)
                        .desired_width(ui.available_width())
                        .hint_text("https://example.com/file.zip"),
                )
                .changed();
            if url_changed && (app.output_suggested || app.output.trim().is_empty()) {
                app.output = PathBuf::from(&app.default_download_directory)
                    .join(suggested_filename(&app.url))
                    .to_string_lossy()
                    .into_owned();
                app.output_suggested = true;
            }
            ui.checkbox(&mut app.video_page_download, "Extract video page with yt-dlp");
            if app.video_page_download {
                ui.label("Requires yt-dlp on PATH. Playlists are disabled; the extractor chooses the file extension.");
            }
            ui.add_space(6.0);
            ui.label(if app.video_page_download {
                "Save as (extension chosen by yt-dlp)"
            } else {
                "Save to"
            });
            ui.horizontal(|ui| {
                let output_changed = ui
                    .add(
                        egui::TextEdit::singleline(&mut app.output)
                            .desired_width(ui.available_width() - 8.0)
                            .hint_text(r"C:\Downloads\file.zip"),
                    )
                    .changed();
                if output_changed {
                    app.output_suggested = false;
                }
                if ui.button("Browse...").clicked() {
                    #[cfg(windows)]
                    {
                        let current_output = PathBuf::from(app.output.trim());
                        let file_name = current_output
                            .file_name()
                            .and_then(|name| name.to_str())
                            .map(str::to_owned)
                            .unwrap_or_else(|| suggested_filename(&app.url));
                        let mut dialog = rfd::FileDialog::new().set_file_name(file_name);
                        let initial_directory = current_output
                            .parent()
                            .filter(|parent| !parent.as_os_str().is_empty())
                            .filter(|parent| parent.is_dir())
                            .map(PathBuf::from)
                            .or_else(|| {
                                std::env::var_os("USERPROFILE")
                                    .map(PathBuf::from)
                                    .map(|home| home.join("Downloads"))
                                    .filter(|folder| folder.is_dir())
                            });
                        if let Some(directory) = initial_directory {
                            dialog = dialog.set_directory(directory);
                        }
                        if let Some(path) = dialog.save_file() {
                            app.output = path.to_string_lossy().into_owned();
                            app.output_suggested = false;
                        }
                    }
                    #[cfg(not(windows))]
                    {
                        app.status = "Destination browsing is currently supported on Windows only."
                            .to_owned();
                    }
                }
            });
            if !app.video_page_download {
                ui.label("Expected SHA-256 (optional)");
                ui.add(
                    egui::TextEdit::singleline(&mut app.sha256)
                        .desired_width(ui.available_width())
                        .hint_text("64 hexadecimal digits"),
                );
            }
            ui.collapsing("Advanced", |ui| {
                ui.label("Database");
                ui.add(
                    egui::TextEdit::singleline(&mut app.database)
                        .desired_width(ui.available_width())
                        .hint_text("Raja database path"),
                );
            });
            ui.separator();
            ui.horizontal(|ui| {
                start_download = ui
                    .add_enabled(
                        !app.url.trim().is_empty() && !app.output.trim().is_empty(),
                        egui::Button::new(if app.video_page_download {
                            "Extract Video"
                        } else {
                            "Start Download"
                        }),
                    )
                    .clicked();
                if ui.button("Cancel").clicked() {
                    app.show_add_dialog = false;
                }
            });
        });
    if !open {
        app.show_add_dialog = false;
    }
    if start_download {
        if app.video_page_download {
            let url = app.url.trim().to_owned();
            let output = app.output.trim().to_owned();
            app.start_video_extraction(&url, &output);
            app.show_add_dialog = false;
            return;
        }
        let mut args = vec![
            "download".to_owned(),
            app.url.trim().to_owned(),
            app.output.trim().to_owned(),
        ];
        if !app.sha256.trim().is_empty() {
            args.push("--sha256".to_owned());
            args.push(app.sha256.trim().to_owned());
        }
        app.run_core(&args);
        app.show_add_dialog = false;
    }
}

fn draw_forget_download_dialog(app: &mut RajaUi, context: &egui::Context) {
    let Some(download) = app
        .downloads
        .iter()
        .find(|download| Some(download.id) == app.selected_download)
    else {
        app.show_forget_dialog = false;
        return;
    };
    if download.status != "complete" {
        app.show_forget_dialog = false;
        return;
    }

    let download_id = download.id;
    let file_name = download.file_name.clone();
    let mut open = true;
    let mut forget = false;
    egui::Window::new("Remove from Raja history?")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(context, |ui| {
            ui.label(format!("Remove \"{file_name}\" from the download list?"));
            ui.label("The downloaded file will not be deleted.");
            ui.separator();
            ui.horizontal(|ui| {
                forget = ui.button("Remove from history").clicked();
                if ui.button("Cancel").clicked() {
                    app.show_forget_dialog = false;
                }
            });
        });
    if !open {
        app.show_forget_dialog = false;
    }
    if forget {
        app.run_core(&["forget".to_owned(), download_id.to_string()]);
        app.show_forget_dialog = false;
        app.selected_download = None;
    }
}

fn draw_scheduler_dialog(app: &mut RajaUi, context: &egui::Context) {
    let mut open = true;
    let mut schedule = false;
    let mut cancel = false;
    egui::Window::new("Schedule download queue")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(context, |ui| {
            ui.label("Start pending downloads after:");
            egui::ComboBox::from_id_salt("queue-schedule-delay")
                .selected_text(format!("{} minutes", app.schedule_after_minutes))
                .show_ui(ui, |ui| {
                    for minutes in [5, 15, 30, 60, 120, 240] {
                        ui.selectable_value(
                            &mut app.schedule_after_minutes,
                            minutes,
                            format!("{minutes} minutes"),
                        );
                    }
                });
            ui.label("Raja must remain open and Windows must stay awake.");
            ui.separator();
            ui.horizontal(|ui| {
                schedule = ui.button("Schedule queue").clicked();
                if app.scheduled_queue_at.is_some() {
                    cancel = ui.button("Cancel schedule").clicked();
                }
                if ui.button("Close").clicked() {
                    app.show_scheduler_dialog = false;
                }
            });
        });
    if !open {
        app.show_scheduler_dialog = false;
    }
    if schedule {
        let delay = Duration::from_secs(u64::from(app.schedule_after_minutes) * 60);
        app.scheduled_queue_at = Instant::now().checked_add(delay);
        app.show_scheduler_dialog = false;
        app.status = format!(
            "Queue scheduled to start in {} minutes.",
            app.schedule_after_minutes
        );
    }
    if cancel {
        app.scheduled_queue_at = None;
        app.show_scheduler_dialog = false;
        app.status = "Scheduled queue cancelled.".to_owned();
    }
}

fn draw_options_dialog(app: &mut RajaUi, context: &egui::Context) {
    let mut open = true;
    let mut select_directory = false;
    let mut save = false;
    egui::Window::new("Raja options")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(context, |ui| {
            ui.label("Default download directory");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut app.default_download_directory)
                        .desired_width(ui.available_width() - 8.0),
                );
                select_directory = ui.button("Browse...").clicked();
            });
            ui.separator();
            ui.horizontal(|ui| {
                save = ui.button("Save").clicked();
                if ui.button("Cancel").clicked() {
                    app.show_options_dialog = false;
                }
            });
        });
    if !open {
        app.show_options_dialog = false;
    }
    if select_directory {
        #[cfg(windows)]
        if let Some(directory) = rfd::FileDialog::new()
            .set_directory(&app.default_download_directory)
            .pick_folder()
        {
            app.default_download_directory = directory.to_string_lossy().into_owned();
        }
        #[cfg(not(windows))]
        {
            app.status = "Directory browsing is currently supported on Windows only.".to_owned();
        }
    }
    if save {
        let directory = PathBuf::from(app.default_download_directory.trim());
        if !directory.is_dir() {
            app.status = format!(
                "Default download directory does not exist: {}",
                directory.display()
            );
            return;
        }
        match Storage::open(&app.database).and_then(|mut storage| {
            storage.set_setting("download_directory", directory.to_string_lossy().as_ref())
        }) {
            Ok(()) => {
                app.default_download_directory = directory.to_string_lossy().into_owned();
                app.status = "Default download directory saved.".to_owned();
                app.show_options_dialog = false;
            }
            Err(error) => app.status = format!("Could not save Raja settings: {error}"),
        }
    }
}

fn default_download_directory() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .map(|home| home.join("Downloads"))
        .filter(|directory| directory.is_dir())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("Downloads")
        })
}

fn queueable_download_ids(downloads: &[DownloadSummary]) -> VecDeque<i64> {
    downloads
        .iter()
        .filter(|download| matches!(download.status.as_str(), "queued" | "paused" | "failed"))
        .map(|download| download.id)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn status_label(status: &str) -> &str {
    match status {
        "complete" => "Complete",
        "active" => "Downloading",
        "paused" => "Paused",
        "failed" => "Error",
        "queued" => "Queued",
        other => other,
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{size:.2} {}", UNITS[unit])
    }
}

fn transfer_rate_label(rate: Option<f64>) -> String {
    rate.map_or_else(
        || "—".to_owned(),
        |bytes_per_second| format!("{}/s", format_bytes(bytes_per_second.round() as u64)),
    )
}

fn time_left_label(
    total_size: Option<u64>,
    durable_bytes: u64,
    rate: Option<f64>,
    is_active: bool,
) -> String {
    let Some(rate) = rate.filter(|rate| is_active && *rate > 0.0) else {
        return "—".to_owned();
    };
    let Some(remaining_bytes) = total_size.map(|size| size.saturating_sub(durable_bytes)) else {
        return "—".to_owned();
    };
    format_duration((remaining_bytes as f64 / rate).ceil() as u64)
}

fn format_duration(seconds: u64) -> String {
    if seconds >= 86_400 {
        format!("{}d {:02}h", seconds / 86_400, seconds / 3_600 % 24)
    } else if seconds >= 3_600 {
        format!("{}h {:02}m", seconds / 3_600, seconds / 60 % 60)
    } else if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

fn last_attempt_label(timestamp: Option<i64>) -> String {
    timestamp
        .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        .map_or_else(
            || "—".to_owned(),
            |date_time| format!("{} UTC", date_time.format("%Y-%m-%d %H:%M")),
        )
}

fn suggested_filename(url: &str) -> String {
    let filename = reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| {
            parsed
                .path_segments()
                .and_then(Iterator::last)
                .filter(|segment| !segment.is_empty())
                .map(str::to_owned)
        })
        .filter(|name| name != "." && name != "..")
        .map(|name| {
            let sanitized = name
                .chars()
                .map(|character| {
                    if character.is_control()
                        || matches!(
                            character,
                            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                        )
                    {
                        '_'
                    } else {
                        character
                    }
                })
                .collect::<String>();
            sanitized.trim_matches([' ', '.']).to_owned()
        })
        .filter(|name| !name.is_empty());
    filename.unwrap_or_else(|| "download".to_owned())
}

fn browser_suggested_filename(url: &str, video: bool) -> String {
    if video
        && let Ok(parsed) = reqwest::Url::parse(url)
        && let Some(host) = parsed.host_str()
        && (host == "youtube.com" || host.ends_with(".youtube.com") || host == "youtu.be")
    {
        let video_id = parsed
            .query_pairs()
            .find(|(key, _)| key == "v")
            .map(|(_, value)| value.into_owned())
            .or_else(|| {
                parsed
                    .path_segments()
                    .and_then(Iterator::last)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "video".to_owned());
        return format!("YouTube-{video_id}.mp4");
    }
    suggested_filename(url)
}

fn video_output_template(output: &std::path::Path) -> Result<PathBuf, String> {
    let file_stem = output
        .file_stem()
        .filter(|stem| !stem.is_empty())
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| "Video destination must include a valid file name.".to_owned())?;
    if file_stem == "." || file_stem == ".." {
        return Err("Video destination must include a valid file name.".to_owned());
    }
    let output_base = output
        .with_file_name(file_stem)
        .to_str()
        .ok_or_else(|| "Video destination must be a valid Unicode path.".to_owned())?
        .replace('%', "%%");
    Ok(PathBuf::from(format!("{output_base}.%(ext)s")))
}

fn video_extraction_arguments(url: &str, output: &std::path::Path) -> Result<Vec<String>, String> {
    video_extraction_arguments_for_height(url, output, Some(1080))
}

fn video_extraction_arguments_for_height(
    url: &str,
    output: &std::path::Path,
    max_height: Option<u32>,
) -> Result<Vec<String>, String> {
    let url = validate_external_url(url)?;
    let output_template = video_output_template(output)?;
    let max_height = max_height.unwrap_or(1080);
    if !(144..=1080).contains(&max_height) {
        return Err("Video quality must be between 144p and 1080p.".to_owned());
    }
    let format =
        format!("bestvideo[height<={max_height}]+bestaudio/best[height<={max_height}]/best");
    Ok(vec![
        "--ignore-config".to_owned(),
        "--newline".to_owned(),
        "--no-colors".to_owned(),
        "--no-playlist".to_owned(),
        "--continue".to_owned(),
        "--no-overwrites".to_owned(),
        "--progress".to_owned(),
        "--format".to_owned(),
        format,
        "--output".to_owned(),
        output_template.to_string_lossy().into_owned(),
        url,
    ])
}

fn video_job_arguments(
    download_id: i64,
    url: &str,
    output: &std::path::Path,
) -> Result<Vec<String>, String> {
    video_job_arguments_for_height(download_id, url, output, Some(1080))
}

fn video_job_arguments_for_height(
    download_id: i64,
    url: &str,
    output: &std::path::Path,
    max_height: Option<u32>,
) -> Result<Vec<String>, String> {
    let mut arguments = video_extraction_arguments_for_height(url, output, max_height)?;
    let url = arguments
        .pop()
        .ok_or_else(|| "Video extraction URL is missing.".to_owned())?;
    arguments.extend([
        "--progress-template".to_owned(),
        format!(
            "download:RAJA_PROGRESS:{download_id}:%(progress.downloaded_bytes)s:%(progress.total_bytes_estimate)s"
        ),
        "--print".to_owned(),
        format!("after_move:RAJA_OUTPUT:{download_id}:%(filepath)s"),
        url,
    ]);
    Ok(arguments)
}

fn parse_launch_url(
    arguments: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<Option<String>, String> {
    let mut arguments = arguments.into_iter();
    let Some(argument) = arguments.next() else {
        return Ok(None);
    };
    if argument == "--add-url" {
        let url = arguments
            .next()
            .ok_or_else(|| "--add-url requires an HTTP or HTTPS URL.".to_owned())?
            .into_string()
            .map_err(|_| "The URL must contain valid Unicode.".to_owned())?;
        if arguments.next().is_some() {
            return Err("Unexpected arguments after --add-url.".to_owned());
        }
        return validate_external_url(&url).map(Some);
    }

    let protocol_url = argument
        .into_string()
        .map_err(|_| "The Raja link must contain valid Unicode.".to_owned())?;
    let parsed = reqwest::Url::parse(&protocol_url)
        .map_err(|_| "The Raja browser link is invalid.".to_owned())?;
    if parsed.scheme() != "raja" || parsed.host_str() != Some("download") {
        return Err("Unsupported Raja link.".to_owned());
    }
    if arguments.next().is_some() {
        return Err("Unexpected arguments after the Raja link.".to_owned());
    }
    let mut urls = parsed
        .query_pairs()
        .filter(|(key, _)| key == "url")
        .map(|(_, value)| value.into_owned());
    let url = urls
        .next()
        .ok_or_else(|| "The Raja browser link does not contain a URL.".to_owned())?;
    if urls.next().is_some() {
        return Err("The Raja browser link contains more than one URL.".to_owned());
    }
    validate_external_url(&url).map(Some)
}

fn validate_external_url(url: &str) -> Result<String, String> {
    let parsed = reqwest::Url::parse(url).map_err(|_| "The download URL is invalid.".to_owned())?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(
            "Only HTTP or HTTPS URLs without embedded credentials are accepted.".to_owned(),
        );
    }
    Ok(parsed.to_string())
}

fn forward_output(reader: impl std::io::Read + Send + 'static, sender: Sender<String>) {
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            match line {
                Ok(line) => {
                    let _ = sender.send(line);
                }
                Err(error) => {
                    let _ = sender.send(format!("Could not read Raja output: {error}"));
                    break;
                }
            }
        }
    });
}

fn parse_video_progress_line(line: &str) -> Option<(i64, u64, Option<u64>)> {
    let progress = line.strip_prefix("RAJA_PROGRESS:")?;
    let mut fields = progress.split(':');
    let download_id = fields.next()?.parse().ok()?;
    let downloaded_bytes = fields.next()?.parse().ok()?;
    let total_bytes = fields.next().and_then(|value| value.parse().ok());
    if fields.next().is_some() {
        return None;
    }
    Some((download_id, downloaded_bytes, total_bytes))
}

fn parse_video_output_line(line: &str) -> Option<(i64, String)> {
    let output = line.strip_prefix("RAJA_OUTPUT:")?;
    let (download_id, output_path) = output.split_once(':')?;
    let download_id = download_id.parse().ok()?;
    if output_path.trim().is_empty() {
        return None;
    }
    Some((download_id, output_path.to_owned()))
}

fn terminate_process_tree(child: &mut Child) -> std::io::Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    #[cfg(windows)]
    {
        let process_id = child.id().to_string();
        let result = Command::new("taskkill")
            .args(["/PID", &process_id, "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()?;
        if !result.status.success() && child.try_wait()?.is_none() {
            return Err(std::io::Error::other(
                String::from_utf8_lossy(&result.stderr).trim().to_owned(),
            ));
        }
    }
    #[cfg(not(windows))]
    child.kill()?;
    child.wait()?;
    Ok(())
}

#[cfg(windows)]
struct SingleInstance(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl SingleInstance {
    fn acquire() -> Result<Option<Self>, String> {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
        use windows_sys::Win32::System::Threading::CreateMutexW;

        let name: Vec<u16> = "Local\\RAJA.DownloadManager\0".encode_utf16().collect();
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(format!(
                "Could not create RAJA instance mutex: {}",
                unsafe { GetLastError() }
            ));
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe {
                CloseHandle(handle);
            }
            return Ok(None);
        }
        Ok(Some(Self(handle)))
    }
}

#[cfg(windows)]
impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

fn main() -> eframe::Result<()> {
    #[cfg(windows)]
    let _instance = match SingleInstance::acquire() {
        Ok(Some(instance)) => instance,
        Ok(None) => return Ok(()),
        Err(error) => return Err(eframe::Error::AppCreation(error.into())),
    };
    #[cfg(windows)]
    let tray = tray::TrayHandle::new().map_err(|error| eframe::Error::AppCreation(error.into()))?;

    let mut arguments = std::env::args_os().skip(1);
    let background = arguments
        .next()
        .is_some_and(|argument| argument == "--background");
    let (initial_url, launch_error) = match parse_launch_url(arguments) {
        Ok(url) => (url, None),
        Err(error) => (None, Some(error)),
    };
    let app = RajaUi::new(
        initial_url,
        launch_error,
        #[cfg(windows)]
        tray,
    )
    .map_err(eframe::Error::AppCreation)?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, 700.0])
            .with_min_inner_size([760.0, 520.0])
            .with_visible(!background),
        ..Default::default()
    };
    eframe::run_native("Raja", options, Box::new(|_| Ok(Box::new(app))))
}

#[cfg(test)]
mod tests {
    use super::{
        DownloadCategory, ProgressSample, browser_suggested_filename, format_duration,
        last_attempt_label, parse_launch_url, parse_video_output_line, parse_video_progress_line,
        queueable_download_ids, suggested_filename, time_left_label, transfer_rate_label,
        video_extraction_arguments, video_extraction_arguments_for_height, video_job_arguments,
        video_output_template,
    };
    use raja_core::storage::DownloadSummary;
    use std::ffi::OsString;
    use std::path::Path;
    use std::time::{Duration, Instant};

    #[test]
    fn suggests_last_url_path_component_without_query_string() {
        assert_eq!(
            suggested_filename("https://example.com/files/report.pdf?token=secret"),
            "report.pdf"
        );
    }

    #[test]
    fn falls_back_to_download_for_urls_without_a_filename() {
        assert_eq!(suggested_filename("https://example.com/"), "download");
        assert_eq!(suggested_filename("not a URL"), "download");
    }

    #[test]
    fn suggests_video_filenames_from_youtube_ids() {
        assert_eq!(
            browser_suggested_filename("https://www.youtube.com/watch?v=abc123", true),
            "YouTube-abc123.mp4"
        );
        assert_eq!(
            browser_suggested_filename("https://youtu.be/clip456", true),
            "YouTube-clip456.mp4"
        );
    }

    #[test]
    fn keeps_encoded_path_separators_inside_the_suggested_name() {
        assert_eq!(
            suggested_filename("https://example.com/files/a%2Fb.txt"),
            "a%2Fb.txt"
        );
    }

    #[test]
    fn video_output_template_uses_chosen_stem_and_escapes_percent_signs() {
        assert_eq!(
            video_output_template(Path::new(r"C:\100% Videos\my 100% clip.mp4")).unwrap(),
            Path::new(r"C:\100%% Videos\my 100%% clip.%(ext)s")
        );
    }

    #[test]
    fn video_output_template_rejects_empty_or_non_unicode_names() {
        assert!(video_output_template(Path::new("")).is_err());
    }

    #[test]
    fn video_extraction_arguments_disable_playlists_and_keep_user_paths_as_arguments() {
        assert_eq!(
            video_extraction_arguments(
                "https://example.com/watch?v=clip",
                Path::new(r"C:\My Videos\clip.mp4")
            )
            .unwrap(),
            [
                "--ignore-config",
                "--newline",
                "--no-colors",
                "--no-playlist",
                "--continue",
                "--no-overwrites",
                "--progress",
                "--format",
                "bestvideo[height<=1080]+bestaudio/best[height<=1080]/best",
                "--output",
                r"C:\My Videos\clip.%(ext)s",
                "https://example.com/watch?v=clip"
            ]
        );
    }

    #[test]
    fn video_format_selection_caps_the_extractor_to_the_chosen_height() {
        assert_eq!(
            video_extraction_arguments_for_height(
                "https://example.com/watch?v=clip",
                Path::new("clip.mp4"),
                Some(720)
            )
            .unwrap()
            .windows(2)
            .find(|pair| pair[0] == "--format")
            .unwrap()[1],
            "bestvideo[height<=720]+bestaudio/best[height<=720]/best"
        );
        assert!(
            video_extraction_arguments_for_height(
                "https://example.com/watch?v=clip",
                Path::new("clip.mp4"),
                Some(4320)
            )
            .is_err()
        );
    }

    #[test]
    fn video_extraction_rejects_non_http_urls() {
        assert!(video_extraction_arguments("file:///C:/private", Path::new("clip.mp4")).is_err());
    }

    #[test]
    fn video_job_arguments_tag_progress_and_final_paths_per_download() {
        let arguments =
            video_job_arguments(9, "https://example.com/watch?v=clip", Path::new("clip.mp4"))
                .unwrap();
        assert!(arguments.contains(
            &"download:RAJA_PROGRESS:9:%(progress.downloaded_bytes)s:%(progress.total_bytes_estimate)s"
                .to_owned()
        ));
        assert!(arguments.contains(&"after_move:RAJA_OUTPUT:9:%(filepath)s".to_owned()));
        assert_eq!(
            arguments.last().map(String::as_str),
            Some("https://example.com/watch?v=clip")
        );
    }

    #[test]
    fn parses_video_progress_and_optional_total_size() {
        assert_eq!(
            parse_video_progress_line("RAJA_PROGRESS:42:128:512"),
            Some((42, 128, Some(512)))
        );
        assert_eq!(
            parse_video_progress_line("RAJA_PROGRESS:42:128:NA"),
            Some((42, 128, None))
        );
        assert_eq!(parse_video_progress_line("[download] 50%"), None);
    }

    #[test]
    fn parses_final_video_paths_including_windows_drive_letters() {
        assert_eq!(
            parse_video_output_line(r"RAJA_OUTPUT:42:C:\Videos\clip.webm"),
            Some((42, r"C:\Videos\clip.webm".to_owned()))
        );
        assert_eq!(parse_video_output_line("RAJA_OUTPUT:42:"), None);
    }

    #[test]
    fn accepts_encoded_http_url_from_browser_protocol_link() {
        let url = "https://example.com/a file.zip?x=1&y=two";
        let mut protocol_link = reqwest::Url::parse("raja://download").unwrap();
        protocol_link.query_pairs_mut().append_pair("url", url);
        assert_eq!(
            parse_launch_url([OsString::from(protocol_link.to_string())]).unwrap(),
            Some("https://example.com/a%20file.zip?x=1&y=two".to_owned())
        );
    }

    #[test]
    fn rejects_non_http_urls_from_external_launch_requests() {
        assert!(
            parse_launch_url([
                OsString::from("--add-url"),
                OsString::from("file:///C:/secret")
            ])
            .is_err()
        );
        assert!(
            parse_launch_url([OsString::from("raja://download?url=javascript%3Aalert(1)")])
                .is_err()
        );
    }

    #[test]
    fn rejects_urls_with_embedded_credentials() {
        assert!(
            parse_launch_url([
                OsString::from("--add-url"),
                OsString::from("https://user:pass@example.com/file")
            ])
            .is_err()
        );
    }

    #[test]
    fn estimates_rate_from_durable_progress_and_expires_when_stalled() {
        let start = Instant::now();
        let mut sample = ProgressSample::new(0, start);
        sample.observe(2_000, start + Duration::from_secs(2));
        assert_eq!(sample.bytes_per_second, Some(1_000.0));
        sample.observe(2_000, start + Duration::from_secs(5));
        assert_eq!(sample.bytes_per_second, Some(0.0));
    }

    #[test]
    fn formats_progress_estimates_without_inventing_unknown_values() {
        assert_eq!(transfer_rate_label(Some(1_024.0)), "1.00 KB/s");
        assert_eq!(
            time_left_label(Some(10_000), 5_000, Some(1_000.0), true),
            "5s"
        );
        assert_eq!(time_left_label(Some(10_000), 5_000, None, true), "—");
        assert_eq!(format_duration(90_061), "1d 01h");
    }

    #[test]
    fn formats_last_attempt_date_in_utc() {
        assert_eq!(last_attempt_label(Some(0)), "1970-01-01 00:00 UTC");
        assert_eq!(last_attempt_label(None), "—");
    }

    #[test]
    fn queue_starts_eligible_downloads_in_oldest_first_order() {
        let downloads = [("queued", 8), ("complete", 7), ("failed", 6), ("paused", 5)]
            .into_iter()
            .map(|(status, id)| DownloadSummary {
                id,
                file_name: format!("{id}.bin"),
                output_path: std::path::PathBuf::from(format!("{id}.bin")),
                status: status.to_owned(),
                download_kind: "http".to_owned(),
                source_url: format!("https://example.test/{id}.bin"),
                total_size: Some(100),
                last_attempt_at: None,
                durable_bytes: 0,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            queueable_download_ids(&downloads)
                .into_iter()
                .collect::<Vec<_>>(),
            vec![5, 6, 8]
        );
        assert!(DownloadCategory::Queues.matches(&downloads[2]));
        assert!(!DownloadCategory::Queues.matches(&downloads[1]));
    }

    #[test]
    fn video_page_jobs_are_in_resume_queue_and_grabber_category() {
        let video = DownloadSummary {
            id: 21,
            file_name: "lecture.mp4".to_owned(),
            output_path: std::path::PathBuf::from("lecture.mp4"),
            status: "paused".to_owned(),
            download_kind: "video".to_owned(),
            source_url: "https://example.test/watch".to_owned(),
            total_size: None,
            last_attempt_at: None,
            durable_bytes: 123,
        };
        assert_eq!(
            queueable_download_ids(std::slice::from_ref(&video)).pop_front(),
            Some(21)
        );
        assert!(DownloadCategory::GrabberProjects.matches(&video));
        assert!(DownloadCategory::Video.matches(&video));
    }

    #[cfg(windows)]
    #[test]
    fn terminate_process_tree_stops_the_requested_process() {
        use super::terminate_process_tree;
        use std::process::{Command, Stdio};

        let mut child = Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        terminate_process_tree(&mut child).unwrap();
        assert!(child.try_wait().unwrap().is_some());
    }
}
