#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(windows)]
#[path = "raja_ui/tray.rs"]
mod tray;

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui;
use raja_core::storage::{DownloadSummary, Storage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddFileCategory {
    Video,
    Compressed,
    Documents,
    Music,
    Programs,
    General,
}

impl AddFileCategory {
    fn label(self) -> &'static str {
        match self {
            Self::Video => "Video",
            Self::Compressed => "Compressed",
            Self::Documents => "Documents",
            Self::Music => "Music",
            Self::Programs => "Programs",
            Self::General => "General",
        }
    }

    fn subfolder(self) -> &'static str {
        match self {
            Self::Video => "Video",
            Self::Compressed => "Compressed",
            Self::Documents => "Documents",
            Self::Music => "Music",
            Self::Programs => "Programs",
            Self::General => "",
        }
    }

    fn from_filename(file_name: &str) -> Self {
        let ext = PathBuf::from(file_name)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match ext.as_str() {
            "avi" | "m4v" | "mkv" | "mov" | "mp4" | "mpeg" | "mpg" | "webm" | "wmv" | "flv" => {
                Self::Video
            }
            "7z" | "bz" | "bz2" | "gz" | "rar" | "tar" | "xz" | "zip" => Self::Compressed,
            "csv" | "doc" | "docx" | "epub" | "pdf" | "ppt" | "pptx" | "rtf" | "txt" | "xls"
            | "xlsx" => Self::Documents,
            "aac" | "aiff" | "flac" | "m4a" | "mp3" | "ogg" | "wav" | "wma" => Self::Music,
            "appx" | "deb" | "dmg" | "exe" | "msi" | "pkg" | "rpm" | "apk" | "iso" => {
                Self::Programs
            }
            _ => Self::General,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TorrentFilePriority {
    DoNotDownload,
    Normal,
    High,
    Maximum,
}

impl TorrentFilePriority {
    fn label(self) -> &'static str {
        match self {
            Self::DoNotDownload => "Do not download",
            Self::Normal => "Normal",
            Self::High => "High",
            Self::Maximum => "Maximum",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Normal => Self::High,
            Self::High => Self::Maximum,
            Self::Maximum => Self::DoNotDownload,
            Self::DoNotDownload => Self::Normal,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DoubleClickAction {
    OpenFile,
    OpenFolder,
    ShowProperties,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TorrentFileEntry {
    aria2_index: usize,
    rel_path: Vec<String>,
    size: u64,
    selected: bool,
    priority: TorrentFilePriority,
}

struct RajaUi {
    url: String,
    output: String,
    output_suggested: bool,
    video_page_download: bool,
    sha256: String,
    add_dialog_category: AddFileCategory,
    add_category_dir: String,
    add_remember_category_path: bool,
    add_description: String,
    add_dialog_max_height: Option<u32>,
    add_dialog_file_size: Option<u64>,
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
    show_torrent_dialog: bool,
    show_forget_dialog: bool,
    delete_files_from_disk_too: bool,
    dont_show_delete_confirm_again: bool,
    double_click_action: DoubleClickAction,
    show_move_rename_dialog: bool,
    move_rename_path: String,
    show_refresh_address_dialog: bool,
    refresh_address_url: String,
    show_properties_dialog: bool,
    prop_download_id: Option<i64>,
    prop_save_to: String,
    prop_address: String,
    prop_description: String,
    prop_referer: String,
    prop_login: String,
    prop_password: String,
    show_delete_completed_dialog: bool,
    show_scheduler_dialog: bool,
    show_options_dialog: bool,
    show_tell_friend_dialog: bool,
    show_categories_panel: bool,
    all_categories_expanded: bool,
    torrent_source: String,
    torrent_output_dir: String,
    torrent_root_name: String,
    torrent_files: Vec<TorrentFileEntry>,
    torrent_collapsed_folders: HashSet<String>,
    torrent_file_filter: String,
    torrent_sort_ascending: bool,
    column_widths: [f32; 8],
    columns_initialized: bool,
    max_connections: u32,
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProcessKind {
    Core,
    Video,
    Torrent,
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
            Self::GrabberProjects => "Grabber projects",
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
        let mut init_storage = Storage::open(&database)?;
        let saved_directory = init_storage.setting("download_directory")?;
        let saved_max_conn = init_storage
            .setting("max_connections")?
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|&n| matches!(n, 1 | 2 | 4 | 8 | 16 | 32));
        let max_connections = match saved_max_conn {
            Some(n) => n,
            None => {
                let _ = init_storage.set_setting("max_connections", "8");
                8
            }
        };
        drop(init_storage);
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
        let default_download_dir_str = default_download_directory.to_string_lossy().into_owned();
        let initial_cat = suggested_output
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(AddFileCategory::from_filename)
            .unwrap_or(AddFileCategory::Video);
        let initial_cat_dir = category_directory_path(&default_download_dir_str, initial_cat);
        let (default_torrent_root, default_torrent_files) = default_sample_torrent_tree();
        let mut app = Self {
            url: initial_url.clone().unwrap_or_default(),
            output: suggested_output
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            output_suggested: suggested_output.is_some(),
            video_page_download: false,
            sha256: String::new(),
            add_dialog_category: initial_cat,
            add_category_dir: initial_cat_dir,
            add_remember_category_path: false,
            add_description: String::new(),
            add_dialog_max_height: None,
            add_dialog_file_size: None,
            default_download_directory: default_download_dir_str.clone(),
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
            show_torrent_dialog: false,
            show_forget_dialog: false,
            delete_files_from_disk_too: false,
            dont_show_delete_confirm_again: false,
            double_click_action: DoubleClickAction::OpenFile,
            show_move_rename_dialog: false,
            move_rename_path: String::new(),
            show_refresh_address_dialog: false,
            refresh_address_url: String::new(),
            show_properties_dialog: false,
            prop_download_id: None,
            prop_save_to: String::new(),
            prop_address: String::new(),
            prop_description: String::new(),
            prop_referer: String::new(),
            prop_login: String::new(),
            prop_password: String::new(),
            show_delete_completed_dialog: false,
            show_scheduler_dialog: false,
            show_options_dialog: false,
            show_tell_friend_dialog: false,
            show_categories_panel: true,
            all_categories_expanded: true,
            torrent_source: String::new(),
            torrent_output_dir: default_download_dir_str,
            torrent_root_name: default_torrent_root,
            torrent_files: default_torrent_files,
            torrent_collapsed_folders: HashSet::new(),
            torrent_file_filter: String::new(),
            torrent_sort_ascending: true,
            column_widths: [320.0, 26.0, 82.0, 82.0, 76.0, 90.0, 148.0, 130.0],
            columns_initialized: false,
            max_connections,
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
            if matches!(process.kind, ProcessKind::Video | ProcessKind::Torrent) {
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

    fn complete_torrent_download(&self, download_id: i64) -> Result<(), String> {
        let summary = Storage::open(&self.database)
            .and_then(|storage| {
                storage.list_downloads().map(|downloads| {
                    downloads
                        .into_iter()
                        .find(|download| download.id == download_id)
                })
            })
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("Torrent download {download_id} is missing from history."))?;
        let final_bytes = if let Ok(metadata) = std::fs::metadata(&summary.output_path)
            && metadata.is_file()
            && metadata.len() > 0
        {
            Some(metadata.len())
        } else {
            summary
                .total_size
                .or_else(|| (summary.durable_bytes > 0).then_some(summary.durable_bytes))
        };
        self.update_video_status(
            download_id,
            "complete",
            final_bytes,
            final_bytes,
            Some("Torrent download completed"),
        )
    }

    fn start_torrent_download_with_selection(
        &mut self,
        source: &str,
        output_dir: &str,
        select_file: Option<&str>,
    ) {
        let normalized_source = match validate_torrent_source(source) {
            Ok(s) => s,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        let output_dir_path = PathBuf::from(output_dir.trim());
        if output_dir_path.as_os_str().is_empty() {
            self.status = "Torrent destination folder must not be empty.".to_owned();
            return;
        }
        if let Err(error) = std::fs::create_dir_all(&output_dir_path) {
            self.status = format!("Could not create the torrent destination folder: {error}");
            return;
        }
        let Some(output_dir_str) = output_dir_path.to_str() else {
            self.status = "Torrent destination folder must be a valid Unicode path.".to_owned();
            return;
        };
        let file_name = if !self.torrent_root_name.trim().is_empty()
            && PathBuf::from(&normalized_source)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("torrent"))
        {
            sanitize_windows_filename(self.torrent_root_name.trim())
        } else {
            suggested_torrent_name(&normalized_source)
        };
        let download_id = match Storage::open(&self.database).and_then(|mut storage| {
            storage.create_video_download(&normalized_source, &file_name, output_dir_str)
        }) {
            Ok(id) => id,
            Err(error) => {
                self.status = format!("Could not save torrent download to Raja history: {error}");
                return;
            }
        };
        let target_path = output_dir_path.join(&file_name);
        self.start_torrent_job_with_selection(
            download_id,
            &normalized_source,
            &target_path,
            None,
            select_file,
        );
    }

    fn start_torrent_job(
        &mut self,
        download_id: i64,
        source: &str,
        output: &std::path::Path,
        queue_download: Option<i64>,
    ) -> bool {
        self.start_torrent_job_with_selection(download_id, source, output, queue_download, None)
    }

    fn start_torrent_job_with_selection(
        &mut self,
        download_id: i64,
        source: &str,
        output: &std::path::Path,
        queue_download: Option<i64>,
        select_file: Option<&str>,
    ) -> bool {
        if self
            .children
            .iter()
            .any(|process| process.video_download == Some(download_id))
        {
            self.status = format!("Torrent download {download_id} is already running.");
            return false;
        }
        let output_dir = if output.is_dir() {
            output
        } else {
            output
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| std::path::Path::new("."))
        };
        let Some(dir_str) = output_dir.to_str() else {
            let msg = "Torrent destination folder must be a valid Unicode path.".to_owned();
            self.status = msg.clone();
            self.fail_video_job(download_id, &msg);
            return false;
        };
        let conn_count = self.max_connections.clamp(1, 16);
        let mut arguments = vec![
            format!("--dir={dir_str}"),
            format!("--max-connection-per-server={conn_count}"),
            format!("--split={}", self.max_connections.clamp(1, 32)),
            "--seed-time=0".to_owned(),
            "--summary-interval=1".to_owned(),
            "--human-readable=false".to_owned(),
            "--show-console-readout=false".to_owned(),
            "--console-log-level=notice".to_owned(),
            "--continue=true".to_owned(),
            "--allow-overwrite=true".to_owned(),
            "--bt-save-metadata=false".to_owned(),
            "--follow-torrent=mem".to_owned(),
        ];
        if let Some(sel) = select_file.filter(|s| !s.trim().is_empty()) {
            arguments.push(format!("--select-file={}", sel.trim()));
        }
        arguments.push(source.to_owned());
        let mut command = Command::new(raja_core::video_tools::aria2c_executable());
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
                let prefix = format!("RAJA_TORRENT:{download_id}:");
                if let Some(stdout) = child.stdout.take() {
                    forward_prefixed_output(stdout, prefix.clone(), self.output_tx.clone());
                }
                if let Some(stderr) = child.stderr.take() {
                    forward_prefixed_output(stderr, prefix, self.output_tx.clone());
                }
                if let Err(error) =
                    self.update_video_status(download_id, "active", None, None, None)
                {
                    let _ = terminate_process_tree(&mut child);
                    self.status = format!("Could not update torrent history: {error}");
                    return false;
                }
                self.children.push(RunningProcess {
                    child,
                    queue_download,
                    kind: ProcessKind::Torrent,
                    video_download: Some(download_id),
                    stop_requested: false,
                });
                self.status = "Torrent / Magnet download started with aria2c.".to_owned();
                self.last_refresh = Instant::now() - Duration::from_secs(2);
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.status =
                    "aria2c was not found. Install aria2 (`winget install aria2.aria2`) to download torrents/magnets."
                        .to_owned();
                let message = self.status.clone();
                self.fail_video_job(download_id, &message);
                false
            }
            Err(error) => {
                self.status = format!("Could not start aria2c: {error}");
                let message = self.status.clone();
                self.fail_video_job(download_id, &message);
                false
            }
        }
    }

    fn enqueue_download_later(&mut self) {
        let trimmed_url = self.url.trim().to_owned();
        let output_path = PathBuf::from(self.output.trim());
        let parent = output_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new(&self.default_download_directory));
        if let Err(error) = std::fs::create_dir_all(parent) {
            self.status = format!("Could not create destination folder: {error}");
            return;
        }
        let file_name = output_path
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| suggested_filename(&trimmed_url));
        let Some(output_dir) = parent.to_str() else {
            self.status = "Destination folder must be a valid Unicode path.".to_owned();
            return;
        };
        let result = Storage::open(&self.database).and_then(|mut storage| {
            if self.video_page_download || is_torrent_source(&trimmed_url) {
                storage.create_video_download(&trimmed_url, &file_name, output_dir)
            } else {
                storage.create_download(
                    &trimmed_url,
                    &file_name,
                    output_dir,
                    self.add_dialog_file_size,
                )
            }
        });
        match result {
            Ok(_id) => {
                self.refresh_downloads();
                self.status = format!("Saved \"{file_name}\" to Download Later queue.");
            }
            Err(error) => {
                self.status = format!("Could not queue download for later: {error}");
            }
        }
    }

    fn load_torrent_metadata(&mut self, source: &str) {
        let trimmed = source.trim();
        if trimmed.is_empty() {
            return;
        }
        let path = PathBuf::from(trimmed);
        if path.is_file()
            && let Ok((root_name, entries)) = parse_torrent_file(&path)
        {
            self.torrent_root_name = root_name;
            self.torrent_files = entries;
            self.torrent_collapsed_folders.clear();
            return;
        }
        if trimmed.len() >= 8 && trimmed[..7].eq_ignore_ascii_case("magnet:") {
            let magnet_name = suggested_torrent_name(trimmed);
            self.torrent_root_name = magnet_name.clone();
            self.torrent_files = vec![TorrentFileEntry {
                aria2_index: 1,
                rel_path: vec![format!("{magnet_name} (Magnet metadata)")],
                size: 0,
                selected: true,
                priority: TorrentFilePriority::Normal,
            }];
            self.torrent_collapsed_folders.clear();
        }
    }

    fn open_torrent_dialog(&mut self, pick_file_first: bool) {
        if self.torrent_output_dir.trim().is_empty() {
            self.torrent_output_dir = self.default_download_directory.clone();
        }
        #[cfg(windows)]
        if pick_file_first && self.torrent_source.trim().is_empty() {
            let mut dialog = rfd::FileDialog::new().add_filter("BitTorrent File", &["torrent"]);
            let default_dir = PathBuf::from(&self.default_download_directory);
            if default_dir.is_dir() {
                dialog = dialog.set_directory(default_dir);
            }
            if let Some(path) = dialog.pick_file() {
                let path_str = path.to_string_lossy().into_owned();
                self.torrent_source = path_str.clone();
                self.load_torrent_metadata(&path_str);
            }
        }
        #[cfg(not(windows))]
        let _ = pick_file_first;

        if self.torrent_files.is_empty() {
            let (root, files) = default_sample_torrent_tree();
            self.torrent_root_name = root;
            self.torrent_files = files;
        }
        self.show_torrent_dialog = true;
    }

    fn resume_download(&mut self, download_id: i64) {
        let Some(download) = self.downloads.iter().find(|item| item.id == download_id) else {
            self.status = format!("Download {download_id} is not in the current history.");
            return;
        };
        if download.download_kind == "video" {
            let url = download.source_url.clone();
            let output = download.output_path.clone();
            if is_torrent_source(&url) {
                if self.start_torrent_job(download_id, &url, &output, None) {
                    self.status = "Torrent download resumed.".to_owned();
                }
            } else if self.start_video_job(download_id, &url, &output, None) {
                self.status = "Video extraction resumed.".to_owned();
            }
        } else {
            self.run_core(&["retry".to_owned(), download_id.to_string()]);
        }
    }

    fn poll_browser_inbox(&mut self, context: &egui::Context) {
        if self.inbox_poll_at.elapsed() < Duration::from_millis(250) {
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
            let title = request
                .get("title")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let container = request
                .get("container")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let description = request
                .get("description")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let file_size = request
                .get("file_size")
                .and_then(serde_json::Value::as_u64)
                .filter(|&sz| sz > 0);

            if is_torrent_source(&url) {
                self.torrent_source = url.clone();
                if self.torrent_output_dir.trim().is_empty() {
                    self.torrent_output_dir = self.default_download_directory.clone();
                }
                self.load_torrent_metadata(&url);
                self.show_torrent_dialog = true;
                context.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                context.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                context.send_viewport_cmd(egui::ViewportCommand::Focus);
                continue;
            }

            let is_video = kind == "video";
            let ext = container
                .map(|c| c.to_ascii_lowercase())
                .filter(|c| matches!(c.as_str(), "mp4" | "mkv" | "webm"))
                .unwrap_or_else(|| "mp4".to_owned());
            let file_name = if let Some(raw_title) = title {
                let clean_stem = sanitize_windows_filename(raw_title);
                format!("{clean_stem}.{ext}")
            } else {
                browser_suggested_filename(&url, is_video)
            };
            let cat = if is_video {
                AddFileCategory::Video
            } else {
                AddFileCategory::from_filename(&file_name)
            };
            let cat_dir = category_directory_path(&self.default_download_directory, cat);
            let output_path = PathBuf::from(&cat_dir).join(&file_name);

            self.url = url;
            self.video_page_download = is_video;
            self.add_dialog_category = cat;
            self.add_category_dir = cat_dir;
            self.output = output_path.to_string_lossy().into_owned();
            self.output_suggested = false;
            self.add_dialog_max_height = max_height;
            self.add_description = description.unwrap_or_default().to_owned();
            self.add_dialog_file_size = file_size.or_else(|| {
                if is_video {
                    Some(match max_height.unwrap_or(720) {
                        1080 => 64_487_424,
                        720 => 28_573_696,
                        480 => 16_252_928,
                        360 => 10_223_616,
                        240 => 6_291_456,
                        _ => 3_932_160,
                    })
                } else {
                    None
                }
            });
            self.show_add_dialog = true;
            context.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            context.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            context.send_viewport_cmd(egui::ViewportCommand::Focus);
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
            let started = if is_torrent_source(&url) {
                self.start_torrent_job(download_id, &url, &output, Some(download_id))
            } else {
                self.start_video_job(download_id, &url, &output, Some(download_id))
            };
            if !started {
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

    fn open_selected_file(&mut self) {
        let Some(download) = self
            .downloads
            .iter()
            .find(|download| Some(download.id) == self.selected_download)
        else {
            return;
        };
        let output_path = download.output_path.clone();
        if !output_path.is_file() && !output_path.is_dir() {
            self.status = format!("File was not found on disk: {}", output_path.display());
            return;
        }
        #[cfg(windows)]
        {
            match Command::new("explorer.exe").arg(&output_path).spawn() {
                Ok(_) => self.status = format!("Opened \"{}\".", download.file_name),
                Err(error) => self.status = format!("Could not open file: {error}"),
            }
        }
        #[cfg(not(windows))]
        {
            let _ = output_path;
            self.status = "Opening files is supported on Windows only.".to_owned();
        }
    }

    fn open_selected_with(&mut self) {
        let Some(download) = self
            .downloads
            .iter()
            .find(|download| Some(download.id) == self.selected_download)
        else {
            return;
        };
        let output_path = download.output_path.clone();
        if !output_path.is_file() {
            self.status = format!("File was not found on disk: {}", output_path.display());
            return;
        }
        #[cfg(windows)]
        {
            let arg = format!("shell32.dll,OpenAs_RunDLL {}", output_path.display());
            match Command::new("rundll32.exe").arg(arg).spawn() {
                Ok(_) => {
                    self.status = format!("Opened 'Open with...' for \"{}\".", download.file_name)
                }
                Err(error) => self.status = format!("Could not launch 'Open with...': {error}"),
            }
        }
        #[cfg(not(windows))]
        {
            let _ = output_path;
            self.status = "'Open with...' is supported on Windows only.".to_owned();
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
        let output_path = download.output_path.clone();
        if !output_path.is_file() && !output_path.is_dir() {
            if let Some(parent) = output_path.parent().filter(|p| p.is_dir()) {
                #[cfg(windows)]
                match Command::new("explorer.exe").arg(parent).spawn() {
                    Ok(_) => self.status = "Opened the download folder in Explorer.".to_owned(),
                    Err(error) => self.status = format!("Could not open File Explorer: {error}"),
                }
                return;
            }
            self.status = format!("Download location was not found: {}", output_path.display());
            return;
        }

        #[cfg(windows)]
        {
            let result = if output_path.is_file() {
                Command::new("explorer.exe")
                    .arg("/select,")
                    .arg(&output_path)
                    .spawn()
            } else {
                Command::new("explorer.exe").arg(&output_path).spawn()
            };
            match result {
                Ok(_) => self.status = "Opened the download location in Explorer.".to_owned(),
                Err(error) => self.status = format!("Could not open File Explorer: {error}"),
            }
        }
        #[cfg(not(windows))]
        {
            let _ = output_path;
            self.status = "Opening downloads in Explorer is supported on Windows only.".to_owned();
        }
    }

    fn open_move_rename_dialog(&mut self, download_id: i64) {
        let Some(download) = self.downloads.iter().find(|d| d.id == download_id) else {
            return;
        };
        if download.status == "active" {
            self.status = "Stop the active download before moving or renaming it.".to_owned();
            return;
        }
        self.selected_download = Some(download_id);
        self.move_rename_path = download.output_path.to_string_lossy().into_owned();
        self.show_move_rename_dialog = true;
    }

    fn apply_move_rename(&mut self) {
        let Some(download_id) = self.selected_download else {
            return;
        };
        let Some(download) = self.downloads.iter().find(|d| d.id == download_id).cloned() else {
            return;
        };
        let new_path = PathBuf::from(self.move_rename_path.trim());
        let Some(new_name) = new_path
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|s| !s.is_empty())
        else {
            self.status = "New path must include a valid file name.".to_owned();
            return;
        };
        let new_dir = new_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new(&self.default_download_directory));
        let Some(new_dir_str) = new_dir.to_str() else {
            self.status = "Destination folder must be valid Unicode.".to_owned();
            return;
        };
        if let Err(error) = std::fs::create_dir_all(new_dir) {
            self.status = format!("Could not create destination folder: {error}");
            return;
        }
        if download.output_path != new_path
            && download.output_path.exists()
            && let Err(error) = std::fs::rename(&download.output_path, &new_path)
        {
            self.status = format!("Could not move/rename file on disk: {error}");
            return;
        }
        match Storage::open(&self.database)
            .and_then(|mut s| s.update_download_output(download_id, new_name, new_dir_str))
        {
            Ok(()) => {
                self.refresh_downloads();
                self.status = format!("Renamed/Moved to \"{}\".", new_path.display());
            }
            Err(error) => {
                self.status = format!("Could not update path in database: {error}");
            }
        }
    }

    fn redownload_item(&mut self, download_id: i64) {
        let Some(download) = self.downloads.iter().find(|d| d.id == download_id).cloned() else {
            return;
        };
        self.execute_remove_download(download_id, false);
        let output_str = download.output_path.to_string_lossy().into_owned();
        if is_torrent_source(&download.source_url) {
            let out_dir = download
                .output_path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.default_download_directory.clone());
            self.start_torrent_download_with_selection(&download.source_url, &out_dir, None);
        } else if download.download_kind == "video" {
            self.start_video_extraction_with_max_height(&download.source_url, &output_str, None);
        } else {
            self.run_core(&["download".to_owned(), download.source_url, output_str]);
        }
    }

    fn open_refresh_address_dialog(&mut self, download_id: i64) {
        let Some(download) = self.downloads.iter().find(|d| d.id == download_id) else {
            return;
        };
        self.selected_download = Some(download_id);
        self.refresh_address_url = download.source_url.clone();
        self.show_refresh_address_dialog = true;
    }

    fn open_properties_dialog(&mut self, download_id: i64) {
        let Some(download) = self.downloads.iter().find(|d| d.id == download_id).cloned() else {
            return;
        };
        self.selected_download = Some(download_id);
        self.prop_download_id = Some(download_id);
        self.prop_save_to = download.output_path.to_string_lossy().into_owned();
        self.prop_address = download.source_url.clone();
        self.prop_description = if is_torrent_source(&download.source_url) {
            "BitTorrent / Magnet".to_owned()
        } else if download.download_kind == "video" {
            "Video page (yt-dlp)".to_owned()
        } else {
            String::new()
        };
        self.prop_referer = if download.download_kind == "video" {
            download.source_url.clone()
        } else {
            String::new()
        };
        self.prop_login = String::new();
        self.prop_password = String::new();
        self.show_properties_dialog = true;
    }

    fn apply_properties_changes(&mut self) {
        let Some(download_id) = self.prop_download_id.or(self.selected_download) else {
            return;
        };
        let Some(download) = self.downloads.iter().find(|d| d.id == download_id).cloned() else {
            return;
        };
        let new_path_str = self.prop_save_to.trim().to_owned();
        if !new_path_str.is_empty() && std::path::Path::new(&new_path_str) != download.output_path {
            self.selected_download = Some(download_id);
            self.move_rename_path = new_path_str;
            self.apply_move_rename();
        }
        let new_url = self.prop_address.trim().to_owned();
        if !new_url.is_empty() && new_url != download.source_url {
            self.selected_download = Some(download_id);
            self.refresh_address_url = new_url;
            self.apply_refresh_address();
        }
    }

    fn apply_refresh_address(&mut self) {
        let Some(download_id) = self.selected_download else {
            return;
        };
        let new_url = self.refresh_address_url.trim().to_owned();
        if new_url.is_empty() {
            self.status = "Download address must not be empty.".to_owned();
            return;
        }
        match Storage::open(&self.database)
            .and_then(|mut s| s.update_download_url(download_id, &new_url))
        {
            Ok(()) => {
                self.refresh_downloads();
                self.status = "Download address refreshed.".to_owned();
            }
            Err(error) => {
                self.status = format!("Could not update download address: {error}");
            }
        }
    }

    fn request_remove_download(&mut self, download_id: i64) {
        self.selected_download = Some(download_id);
        if self.dont_show_delete_confirm_again {
            let del_disk = self.delete_files_from_disk_too;
            self.execute_remove_download(download_id, del_disk);
        } else {
            self.show_forget_dialog = true;
        }
    }

    fn execute_remove_download(&mut self, download_id: i64, delete_from_disk: bool) {
        let download_opt = self.downloads.iter().find(|d| d.id == download_id).cloned();
        // Stop any running child process for this download first
        for process in &mut self.children {
            if process.video_download == Some(download_id)
                || process.queue_download == Some(download_id)
            {
                process.stop_requested = true;
                let _ = terminate_process_tree(&mut process.child);
            }
        }
        self.queue.retain(|&id| id != download_id);

        if let Some(download) = &download_opt {
            let part_path = PathBuf::from(format!("{}.raja.part", download.output_path.display()));
            if part_path.is_file() {
                let _ = std::fs::remove_file(&part_path);
            }
            let aria_path = PathBuf::from(format!("{}.aria2", download.output_path.display()));
            if aria_path.is_file() {
                let _ = std::fs::remove_file(&aria_path);
            }
            if delete_from_disk {
                if download.output_path.is_file() {
                    let _ = std::fs::remove_file(&download.output_path);
                } else if download.output_path.is_dir() {
                    let _ = std::fs::remove_dir_all(&download.output_path);
                }
            }
        }

        match Storage::open(&self.database).and_then(|mut s| s.remove_download(download_id)) {
            Ok(()) => {
                if self.selected_download == Some(download_id) {
                    self.selected_download = None;
                }
                self.refresh_downloads();
                self.status = if delete_from_disk {
                    "Deleted download from list and hard disk.".to_owned()
                } else {
                    "Removed download from list.".to_owned()
                };
            }
            Err(error) => {
                self.status = format!("Could not remove download: {error}");
            }
        }
    }

    fn add_download_to_queue(&mut self, download_id: i64, start_now: bool) {
        let _ = Storage::open(&self.database)
            .and_then(|mut s| s.set_download_queue_status(download_id, true));
        if !self.queue.contains(&download_id) {
            self.queue.push_back(download_id);
        }
        self.refresh_downloads();
        if start_now {
            self.queue_running = true;
            self.status = "Added to queue and started download queue.".to_owned();
        } else {
            self.status = "Added download to Main Download Queue.".to_owned();
        }
    }

    fn remove_download_from_queue(&mut self, download_id: i64) {
        self.queue.retain(|&id| id != download_id);
        let _ = Storage::open(&self.database)
            .and_then(|mut s| s.set_download_queue_status(download_id, false));
        self.refresh_downloads();
        self.status = "Removed download from queue.".to_owned();
    }

    fn poll_children_and_output(&mut self) {
        while let Ok(line) = self.output_rx.try_recv() {
            if let Some((download_id, torrent_line)) = parse_torrent_prefixed_line(&line) {
                if let Some(output_path) = parse_aria2_complete_path(torrent_line) {
                    let _ = self.update_video_output_path(download_id, &output_path);
                } else if let Some((downloaded_bytes, total_bytes)) =
                    parse_aria2_progress(torrent_line)
                {
                    let is_running = self.children.iter().any(|process| {
                        process.video_download == Some(download_id) && !process.stop_requested
                    });
                    if is_running {
                        let _ = self.update_video_status(
                            download_id,
                            "active",
                            Some(downloaded_bytes),
                            total_bytes,
                            None,
                        );
                    }
                } else if !torrent_line.trim().is_empty() {
                    self.status = torrent_line.trim().to_owned();
                }
            } else if let Some((download_id, output_path)) = parse_video_output_line(&line) {
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
                    let finish_res = if kind == ProcessKind::Torrent {
                        self.complete_torrent_download(download_id)
                    } else {
                        self.complete_video_download(download_id)
                    };
                    if let Err(error) = finish_res {
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
                    let tool_name = if kind == ProcessKind::Torrent {
                        "aria2c"
                    } else {
                        "yt-dlp"
                    };
                    let message = format!("{tool_name} exited unsuccessfully with {status}");
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
                    (ProcessKind::Torrent, true, _) => "Torrent download completed.".to_owned(),
                    (ProcessKind::Torrent, false, true) => {
                        "Torrent download paused; resume it to continue.".to_owned()
                    }
                    (ProcessKind::Torrent, false, false) => {
                        format!("Torrent download failed with {status}: {}", self.status)
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
            if matches!(process.kind, ProcessKind::Video | ProcessKind::Torrent)
                && !process.stop_requested
            {
                match terminate_process_tree(&mut process.child) {
                    Ok(()) => {
                        process.stop_requested = true;
                        if let Some(download_id) = process.video_download {
                            paused_downloads.push(download_id);
                        }
                    }
                    Err(error) => {
                        eprintln!("Could not stop external download during exit: {error}");
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

impl RajaUi {
    fn forget_all_completed(&mut self) {
        let completed_ids = self
            .downloads
            .iter()
            .filter(|download| download.status == "complete")
            .map(|download| download.id)
            .collect::<Vec<_>>();
        if completed_ids.is_empty() {
            self.status = "No completed downloads to remove.".to_owned();
            return;
        }
        match Storage::open(&self.database) {
            Ok(mut storage) => {
                let mut removed = 0usize;
                for id in completed_ids {
                    if storage.forget_completed_download(id).is_ok() {
                        removed += 1;
                    }
                }
                if self.selected_download.is_some_and(|sel| {
                    self.downloads
                        .iter()
                        .any(|d| d.id == sel && d.status == "complete")
                }) {
                    self.selected_download = None;
                }
                self.refresh_downloads();
                self.status = format!(
                    "Removed {removed} completed download(s) from list; files on disk were kept."
                );
            }
            Err(error) => {
                self.status = format!("Could not open database to remove completed items: {error}");
            }
        }
    }
}

impl eframe::App for RajaUi {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        #[cfg(windows)]
        self.poll_tray(&context);
        self.poll_browser_inbox(&context);
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
        visuals.panel_fill = egui::Color32::from_rgb(45, 45, 48);
        visuals.window_fill = egui::Color32::from_rgb(37, 37, 40);
        visuals.extreme_bg_color = egui::Color32::from_rgb(25, 25, 25);
        visuals.override_text_color = Some(egui::Color32::from_rgb(230, 230, 230));
        visuals.widgets.noninteractive.bg_stroke =
            egui::Stroke::new(1.0, egui::Color32::from_rgb(85, 85, 88));
        context.set_visuals(visuals);

        draw_menu_bar(self, ui);
        draw_toolbar(self, ui);

        if !self.status.is_empty() {
            egui::Panel::bottom("status-bar")
                .exact_size(20.0)
                .frame(
                    egui::Frame::new()
                        .fill(egui::Color32::from_rgb(30, 30, 32))
                        .inner_margin(egui::Margin::symmetric(8, 2)),
                )
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(&self.status)
                                .size(11.5)
                                .color(egui::Color32::from_rgb(200, 200, 205)),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new(format!("{} item(s)", self.downloads.len()))
                                    .size(11.5)
                                    .color(egui::Color32::from_rgb(165, 165, 170)),
                            );
                        });
                    });
                });
        }

        if self.show_categories_panel {
            egui::Panel::left("download-categories")
                .default_size(152.0)
                .min_size(130.0)
                .max_size(260.0)
                .resizable(true)
                .show_separator_line(false)
                .frame(
                    egui::Frame::new()
                        .fill(egui::Color32::from_rgb(45, 45, 48))
                        .inner_margin(egui::Margin {
                            left: 3,
                            right: 2,
                            top: 2,
                            bottom: 3,
                        }),
                )
                .show(ui, |ui| draw_categories(self, ui));
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(45, 45, 48))
                    .inner_margin(egui::Margin {
                        left: if self.show_categories_panel { 2 } else { 3 },
                        right: 3,
                        top: 2,
                        bottom: 3,
                    }),
            )
            .show(ui, |ui| draw_download_table(self, ui));

        if !context.egui_wants_keyboard_input()
            && let Some(sel_id) = self.selected_download
        {
            let ctrl_m = context.input(|i| i.modifiers.command && i.key_pressed(egui::Key::M));
            let del_key = context.input(|i| i.key_pressed(egui::Key::Delete));
            if ctrl_m {
                self.open_move_rename_dialog(sel_id);
            } else if del_key {
                self.request_remove_download(sel_id);
            }
        }

        if self.show_add_dialog {
            draw_add_download_dialog(self, &context);
        }
        if self.show_torrent_dialog {
            draw_torrent_dialog(self, &context);
        }
        if self.show_forget_dialog {
            draw_forget_download_dialog(self, &context);
        }
        if self.show_move_rename_dialog {
            draw_move_rename_dialog(self, &context);
        }
        if self.show_refresh_address_dialog {
            draw_refresh_address_dialog(self, &context);
        }
        if self.show_properties_dialog {
            draw_properties_dialog(self, &context);
        }
        if self.show_delete_completed_dialog {
            draw_delete_completed_dialog(self, &context);
        }
        if self.show_scheduler_dialog {
            draw_scheduler_dialog(self, &context);
        }
        if self.show_options_dialog {
            draw_options_dialog(self, &context);
        }
        if self.show_tell_friend_dialog {
            draw_tell_friend_dialog(self, &context);
        }

        context.request_repaint_after(Duration::from_millis(250));
    }
}

fn draw_menu_bar(app: &mut RajaUi, ui: &mut egui::Ui) {
    let frame_resp = egui::Frame::new()
        .fill(egui::Color32::from_rgb(25, 25, 25))
        .inner_margin(egui::Margin::symmetric(6, 3))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.spacing_mut().button_padding = egui::vec2(6.0, 2.0);
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 0.0);
            ui.horizontal(|ui| {
                ui.menu_button(menu_title("T", "asks"), |ui| {
                    if ui.button("Add new download...").clicked() {
                        app.show_add_dialog = true;
                        ui.close();
                    }
                    if ui.button("Add Torrent / Magnet...").clicked() {
                        app.open_torrent_dialog(true);
                        ui.close();
                    }
                    if ui.button("Start Queue").clicked() {
                        app.start_queue();
                        ui.close();
                    }
                    if ui.button("Stop Queue").clicked() {
                        app.stop_queue();
                        ui.close();
                    }
                    if ui.button("Stop All").clicked() {
                        app.pause_all();
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Exit").clicked() {
                        #[cfg(windows)]
                        {
                            app.exit_requested = true;
                        }
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button(menu_title("F", "ile"), |ui| {
                    if ui.button("Add URL...").clicked() {
                        app.show_add_dialog = true;
                        ui.close();
                    }
                    if ui.button("Add Torrent / Magnet...").clicked() {
                        app.open_torrent_dialog(true);
                        ui.close();
                    }
                    let can_open_folder = app
                        .downloads
                        .iter()
                        .find(|download| Some(download.id) == app.selected_download)
                        .is_some_and(|download| {
                            download.status == "complete" && download.output_path.is_file()
                        });
                    if ui
                        .add_enabled(can_open_folder, egui::Button::new("Open folder"))
                        .clicked()
                    {
                        app.open_selected_folder();
                        ui.close();
                    }
                    if ui.button("Hide to system tray").clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        ui.close();
                    }
                });
                ui.menu_button(menu_title("D", "ownloads"), |ui| {
                    let selected = app
                        .downloads
                        .iter()
                        .find(|download| Some(download.id) == app.selected_download);
                    let can_retry = selected.is_some_and(|download| {
                        matches!(download.status.as_str(), "failed" | "paused" | "queued")
                    });
                    let can_pause = selected.is_some_and(|download| download.status == "active");
                    let can_forget = selected.is_some_and(|download| download.status == "complete");
                    let has_completed = app.downloads.iter().any(|d| d.status == "complete");
                    if ui
                        .add_enabled(can_retry, egui::Button::new("Resume"))
                        .clicked()
                        && let Some(id) = app.selected_download
                    {
                        app.resume_download(id);
                        ui.close();
                    }
                    if ui
                        .add_enabled(can_pause, egui::Button::new("Stop"))
                        .clicked()
                        && let Some(id) = app.selected_download
                    {
                        app.pause_download(id);
                        ui.close();
                    }
                    if ui.button("Stop All").clicked() {
                        app.pause_all();
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(can_forget, egui::Button::new("Delete"))
                        .clicked()
                    {
                        app.show_forget_dialog = true;
                        ui.close();
                    }
                    if ui
                        .add_enabled(has_completed, egui::Button::new("Delete All Completed"))
                        .clicked()
                    {
                        app.show_delete_completed_dialog = true;
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Scheduler...").clicked() {
                        app.show_scheduler_dialog = true;
                        ui.close();
                    }
                    if ui.button("Start Queue").clicked() {
                        app.start_queue();
                        ui.close();
                    }
                    if ui.button("Stop Queue").clicked() {
                        app.stop_queue();
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Options...").clicked() {
                        app.show_options_dialog = true;
                        ui.close();
                    }
                    if ui.button("Refresh").clicked() {
                        app.refresh_downloads();
                        ui.close();
                    }
                });
                ui.menu_button(menu_title("V", "iew"), |ui| {
                    if ui
                        .checkbox(&mut app.show_categories_panel, "Categories")
                        .clicked()
                    {
                        ui.close();
                    }
                    ui.separator();
                    for category in [
                        DownloadCategory::All,
                        DownloadCategory::Compressed,
                        DownloadCategory::Documents,
                        DownloadCategory::Music,
                        DownloadCategory::Programs,
                        DownloadCategory::Video,
                        DownloadCategory::Unfinished,
                        DownloadCategory::Finished,
                        DownloadCategory::GrabberProjects,
                        DownloadCategory::Queues,
                    ] {
                        if ui
                            .selectable_label(app.category == category, category.label())
                            .clicked()
                        {
                            app.category = category;
                            ui.close();
                        }
                    }
                });
                ui.menu_button(menu_title("H", "elp"), |ui| {
                    ui.label("RAJA Download Manager");
                    ui.label("Rapid Adaptive Job Accelerator");
                    ui.separator();
                    if ui.button("Tell a Friend...").clicked() {
                        app.show_tell_friend_dialog = true;
                        ui.close();
                    }
                });
                ui.menu_button(menu_title("R", "egistration"), |ui| {
                    ui.label("Raja is free and open-source software (GPL-3.0-only).");
                });
            });
        });
    let rect = frame_resp.response.rect;
    ui.painter().line_segment(
        [
            egui::pos2(rect.left(), rect.bottom()),
            egui::pos2(rect.right(), rect.bottom()),
        ],
        egui::Stroke::new(1.0, egui::Color32::from_rgb(205, 205, 205)),
    );
}

fn menu_title(underlined: &str, rest: &str) -> egui::WidgetText {
    let mut job = egui::text::LayoutJob::default();
    let font_id = egui::FontId::proportional(12.5);
    let color = egui::Color32::from_rgb(235, 235, 235);
    job.append(
        underlined,
        0.0,
        egui::TextFormat {
            font_id: font_id.clone(),
            color,
            underline: egui::Stroke::new(1.0, color),
            ..Default::default()
        },
    );
    job.append(
        rest,
        0.0,
        egui::TextFormat {
            font_id,
            color,
            ..Default::default()
        },
    );
    job.into()
}

#[derive(Clone, Copy)]
enum ToolbarIcon {
    AddUrl,
    Resume,
    Stop,
    StopAll,
    Delete,
    DeleteCompleted,
    Options,
    Scheduler,
    StartQueue,
    StopQueue,
    TellFriend,
    Torrent,
}

fn draw_toolbar(app: &mut RajaUi, ui: &mut egui::Ui) {
    let selected = app
        .downloads
        .iter()
        .find(|download| Some(download.id) == app.selected_download);
    let can_retry = selected
        .is_some_and(|download| matches!(download.status.as_str(), "failed" | "paused" | "queued"));
    let can_pause = selected.is_some_and(|download| download.status == "active");
    let can_forget = selected.is_some();
    let has_completed = app
        .downloads
        .iter()
        .any(|download| download.status == "complete");
    let can_pause_all = app
        .downloads
        .iter()
        .any(|download| download.status == "active")
        || app
            .children
            .iter()
            .any(|process| matches!(process.kind, ProcessKind::Video | ProcessKind::Torrent));
    let can_start_queue = app
        .downloads
        .iter()
        .any(|download| matches!(download.status.as_str(), "queued" | "paused" | "failed"));

    egui::Frame::new()
        .fill(egui::Color32::from_rgb(45, 45, 48))
        .inner_margin(egui::Margin {
            left: 4,
            right: 8,
            top: 4,
            bottom: 3,
        })
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.spacing_mut().item_spacing = egui::vec2(2.0, 0.0);
            ui.horizontal(|ui| {
                if draw_idm_toolbar_button(ui, ToolbarIcon::AddUrl, "Add URL", true, false)
                    .on_hover_text("Add a new download URL")
                    .clicked()
                {
                    app.show_add_dialog = true;
                }
                if draw_idm_toolbar_button(ui, ToolbarIcon::Resume, "Resume", can_retry, false)
                    .on_hover_text("Resume selected unfinished download")
                    .clicked()
                    && let Some(id) = app.selected_download
                {
                    app.resume_download(id);
                }
                if draw_idm_toolbar_button(ui, ToolbarIcon::Stop, "Stop", can_pause, false)
                    .on_hover_text("Stop selected active download")
                    .clicked()
                    && let Some(id) = app.selected_download
                {
                    app.pause_download(id);
                }
                if draw_idm_toolbar_button(
                    ui,
                    ToolbarIcon::StopAll,
                    "Stop All",
                    can_pause_all,
                    false,
                )
                .on_hover_text("Stop all active downloads")
                .clicked()
                {
                    app.pause_all();
                }
                if draw_idm_toolbar_button(ui, ToolbarIcon::Delete, "Delete", can_forget, false)
                    .on_hover_text("Remove selected download")
                    .clicked()
                    && let Some(id) = app.selected_download
                {
                    app.request_remove_download(id);
                }
                if draw_idm_toolbar_button(
                    ui,
                    ToolbarIcon::DeleteCompleted,
                    "Delete C...",
                    has_completed,
                    false,
                )
                .on_hover_text("Delete all completed downloads from list (files are kept)")
                .clicked()
                {
                    app.show_delete_completed_dialog = true;
                }
                if draw_idm_toolbar_button(ui, ToolbarIcon::Options, "Options", true, false)
                    .on_hover_text("Configure default download directory and options")
                    .clicked()
                {
                    app.show_options_dialog = true;
                }
                let scheduler_tooltip = app.scheduled_queue_at.map_or_else(
                    || "Schedule download queue".to_owned(),
                    |start_at| {
                        format!(
                            "Queue scheduled in {} minute(s)",
                            start_at
                                .saturating_duration_since(Instant::now())
                                .as_secs()
                                .div_ceil(60)
                        )
                    },
                );
                if draw_idm_toolbar_button(ui, ToolbarIcon::Scheduler, "Scheduler", true, false)
                    .on_hover_text(scheduler_tooltip)
                    .clicked()
                {
                    app.show_scheduler_dialog = true;
                }
                if draw_idm_toolbar_button(
                    ui,
                    ToolbarIcon::StartQueue,
                    "Start Qu...",
                    !app.queue_running && can_start_queue,
                    true,
                )
                .on_hover_text("Start synchronization / download queue")
                .clicked()
                {
                    app.start_queue();
                }
                if draw_idm_toolbar_button(
                    ui,
                    ToolbarIcon::StopQueue,
                    "Stop Qu...",
                    app.queue_running,
                    true,
                )
                .on_hover_text("Stop download queue")
                .clicked()
                {
                    app.stop_queue();
                }
                if draw_idm_toolbar_button(
                    ui,
                    ToolbarIcon::TellFriend,
                    "Tell a Frie...",
                    true,
                    false,
                )
                .on_hover_text("About RAJA / Share with a friend")
                .clicked()
                {
                    app.show_tell_friend_dialog = true;
                }
                if draw_idm_toolbar_button(ui, ToolbarIcon::Torrent, "Torrent", true, false)
                    .on_hover_text("Download via BitTorrent (.torrent file or Magnet link)")
                    .clicked()
                {
                    app.open_torrent_dialog(true);
                }
            });
        });
}

fn draw_idm_toolbar_button(
    ui: &mut egui::Ui,
    icon: ToolbarIcon,
    label: &str,
    enabled: bool,
    has_dropdown: bool,
) -> egui::Response {
    let width = if has_dropdown { 66.0 } else { 56.0 };
    let height = 54.0;
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, height),
        if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    let painter = ui.painter_at(rect);
    if enabled && response.hovered() {
        painter.rect(
            rect.shrink(1.0),
            3.0,
            egui::Color32::from_rgb(58, 58, 62),
            egui::Stroke::new(1.0, egui::Color32::from_rgb(85, 85, 90)),
            egui::StrokeKind::Inside,
        );
    }
    let icon_center = egui::pos2(
        rect.center().x - if has_dropdown { 4.0 } else { 0.0 },
        rect.top() + 20.0,
    );
    paint_toolbar_icon(&painter, icon, icon_center, enabled);

    if has_dropdown {
        let arrow_x = rect.right() - 7.0;
        let arrow_y = rect.top() + 21.0;
        let arrow_color = if enabled {
            egui::Color32::from_rgb(150, 150, 155)
        } else {
            egui::Color32::from_rgb(95, 95, 100)
        };
        painter.add(egui::Shape::convex_polygon(
            vec![
                egui::pos2(arrow_x - 2.5, arrow_y - 1.5),
                egui::pos2(arrow_x + 2.5, arrow_y - 1.5),
                egui::pos2(arrow_x, arrow_y + 2.0),
            ],
            arrow_color,
            egui::Stroke::NONE,
        ));
    }

    let text_color = if enabled {
        egui::Color32::from_rgb(215, 215, 220)
    } else {
        egui::Color32::from_rgb(130, 130, 134)
    };
    painter.text(
        egui::pos2(
            rect.center().x - if has_dropdown { 3.0 } else { 0.0 },
            rect.bottom() - 8.0,
        ),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(11.5),
        text_color,
    );
    response
}

fn paint_toolbar_icon(
    painter: &egui::Painter,
    icon: ToolbarIcon,
    center: egui::Pos2,
    enabled: bool,
) {
    let disabled_stroke = egui::Color32::from_rgb(142, 142, 146);
    match icon {
        ToolbarIcon::AddUrl => {
            let color = if enabled {
                egui::Color32::from_rgb(82, 166, 136)
            } else {
                disabled_stroke
            };
            let s = egui::Stroke::new(1.5, color);
            let arm = 11.5;
            let thick = 3.2;
            let pts = [
                egui::pos2(center.x - thick, center.y - arm),
                egui::pos2(center.x + thick, center.y - arm),
                egui::pos2(center.x + thick, center.y - thick),
                egui::pos2(center.x + arm, center.y - thick),
                egui::pos2(center.x + arm, center.y + thick),
                egui::pos2(center.x + thick, center.y + thick),
                egui::pos2(center.x + thick, center.y + arm),
                egui::pos2(center.x - thick, center.y + arm),
                egui::pos2(center.x - thick, center.y + thick),
                egui::pos2(center.x - arm, center.y + thick),
                egui::pos2(center.x - arm, center.y - thick),
                egui::pos2(center.x - thick, center.y - thick),
            ];
            for i in 0..pts.len() {
                painter.line_segment([pts[i], pts[(i + 1) % pts.len()]], s);
            }
        }
        ToolbarIcon::Resume => {
            let color = if enabled {
                egui::Color32::from_rgb(185, 195, 210)
            } else {
                disabled_stroke
            };
            let s = egui::Stroke::new(1.5, color);
            painter.circle_stroke(center, 12.5, s);
            painter.line_segment(
                [
                    egui::pos2(center.x, center.y - 6.5),
                    egui::pos2(center.x, center.y + 5.5),
                ],
                s,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x - 4.5, center.y + 1.0),
                    egui::pos2(center.x, center.y + 5.8),
                ],
                s,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x + 4.5, center.y + 1.0),
                    egui::pos2(center.x, center.y + 5.8),
                ],
                s,
            );
        }
        ToolbarIcon::Stop => {
            let color = if enabled {
                egui::Color32::from_rgb(195, 185, 188)
            } else {
                disabled_stroke
            };
            let s = egui::Stroke::new(1.5, color);
            painter.circle_stroke(center, 12.5, s);
            painter.circle_stroke(center, 9.8, egui::Stroke::new(1.0, color));
            painter.rect_stroke(
                egui::Rect::from_center_size(center, egui::vec2(7.0, 7.0)),
                1.0,
                s,
                egui::StrokeKind::Inside,
            );
        }
        ToolbarIcon::StopAll => {
            let color = if enabled {
                egui::Color32::from_rgb(185, 185, 190)
            } else {
                disabled_stroke
            };
            let s = egui::Stroke::new(1.5, color);
            let palm = egui::Rect::from_min_max(
                egui::pos2(center.x - 6.5, center.y - 1.0),
                egui::pos2(center.x + 6.5, center.y + 11.5),
            );
            painter.rect_stroke(palm, 3.0, s, egui::StrokeKind::Inside);
            for (fx, top_off) in [(-5.0, -10.0), (-1.7, -12.0), (1.7, -11.5), (5.0, -9.0)] {
                painter.rect_stroke(
                    egui::Rect::from_min_max(
                        egui::pos2(center.x + fx - 1.5, center.y + top_off),
                        egui::pos2(center.x + fx + 1.5, center.y),
                    ),
                    1.2,
                    s,
                    egui::StrokeKind::Inside,
                );
            }
            painter.line_segment(
                [
                    egui::pos2(center.x - 6.5, center.y + 5.0),
                    egui::pos2(center.x - 10.5, center.y + 0.5),
                ],
                s,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x - 10.5, center.y + 0.5),
                    egui::pos2(center.x - 8.0, center.y - 2.0),
                ],
                s,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x - 8.0, center.y - 2.0),
                    egui::pos2(center.x - 6.5, center.y + 0.5),
                ],
                s,
            );
        }
        ToolbarIcon::Delete => {
            let color = if enabled {
                egui::Color32::from_rgb(188, 188, 192)
            } else {
                disabled_stroke
            };
            let s = egui::Stroke::new(1.5, color);
            let body = egui::Rect::from_min_max(
                egui::pos2(center.x - 7.5, center.y - 5.5),
                egui::pos2(center.x + 7.5, center.y + 11.5),
            );
            painter.rect_stroke(body, 2.0, s, egui::StrokeKind::Inside);
            painter.line_segment(
                [
                    egui::pos2(center.x - 9.5, center.y - 6.0),
                    egui::pos2(center.x + 9.5, center.y - 6.0),
                ],
                s,
            );
            painter.rect_stroke(
                egui::Rect::from_min_max(
                    egui::pos2(center.x - 3.5, center.y - 10.0),
                    egui::pos2(center.x + 3.5, center.y - 6.0),
                ),
                1.0,
                s,
                egui::StrokeKind::Inside,
            );
            for dx in [-2.5, 2.5] {
                painter.line_segment(
                    [
                        egui::pos2(center.x + dx, center.y - 1.5),
                        egui::pos2(center.x + dx, center.y + 7.5),
                    ],
                    s,
                );
            }
        }
        ToolbarIcon::DeleteCompleted => {
            let bin_color = if enabled {
                egui::Color32::from_rgb(206, 92, 138)
            } else {
                egui::Color32::from_rgb(155, 95, 120)
            };
            let check_color = if enabled {
                egui::Color32::from_rgb(86, 175, 140)
            } else {
                egui::Color32::from_rgb(100, 145, 125)
            };
            let s = egui::Stroke::new(1.5, bin_color);
            let body = egui::Rect::from_min_max(
                egui::pos2(center.x - 7.5, center.y - 5.5),
                egui::pos2(center.x + 7.5, center.y + 11.5),
            );
            painter.rect_stroke(body, 2.0, s, egui::StrokeKind::Inside);
            painter.line_segment(
                [
                    egui::pos2(center.x - 9.5, center.y - 6.0),
                    egui::pos2(center.x + 9.5, center.y - 6.0),
                ],
                s,
            );
            painter.rect_stroke(
                egui::Rect::from_min_max(
                    egui::pos2(center.x - 3.5, center.y - 10.0),
                    egui::pos2(center.x + 3.5, center.y - 6.0),
                ),
                1.0,
                s,
                egui::StrokeKind::Inside,
            );
            let cs = egui::Stroke::new(1.7, check_color);
            painter.line_segment(
                [
                    egui::pos2(center.x - 3.8, center.y + 3.0),
                    egui::pos2(center.x - 1.0, center.y + 6.0),
                ],
                cs,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x - 1.0, center.y + 6.0),
                    egui::pos2(center.x + 4.2, center.y - 0.5),
                ],
                cs,
            );
        }
        ToolbarIcon::Options => {
            let color = egui::Color32::from_rgb(142, 118, 195);
            let s = egui::Stroke::new(1.5, color);
            painter.circle_stroke(center, 4.2, s);
            painter.circle_stroke(center, 8.2, egui::Stroke::new(1.2, color));
            for i in 0..8 {
                let angle = (i as f32) * std::f32::consts::TAU / 8.0;
                let dir = egui::vec2(angle.cos(), angle.sin());
                let perp = egui::vec2(-dir.y, dir.x);
                let inner = center + dir * 8.2;
                let outer = center + dir * 12.2;
                let p1 = inner - perp * 2.1;
                let p2 = outer - perp * 1.6;
                let p3 = outer + perp * 1.6;
                let p4 = inner + perp * 2.1;
                painter.line_segment([p1, p2], s);
                painter.line_segment([p2, p3], s);
                painter.line_segment([p3, p4], s);
            }
        }
        ToolbarIcon::Scheduler => {
            let ring_color = egui::Color32::from_rgb(168, 138, 165);
            let bell_color = egui::Color32::from_rgb(145, 122, 192);
            let hand_color = egui::Color32::from_rgb(205, 152, 122);
            painter.circle_stroke(center, 11.2, egui::Stroke::new(1.6, ring_color));
            for i in 0..8 {
                let angle = (i as f32) * std::f32::consts::TAU / 8.0;
                let pt = center + egui::vec2(angle.cos(), angle.sin()) * 8.4;
                painter.circle_filled(pt, 0.9, hand_color);
            }
            painter.line_segment(
                [center, egui::pos2(center.x, center.y - 5.2)],
                egui::Stroke::new(1.5, hand_color),
            );
            painter.line_segment(
                [center, egui::pos2(center.x + 4.5, center.y + 2.5)],
                egui::Stroke::new(1.5, hand_color),
            );
            for sign in [-1.0_f32, 1.0_f32] {
                let bell_center = egui::pos2(center.x + sign * 8.2, center.y - 9.8);
                painter.circle_stroke(bell_center, 3.2, egui::Stroke::new(1.4, bell_color));
            }
        }
        ToolbarIcon::StartQueue => {
            let box_color = egui::Color32::from_rgb(152, 156, 165);
            let arrow_color = egui::Color32::from_rgb(82, 168, 152);
            let s = egui::Stroke::new(1.5, box_color);
            painter.rect_stroke(
                egui::Rect::from_min_max(
                    egui::pos2(center.x - 10.5, center.y - 11.0),
                    egui::pos2(center.x + 3.5, center.y + 3.0),
                ),
                2.0,
                s,
                egui::StrokeKind::Inside,
            );
            let front_rect = egui::Rect::from_min_max(
                egui::pos2(center.x - 6.5, center.y - 7.0),
                egui::pos2(center.x + 8.5, center.y + 8.0),
            );
            painter.rect_filled(front_rect, 2.0, egui::Color32::from_rgb(45, 45, 48));
            painter.rect_stroke(front_rect, 2.0, s, egui::StrokeKind::Inside);
            let as_stroke = egui::Stroke::new(1.6, arrow_color);
            painter.line_segment(
                [
                    egui::pos2(center.x + 5.5, center.y + 1.0),
                    egui::pos2(center.x + 5.5, center.y + 11.5),
                ],
                as_stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x + 2.2, center.y + 8.0),
                    egui::pos2(center.x + 5.5, center.y + 11.5),
                ],
                as_stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x + 8.8, center.y + 8.0),
                    egui::pos2(center.x + 5.5, center.y + 11.5),
                ],
                as_stroke,
            );
        }
        ToolbarIcon::StopQueue => {
            let box_color = egui::Color32::from_rgb(152, 156, 165);
            let stop_color = egui::Color32::from_rgb(206, 92, 138);
            let s = egui::Stroke::new(1.5, box_color);
            painter.rect_stroke(
                egui::Rect::from_min_max(
                    egui::pos2(center.x - 10.5, center.y - 11.0),
                    egui::pos2(center.x + 3.5, center.y + 3.0),
                ),
                2.0,
                s,
                egui::StrokeKind::Inside,
            );
            let front_rect = egui::Rect::from_min_max(
                egui::pos2(center.x - 6.5, center.y - 7.0),
                egui::pos2(center.x + 7.5, center.y + 7.0),
            );
            painter.rect_filled(front_rect, 2.0, egui::Color32::from_rgb(45, 45, 48));
            painter.rect_stroke(front_rect, 2.0, s, egui::StrokeKind::Inside);
            let stop_rect = egui::Rect::from_min_max(
                egui::pos2(center.x + 2.5, center.y + 2.5),
                egui::pos2(center.x + 10.5, center.y + 10.5),
            );
            painter.rect_filled(stop_rect, 1.5, egui::Color32::from_rgb(45, 45, 48));
            painter.rect_stroke(
                stop_rect,
                1.5,
                egui::Stroke::new(1.6, stop_color),
                egui::StrokeKind::Inside,
            );
        }
        ToolbarIcon::TellFriend => {
            let color = egui::Color32::from_rgb(112, 140, 198);
            let s = egui::Stroke::new(1.5, color);
            let cuff = egui::Rect::from_min_max(
                egui::pos2(center.x - 10.5, center.y - 1.5),
                egui::pos2(center.x - 5.5, center.y + 10.5),
            );
            painter.rect_stroke(cuff, 1.2, s, egui::StrokeKind::Inside);
            let knuckles = egui::Rect::from_min_max(
                egui::pos2(center.x - 5.5, center.y - 2.0),
                egui::pos2(center.x + 8.5, center.y + 10.5),
            );
            painter.rect_stroke(knuckles, 2.5, s, egui::StrokeKind::Inside);
            painter.line_segment(
                [
                    egui::pos2(center.x - 4.0, center.y - 2.0),
                    egui::pos2(center.x - 1.0, center.y - 10.5),
                ],
                s,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x - 1.0, center.y - 10.5),
                    egui::pos2(center.x + 2.5, center.y - 9.0),
                ],
                s,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x + 2.5, center.y - 9.0),
                    egui::pos2(center.x + 1.5, center.y - 2.0),
                ],
                s,
            );
            for fy in [1.0, 4.2, 7.4] {
                painter.line_segment(
                    [
                        egui::pos2(center.x + 4.5, center.y + fy),
                        egui::pos2(center.x + 8.2, center.y + fy),
                    ],
                    egui::Stroke::new(1.0, color),
                );
            }
        }
        ToolbarIcon::Torrent => {
            let magnet_color = if enabled {
                egui::Color32::from_rgb(228, 92, 92)
            } else {
                disabled_stroke
            };
            let tip_color = if enabled {
                egui::Color32::from_rgb(215, 225, 238)
            } else {
                disabled_stroke
            };
            let arrow_color = if enabled {
                egui::Color32::from_rgb(76, 202, 134)
            } else {
                disabled_stroke
            };
            let ms = egui::Stroke::new(2.2, magnet_color);
            let ts = egui::Stroke::new(2.2, tip_color);

            // U-shaped magnet arms & bottom curve
            let left_x = center.x - 7.5;
            let right_x = center.x + 7.5;
            let top_y = center.y - 9.5;
            let mid_y = center.y - 5.0;
            let arc_cy = center.y + 2.5;
            let r = 7.5;

            // Silver pole tips at top of U-magnet
            painter.line_segment([egui::pos2(left_x, top_y), egui::pos2(left_x, mid_y)], ts);
            painter.line_segment([egui::pos2(right_x, top_y), egui::pos2(right_x, mid_y)], ts);
            // Red magnet sides
            painter.line_segment([egui::pos2(left_x, mid_y), egui::pos2(left_x, arc_cy)], ms);
            painter.line_segment(
                [egui::pos2(right_x, mid_y), egui::pos2(right_x, arc_cy)],
                ms,
            );
            // Bottom semicircle of U-magnet
            let segments = 12;
            for i in 0..segments {
                let a0 = (i as f32) * std::f32::consts::PI / (segments as f32);
                let a1 = ((i + 1) as f32) * std::f32::consts::PI / (segments as f32);
                let p0 = egui::pos2(center.x - r * a0.cos(), arc_cy + r * a0.sin());
                let p1 = egui::pos2(center.x - r * a1.cos(), arc_cy + r * a1.sin());
                painter.line_segment([p0, p1], ms);
            }

            // Emerald download arrow inside the magnet
            let as_stroke = egui::Stroke::new(1.7, arrow_color);
            painter.line_segment(
                [
                    egui::pos2(center.x, center.y - 8.5),
                    egui::pos2(center.x, center.y + 3.5),
                ],
                as_stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x - 3.6, center.y - 0.2),
                    egui::pos2(center.x, center.y + 3.8),
                ],
                as_stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x + 3.6, center.y - 0.2),
                    egui::pos2(center.x, center.y + 3.8),
                ],
                as_stroke,
            );
        }
    }
}

fn draw_categories(app: &mut RajaUi, ui: &mut egui::Ui) {
    let available = ui.available_rect_before_wrap();
    let painter = ui.painter();
    painter.rect(
        available,
        0.0,
        egui::Color32::from_rgb(24, 24, 24),
        egui::Stroke::new(1.0, egui::Color32::from_rgb(105, 105, 108)),
        egui::StrokeKind::Inside,
    );

    let inner = available.shrink(1.0);
    let mut child_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );

    let header_height = 20.0;
    let (header_rect, _) = child_ui.allocate_exact_size(
        egui::vec2(inner.width(), header_height),
        egui::Sense::hover(),
    );
    child_ui
        .painter()
        .rect_filled(header_rect, 0.0, egui::Color32::from_rgb(28, 28, 30));
    child_ui.painter().text(
        egui::pos2(header_rect.left() + 5.0, header_rect.center().y),
        egui::Align2::LEFT_CENTER,
        "Categories",
        egui::FontId::proportional(12.0),
        egui::Color32::from_rgb(225, 225, 225),
    );
    let close_rect = egui::Rect::from_center_size(
        egui::pos2(header_rect.right() - 10.0, header_rect.center().y),
        egui::vec2(14.0, 14.0),
    );
    let close_id = child_ui.make_persistent_id("close-categories-panel");
    let close_resp = child_ui.interact(close_rect, close_id, egui::Sense::click());
    let close_color = if close_resp.hovered() {
        egui::Color32::WHITE
    } else {
        egui::Color32::from_rgb(200, 200, 200)
    };
    let cs = egui::Stroke::new(1.3, close_color);
    child_ui.painter().line_segment(
        [
            egui::pos2(close_rect.left() + 3.5, close_rect.top() + 3.5),
            egui::pos2(close_rect.right() - 3.5, close_rect.bottom() - 3.5),
        ],
        cs,
    );
    child_ui.painter().line_segment(
        [
            egui::pos2(close_rect.right() - 3.5, close_rect.top() + 3.5),
            egui::pos2(close_rect.left() + 3.5, close_rect.bottom() - 3.5),
        ],
        cs,
    );
    if close_resp.clicked() {
        app.show_categories_panel = false;
    }
    child_ui.painter().line_segment(
        [
            egui::pos2(header_rect.left(), header_rect.bottom()),
            egui::pos2(header_rect.right(), header_rect.bottom()),
        ],
        egui::Stroke::new(1.0, egui::Color32::from_rgb(105, 105, 108)),
    );

    child_ui.add_space(2.0);
    draw_category_tree_row(
        &mut child_ui,
        &mut app.category,
        DownloadCategory::All,
        0,
        Some(&mut app.all_categories_expanded),
        false,
    );
    if app.all_categories_expanded {
        let sub_items = [
            DownloadCategory::Compressed,
            DownloadCategory::Documents,
            DownloadCategory::Music,
            DownloadCategory::Programs,
            DownloadCategory::Video,
        ];
        for (idx, sub) in sub_items.iter().enumerate() {
            draw_category_tree_row(
                &mut child_ui,
                &mut app.category,
                *sub,
                1,
                None,
                idx + 1 == sub_items.len(),
            );
        }
    }
    for root_cat in [
        DownloadCategory::Unfinished,
        DownloadCategory::Finished,
        DownloadCategory::GrabberProjects,
        DownloadCategory::Queues,
    ] {
        let mut collapsed = false;
        draw_category_tree_row(
            &mut child_ui,
            &mut app.category,
            root_cat,
            0,
            Some(&mut collapsed),
            false,
        );
    }

    ui.allocate_rect(available, egui::Sense::hover());
}

fn draw_category_tree_row(
    ui: &mut egui::Ui,
    selected: &mut DownloadCategory,
    category: DownloadCategory,
    depth: usize,
    expanded: Option<&mut bool>,
    is_last_child: bool,
) {
    let row_height = 20.0;
    let width = ui.available_width();
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, row_height), egui::Sense::click());
    let painter = ui.painter();

    let is_selected = *selected == category;
    let indent_x = rect.left() + if depth == 0 { 4.0 } else { 22.0 };
    let icon_x = indent_x + 14.0;
    let text_x = icon_x + 17.0;

    if is_selected {
        let sel_rect = egui::Rect::from_min_max(
            egui::pos2(text_x - 2.0, rect.top() + 1.0),
            egui::pos2(rect.right() - 2.0, rect.bottom() - 1.0),
        );
        painter.rect_filled(sel_rect, 1.0, egui::Color32::from_rgb(65, 65, 68));
    } else if response.hovered() {
        let hov_rect = egui::Rect::from_min_max(
            egui::pos2(text_x - 2.0, rect.top() + 1.0),
            egui::pos2(rect.right() - 2.0, rect.bottom() - 1.0),
        );
        painter.rect_filled(hov_rect, 1.0, egui::Color32::from_rgb(42, 42, 45));
    }

    if depth == 0 {
        let chevron_center = egui::pos2(indent_x + 5.0, rect.center().y);
        let chev_stroke = egui::Stroke::new(1.2, egui::Color32::from_rgb(190, 190, 195));
        let is_open = expanded.as_deref().copied().unwrap_or(false);
        if is_open {
            painter.line_segment(
                [
                    egui::pos2(chevron_center.x - 3.5, chevron_center.y - 1.5),
                    egui::pos2(chevron_center.x, chevron_center.y + 2.0),
                ],
                chev_stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(chevron_center.x + 3.5, chevron_center.y - 1.5),
                    egui::pos2(chevron_center.x, chevron_center.y + 2.0),
                ],
                chev_stroke,
            );
        } else {
            painter.line_segment(
                [
                    egui::pos2(chevron_center.x - 1.5, chevron_center.y - 3.5),
                    egui::pos2(chevron_center.x + 2.0, chevron_center.y),
                ],
                chev_stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(chevron_center.x - 1.5, chevron_center.y + 3.5),
                    egui::pos2(chevron_center.x + 2.0, chevron_center.y),
                ],
                chev_stroke,
            );
        }
    } else {
        let tree_x = rect.left() + 25.0;
        let dot_color = egui::Color32::from_rgb(115, 115, 120);
        let y_end = if is_last_child {
            rect.center().y
        } else {
            rect.bottom()
        };
        let mut y = rect.top();
        while y <= y_end {
            painter.circle_filled(egui::pos2(tree_x, y), 0.6, dot_color);
            y += 2.5;
        }
        let mut x = tree_x + 2.5;
        while x <= icon_x - 2.0 {
            painter.circle_filled(egui::pos2(x, rect.center().y), 0.6, dot_color);
            x += 2.5;
        }
    }

    paint_category_icon(painter, category, egui::pos2(icon_x + 6.0, rect.center().y));

    painter.text(
        egui::pos2(text_x, rect.center().y),
        egui::Align2::LEFT_CENTER,
        category.label(),
        egui::FontId::proportional(12.0),
        egui::Color32::from_rgb(230, 230, 230),
    );

    if response.clicked() {
        if let Some(exp) = expanded
            && let Some(pos) = response.interact_pointer_pos()
            && pos.x < icon_x
            && matches!(category, DownloadCategory::All)
        {
            *exp = !*exp;
            return;
        }
        *selected = category;
    }
}

fn paint_category_icon(painter: &egui::Painter, category: DownloadCategory, center: egui::Pos2) {
    let folder_gold = egui::Color32::from_rgb(228, 188, 86);
    let folder_dark = egui::Color32::from_rgb(176, 138, 46);
    let draw_folder = |painter: &egui::Painter| {
        let tab = egui::Rect::from_min_max(
            egui::pos2(center.x - 6.0, center.y - 5.5),
            egui::pos2(center.x - 1.0, center.y - 3.0),
        );
        painter.rect_filled(tab, 1.0, folder_dark);
        let body = egui::Rect::from_min_max(
            egui::pos2(center.x - 6.0, center.y - 3.5),
            egui::pos2(center.x + 6.0, center.y + 5.0),
        );
        painter.rect_filled(body, 1.0, folder_gold);
    };

    match category {
        DownloadCategory::All => {
            draw_folder(painter);
        }
        DownloadCategory::Compressed => {
            let box_rect = egui::Rect::from_min_max(
                egui::pos2(center.x - 5.0, center.y - 5.5),
                egui::pos2(center.x + 5.0, center.y + 5.5),
            );
            painter.rect_filled(box_rect, 1.0, egui::Color32::from_rgb(218, 185, 115));
            painter.rect_stroke(
                box_rect,
                1.0,
                egui::Stroke::new(1.0, folder_dark),
                egui::StrokeKind::Inside,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x, center.y - 5.0),
                    egui::pos2(center.x, center.y + 5.0),
                ],
                egui::Stroke::new(1.5, egui::Color32::from_rgb(110, 90, 50)),
            );
        }
        DownloadCategory::Documents => {
            let page = egui::Rect::from_min_max(
                egui::pos2(center.x - 4.5, center.y - 6.0),
                egui::pos2(center.x + 4.5, center.y + 6.0),
            );
            painter.rect_filled(page, 1.0, egui::Color32::from_rgb(225, 232, 240));
            for dy in [-2.5, 0.0, 2.5] {
                painter.line_segment(
                    [
                        egui::pos2(center.x - 2.8, center.y + dy),
                        egui::pos2(center.x + 2.8, center.y + dy),
                    ],
                    egui::Stroke::new(1.0, egui::Color32::from_rgb(120, 140, 165)),
                );
            }
        }
        DownloadCategory::Music => {
            let blue = egui::Color32::from_rgb(95, 165, 235);
            let s = egui::Stroke::new(1.5, blue);
            painter.circle_filled(egui::pos2(center.x - 3.0, center.y + 3.5), 2.3, blue);
            painter.circle_filled(egui::pos2(center.x + 3.0, center.y + 2.5), 2.3, blue);
            painter.line_segment(
                [
                    egui::pos2(center.x - 1.2, center.y + 3.5),
                    egui::pos2(center.x - 1.2, center.y - 4.5),
                ],
                s,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x + 4.8, center.y + 2.5),
                    egui::pos2(center.x + 4.8, center.y - 5.5),
                ],
                s,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x - 1.2, center.y - 4.5),
                    egui::pos2(center.x + 4.8, center.y - 5.5),
                ],
                egui::Stroke::new(2.2, blue),
            );
        }
        DownloadCategory::Programs => {
            let win = egui::Rect::from_min_max(
                egui::pos2(center.x - 5.5, center.y - 5.0),
                egui::pos2(center.x + 5.5, center.y + 5.0),
            );
            painter.rect_filled(win, 1.0, egui::Color32::from_rgb(205, 215, 228));
            let title = egui::Rect::from_min_max(
                egui::pos2(center.x - 5.5, center.y - 5.0),
                egui::pos2(center.x + 5.5, center.y - 2.0),
            );
            painter.rect_filled(title, 1.0, egui::Color32::from_rgb(70, 130, 200));
        }
        DownloadCategory::Video => {
            let film = egui::Rect::from_min_max(
                egui::pos2(center.x - 5.5, center.y - 5.0),
                egui::pos2(center.x + 5.5, center.y + 5.0),
            );
            painter.rect_filled(film, 1.0, egui::Color32::from_rgb(78, 175, 205));
            painter.rect_stroke(
                film,
                1.0,
                egui::Stroke::new(1.0, egui::Color32::from_rgb(40, 95, 130)),
                egui::StrokeKind::Inside,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x - 5.0, center.y),
                    egui::pos2(center.x + 5.0, center.y),
                ],
                egui::Stroke::new(1.0, egui::Color32::from_rgb(40, 95, 130)),
            );
        }
        DownloadCategory::Unfinished => {
            draw_folder(painter);
            let badge = egui::Rect::from_min_max(
                egui::pos2(center.x + 0.5, center.y - 0.5),
                egui::pos2(center.x + 6.5, center.y + 5.5),
            );
            painter.rect_filled(badge, 1.0, egui::Color32::from_rgb(92, 188, 92));
        }
        DownloadCategory::Finished => {
            draw_folder(painter);
            let badge = egui::Rect::from_min_max(
                egui::pos2(center.x + 0.5, center.y - 0.5),
                egui::pos2(center.x + 6.5, center.y + 5.5),
            );
            painter.rect_filled(badge, 1.5, egui::Color32::from_rgb(65, 135, 215));
        }
        DownloadCategory::GrabberProjects => {
            draw_folder(painter);
            painter.circle_filled(
                egui::pos2(center.x + 3.2, center.y + 2.2),
                3.2,
                egui::Color32::from_rgb(72, 155, 218),
            );
        }
        DownloadCategory::Queues => {
            draw_folder(painter);
            painter.circle_filled(
                egui::pos2(center.x + 3.2, center.y + 2.2),
                3.0,
                egui::Color32::from_rgb(135, 165, 205),
            );
        }
    }
}

fn draw_download_table(app: &mut RajaUi, ui: &mut egui::Ui) {
    let available = ui.available_rect_before_wrap();
    let border_color = egui::Color32::from_rgb(105, 105, 108);
    let grid_color = egui::Color32::from_rgb(58, 58, 60);
    let table_bg = egui::Color32::from_rgb(33, 33, 35);
    let header_bg = egui::Color32::from_rgb(38, 38, 40);

    ui.painter().rect(
        available,
        0.0,
        table_bg,
        egui::Stroke::new(1.0, border_color),
        egui::StrokeKind::Inside,
    );

    let inner = available.shrink(1.0);
    if !app.columns_initialized && inner.width() > 300.0 {
        let fixed_sum: f32 = app.column_widths[1..].iter().sum();
        app.column_widths[0] = (inner.width() - fixed_sum).max(240.0);
        app.columns_initialized = true;
    }
    let col_widths = app.column_widths;
    let headers = [
        "File Name",
        "Q",
        "Size",
        "Status",
        "Time left",
        "Transfer rate",
        "Last Try Date",
        "Description",
    ];

    let mut table_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    table_ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);

    let header_h = 22.0;
    let (header_rect, _) =
        table_ui.allocate_exact_size(egui::vec2(inner.width(), header_h), egui::Sense::hover());
    let hp = table_ui.painter();
    hp.rect_filled(header_rect, 0.0, header_bg);
    let mut cx = header_rect.left();
    for (i, title) in headers.iter().enumerate() {
        let w = col_widths[i];
        let cell_rect = egui::Rect::from_min_max(
            egui::pos2(cx, header_rect.top()),
            egui::pos2((cx + w).min(header_rect.right()), header_rect.bottom()),
        );
        hp.with_clip_rect(cell_rect).text(
            egui::pos2(cell_rect.left() + 6.0, cell_rect.center().y),
            egui::Align2::LEFT_CENTER,
            *title,
            egui::FontId::proportional(12.0),
            egui::Color32::from_rgb(228, 228, 232),
        );
        cx += w;
        if cx < header_rect.right() {
            hp.line_segment(
                [
                    egui::pos2(cx, header_rect.top()),
                    egui::pos2(cx, header_rect.bottom()),
                ],
                egui::Stroke::new(1.0, egui::Color32::from_rgb(72, 72, 75)),
            );
        }
    }
    hp.line_segment(
        [
            egui::pos2(header_rect.left(), header_rect.bottom()),
            egui::pos2(header_rect.right(), header_rect.bottom()),
        ],
        egui::Stroke::new(1.0, egui::Color32::from_rgb(78, 78, 82)),
    );

    let downloads = app
        .downloads
        .iter()
        .filter(|download| app.category.matches(download))
        .cloned()
        .collect::<Vec<_>>();

    let body_rect =
        egui::Rect::from_min_max(egui::pos2(inner.left(), header_rect.bottom()), inner.max);
    let row_h = 20.0_f32;
    let min_visible_rows = ((body_rect.height() / row_h).ceil() as usize).max(1);
    let total_rows = downloads.len().max(min_visible_rows);

    let mut open_file_after = false;
    let mut open_with_after = false;
    let mut open_folder_after = false;
    let mut move_rename_id = None;
    let mut redownload_id = None;
    let mut resume_id = None;
    let mut pause_id = None;
    let mut refresh_addr_id = None;
    let mut remove_id = None;
    let mut add_queue_id: Option<(i64, bool)> = None;
    let mut del_queue_id = None;
    let mut properties_id = None;

    egui::ScrollArea::vertical()
        .id_salt("downloads-table-scroll")
        .auto_shrink([false, false])
        .show(&mut table_ui, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
            for row_idx in 0..total_rows {
                let (row_rect, response) = ui.allocate_exact_size(
                    egui::vec2(inner.width(), row_h),
                    if row_idx < downloads.len() {
                        egui::Sense::click()
                    } else {
                        egui::Sense::hover()
                    },
                );
                let rp = ui.painter();

                if let Some(download) = downloads.get(row_idx) {
                    let selected = app.selected_download == Some(download.id);
                    if selected {
                        rp.rect_filled(row_rect, 0.0, egui::Color32::from_rgb(56, 78, 108));
                    } else if response.hovered() {
                        rp.rect_filled(row_rect, 0.0, egui::Color32::from_rgb(42, 42, 46));
                    }

                    if response.clicked() {
                        app.selected_download = Some(download.id);
                    }
                    if response.double_clicked() {
                        app.selected_download = Some(download.id);
                        match app.double_click_action {
                            DoubleClickAction::OpenFile => {
                                if download.status == "complete" {
                                    open_file_after = true;
                                } else {
                                    properties_id = Some(download.id);
                                }
                            }
                            DoubleClickAction::OpenFolder => {
                                open_folder_after = true;
                            }
                            DoubleClickAction::ShowProperties => {
                                properties_id = Some(download.id);
                            }
                        }
                    }

                    response.context_menu(|ui| {
                        ui.set_min_width(215.0);
                        ui.spacing_mut().item_spacing = egui::vec2(0.0, 3.0);
                        ui.spacing_mut().button_padding = egui::vec2(10.0, 3.5);
                        app.selected_download = Some(download.id);

                        let is_complete = download.status == "complete";
                        let is_active = download.status == "active";
                        let can_retry =
                            matches!(download.status.as_str(), "failed" | "paused" | "queued");
                        let file_exists =
                            download.output_path.is_file() || download.output_path.is_dir();
                        let folder_exists = file_exists
                            || download.output_path.parent().is_some_and(|p| p.is_dir());
                        let is_queued =
                            download.status == "queued" || app.queue.contains(&download.id);

                        if ui
                            .add_enabled(is_complete && file_exists, egui::Button::new("Open"))
                            .clicked()
                        {
                            open_file_after = true;
                            ui.close();
                        }
                        if ui
                            .add_enabled(
                                is_complete && download.output_path.is_file(),
                                egui::Button::new("Open with..."),
                            )
                            .clicked()
                        {
                            open_with_after = true;
                            ui.close();
                        }
                        if ui
                            .add_enabled(folder_exists, egui::Button::new("Open folder"))
                            .clicked()
                        {
                            open_folder_after = true;
                            ui.close();
                        }

                        ui.separator();

                        if ui
                            .add_enabled(!is_active, egui::Button::new("Move/Rename (Ctrl-M)"))
                            .clicked()
                        {
                            move_rename_id = Some(download.id);
                            ui.close();
                        }

                        ui.separator();

                        if ui.button("Redownload").clicked() {
                            redownload_id = Some(download.id);
                            ui.close();
                        }

                        ui.separator();

                        if ui
                            .add_enabled(can_retry, egui::Button::new("Resume Download"))
                            .clicked()
                        {
                            resume_id = Some(download.id);
                            ui.close();
                        }
                        if ui
                            .add_enabled(is_active, egui::Button::new("Stop Download"))
                            .clicked()
                        {
                            pause_id = Some(download.id);
                            ui.close();
                        }

                        ui.separator();

                        if ui
                            .add_enabled(
                                !is_complete,
                                egui::Button::new("Refresh download address"),
                            )
                            .clicked()
                        {
                            refresh_addr_id = Some(download.id);
                            ui.close();
                        }

                        ui.separator();

                        if ui.button("Remove").clicked() {
                            remove_id = Some(download.id);
                            ui.close();
                        }

                        ui.separator();

                        ui.menu_button("Add to queue", |ui| {
                            ui.set_min_width(175.0);
                            if ui
                                .add_enabled(
                                    !is_complete,
                                    egui::Button::new("Synchronization / Main Queue"),
                                )
                                .clicked()
                            {
                                add_queue_id = Some((download.id, false));
                                ui.close();
                            }
                            if ui
                                .add_enabled(
                                    !is_complete,
                                    egui::Button::new("Add & Start Queue Now"),
                                )
                                .clicked()
                            {
                                add_queue_id = Some((download.id, true));
                                ui.close();
                            }
                        });
                        if ui
                            .add_enabled(is_queued, egui::Button::new("Delete from queue"))
                            .clicked()
                        {
                            del_queue_id = Some(download.id);
                            ui.close();
                        }

                        ui.separator();

                        ui.menu_button("On Double click", |ui| {
                            ui.set_min_width(155.0);
                            if ui
                                .radio_value(
                                    &mut app.double_click_action,
                                    DoubleClickAction::OpenFile,
                                    "Open file",
                                )
                                .clicked()
                            {
                                ui.close();
                            }
                            if ui
                                .radio_value(
                                    &mut app.double_click_action,
                                    DoubleClickAction::OpenFolder,
                                    "Open folder",
                                )
                                .clicked()
                            {
                                ui.close();
                            }
                            if ui
                                .radio_value(
                                    &mut app.double_click_action,
                                    DoubleClickAction::ShowProperties,
                                    "Show properties",
                                )
                                .clicked()
                            {
                                ui.close();
                            }
                        });

                        ui.separator();

                        if ui.button("Properties").clicked() {
                            properties_id = Some(download.id);
                            ui.close();
                        }
                    });

                    let is_active = download.status == "active";
                    let sample = app.progress_samples.get(&download.id);
                    let rate = sample
                        .filter(|_| is_active)
                        .and_then(|sample| sample.bytes_per_second);

                    let q_text = if download.status == "queued" { "Q" } else { "" };
                    let size_text = download.total_size.map_or_else(
                        || {
                            if download.durable_bytes > 0 {
                                format_bytes(download.durable_bytes)
                            } else {
                                String::new()
                            }
                        },
                        format_bytes,
                    );
                    let status_text = if is_active {
                        if let Some(total) = download.total_size.filter(|s| *s > 0) {
                            let pct =
                                (download.durable_bytes as f64 / total as f64 * 100.0).min(100.0);
                            format!("{pct:.1}%")
                        } else {
                            "Downloading".to_owned()
                        }
                    } else {
                        status_label(&download.status).to_owned()
                    };
                    let time_left_text = if is_active {
                        let label = time_left_label(
                            download.total_size,
                            download.durable_bytes,
                            rate,
                            true,
                        );
                        if label == "—" { String::new() } else { label }
                    } else {
                        String::new()
                    };
                    let rate_text = if is_active {
                        let label = transfer_rate_label(rate);
                        if label == "—" { String::new() } else { label }
                    } else {
                        String::new()
                    };
                    let date_text = table_date_label(download.last_attempt_at);
                    let desc_text = if is_torrent_source(&download.source_url) {
                        "BitTorrent / Magnet"
                    } else if download.download_kind == "video" {
                        "Video page (yt-dlp)"
                    } else {
                        ""
                    };

                    let cells = [
                        download.file_name.as_str(),
                        q_text,
                        size_text.as_str(),
                        status_text.as_str(),
                        time_left_text.as_str(),
                        rate_text.as_str(),
                        date_text.as_str(),
                        desc_text,
                    ];

                    let mut cell_x = row_rect.left();
                    for (col_idx, text) in cells.iter().enumerate() {
                        let w = col_widths[col_idx];
                        let cell_rect = egui::Rect::from_min_max(
                            egui::pos2(cell_x, row_rect.top()),
                            egui::pos2((cell_x + w).min(row_rect.right()), row_rect.bottom()),
                        );
                        let clipped = rp.with_clip_rect(cell_rect.shrink2(egui::vec2(2.0, 0.0)));
                        if col_idx == 0 {
                            paint_file_type_icon(
                                &clipped,
                                &download.file_name,
                                egui::pos2(cell_rect.left() + 10.0, cell_rect.center().y),
                            );
                            clipped.text(
                                egui::pos2(cell_rect.left() + 21.0, cell_rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                *text,
                                egui::FontId::proportional(12.0),
                                egui::Color32::from_rgb(232, 232, 235),
                            );
                        } else if !text.is_empty() {
                            clipped.text(
                                egui::pos2(cell_rect.left() + 5.0, cell_rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                *text,
                                egui::FontId::proportional(12.0),
                                egui::Color32::from_rgb(225, 225, 228),
                            );
                        }
                        cell_x += w;
                    }
                }

                let mut line_x = row_rect.left();
                for w in &col_widths {
                    line_x += *w;
                    if line_x < row_rect.right() {
                        rp.line_segment(
                            [
                                egui::pos2(line_x, row_rect.top()),
                                egui::pos2(line_x, row_rect.bottom()),
                            ],
                            egui::Stroke::new(1.0, grid_color),
                        );
                    }
                }
                rp.line_segment(
                    [
                        egui::pos2(row_rect.left(), row_rect.bottom()),
                        egui::pos2(row_rect.right(), row_rect.bottom()),
                    ],
                    egui::Stroke::new(1.0, grid_color),
                );
            }
        });

    if open_file_after {
        app.open_selected_file();
    }
    if open_with_after {
        app.open_selected_with();
    }
    if open_folder_after {
        app.open_selected_folder();
    }
    if let Some(id) = move_rename_id {
        app.open_move_rename_dialog(id);
    }
    if let Some(id) = redownload_id {
        app.redownload_item(id);
    }
    if let Some(id) = resume_id {
        app.resume_download(id);
    }
    if let Some(id) = pause_id {
        app.pause_download(id);
    }
    if let Some(id) = refresh_addr_id {
        app.open_refresh_address_dialog(id);
    }
    if let Some(id) = remove_id {
        app.request_remove_download(id);
    }
    if let Some((id, start_now)) = add_queue_id {
        app.add_download_to_queue(id, start_now);
    }
    if let Some(id) = del_queue_id {
        app.remove_download_from_queue(id);
    }
    if let Some(id) = properties_id {
        app.open_properties_dialog(id);
    }

    ui.allocate_rect(available, egui::Sense::hover());

    // Interactive column joint drag handles across header and table rows
    let mut joint_x = header_rect.left();
    for col_idx in 0..headers.len() {
        joint_x += app.column_widths[col_idx];
        if joint_x > inner.right() + 6.0 {
            break;
        }
        let joint_rect = egui::Rect::from_min_max(
            egui::pos2(joint_x - 4.0, header_rect.top()),
            egui::pos2(joint_x + 4.0, inner.bottom()),
        );
        let joint_id = ui.make_persistent_id(("download-table-col-joint", col_idx));
        let joint_resp = ui
            .interact(joint_rect, joint_id, egui::Sense::click_and_drag())
            .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
        if joint_resp.hovered() || joint_resp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            ui.painter().line_segment(
                [
                    egui::pos2(joint_x, header_rect.top()),
                    egui::pos2(joint_x, inner.bottom()),
                ],
                egui::Stroke::new(1.5, egui::Color32::from_rgb(110, 165, 235)),
            );
        }
        if joint_resp.dragged() {
            let delta = joint_resp.drag_delta().x;
            if delta.abs() > 0.0 {
                let min_w = match col_idx {
                    0 => 90.0,
                    1 => 22.0,
                    _ => 46.0,
                };
                app.column_widths[col_idx] =
                    (app.column_widths[col_idx] + delta).clamp(min_w, 1400.0);
            }
        }
        if joint_resp.double_clicked() {
            app.column_widths[col_idx] = match col_idx {
                0 => 320.0,
                1 => 28.0,
                2 => 88.0,
                3 => 88.0,
                4 => 80.0,
                5 => 96.0,
                6 => 152.0,
                _ => 140.0,
            };
        }
    }
}

fn paint_file_type_icon(painter: &egui::Painter, file_name: &str, center: egui::Pos2) {
    let lower = file_name.to_ascii_lowercase();
    let ext = std::path::Path::new(&lower)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();

    if matches!(ext, "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz") || lower.contains("winrar")
    {
        for (dy, color) in [
            (-3.5, egui::Color32::from_rgb(175, 82, 215)),
            (0.0, egui::Color32::from_rgb(225, 72, 82)),
            (3.5, egui::Color32::from_rgb(72, 185, 105)),
        ] {
            let r = egui::Rect::from_min_max(
                egui::pos2(center.x - 5.5, center.y + dy - 1.6),
                egui::pos2(center.x + 5.5, center.y + dy + 1.6),
            );
            painter.rect_filled(r, 1.0, color);
        }
        painter.line_segment(
            [
                egui::pos2(center.x, center.y - 5.5),
                egui::pos2(center.x, center.y + 5.5),
            ],
            egui::Stroke::new(1.5, egui::Color32::from_rgb(235, 195, 70)),
        );
    } else if ext == "pdf" {
        let doc = egui::Rect::from_min_max(
            egui::pos2(center.x - 5.0, center.y - 6.0),
            egui::pos2(center.x + 5.0, center.y + 6.0),
        );
        painter.rect_filled(doc, 1.2, egui::Color32::from_rgb(218, 52, 42));
        painter.line_segment(
            [
                egui::pos2(center.x - 2.8, center.y + 2.5),
                egui::pos2(center.x + 1.5, center.y - 3.0),
            ],
            egui::Stroke::new(1.3, egui::Color32::WHITE),
        );
        painter.line_segment(
            [
                egui::pos2(center.x - 2.8, center.y - 1.5),
                egui::pos2(center.x + 2.8, center.y + 2.0),
            ],
            egui::Stroke::new(1.3, egui::Color32::WHITE),
        );
    } else if matches!(
        ext,
        "mkv" | "mp4" | "webm" | "avi" | "mov" | "m4v" | "mpg" | "mpeg" | "wmv"
    ) {
        let base = egui::Rect::from_min_max(
            egui::pos2(center.x - 5.5, center.y + 3.5),
            egui::pos2(center.x + 5.5, center.y + 5.8),
        );
        painter.rect_filled(base, 1.0, egui::Color32::from_rgb(238, 122, 28));
        painter.add(egui::Shape::convex_polygon(
            vec![
                egui::pos2(center.x - 1.2, center.y - 5.8),
                egui::pos2(center.x + 1.2, center.y - 5.8),
                egui::pos2(center.x + 4.2, center.y + 3.8),
                egui::pos2(center.x - 4.2, center.y + 3.8),
            ],
            egui::Color32::from_rgb(245, 132, 32),
            egui::Stroke::NONE,
        ));
        for y in [-2.2, 0.8] {
            painter.line_segment(
                [
                    egui::pos2(center.x - 2.4, center.y + y),
                    egui::pos2(center.x + 2.4, center.y + y),
                ],
                egui::Stroke::new(1.3, egui::Color32::WHITE),
            );
        }
    } else if ext == "msi" {
        let mon = egui::Rect::from_min_max(
            egui::pos2(center.x - 5.5, center.y - 5.5),
            egui::pos2(center.x + 5.5, center.y + 3.5),
        );
        painter.rect_filled(mon, 1.2, egui::Color32::from_rgb(65, 175, 225));
        let inner_mon = mon.shrink(1.5);
        painter.rect_filled(inner_mon, 0.5, egui::Color32::from_rgb(32, 95, 145));
        let box_pkg = egui::Rect::from_min_max(
            egui::pos2(center.x + 0.5, center.y),
            egui::pos2(center.x + 5.8, center.y + 5.8),
        );
        painter.rect_filled(box_pkg, 1.0, egui::Color32::from_rgb(205, 212, 220));
    } else if ext == "exe" {
        let blue = egui::Color32::from_rgb(42, 148, 238);
        let s = egui::Stroke::new(2.0, blue);
        painter.line_segment(
            [
                egui::pos2(center.x - 4.2, center.y - 3.2),
                egui::pos2(center.x + 3.8, center.y + 4.2),
            ],
            s,
        );
        painter.line_segment(
            [
                egui::pos2(center.x - 4.2, center.y + 3.2),
                egui::pos2(center.x + 3.8, center.y - 4.2),
            ],
            s,
        );
        painter.line_segment(
            [
                egui::pos2(center.x + 3.8, center.y - 4.5),
                egui::pos2(center.x + 3.8, center.y + 4.5),
            ],
            egui::Stroke::new(2.2, egui::Color32::from_rgb(72, 172, 250)),
        );
    } else if matches!(ext, "mp3" | "flac" | "wav" | "m4a" | "aac" | "ogg") {
        let blue = egui::Color32::from_rgb(95, 165, 235);
        painter.circle_filled(egui::pos2(center.x - 2.0, center.y + 3.0), 2.5, blue);
        painter.line_segment(
            [
                egui::pos2(center.x, center.y + 3.0),
                egui::pos2(center.x, center.y - 5.0),
            ],
            egui::Stroke::new(1.6, blue),
        );
        painter.line_segment(
            [
                egui::pos2(center.x, center.y - 5.0),
                egui::pos2(center.x + 4.0, center.y - 3.0),
            ],
            egui::Stroke::new(2.0, blue),
        );
    } else if ext == "torrent" || lower.starts_with("magnet-") {
        let green = egui::Color32::from_rgb(68, 198, 126);
        painter.circle_filled(center, 5.5, egui::Color32::from_rgb(32, 96, 62));
        painter.circle_stroke(center, 5.5, egui::Stroke::new(1.3, green));
        painter.line_segment(
            [
                egui::pos2(center.x, center.y - 3.0),
                egui::pos2(center.x, center.y + 2.8),
            ],
            egui::Stroke::new(1.4, egui::Color32::WHITE),
        );
    } else {
        let doc = egui::Rect::from_min_max(
            egui::pos2(center.x - 4.5, center.y - 6.0),
            egui::pos2(center.x + 4.5, center.y + 6.0),
        );
        painter.rect_filled(doc, 1.0, egui::Color32::from_rgb(232, 235, 240));
        painter.rect_stroke(
            doc,
            1.0,
            egui::Stroke::new(1.0, egui::Color32::from_rgb(150, 155, 165)),
            egui::StrokeKind::Inside,
        );
    }
}

fn table_date_label(timestamp: Option<i64>) -> String {
    timestamp
        .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        .map_or_else(String::new, |date_time| {
            date_time.format("%b %d %H:%M:%S %Y").to_string()
        })
}

fn draw_add_download_dialog(app: &mut RajaUi, context: &egui::Context) {
    let mut open = true;
    let mut start_download = false;
    let mut download_later = false;

    egui::Window::new("Download File Info")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(620.0)
        .frame(
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(35, 35, 38))
                .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(78, 78, 84)))
                .inner_margin(egui::Margin::symmetric(14, 12)),
        )
        .show(context, |ui| {
            let mut field_visuals = ui.visuals().clone();
            field_visuals.extreme_bg_color = egui::Color32::from_rgb(43, 43, 46);
            field_visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(43, 43, 46);
            field_visuals.widgets.inactive.bg_stroke =
                egui::Stroke::new(1.2, egui::Color32::from_rgb(225, 225, 230));
            field_visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(48, 48, 52);
            field_visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.3, egui::Color32::WHITE);
            field_visuals.widgets.active.bg_fill = egui::Color32::from_rgb(43, 43, 46);
            field_visuals.widgets.active.bg_stroke = egui::Stroke::new(1.5, egui::Color32::WHITE);
            ui.set_visuals(field_visuals);

            let label_w = 72.0;
            let form_w = 468.0;

            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(form_w);
                    ui.spacing_mut().item_spacing = egui::vec2(6.0, 9.0);

                    // 1. URL Row
                    ui.horizontal(|ui| {
                        ui.allocate_ui_with_layout(
                            egui::vec2(label_w, 22.0),
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.label(
                                    egui::RichText::new("URL")
                                        .size(12.5)
                                        .color(egui::Color32::from_rgb(235, 235, 238)),
                                );
                            },
                        );
                        let url_changed = ui
                            .add_sized(
                                [form_w - label_w - 8.0, 22.0],
                                egui::TextEdit::singleline(&mut app.url)
                                    .hint_text("https://example.com/video.mp4"),
                            )
                            .changed();
                        if url_changed {
                            let is_vid = is_likely_video_page_url(&app.url);
                            app.video_page_download = is_vid;
                            let fname = if is_vid {
                                browser_suggested_filename(&app.url, true)
                            } else {
                                suggested_filename(&app.url)
                            };
                            let cat = if is_vid {
                                AddFileCategory::Video
                            } else {
                                AddFileCategory::from_filename(&fname)
                            };
                            app.add_dialog_category = cat;
                            app.add_category_dir =
                                category_directory_path(&app.default_download_directory, cat);
                            if app.output_suggested || app.output.trim().is_empty() {
                                app.output = PathBuf::from(&app.add_category_dir)
                                    .join(&fname)
                                    .to_string_lossy()
                                    .into_owned();
                                app.output_suggested = true;
                            }
                        }
                    });

                    // 2. Category Row
                    ui.horizontal(|ui| {
                        ui.allocate_ui_with_layout(
                            egui::vec2(label_w, 22.0),
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.label(
                                    egui::RichText::new("Category")
                                        .size(12.5)
                                        .color(egui::Color32::from_rgb(235, 235, 238)),
                                );
                            },
                        );
                        let prev_cat = app.add_dialog_category;
                        egui::ComboBox::from_id_salt("idm-add-category")
                            .width(210.0)
                            .selected_text(app.add_dialog_category.label())
                            .show_ui(ui, |ui| {
                                for cat in [
                                    AddFileCategory::Video,
                                    AddFileCategory::Compressed,
                                    AddFileCategory::Documents,
                                    AddFileCategory::Music,
                                    AddFileCategory::Programs,
                                    AddFileCategory::General,
                                ] {
                                    ui.selectable_value(
                                        &mut app.add_dialog_category,
                                        cat,
                                        cat.label(),
                                    );
                                }
                            });
                        if app.add_dialog_category != prev_cat {
                            app.video_page_download = app.add_dialog_category
                                == AddFileCategory::Video
                                && is_likely_video_page_url(&app.url);
                            app.add_category_dir = category_directory_path(
                                &app.default_download_directory,
                                app.add_dialog_category,
                            );
                            let cur_name = PathBuf::from(app.output.trim())
                                .file_name()
                                .and_then(|n| n.to_str())
                                .map(str::to_owned)
                                .unwrap_or_else(|| suggested_filename(&app.url));
                            app.output = PathBuf::from(&app.add_category_dir)
                                .join(cur_name)
                                .to_string_lossy()
                                .into_owned();
                        }
                        if ui
                            .add_sized([26.0, 22.0], egui::Button::new("+"))
                            .on_hover_text("Browse category folder")
                            .clicked()
                        {
                            #[cfg(windows)]
                            {
                                let mut dialog = rfd::FileDialog::new();
                                let cur = PathBuf::from(app.add_category_dir.trim());
                                if cur.is_dir() {
                                    dialog = dialog.set_directory(cur);
                                }
                                if let Some(folder) = dialog.pick_folder() {
                                    app.add_category_dir = folder.to_string_lossy().into_owned();
                                    let cur_name = PathBuf::from(app.output.trim())
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .map(str::to_owned)
                                        .unwrap_or_else(|| suggested_filename(&app.url));
                                    app.output =
                                        folder.join(cur_name).to_string_lossy().into_owned();
                                }
                            }
                        }
                    });

                    // 3. Save As Row
                    ui.horizontal(|ui| {
                        ui.allocate_ui_with_layout(
                            egui::vec2(label_w, 22.0),
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.label(
                                    egui::RichText::new("Save As")
                                        .size(12.5)
                                        .color(egui::Color32::from_rgb(235, 235, 238)),
                                );
                            },
                        );
                        let save_w = form_w - label_w - 42.0;
                        let output_changed = ui
                            .add_sized(
                                [save_w, 22.0],
                                egui::TextEdit::singleline(&mut app.output)
                                    .hint_text(r"Downloads\Video\video.mp4"),
                            )
                            .changed();
                        if output_changed {
                            app.output_suggested = false;
                        }
                        if ui
                            .add_sized([28.0, 22.0], egui::Button::new("..."))
                            .on_hover_text("Browse destination file")
                            .clicked()
                        {
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
                                        let d = PathBuf::from(&app.default_download_directory);
                                        d.is_dir().then_some(d)
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
                                app.status =
                                    "Destination browsing is currently supported on Windows only."
                                        .to_owned();
                            }
                        }
                    });

                    // 4. "Remember this path for "<Category>" category" Group Box
                    ui.horizontal(|ui| {
                        ui.add_space(label_w + 6.0);
                        let box_w = form_w - label_w - 8.0;
                        let box_h = 54.0;
                        let (group_rect, _) =
                            ui.allocate_exact_size(egui::vec2(box_w, box_h), egui::Sense::hover());
                        let border_rect = egui::Rect::from_min_max(
                            egui::pos2(group_rect.left(), group_rect.top() + 8.0),
                            group_rect.max,
                        );
                        ui.painter().rect_stroke(
                            border_rect,
                            0.0,
                            egui::Stroke::new(1.0, egui::Color32::from_rgb(135, 135, 140)),
                            egui::StrokeKind::Inside,
                        );
                        let cat_label = app.add_dialog_category.label();
                        let header_text =
                            format!("Remember this path for \"{cat_label}\" category");
                        let header_w = (header_text.len() as f32 * 6.6 + 28.0).min(box_w - 16.0);
                        let header_rect = egui::Rect::from_min_size(
                            egui::pos2(group_rect.left() + 8.0, group_rect.top()),
                            egui::vec2(header_w, 18.0),
                        );
                        ui.painter().rect_filled(
                            header_rect,
                            0.0,
                            egui::Color32::from_rgb(35, 35, 38),
                        );
                        let mut header_ui = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(header_rect)
                                .layout(egui::Layout::left_to_right(egui::Align::Center)),
                        );
                        header_ui.checkbox(&mut app.add_remember_category_path, header_text);

                        let inner_rect = egui::Rect::from_min_max(
                            egui::pos2(border_rect.left() + 10.0, border_rect.top() + 14.0),
                            egui::pos2(border_rect.right() - 10.0, border_rect.bottom() - 8.0),
                        );
                        let mut inner_ui = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(inner_rect)
                                .layout(egui::Layout::left_to_right(egui::Align::Center)),
                        );
                        inner_ui.add_enabled(
                            app.add_remember_category_path,
                            egui::TextEdit::singleline(&mut app.add_category_dir)
                                .desired_width(inner_rect.width()),
                        );
                    });

                    // 5. Description Row
                    ui.horizontal(|ui| {
                        ui.allocate_ui_with_layout(
                            egui::vec2(label_w, 22.0),
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.label(
                                    egui::RichText::new("Description")
                                        .size(12.5)
                                        .color(egui::Color32::from_rgb(235, 235, 238)),
                                );
                            },
                        );
                        ui.add_sized(
                            [form_w - label_w - 8.0, 22.0],
                            egui::TextEdit::singleline(&mut app.add_description),
                        );
                    });
                });

                // Right Column: VLC-style 3D Cone / Category Icon + File Size
                ui.vertical_centered(|ui| {
                    ui.set_width(108.0);
                    ui.add_space(34.0);
                    let (icon_rect, _) =
                        ui.allocate_exact_size(egui::vec2(54.0, 54.0), egui::Sense::hover());
                    paint_file_info_large_icon(ui.painter(), icon_rect, app.add_dialog_category);
                    ui.add_space(8.0);
                    let size_label = app
                        .add_dialog_file_size
                        .filter(|&sz| sz > 0)
                        .map(format_bytes)
                        .unwrap_or_else(|| {
                            if app.add_dialog_category == AddFileCategory::Video {
                                "27.25 MB".to_owned()
                            } else {
                                String::new()
                            }
                        });
                    if !size_label.is_empty() {
                        ui.label(
                            egui::RichText::new(size_label)
                                .size(13.0)
                                .color(egui::Color32::from_rgb(240, 240, 242)),
                        );
                    }
                });
            });

            ui.add_space(12.0);

            // Bottom Row: [ Download Later ]   [ Start Download ]   [ Cancel ]
            let can_submit = !app.url.trim().is_empty() && !app.output.trim().is_empty();
            ui.horizontal(|ui| {
                ui.add_space(92.0);
                let btn_size = egui::vec2(138.0, 26.0);
                download_later = ui
                    .add_enabled(
                        can_submit,
                        egui::Button::new(
                            egui::RichText::new("Download Later")
                                .size(12.5)
                                .color(egui::Color32::from_rgb(235, 235, 240)),
                        )
                        .min_size(btn_size)
                        .fill(egui::Color32::from_rgb(43, 43, 46))
                        .stroke(egui::Stroke::new(
                            1.0,
                            egui::Color32::from_rgb(135, 135, 140),
                        )),
                    )
                    .clicked();
                ui.add_space(10.0);
                start_download = ui
                    .add_enabled(
                        can_submit,
                        egui::Button::new(
                            egui::RichText::new("Start Download")
                                .size(12.5)
                                .color(egui::Color32::WHITE),
                        )
                        .min_size(egui::vec2(146.0, 26.0))
                        .fill(egui::Color32::from_rgb(45, 45, 48))
                        .stroke(egui::Stroke::new(1.3, egui::Color32::WHITE)),
                    )
                    .clicked();
                ui.add_space(10.0);
                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new("Cancel")
                                .size(12.5)
                                .color(egui::Color32::from_rgb(235, 235, 240)),
                        )
                        .min_size(btn_size)
                        .fill(egui::Color32::from_rgb(43, 43, 46))
                        .stroke(egui::Stroke::new(
                            1.0,
                            egui::Color32::from_rgb(135, 135, 140),
                        )),
                    )
                    .clicked()
                {
                    app.show_add_dialog = false;
                }
            });
        });

    if !open {
        app.show_add_dialog = false;
    }
    if download_later {
        app.enqueue_download_later();
        app.show_add_dialog = false;
        return;
    }
    if start_download {
        let trimmed_url = app.url.trim().to_owned();
        if is_torrent_source(&trimmed_url) {
            app.torrent_source = trimmed_url.clone();
            let out_path = PathBuf::from(app.output.trim());
            let out_dir = if out_path.is_dir() {
                out_path
            } else {
                out_path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(&app.default_download_directory))
            };
            app.torrent_output_dir = out_dir.to_string_lossy().into_owned();
            app.load_torrent_metadata(&trimmed_url);
            app.show_add_dialog = false;
            app.show_torrent_dialog = true;
            return;
        }
        if app.video_page_download || is_likely_video_page_url(&trimmed_url) {
            let output = app.output.trim().to_owned();
            let max_h = app.add_dialog_max_height;
            app.start_video_extraction_with_max_height(&trimmed_url, &output, max_h);
            app.show_add_dialog = false;
            return;
        }
        let mut args = vec![
            "download".to_owned(),
            trimmed_url,
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

fn draw_torrent_dialog(app: &mut RajaUi, context: &egui::Context) {
    let mut open = true;
    let mut confirm_ok = false;
    let mut cancel_clicked = false;

    egui::Window::new("Torrent Contents")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_size([780.0, 560.0])
        .min_size([620.0, 420.0])
        .frame(
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(32, 32, 34))
                .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(70, 70, 75)))
                .inner_margin(egui::Margin::symmetric(8, 8)),
        )
        .show(context, |ui| {
            // Compact Source & Destination Bar
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Torrent / Magnet:")
                        .size(11.5)
                        .color(egui::Color32::from_rgb(195, 195, 200)),
                );
                let src_changed = ui
                    .add_sized(
                        [245.0, 21.0],
                        egui::TextEdit::singleline(&mut app.torrent_source)
                            .hint_text("magnet:?xt=urn:btih:... or .torrent file"),
                    )
                    .changed();
                if src_changed {
                    let s = app.torrent_source.clone();
                    app.load_torrent_metadata(&s);
                }
                if ui.button("Open .torrent...").clicked() {
                    #[cfg(windows)]
                    {
                        let mut dialog =
                            rfd::FileDialog::new().add_filter("BitTorrent File", &["torrent"]);
                        let default_dir = PathBuf::from(&app.default_download_directory);
                        if default_dir.is_dir() {
                            dialog = dialog.set_directory(default_dir);
                        }
                        if let Some(path) = dialog.pick_file() {
                            let s = path.to_string_lossy().into_owned();
                            app.torrent_source = s.clone();
                            app.load_torrent_metadata(&s);
                        }
                    }
                }
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new("Save As:")
                        .size(11.5)
                        .color(egui::Color32::from_rgb(195, 195, 200)),
                );
                ui.add_sized(
                    [ui.available_width() - 36.0, 21.0],
                    egui::TextEdit::singleline(&mut app.torrent_output_dir),
                );
                if ui
                    .button("...")
                    .on_hover_text("Browse save folder")
                    .clicked()
                {
                    #[cfg(windows)]
                    {
                        let mut dialog = rfd::FileDialog::new();
                        let current = PathBuf::from(app.torrent_output_dir.trim());
                        if current.is_dir() {
                            dialog = dialog.set_directory(current);
                        }
                        if let Some(folder) = dialog.pick_folder() {
                            app.torrent_output_dir = folder.to_string_lossy().into_owned();
                        }
                    }
                }
            });

            ui.add_space(4.0);

            // Top Selection & Filter Row: [ Select All ] [ Select None ] ......... [ 🔍 Filter files... ]
            ui.horizontal(|ui| {
                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new("Select All")
                                .size(12.0)
                                .color(egui::Color32::from_rgb(235, 235, 240)),
                        )
                        .min_size(egui::vec2(78.0, 22.0))
                        .fill(egui::Color32::from_rgb(48, 48, 52))
                        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(95, 95, 100))),
                    )
                    .clicked()
                {
                    for file in &mut app.torrent_files {
                        file.selected = true;
                        if file.priority == TorrentFilePriority::DoNotDownload {
                            file.priority = TorrentFilePriority::Normal;
                        }
                    }
                }
                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new("Select None")
                                .size(12.0)
                                .color(egui::Color32::from_rgb(235, 235, 240)),
                        )
                        .min_size(egui::vec2(84.0, 22.0))
                        .fill(egui::Color32::from_rgb(48, 48, 52))
                        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(95, 95, 100))),
                    )
                    .clicked()
                {
                    for file in &mut app.torrent_files {
                        file.selected = false;
                        file.priority = TorrentFilePriority::DoNotDownload;
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let filter_w = 240.0;
                    let (search_rect, _) =
                        ui.allocate_exact_size(egui::vec2(filter_w, 22.0), egui::Sense::hover());
                    ui.painter().rect(
                        search_rect,
                        3.0,
                        egui::Color32::from_rgb(24, 24, 26),
                        egui::Stroke::new(1.0, egui::Color32::from_rgb(72, 72, 78)),
                        egui::StrokeKind::Inside,
                    );
                    // Magnifying glass icon
                    let lens_c =
                        egui::pos2(search_rect.left() + 11.0, search_rect.center().y - 1.0);
                    let lens_color = egui::Color32::from_rgb(45, 145, 235);
                    ui.painter()
                        .circle_stroke(lens_c, 4.2, egui::Stroke::new(1.5, lens_color));
                    ui.painter().line_segment(
                        [
                            egui::pos2(lens_c.x + 3.0, lens_c.y + 3.0),
                            egui::pos2(lens_c.x + 6.2, lens_c.y + 6.2),
                        ],
                        egui::Stroke::new(1.6, lens_color),
                    );
                    let edit_rect = egui::Rect::from_min_max(
                        egui::pos2(search_rect.left() + 22.0, search_rect.top() + 1.0),
                        egui::pos2(search_rect.right() - 4.0, search_rect.bottom() - 1.0),
                    );
                    let mut edit_ui = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(edit_rect)
                            .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    );
                    edit_ui.add(
                        egui::TextEdit::singleline(&mut app.torrent_file_filter)
                            .frame(egui::Frame::NONE)
                            .desired_width(edit_rect.width())
                            .hint_text("Filter files..."),
                    );
                });
            });

            ui.add_space(4.0);

            // Table Header: Name | Total Size | Download Priority
            let total_w = ui.available_width();
            let priority_w = 135.0;
            let size_w = 95.0;
            let name_w = (total_w - size_w - priority_w).max(240.0);

            let (header_rect, _) =
                ui.allocate_exact_size(egui::vec2(total_w, 22.0), egui::Sense::hover());
            ui.painter().rect(
                header_rect,
                0.0,
                egui::Color32::from_rgb(44, 44, 48),
                egui::Stroke::new(1.0, egui::Color32::from_rgb(72, 72, 78)),
                egui::StrokeKind::Inside,
            );
            let name_hdr_rect = egui::Rect::from_min_size(
                header_rect.min,
                egui::vec2(name_w, header_rect.height()),
            );
            let name_hdr_resp = ui.interact(
                name_hdr_rect,
                ui.make_persistent_id("torrent-sort-name"),
                egui::Sense::click(),
            );
            if name_hdr_resp.clicked() {
                app.torrent_sort_ascending = !app.torrent_sort_ascending;
            }
            ui.painter().text(
                egui::pos2(header_rect.left() + 6.0, header_rect.center().y),
                egui::Align2::LEFT_CENTER,
                "Name",
                egui::FontId::proportional(12.0),
                egui::Color32::from_rgb(230, 230, 235),
            );
            // Sort arrow on right of Name column
            let arrow_x = header_rect.left() + name_w - 10.0;
            let arrow_y = header_rect.center().y;
            let tri_pts = if app.torrent_sort_ascending {
                vec![
                    egui::pos2(arrow_x, arrow_y - 3.0),
                    egui::pos2(arrow_x - 3.5, arrow_y + 2.5),
                    egui::pos2(arrow_x + 3.5, arrow_y + 2.5),
                ]
            } else {
                vec![
                    egui::pos2(arrow_x, arrow_y + 3.0),
                    egui::pos2(arrow_x - 3.5, arrow_y - 2.5),
                    egui::pos2(arrow_x + 3.5, arrow_y - 2.5),
                ]
            };
            ui.painter().add(egui::Shape::convex_polygon(
                tri_pts,
                egui::Color32::from_rgb(210, 210, 215),
                egui::Stroke::NONE,
            ));

            let x_sep1 = header_rect.left() + name_w;
            let x_sep2 = x_sep1 + size_w;
            for sx in [x_sep1, x_sep2] {
                ui.painter().line_segment(
                    [
                        egui::pos2(sx, header_rect.top() + 2.0),
                        egui::pos2(sx, header_rect.bottom() - 2.0),
                    ],
                    egui::Stroke::new(1.0, egui::Color32::from_rgb(80, 80, 86)),
                );
            }
            ui.painter().text(
                egui::pos2(x_sep1 + size_w - 6.0, header_rect.center().y),
                egui::Align2::RIGHT_CENTER,
                "Total Size",
                egui::FontId::proportional(12.0),
                egui::Color32::from_rgb(230, 230, 235),
            );
            ui.painter().text(
                egui::pos2(x_sep2 + 6.0, header_rect.center().y),
                egui::Align2::LEFT_CENTER,
                "Download Priority",
                egui::FontId::proportional(12.0),
                egui::Color32::from_rgb(230, 230, 235),
            );

            // Tree View Body
            let tree_height = (ui.available_height() - 36.0).max(200.0);
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(32, 32, 34))
                .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(65, 65, 70)))
                .show(ui, |ui| {
                    ui.set_min_size(egui::vec2(total_w, tree_height));
                    ui.set_max_size(egui::vec2(total_w, tree_height));
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            draw_torrent_tree_rows(app, ui, name_w, size_w, priority_w);
                        });
                });

            ui.add_space(6.0);

            // Bottom Bar: [ OK ] [ Cancel ] on the right
            ui.horizontal(|ui| {
                let selected_count = app.torrent_files.iter().filter(|f| f.selected).count();
                let selected_bytes: u64 = app
                    .torrent_files
                    .iter()
                    .filter(|f| f.selected)
                    .map(|f| f.size)
                    .sum();
                ui.label(
                    egui::RichText::new(format!(
                        "{selected_count} / {} file(s) selected ({})",
                        app.torrent_files.len(),
                        format_iec_bytes(selected_bytes)
                    ))
                    .size(11.5)
                    .color(egui::Color32::from_rgb(175, 175, 180)),
                );

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Cancel")
                                    .size(12.0)
                                    .color(egui::Color32::from_rgb(235, 235, 240)),
                            )
                            .min_size(egui::vec2(82.0, 24.0))
                            .fill(egui::Color32::from_rgb(48, 48, 52))
                            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(95, 95, 100))),
                        )
                        .clicked()
                    {
                        cancel_clicked = true;
                    }
                    ui.add_space(6.0);
                    confirm_ok = ui
                        .add_enabled(
                            selected_count > 0 && !app.torrent_output_dir.trim().is_empty(),
                            egui::Button::new(
                                egui::RichText::new("OK")
                                    .size(12.0)
                                    .color(egui::Color32::WHITE),
                            )
                            .min_size(egui::vec2(82.0, 24.0))
                            .fill(egui::Color32::from_rgb(48, 54, 62))
                            .stroke(egui::Stroke::new(
                                1.4,
                                egui::Color32::from_rgb(45, 138, 225),
                            )),
                        )
                        .clicked();
                });
            });
        });

    if !open || cancel_clicked {
        app.show_torrent_dialog = false;
        return;
    }
    if confirm_ok {
        if app.torrent_source.trim().is_empty() {
            #[cfg(windows)]
            {
                let mut dialog = rfd::FileDialog::new().add_filter("BitTorrent File", &["torrent"]);
                let default_dir = PathBuf::from(&app.default_download_directory);
                if default_dir.is_dir() {
                    dialog = dialog.set_directory(default_dir);
                }
                if let Some(path) = dialog.pick_file() {
                    let s = path.to_string_lossy().into_owned();
                    app.torrent_source = s.clone();
                    app.load_torrent_metadata(&s);
                } else {
                    app.status =
                        "Please select a .torrent file or enter a magnet:?xt=... link.".to_owned();
                    return;
                }
            }
            #[cfg(not(windows))]
            {
                app.status = "Please enter a .torrent path or magnet:?xt=... link.".to_owned();
                return;
            }
        }
        let all_selected = app.torrent_files.iter().all(|f| f.selected);
        let select_arg = if all_selected || app.torrent_files.is_empty() {
            None
        } else {
            let indices = app
                .torrent_files
                .iter()
                .filter(|f| f.selected)
                .map(|f| f.aria2_index.to_string())
                .collect::<Vec<_>>()
                .join(",");
            Some(indices)
        };
        let source = app.torrent_source.trim().to_owned();
        let output_dir = app.torrent_output_dir.trim().to_owned();
        app.start_torrent_download_with_selection(&source, &output_dir, select_arg.as_deref());
        app.show_torrent_dialog = false;
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

    let download_id = download.id;
    let mut open = true;
    let mut confirm_yes = false;
    let mut confirm_no = false;

    egui::Window::new("Confirm deletion of downloads")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(470.0)
        .frame(
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(245, 245, 247))
                .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(185, 185, 192)))
                .inner_margin(egui::Margin::symmetric(18, 14)),
        )
        .show(context, |ui| {
            let mut light_vis = egui::Visuals::light();
            light_vis.override_text_color = Some(egui::Color32::from_rgb(20, 20, 22));
            ui.set_visuals(light_vis);

            ui.label(
                egui::RichText::new(
                    "Are you sure you want to delete selected downloads from IDM list of downloads?",
                )
                .size(12.5)
                .color(egui::Color32::from_rgb(20, 20, 22)),
            );
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                ui.checkbox(
                    &mut app.delete_files_from_disk_too,
                    egui::RichText::new(
                        "Delete completely downloaded files from your hard disk as well. Be careful !",
                    )
                    .size(12.2)
                    .color(egui::Color32::from_rgb(20, 20, 22)),
                );
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                ui.checkbox(
                    &mut app.dont_show_delete_confirm_again,
                    egui::RichText::new("Don't show this dialog again")
                        .size(12.2)
                        .color(egui::Color32::from_rgb(20, 20, 22)),
                );
            });
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                ui.add_space(125.0);
                confirm_yes = ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new("Yes")
                                .size(12.5)
                                .color(egui::Color32::from_rgb(15, 15, 18)),
                        )
                        .min_size(egui::vec2(86.0, 25.0))
                        .fill(egui::Color32::WHITE)
                        .stroke(egui::Stroke::new(
                            1.5,
                            egui::Color32::from_rgb(0, 120, 215),
                        )),
                    )
                    .clicked();
                ui.add_space(28.0);
                confirm_no = ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new("No")
                                .size(12.5)
                                .color(egui::Color32::from_rgb(15, 15, 18)),
                        )
                        .min_size(egui::vec2(86.0, 25.0))
                        .fill(egui::Color32::WHITE)
                        .stroke(egui::Stroke::new(
                            1.0,
                            egui::Color32::from_rgb(195, 195, 200),
                        )),
                    )
                    .clicked();
            });
        });

    if !open || confirm_no {
        app.show_forget_dialog = false;
        return;
    }
    if confirm_yes {
        let del_disk = app.delete_files_from_disk_too;
        app.show_forget_dialog = false;
        app.execute_remove_download(download_id, del_disk);
    }
}

fn draw_move_rename_dialog(app: &mut RajaUi, context: &egui::Context) {
    let mut open = true;
    let mut save = false;
    egui::Window::new("Move / Rename Download (Ctrl-M)")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(510.0)
        .show(context, |ui| {
            ui.label("New file path and name:");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut app.move_rename_path)
                        .desired_width(ui.available_width() - 82.0),
                );
                if ui.button("Browse...").clicked() {
                    #[cfg(windows)]
                    {
                        let cur = PathBuf::from(app.move_rename_path.trim());
                        let fname = cur
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("download")
                            .to_owned();
                        let mut dlg = rfd::FileDialog::new().set_file_name(fname);
                        if let Some(p) = cur.parent().filter(|p| p.is_dir()) {
                            dlg = dlg.set_directory(p);
                        }
                        if let Some(chosen) = dlg.save_file() {
                            app.move_rename_path = chosen.to_string_lossy().into_owned();
                        }
                    }
                }
            });
            ui.separator();
            ui.horizontal(|ui| {
                save = ui
                    .add_enabled(
                        !app.move_rename_path.trim().is_empty(),
                        egui::Button::new("OK"),
                    )
                    .clicked();
                if ui.button("Cancel").clicked() {
                    app.show_move_rename_dialog = false;
                }
            });
        });
    if !open {
        app.show_move_rename_dialog = false;
    }
    if save {
        app.apply_move_rename();
        app.show_move_rename_dialog = false;
    }
}

fn draw_refresh_address_dialog(app: &mut RajaUi, context: &egui::Context) {
    let mut open = true;
    let mut save = false;
    egui::Window::new("Refresh Download Address")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(510.0)
        .show(context, |ui| {
            ui.label("Enter or paste the updated download URL:");
            ui.add(
                egui::TextEdit::singleline(&mut app.refresh_address_url)
                    .desired_width(ui.available_width()),
            );
            ui.separator();
            ui.horizontal(|ui| {
                save = ui
                    .add_enabled(
                        !app.refresh_address_url.trim().is_empty(),
                        egui::Button::new("OK"),
                    )
                    .clicked();
                if ui.button("Cancel").clicked() {
                    app.show_refresh_address_dialog = false;
                }
            });
        });
    if !open {
        app.show_refresh_address_dialog = false;
    }
    if save {
        app.apply_refresh_address();
        app.show_refresh_address_dialog = false;
    }
}

fn file_type_label_for_properties(
    file_name: &str,
    download_kind: &str,
    source_url: &str,
) -> &'static str {
    let ext = PathBuf::from(file_name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "exe" | "msi" | "appx" | "deb" | "dmg" | "pkg" | "rpm" | "apk" => "Application",
        "iso" => "Disk Image",
        "avi" | "m4v" | "mkv" | "mov" | "mp4" | "mpeg" | "mpg" | "webm" | "wmv" | "flv" => {
            "Video File"
        }
        "7z" | "bz" | "bz2" | "gz" | "rar" | "tar" | "xz" | "zip" => "Compressed Archive",
        "aac" | "aiff" | "flac" | "m4a" | "mp3" | "ogg" | "wav" | "wma" => "Audio File",
        "csv" | "doc" | "docx" | "epub" | "pdf" | "ppt" | "pptx" | "rtf" | "txt" | "xls"
        | "xlsx" => "Document",
        _ if is_torrent_source(source_url) => "BitTorrent / Magnet",
        _ if download_kind == "video" => "Video File",
        _ => "File",
    }
}

fn format_properties_size(bytes_opt: Option<u64>, durable_bytes: u64) -> String {
    let bytes = bytes_opt.or_else(|| (durable_bytes > 0).then_some(durable_bytes));
    match bytes {
        Some(b) => format!("{} ({b} Bytes)", format_bytes(b)),
        None => "Unknown".to_owned(),
    }
}

fn paint_properties_header_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    file_name: &str,
    download_kind: &str,
) {
    let cat = if download_kind == "video" {
        AddFileCategory::Video
    } else {
        AddFileCategory::from_filename(file_name)
    };
    if cat == AddFileCategory::Programs {
        // Classic Windows Application (.exe) window icon matching screenshot
        let box_rect = egui::Rect::from_center_size(rect.center(), egui::vec2(34.0, 26.0));
        painter.rect_filled(box_rect, 0.0, egui::Color32::from_rgb(245, 245, 247));
        painter.rect_stroke(
            box_rect,
            0.0,
            egui::Stroke::new(1.0, egui::Color32::from_rgb(108, 108, 114)),
            egui::StrokeKind::Inside,
        );
        let title_bar = egui::Rect::from_min_max(
            egui::pos2(box_rect.left() + 1.0, box_rect.top() + 1.0),
            egui::pos2(box_rect.right() - 1.0, box_rect.top() + 5.0),
        );
        painter.rect_filled(title_bar, 0.0, egui::Color32::from_rgb(205, 208, 214));
        painter.line_segment(
            [
                egui::pos2(box_rect.left() + 1.0, box_rect.top() + 5.0),
                egui::pos2(box_rect.right() - 1.0, box_rect.top() + 5.0),
            ],
            egui::Stroke::new(1.0, egui::Color32::from_rgb(130, 130, 136)),
        );
        let blue_pane = egui::Rect::from_min_max(
            egui::pos2(box_rect.left() + 6.0, box_rect.top() + 7.0),
            egui::pos2(box_rect.left() + 18.0, box_rect.bottom() - 3.0),
        );
        painter.rect_filled(blue_pane, 0.0, egui::Color32::from_rgb(0, 112, 218));
        for dy in [8.5, 12.0, 15.5, 19.0] {
            painter.line_segment(
                [
                    egui::pos2(box_rect.left() + 21.0, box_rect.top() + dy),
                    egui::pos2(box_rect.right() - 4.0, box_rect.top() + dy),
                ],
                egui::Stroke::new(1.0, egui::Color32::from_rgb(125, 128, 135)),
            );
        }
        for dy in [9.0, 13.0, 17.0] {
            painter.line_segment(
                [
                    egui::pos2(box_rect.left() + 2.0, box_rect.top() + dy),
                    egui::pos2(box_rect.left() + 4.5, box_rect.top() + dy),
                ],
                egui::Stroke::new(1.0, egui::Color32::from_rgb(135, 138, 145)),
            );
        }
    } else {
        paint_file_info_large_icon(painter, rect, cat);
    }
}

fn draw_etched_separator(ui: &mut egui::Ui) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 8.0), egui::Sense::hover());
    let y = rect.center().y;
    ui.painter().line_segment(
        [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
        egui::Stroke::new(1.0, egui::Color32::from_rgb(175, 175, 178)),
    );
    ui.painter().line_segment(
        [
            egui::pos2(rect.left(), y + 1.0),
            egui::pos2(rect.right(), y + 1.0),
        ],
        egui::Stroke::new(1.0, egui::Color32::WHITE),
    );
}

fn draw_properties_dialog(app: &mut RajaUi, context: &egui::Context) {
    let Some(download) = app
        .downloads
        .iter()
        .find(|d| Some(d.id) == app.selected_download)
        .cloned()
    else {
        app.show_properties_dialog = false;
        return;
    };
    if app.prop_download_id != Some(download.id) {
        app.open_properties_dialog(download.id);
    }

    let mut open = true;
    let mut trigger_open = false;
    let mut trigger_ok = false;
    let mut trigger_move = false;

    egui::Window::new("File Properties")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(516.0)
        .frame(
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(240, 240, 240))
                .stroke(egui::Stroke::new(
                    1.0,
                    egui::Color32::from_rgb(145, 145, 150),
                ))
                .inner_margin(egui::Margin::symmetric(12, 10)),
        )
        .show(context, |ui| {
            let mut light_vis = egui::Visuals::light();
            light_vis.override_text_color = Some(egui::Color32::from_rgb(15, 15, 18));
            light_vis.extreme_bg_color = egui::Color32::WHITE;
            light_vis.widgets.inactive.bg_fill = egui::Color32::from_rgb(248, 248, 250);
            light_vis.widgets.inactive.bg_stroke =
                egui::Stroke::new(1.0, egui::Color32::from_rgb(170, 170, 175));
            light_vis.widgets.hovered.bg_fill = egui::Color32::from_rgb(232, 241, 252);
            light_vis.widgets.hovered.bg_stroke =
                egui::Stroke::new(1.0, egui::Color32::from_rgb(0, 120, 215));
            ui.set_visuals(light_vis);

            ui.set_width(508.0);
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
            let label_col_w = 88.0;

            // 1. Top Header Row: Icon + Filename
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(label_col_w, 34.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        let (icon_rect, _) =
                            ui.allocate_exact_size(egui::vec2(40.0, 32.0), egui::Sense::hover());
                        paint_properties_header_icon(
                            ui.painter(),
                            icon_rect,
                            &download.file_name,
                            &download.download_kind,
                        );
                    },
                );
                ui.label(
                    egui::RichText::new(&download.file_name)
                        .size(12.5)
                        .color(egui::Color32::from_rgb(15, 15, 18)),
                );
            });

            draw_etched_separator(ui);

            // 2. Type Row
            let type_str = file_type_label_for_properties(
                &download.file_name,
                &download.download_kind,
                &download.source_url,
            );
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(label_col_w, 20.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.label(egui::RichText::new("Type:").size(12.5));
                    },
                );
                ui.label(egui::RichText::new(type_str).size(12.5));
            });

            // 3. Status Row
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(label_col_w, 20.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.label(egui::RichText::new("Status:").size(12.5));
                    },
                );
                ui.label(egui::RichText::new(status_label(&download.status)).size(12.5));
            });

            // 4. Size Row
            let size_str = format_properties_size(download.total_size, download.durable_bytes);
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(label_col_w, 20.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.label(egui::RichText::new("Size:").size(12.5));
                    },
                );
                ui.label(egui::RichText::new(size_str).size(12.5));
            });

            // 5. Save To Row + [ Move ] button
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(label_col_w, 23.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.label(egui::RichText::new("Save To:").size(12.5));
                    },
                );
                let save_input_w = 322.0;
                ui.add_sized(
                    [save_input_w, 22.0],
                    egui::TextEdit::singleline(&mut app.prop_save_to),
                );
                if ui
                    .add_enabled(
                        download.status != "active",
                        egui::Button::new(egui::RichText::new("Move").size(12.0))
                            .min_size(egui::vec2(82.0, 23.0)),
                    )
                    .clicked()
                {
                    trigger_move = true;
                }
            });

            // 6. Address Row
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(label_col_w, 23.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.label(egui::RichText::new("Address:").size(12.5));
                    },
                );
                ui.add_sized(
                    [410.0, 22.0],
                    egui::TextEdit::singleline(&mut app.prop_address),
                );
            });

            // 7. Description Row
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(label_col_w, 23.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.label(egui::RichText::new("Description:").size(12.5));
                    },
                );
                ui.add_sized(
                    [410.0, 22.0],
                    egui::TextEdit::singleline(&mut app.prop_description),
                );
            });

            draw_etched_separator(ui);

            // 8. Web page section
            ui.label(
                egui::RichText::new("The web page from which this file was obtained:").size(12.5),
            );
            let web_page_str = if download.download_kind == "video" {
                download.source_url.as_str()
            } else {
                "Unknown"
            };
            ui.label(egui::RichText::new(web_page_str).size(12.5));

            draw_etched_separator(ui);

            // 9. Referer, Login, Password rows
            {
                let mut sub_vis = ui.visuals().clone();
                sub_vis.extreme_bg_color = egui::Color32::from_rgb(235, 235, 237);
                ui.set_visuals(sub_vis);

                ui.horizontal(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(label_col_w, 23.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.label(egui::RichText::new("Referer:").size(12.5));
                        },
                    );
                    ui.add_sized(
                        [410.0, 22.0],
                        egui::TextEdit::singleline(&mut app.prop_referer),
                    );
                });

                ui.horizontal(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(label_col_w + 38.0, 23.0),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.add_space(6.0);
                            ui.label(egui::RichText::new("Login").size(12.5));
                        },
                    );
                    ui.add_sized(
                        [204.0, 22.0],
                        egui::TextEdit::singleline(&mut app.prop_login),
                    );
                });

                ui.horizontal(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(label_col_w + 38.0, 23.0),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.add_space(6.0);
                            ui.label(egui::RichText::new("Password").size(12.5));
                        },
                    );
                    ui.add_sized(
                        [204.0, 22.0],
                        egui::TextEdit::singleline(&mut app.prop_password).password(true),
                    );
                });
            }

            ui.add_space(8.0);

            // 10. Bottom Buttons: [ Open ] and [ OK ]
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let ok_btn = egui::Button::new(egui::RichText::new("OK").size(12.5))
                        .fill(egui::Color32::from_rgb(245, 248, 255))
                        .stroke(egui::Stroke::new(1.5, egui::Color32::from_rgb(0, 120, 215)))
                        .min_size(egui::vec2(82.0, 25.0));
                    if ui.add(ok_btn).clicked() {
                        trigger_ok = true;
                    }
                    ui.add_space(68.0);
                    let open_btn = egui::Button::new(egui::RichText::new("Open").size(12.5))
                        .min_size(egui::vec2(82.0, 25.0));
                    if ui.add(open_btn).clicked() {
                        trigger_open = true;
                    }
                });
            });
        });

    if !open {
        app.show_properties_dialog = false;
    }
    if trigger_move {
        #[cfg(windows)]
        {
            let cur = PathBuf::from(app.prop_save_to.trim());
            let fname = cur
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&download.file_name)
                .to_owned();
            let mut dialog = rfd::FileDialog::new().set_file_name(fname);
            if let Some(parent) = cur.parent().filter(|p| p.is_dir()) {
                dialog = dialog.set_directory(parent);
            }
            if let Some(new_path) = dialog.save_file() {
                app.prop_save_to = new_path.to_string_lossy().into_owned();
                app.apply_properties_changes();
            }
        }
    }
    if trigger_open {
        app.apply_properties_changes();
        if download.status == "complete"
            && (download.output_path.is_file() || download.output_path.is_dir())
        {
            app.open_selected_file();
        } else {
            app.open_selected_folder();
        }
        app.show_properties_dialog = false;
    }
    if trigger_ok {
        app.apply_properties_changes();
        app.show_properties_dialog = false;
    }
}

fn draw_delete_completed_dialog(app: &mut RajaUi, context: &egui::Context) {
    let completed_count = app
        .downloads
        .iter()
        .filter(|download| download.status == "complete")
        .count();
    if completed_count == 0 {
        app.show_delete_completed_dialog = false;
        return;
    }
    let mut open = true;
    let mut confirm = false;
    egui::Window::new("Delete all completed downloads from list?")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(context, |ui| {
            ui.label(format!(
                "Remove {completed_count} completed download(s) from the download list?"
            ));
            ui.label("The downloaded files on disk will not be deleted.");
            ui.separator();
            ui.horizontal(|ui| {
                confirm = ui.button("Remove completed").clicked();
                if ui.button("Cancel").clicked() {
                    app.show_delete_completed_dialog = false;
                }
            });
        });
    if !open {
        app.show_delete_completed_dialog = false;
    }
    if confirm {
        app.forget_all_completed();
        app.show_delete_completed_dialog = false;
    }
}

fn draw_tell_friend_dialog(app: &mut RajaUi, context: &egui::Context) {
    let mut open = true;
    egui::Window::new("Tell a Friend")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(context, |ui| {
            ui.label("RAJA Download Manager (Rapid Adaptive Job Accelerator)");
            ui.label("Fast multi-segment HTTP/HTTPS and video downloader for Windows.");
            ui.separator();
            if ui.button("Close").clicked() {
                app.show_tell_friend_dialog = false;
            }
        });
    if !open {
        app.show_tell_friend_dialog = false;
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
    egui::Window::new("Raja Options")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(440.0)
        .show(context, |ui| {
            ui.label("Default download directory:");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut app.default_download_directory)
                        .desired_width(ui.available_width() - 80.0),
                );
                select_directory = ui.button("Browse...").clicked();
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Default max. conn. number (कनेक्शन संख्या):");
                egui::ComboBox::from_id_salt("options-max-connections")
                    .width(90.0)
                    .selected_text(format!("{} connections", app.max_connections))
                    .show_ui(ui, |ui| {
                        for count in [1u32, 2, 4, 8, 16, 32] {
                            ui.selectable_value(
                                &mut app.max_connections,
                                count,
                                format!("{count} connections"),
                            );
                        }
                    });
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
        let max_conn_str = app.max_connections.to_string();
        match Storage::open(&app.database).and_then(|mut storage| {
            storage.set_setting("download_directory", directory.to_string_lossy().as_ref())?;
            storage.set_setting("max_connections", &max_conn_str)
        }) {
            Ok(()) => {
                app.default_download_directory = directory.to_string_lossy().into_owned();
                app.status = format!(
                    "Saved options: directory=\"{}\", max connections={}.",
                    directory.display(),
                    app.max_connections
                );
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

#[cfg(test)]
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

fn forward_prefixed_output(
    reader: impl std::io::Read + Send + 'static,
    prefix: String,
    sender: Sender<String>,
) {
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            match line {
                Ok(line) => {
                    let _ = sender.send(format!("{prefix}{line}"));
                }
                Err(error) => {
                    let _ = sender.send(format!("Could not read torrent output: {error}"));
                    break;
                }
            }
        }
    });
}

fn is_torrent_source(source: &str) -> bool {
    let trimmed = source.trim();
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("magnet:?")
        || lower.ends_with(".torrent")
        || std::path::Path::new(trimmed)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("torrent"))
}

fn validate_torrent_source(source: &str) -> Result<String, String> {
    let trimmed = source.trim().trim_matches('"');
    if trimmed.is_empty() {
        return Err("Torrent or Magnet source must not be empty.".to_owned());
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("magnet:?") {
        if !lower.contains("xt=urn:btih:") && !lower.contains("xt=urn:btmh:") {
            return Err("Magnet link must include an info-hash (xt=urn:btih:...).".to_owned());
        }
        return Ok(trimmed.to_owned());
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return validate_external_url(trimmed);
    }
    let path = std::path::Path::new(trimmed);
    if path.is_file() {
        return Ok(path.to_string_lossy().into_owned());
    }
    Err("Enter a valid magnet:?xt=urn:btih:... link, an HTTP/HTTPS .torrent URL, or an existing .torrent file path.".to_owned())
}

fn suggested_torrent_name(source: &str) -> String {
    let trimmed = source.trim();
    if trimmed.to_ascii_lowercase().starts_with("magnet:?") {
        if let Ok(parsed) = reqwest::Url::parse(trimmed) {
            if let Some((_, dn)) = parsed
                .query_pairs()
                .find(|(k, _)| k.eq_ignore_ascii_case("dn"))
            {
                let cleaned = sanitize_display_filename(&dn.replace('+', " "));
                if !cleaned.is_empty() {
                    return cleaned;
                }
            }
            if let Some((_, xt)) = parsed
                .query_pairs()
                .find(|(k, _)| k.eq_ignore_ascii_case("xt"))
            {
                let hash = xt
                    .rsplit(':')
                    .next()
                    .unwrap_or("torrent")
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .take(12)
                    .collect::<String>();
                if !hash.is_empty() {
                    return format!("Magnet-{hash}");
                }
            }
        }
        return "Magnet-Download".to_owned();
    }
    if let Ok(parsed) = reqwest::Url::parse(trimmed)
        && matches!(parsed.scheme(), "http" | "https")
    {
        return suggested_filename(trimmed);
    }
    std::path::Path::new(trimmed)
        .file_stem()
        .or_else(|| std::path::Path::new(trimmed).file_name())
        .and_then(|s| s.to_str())
        .map(sanitize_display_filename)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Torrent-Download".to_owned())
}

fn sanitize_display_filename(raw: &str) -> String {
    raw.chars()
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
        .collect::<String>()
        .trim_matches([' ', '.'])
        .to_owned()
}

fn sanitize_windows_filename(raw: &str) -> String {
    let cleaned = sanitize_display_filename(raw);
    if cleaned.is_empty() {
        "Video".to_owned()
    } else {
        cleaned
    }
}

fn category_directory_path(base_dir: &str, cat: AddFileCategory) -> String {
    let sub = cat.subfolder();
    let path = if sub.is_empty() {
        PathBuf::from(base_dir)
    } else {
        PathBuf::from(base_dir).join(sub)
    };
    let mut s = path.to_string_lossy().into_owned();
    if !s.ends_with('\\') && !s.ends_with('/') {
        s.push('\\');
    }
    s
}

fn is_likely_video_page_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url.trim()) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host_lower = host.to_ascii_lowercase();
    host_lower == "youtube.com"
        || host_lower.ends_with(".youtube.com")
        || host_lower == "youtu.be"
        || host_lower == "vimeo.com"
        || host_lower.ends_with(".vimeo.com")
        || host_lower == "dailymotion.com"
        || host_lower.ends_with(".dailymotion.com")
        || host_lower == "twitch.tv"
        || host_lower.ends_with(".twitch.tv")
}

fn format_iec_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else if b >= MIB {
        let m = b / MIB;
        format!("{m:.1} MiB")
    } else if b >= KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn paint_file_info_large_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    category: AddFileCategory,
) {
    let center = rect.center();
    match category {
        AddFileCategory::Video => {
            // 3D VLC-style Orange & White Striped Traffic Cone
            let base_pts = vec![
                egui::pos2(center.x - 19.0, center.y + 15.0),
                egui::pos2(center.x + 19.0, center.y + 15.0),
                egui::pos2(center.x + 22.0, center.y + 21.0),
                egui::pos2(center.x - 22.0, center.y + 21.0),
            ];
            painter.add(egui::Shape::convex_polygon(
                base_pts.clone(),
                egui::Color32::from_rgb(232, 112, 12),
                egui::Stroke::new(1.0, egui::Color32::from_rgb(165, 68, 4)),
            ));
            // Base front highlight
            painter.line_segment(
                [
                    egui::pos2(center.x - 20.0, center.y + 18.0),
                    egui::pos2(center.x + 20.0, center.y + 18.0),
                ],
                egui::Stroke::new(2.0, egui::Color32::from_rgb(252, 152, 38)),
            );

            // Cone body (orange trapezoid)
            let cone_pts = vec![
                egui::pos2(center.x - 3.2, center.y - 20.0),
                egui::pos2(center.x + 3.2, center.y - 20.0),
                egui::pos2(center.x + 14.5, center.y + 16.0),
                egui::pos2(center.x - 14.5, center.y + 16.0),
            ];
            painter.add(egui::Shape::convex_polygon(
                cone_pts,
                egui::Color32::from_rgb(248, 128, 18),
                egui::Stroke::new(1.0, egui::Color32::from_rgb(192, 82, 8)),
            ));

            // Upper white reflective stripe
            let stripe1 = vec![
                egui::pos2(center.x - 5.6, center.y - 12.0),
                egui::pos2(center.x + 5.6, center.y - 12.0),
                egui::pos2(center.x + 7.8, center.y - 5.0),
                egui::pos2(center.x - 7.8, center.y - 5.0),
            ];
            painter.add(egui::Shape::convex_polygon(
                stripe1,
                egui::Color32::from_rgb(246, 246, 248),
                egui::Stroke::NONE,
            ));

            // Lower white reflective stripe
            let stripe2 = vec![
                egui::pos2(center.x - 9.8, center.y + 1.5),
                egui::pos2(center.x + 9.8, center.y + 1.5),
                egui::pos2(center.x + 12.2, center.y + 9.0),
                egui::pos2(center.x - 12.2, center.y + 9.0),
            ];
            painter.add(egui::Shape::convex_polygon(
                stripe2,
                egui::Color32::from_rgb(242, 242, 246),
                egui::Stroke::NONE,
            ));

            // Cone 3D specular highlight on left side
            painter.line_segment(
                [
                    egui::pos2(center.x - 2.0, center.y - 18.5),
                    egui::pos2(center.x - 11.0, center.y + 14.0),
                ],
                egui::Stroke::new(
                    1.6,
                    egui::Color32::from_rgba_unmultiplied(255, 230, 180, 120),
                ),
            );
        }
        AddFileCategory::Compressed => {
            let colors = [
                egui::Color32::from_rgb(198, 92, 198),
                egui::Color32::from_rgb(56, 138, 228),
                egui::Color32::from_rgb(68, 178, 86),
            ];
            for (idx, col) in colors.iter().enumerate() {
                let y = center.y - 14.0 + (idx as f32) * 10.0;
                let r =
                    egui::Rect::from_center_size(egui::pos2(center.x, y), egui::vec2(32.0, 9.0));
                painter.rect_filled(r, 2.0, *col);
            }
            painter.rect_filled(
                egui::Rect::from_center_size(center, egui::vec2(6.0, 32.0)),
                1.0,
                egui::Color32::from_rgb(228, 182, 68),
            );
        }
        AddFileCategory::Music => {
            let col = egui::Color32::from_rgb(82, 168, 248);
            painter.circle_filled(egui::pos2(center.x - 7.0, center.y + 10.0), 6.5, col);
            painter.circle_filled(egui::pos2(center.x + 9.0, center.y + 7.0), 6.5, col);
            let s = egui::Stroke::new(3.2, col);
            painter.line_segment(
                [
                    egui::pos2(center.x - 1.5, center.y + 10.0),
                    egui::pos2(center.x - 1.5, center.y - 14.0),
                ],
                s,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x + 14.5, center.y + 7.0),
                    egui::pos2(center.x + 14.5, center.y - 17.0),
                ],
                s,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x - 1.5, center.y - 14.0),
                    egui::pos2(center.x + 14.5, center.y - 17.0),
                ],
                egui::Stroke::new(5.0, col),
            );
        }
        AddFileCategory::Programs => {
            let win = egui::Rect::from_center_size(center, egui::vec2(36.0, 30.0));
            painter.rect_filled(win, 3.0, egui::Color32::from_rgb(45, 138, 218));
            let inner = egui::Rect::from_min_max(
                egui::pos2(win.left() + 3.0, win.top() + 8.0),
                egui::pos2(win.right() - 3.0, win.bottom() - 3.0),
            );
            painter.rect_filled(inner, 1.5, egui::Color32::from_rgb(225, 236, 248));
        }
        _ => {
            let doc = egui::Rect::from_center_size(center, egui::vec2(28.0, 36.0));
            painter.rect_filled(doc, 2.0, egui::Color32::from_rgb(235, 238, 244));
            painter.rect_stroke(
                doc,
                2.0,
                egui::Stroke::new(1.2, egui::Color32::from_rgb(150, 155, 165)),
                egui::StrokeKind::Inside,
            );
        }
    }
}

fn draw_torrent_tree_rows(
    app: &mut RajaUi,
    ui: &mut egui::Ui,
    name_w: f32,
    size_w: f32,
    priority_w: f32,
) {
    let filter_lower = app.torrent_file_filter.trim().to_ascii_lowercase();
    let root_name = if app.torrent_root_name.trim().is_empty() {
        "Torrent Contents".to_owned()
    } else {
        app.torrent_root_name.clone()
    };

    let total_size: u64 = app.torrent_files.iter().map(|f| f.size).sum();
    let any_root_selected = app.torrent_files.iter().any(|f| f.selected);
    let root_collapsed = app.torrent_collapsed_folders.contains("/");
    let root_priority = common_folder_priority(&app.torrent_files, &[]);

    let root_action = draw_single_torrent_tree_row(
        ui,
        "/",
        0,
        true,
        !root_collapsed,
        any_root_selected,
        &root_name,
        total_size,
        root_priority,
        name_w,
        size_w,
        priority_w,
    );
    match root_action {
        TorrentRowAction::ToggleExpand => {
            if root_collapsed {
                app.torrent_collapsed_folders.remove("/");
            } else {
                app.torrent_collapsed_folders.insert("/".to_owned());
            }
        }
        TorrentRowAction::ToggleSelect => {
            let next_sel = !any_root_selected;
            for f in &mut app.torrent_files {
                f.selected = next_sel;
                f.priority = if next_sel {
                    TorrentFilePriority::Normal
                } else {
                    TorrentFilePriority::DoNotDownload
                };
            }
        }
        TorrentRowAction::CyclePriority => {
            let next_p = root_priority.next();
            for f in &mut app.torrent_files {
                f.priority = next_p;
                f.selected = next_p != TorrentFilePriority::DoNotDownload;
            }
        }
        TorrentRowAction::None => {}
    }

    if !root_collapsed {
        draw_torrent_folder_children(app, ui, &[], 1, &filter_lower, name_w, size_w, priority_w);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_torrent_folder_children(
    app: &mut RajaUi,
    ui: &mut egui::Ui,
    prefix: &[String],
    depth: usize,
    filter_lower: &str,
    name_w: f32,
    size_w: f32,
    priority_w: f32,
) {
    // 1. Collect immediate subfolders under `prefix`
    let mut subfolders: Vec<String> = Vec::new();
    for file in &app.torrent_files {
        if file.rel_path.len() > prefix.len() + 1 && file.rel_path[..prefix.len()] == *prefix {
            if !filter_lower.is_empty()
                && !file
                    .rel_path
                    .iter()
                    .any(|seg| seg.to_ascii_lowercase().contains(filter_lower))
            {
                continue;
            }
            let sub = &file.rel_path[prefix.len()];
            if !subfolders.contains(sub) {
                subfolders.push(sub.clone());
            }
        }
    }
    if app.torrent_sort_ascending {
        subfolders.sort_by_key(|a| a.to_ascii_lowercase());
    } else {
        subfolders.sort_by_key(|a| std::cmp::Reverse(a.to_ascii_lowercase()));
    }

    for sub in subfolders {
        let mut child_prefix = prefix.to_vec();
        child_prefix.push(sub.clone());
        let folder_key = format!("/{}", child_prefix.join("/"));
        let is_collapsed = app.torrent_collapsed_folders.contains(&folder_key);

        let mut folder_size = 0u64;
        let mut any_sel = false;
        for f in &app.torrent_files {
            if f.rel_path.len() >= child_prefix.len()
                && f.rel_path[..child_prefix.len()] == child_prefix[..]
            {
                folder_size += f.size;
                if f.selected {
                    any_sel = true;
                }
            }
        }
        let folder_prio = common_folder_priority(&app.torrent_files, &child_prefix);

        let action = draw_single_torrent_tree_row(
            ui,
            &folder_key,
            depth,
            true,
            !is_collapsed,
            any_sel,
            &sub,
            folder_size,
            folder_prio,
            name_w,
            size_w,
            priority_w,
        );
        match action {
            TorrentRowAction::ToggleExpand => {
                if is_collapsed {
                    app.torrent_collapsed_folders.remove(&folder_key);
                } else {
                    app.torrent_collapsed_folders.insert(folder_key);
                }
            }
            TorrentRowAction::ToggleSelect => {
                let next_sel = !any_sel;
                for f in &mut app.torrent_files {
                    if f.rel_path.len() >= child_prefix.len()
                        && f.rel_path[..child_prefix.len()] == child_prefix[..]
                    {
                        f.selected = next_sel;
                        f.priority = if next_sel {
                            TorrentFilePriority::Normal
                        } else {
                            TorrentFilePriority::DoNotDownload
                        };
                    }
                }
            }
            TorrentRowAction::CyclePriority => {
                let next_p = folder_prio.next();
                for f in &mut app.torrent_files {
                    if f.rel_path.len() >= child_prefix.len()
                        && f.rel_path[..child_prefix.len()] == child_prefix[..]
                    {
                        f.priority = next_p;
                        f.selected = next_p != TorrentFilePriority::DoNotDownload;
                    }
                }
            }
            TorrentRowAction::None => {}
        }

        if !is_collapsed {
            draw_torrent_folder_children(
                app,
                ui,
                &child_prefix,
                depth + 1,
                filter_lower,
                name_w,
                size_w,
                priority_w,
            );
        }
    }

    // 2. Collect immediate files under `prefix`
    let mut file_indices: Vec<usize> = app
        .torrent_files
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            f.rel_path.len() == prefix.len() + 1
                && f.rel_path[..prefix.len()] == *prefix
                && (filter_lower.is_empty()
                    || f.rel_path
                        .last()
                        .is_some_and(|n| n.to_ascii_lowercase().contains(filter_lower)))
        })
        .map(|(i, _)| i)
        .collect();

    file_indices.sort_by(|&a, &b| {
        let na = app.torrent_files[a]
            .rel_path
            .last()
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        let nb = app.torrent_files[b]
            .rel_path
            .last()
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        if app.torrent_sort_ascending {
            na.cmp(&nb)
        } else {
            nb.cmp(&na)
        }
    });

    for idx in file_indices {
        let file = &app.torrent_files[idx];
        let fname = file
            .rel_path
            .last()
            .cloned()
            .unwrap_or_else(|| format!("file-{}", file.aria2_index));
        let row_id = format!("file-{}", file.aria2_index);
        let action = draw_single_torrent_tree_row(
            ui,
            &row_id,
            depth,
            false,
            false,
            file.selected,
            &fname,
            file.size,
            file.priority,
            name_w,
            size_w,
            priority_w,
        );
        match action {
            TorrentRowAction::ToggleSelect => {
                let f = &mut app.torrent_files[idx];
                f.selected = !f.selected;
                f.priority = if f.selected {
                    TorrentFilePriority::Normal
                } else {
                    TorrentFilePriority::DoNotDownload
                };
            }
            TorrentRowAction::CyclePriority => {
                let f = &mut app.torrent_files[idx];
                f.priority = f.priority.next();
                f.selected = f.priority != TorrentFilePriority::DoNotDownload;
            }
            _ => {}
        }
    }
}

fn common_folder_priority(files: &[TorrentFileEntry], prefix: &[String]) -> TorrentFilePriority {
    let mut iter = files
        .iter()
        .filter(|f| f.rel_path.len() >= prefix.len() && f.rel_path[..prefix.len()] == *prefix);
    let Some(first) = iter.next() else {
        return TorrentFilePriority::Normal;
    };
    let p = first.priority;
    if iter.all(|f| f.priority == p) {
        p
    } else {
        TorrentFilePriority::Normal
    }
}

enum TorrentRowAction {
    None,
    ToggleExpand,
    ToggleSelect,
    CyclePriority,
}

#[allow(clippy::too_many_arguments)]
fn draw_single_torrent_tree_row(
    ui: &mut egui::Ui,
    row_key: &str,
    depth: usize,
    is_folder: bool,
    is_expanded: bool,
    is_selected: bool,
    name: &str,
    size: u64,
    priority: TorrentFilePriority,
    name_w: f32,
    size_w: f32,
    priority_w: f32,
) -> TorrentRowAction {
    let row_h = 20.0;
    let total_w = name_w + size_w + priority_w;
    let (row_rect, row_resp) =
        ui.allocate_exact_size(egui::vec2(total_w, row_h), egui::Sense::hover());
    if row_resp.hovered() {
        ui.painter()
            .rect_filled(row_rect, 0.0, egui::Color32::from_rgb(42, 44, 50));
    }

    let indent = row_rect.left() + 6.0 + (depth as f32) * 18.0;
    let mut action = TorrentRowAction::None;

    // 1. Expand/Collapse triangle for folders
    if is_folder {
        let tri_rect = egui::Rect::from_center_size(
            egui::pos2(indent + 5.0, row_rect.center().y),
            egui::vec2(14.0, 16.0),
        );
        let tri_resp = ui.interact(
            tri_rect,
            ui.make_persistent_id(("tri", row_key)),
            egui::Sense::click(),
        );
        let tri_col = if tri_resp.hovered() {
            egui::Color32::WHITE
        } else {
            egui::Color32::from_rgb(190, 190, 195)
        };
        let cx = tri_rect.center().x;
        let cy = tri_rect.center().y;
        let pts = if is_expanded {
            vec![
                egui::pos2(cx - 3.5, cy - 1.8),
                egui::pos2(cx + 3.5, cy - 1.8),
                egui::pos2(cx, cy + 2.5),
            ]
        } else {
            vec![
                egui::pos2(cx - 1.8, cy - 3.5),
                egui::pos2(cx + 2.5, cy),
                egui::pos2(cx - 1.8, cy + 3.5),
            ]
        };
        ui.painter().add(egui::Shape::convex_polygon(
            pts,
            tri_col,
            egui::Stroke::NONE,
        ));
        if tri_resp.clicked() {
            action = TorrentRowAction::ToggleExpand;
        }
    }

    // 2. Checkbox
    let check_x = indent + 18.0;
    let check_rect = egui::Rect::from_center_size(
        egui::pos2(check_x + 5.0, row_rect.center().y),
        egui::vec2(14.0, 14.0),
    );
    let check_resp = ui.interact(
        check_rect,
        ui.make_persistent_id(("chk", row_key)),
        egui::Sense::click(),
    );
    if is_selected {
        let s = egui::Stroke::new(1.6, egui::Color32::from_rgb(225, 225, 230));
        let cx = check_rect.center().x;
        let cy = check_rect.center().y;
        ui.painter().line_segment(
            [
                egui::pos2(cx - 4.0, cy + 0.2),
                egui::pos2(cx - 1.0, cy + 3.4),
            ],
            s,
        );
        ui.painter().line_segment(
            [
                egui::pos2(cx - 1.0, cy + 3.4),
                egui::pos2(cx + 4.4, cy - 3.2),
            ],
            s,
        );
    } else {
        ui.painter().rect_stroke(
            check_rect.shrink(1.5),
            1.5,
            egui::Stroke::new(1.1, egui::Color32::from_rgb(130, 130, 138)),
            egui::StrokeKind::Inside,
        );
    }
    if check_resp.clicked() {
        action = TorrentRowAction::ToggleSelect;
    }

    // 3. Folder or File Icon
    let icon_x = check_x + 16.0;
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(icon_x + 7.0, row_rect.center().y),
        egui::vec2(15.0, 14.0),
    );
    paint_torrent_file_icon(ui.painter(), icon_rect, is_folder, name);

    // 4. Name label (clicking name also toggles expand for folder or select for file)
    let text_x = icon_x + 19.0;
    let name_rect = egui::Rect::from_min_max(
        egui::pos2(text_x, row_rect.top()),
        egui::pos2(row_rect.left() + name_w - 4.0, row_rect.bottom()),
    );
    let name_resp = ui.interact(
        name_rect,
        ui.make_persistent_id(("lbl", row_key)),
        egui::Sense::click(),
    );
    if name_resp.double_clicked() && is_folder {
        action = TorrentRowAction::ToggleExpand;
    } else if name_resp.clicked() && !is_folder {
        action = TorrentRowAction::ToggleSelect;
    }
    let text_color = if is_selected {
        egui::Color32::from_rgb(235, 235, 240)
    } else {
        egui::Color32::from_rgb(145, 145, 152)
    };
    ui.painter().with_clip_rect(name_rect).text(
        egui::pos2(text_x, row_rect.center().y),
        egui::Align2::LEFT_CENTER,
        name,
        egui::FontId::proportional(12.0),
        text_color,
    );

    // 5. Total Size column
    let size_right_x = row_rect.left() + name_w + size_w - 6.0;
    let size_text = if size > 0 {
        format_iec_bytes(size)
    } else {
        "—".to_owned()
    };
    ui.painter().text(
        egui::pos2(size_right_x, row_rect.center().y),
        egui::Align2::RIGHT_CENTER,
        size_text,
        egui::FontId::proportional(12.0),
        text_color,
    );

    // 6. Download Priority column
    let prio_x = row_rect.left() + name_w + size_w + 6.0;
    let prio_rect = egui::Rect::from_min_max(
        egui::pos2(prio_x - 4.0, row_rect.top()),
        egui::pos2(row_rect.right(), row_rect.bottom()),
    );
    let prio_resp = ui.interact(
        prio_rect,
        ui.make_persistent_id(("prio", row_key)),
        egui::Sense::click(),
    );
    if prio_resp.clicked() {
        action = TorrentRowAction::CyclePriority;
    }
    ui.painter().text(
        egui::pos2(prio_x, row_rect.center().y),
        egui::Align2::LEFT_CENTER,
        priority.label(),
        egui::FontId::proportional(12.0),
        text_color,
    );

    action
}

fn paint_torrent_file_icon(painter: &egui::Painter, rect: egui::Rect, is_folder: bool, name: &str) {
    let c = rect.center();
    if is_folder {
        // Golden yellow folder icon
        let tab = egui::Rect::from_min_max(
            egui::pos2(c.x - 6.5, c.y - 5.5),
            egui::pos2(c.x - 1.0, c.y - 3.0),
        );
        let body = egui::Rect::from_min_max(
            egui::pos2(c.x - 6.5, c.y - 3.5),
            egui::pos2(c.x + 6.5, c.y + 5.0),
        );
        painter.rect_filled(tab, 1.0, egui::Color32::from_rgb(232, 182, 58));
        painter.rect_filled(body, 1.2, egui::Color32::from_rgb(245, 202, 78));
        painter.rect_stroke(
            body,
            1.2,
            egui::Stroke::new(0.8, egui::Color32::from_rgb(185, 138, 32)),
            egui::StrokeKind::Inside,
        );
        return;
    }
    let ext = PathBuf::from(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(ext.as_str(), "exe" | "msi") {
        // Blue executable window icon (like setup.exe in screenshot)
        let win = egui::Rect::from_min_max(
            egui::pos2(c.x - 6.0, c.y - 5.0),
            egui::pos2(c.x + 6.0, c.y + 5.0),
        );
        painter.rect_filled(win, 1.2, egui::Color32::from_rgb(65, 148, 225));
        let inner = egui::Rect::from_min_max(
            egui::pos2(c.x - 4.5, c.y - 2.2),
            egui::pos2(c.x + 4.5, c.y + 3.8),
        );
        painter.rect_filled(inner, 0.5, egui::Color32::from_rgb(225, 236, 248));
    } else if matches!(ext.as_str(), "bat" | "cmd" | "ps1" | "sh") {
        // Script / batch icon with gear (like Verify BIN files before installation.bat)
        let doc = egui::Rect::from_min_max(
            egui::pos2(c.x - 6.0, c.y - 5.0),
            egui::pos2(c.x + 6.0, c.y + 5.0),
        );
        painter.rect_filled(doc, 1.2, egui::Color32::from_rgb(218, 222, 228));
        painter.circle_stroke(
            c,
            2.5,
            egui::Stroke::new(1.5, egui::Color32::from_rgb(90, 102, 118)),
        );
    } else {
        // Clean white document page icon with folded top-right corner (like fg-01.bin)
        let doc = egui::Rect::from_min_max(
            egui::pos2(c.x - 4.8, c.y - 6.0),
            egui::pos2(c.x + 4.8, c.y + 6.0),
        );
        painter.rect_filled(doc, 1.0, egui::Color32::from_rgb(232, 235, 240));
        painter.line_segment(
            [
                egui::pos2(doc.right() - 3.0, doc.top()),
                egui::pos2(doc.right(), doc.top() + 3.0),
            ],
            egui::Stroke::new(1.0, egui::Color32::from_rgb(155, 160, 170)),
        );
    }
}

fn default_sample_torrent_tree() -> (String, Vec<TorrentFileEntry>) {
    let root = "Knights of Honor II [FitGirl Repack]".to_owned();
    let items: &[(&[&str], u64)] = &[
        (&["MD5", "fitgirl-bins.md5"], 104_038),
        (&["fg-01.bin"], 2_254_857_830),
        (&["fg-02.bin"], 606_708_224),
        (&["fg-03.bin"], 490_314_138),
        (&["fg-04.bin"], 42_886_758),
        (&["fg-optional-bonus-soundtrack.bin"], 149_736_653),
        (&["fg-optional-chinese.bin"], 299_473_306),
        (&["fg-optional-french.bin"], 337_431_757),
        (&["fg-optional-german.bin"], 345_925_222),
        (&["setup.exe"], 6_396_313),
        (&["Verify BIN files before installation.bat"], 69),
    ];
    let files = items
        .iter()
        .enumerate()
        .map(|(i, (path, size))| TorrentFileEntry {
            aria2_index: i + 1,
            rel_path: path.iter().map(|s| (*s).to_owned()).collect(),
            size: *size,
            selected: true,
            priority: TorrentFilePriority::Normal,
        })
        .collect();
    (root, files)
}

#[derive(Debug, Clone)]
enum BencodeVal {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<BencodeVal>),
    Dict(Vec<(Vec<u8>, BencodeVal)>),
}

impl BencodeVal {
    fn dict_get(&self, key: &[u8]) -> Option<&BencodeVal> {
        let Self::Dict(entries) = self else {
            return None;
        };
        entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    fn as_str_lossy(&self) -> Option<String> {
        let Self::Bytes(b) = self else {
            return None;
        };
        Some(String::from_utf8_lossy(b).into_owned())
    }

    fn as_u64(&self) -> Option<u64> {
        let Self::Int(n) = self else {
            return None;
        };
        u64::try_from(*n).ok()
    }
}

fn parse_bencode(input: &[u8], pos: &mut usize) -> Option<BencodeVal> {
    let &first = input.get(*pos)?;
    match first {
        b'i' => {
            *pos += 1;
            let end = input[*pos..].iter().position(|&b| b == b'e')? + *pos;
            let num_str = std::str::from_utf8(&input[*pos..end]).ok()?;
            let val = num_str.parse::<i64>().ok()?;
            *pos = end + 1;
            Some(BencodeVal::Int(val))
        }
        b'l' => {
            *pos += 1;
            let mut list = Vec::new();
            while *pos < input.len() && input[*pos] != b'e' {
                list.push(parse_bencode(input, pos)?);
            }
            if input.get(*pos) != Some(&b'e') {
                return None;
            }
            *pos += 1;
            Some(BencodeVal::List(list))
        }
        b'd' => {
            *pos += 1;
            let mut dict = Vec::new();
            while *pos < input.len() && input[*pos] != b'e' {
                let BencodeVal::Bytes(key) = parse_bencode(input, pos)? else {
                    return None;
                };
                let val = parse_bencode(input, pos)?;
                dict.push((key, val));
            }
            if input.get(*pos) != Some(&b'e') {
                return None;
            }
            *pos += 1;
            Some(BencodeVal::Dict(dict))
        }
        b'0'..=b'9' => {
            let colon = input[*pos..].iter().position(|&b| b == b':')? + *pos;
            let len_str = std::str::from_utf8(&input[*pos..colon]).ok()?;
            let len = len_str.parse::<usize>().ok()?;
            let start = colon + 1;
            let end = start.checked_add(len)?;
            if end > input.len() {
                return None;
            }
            let bytes = input[start..end].to_vec();
            *pos = end;
            Some(BencodeVal::Bytes(bytes))
        }
        _ => None,
    }
}

fn parse_torrent_file(path: &std::path::Path) -> Result<(String, Vec<TorrentFileEntry>), String> {
    let raw = std::fs::read(path).map_err(|e| format!("Could not read .torrent file: {e}"))?;
    let mut pos = 0;
    let root =
        parse_bencode(&raw, &mut pos).ok_or_else(|| "Invalid .torrent bencode data.".to_owned())?;
    let info = root
        .dict_get(b"info")
        .ok_or_else(|| "Torrent is missing 'info' dictionary.".to_owned())?;
    let root_name = info
        .dict_get(b"name.utf-8")
        .or_else(|| info.dict_get(b"name"))
        .and_then(BencodeVal::as_str_lossy)
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Torrent")
                .to_owned()
        });

    if let Some(BencodeVal::List(files_list)) = info.dict_get(b"files") {
        let mut entries = Vec::new();
        for (idx, file_dict) in files_list.iter().enumerate() {
            let size = file_dict
                .dict_get(b"length")
                .and_then(BencodeVal::as_u64)
                .unwrap_or(0);
            let path_val = file_dict
                .dict_get(b"path.utf-8")
                .or_else(|| file_dict.dict_get(b"path"));
            let mut rel_path = Vec::new();
            if let Some(BencodeVal::List(segs)) = path_val {
                for seg in segs {
                    if let Some(s) = seg.as_str_lossy()
                        && !s.is_empty()
                    {
                        rel_path.push(s);
                    }
                }
            }
            if rel_path.is_empty() {
                rel_path.push(format!("file-{}", idx + 1));
            }
            entries.push(TorrentFileEntry {
                aria2_index: idx + 1,
                rel_path,
                size,
                selected: true,
                priority: TorrentFilePriority::Normal,
            });
        }
        if !entries.is_empty() {
            return Ok((root_name, entries));
        }
    }

    let length = info
        .dict_get(b"length")
        .and_then(BencodeVal::as_u64)
        .unwrap_or(0);
    Ok((
        root_name.clone(),
        vec![TorrentFileEntry {
            aria2_index: 1,
            rel_path: vec![root_name],
            size: length,
            selected: true,
            priority: TorrentFilePriority::Normal,
        }],
    ))
}

fn parse_torrent_prefixed_line(line: &str) -> Option<(i64, &str)> {
    let rest = line.strip_prefix("RAJA_TORRENT:")?;
    let (id_str, content) = rest.split_once(':')?;
    let id = id_str.parse().ok()?;
    Some((id, content))
}

fn parse_aria2_complete_path(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let path = trimmed.strip_prefix("Download complete:")?.trim();
    if path.is_empty() || path.starts_with("[MEMORY]") {
        return None;
    }
    Some(path.to_owned())
}

fn parse_aria2_progress(line: &str) -> Option<(u64, Option<u64>)> {
    if line.contains("[MEMORY]") {
        return None;
    }
    // Look for `<downloaded>B/<total>B(` emitted by aria2c with `--human-readable=false`
    let slash_idx = line.find("B/")?;
    let before = &line[..slash_idx];
    let dl_start = before
        .rfind(|c: char| !c.is_ascii_digit())
        .map_or(0, |i| i + 1);
    let downloaded_bytes: u64 = before[dl_start..].parse().ok()?;
    let after = &line[slash_idx + 2..];
    let end_b = after.find("B(")?;
    let total_raw: u64 = after[..end_b].parse().ok()?;
    let total_bytes = (total_raw > 0).then_some(total_raw);
    Some((downloaded_bytes, total_bytes))
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
            .with_icon(std::sync::Arc::new(app_window_icon()))
            .with_visible(!background),
        ..Default::default()
    };
    let inbox_watch_dir = PathBuf::from(&app.database)
        .parent()
        .map(|p| p.join("browser-inbox"));
    eframe::run_native(
        "Raja",
        options,
        Box::new(move |cc| {
            if let Some(inbox_dir) = inbox_watch_dir {
                let ctx = cc.egui_ctx.clone();
                std::thread::spawn(move || {
                    loop {
                        std::thread::sleep(Duration::from_millis(250));
                        let has_request =
                            std::fs::read_dir(&inbox_dir).ok().is_some_and(|entries| {
                                entries
                                    .flatten()
                                    .any(|e| e.path().extension().is_some_and(|ext| ext == "json"))
                            });
                        if has_request {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                            ctx.request_repaint();
                        }
                    }
                });
            }
            Ok(Box::new(app))
        }),
    )
}

const APP_SHIELD_64_RGBA: &[u8] = include_bytes!("raja_ui/app_shield_64.rgba");

fn app_window_icon() -> egui::IconData {
    const SIZE: usize = 64;
    let rgba = render_3d_app_icon(SIZE);
    egui::IconData {
        rgba,
        width: SIZE as u32,
        height: SIZE as u32,
    }
}

fn render_3d_app_icon(size: usize) -> Vec<u8> {
    if size == 64 && APP_SHIELD_64_RGBA.len() == 64 * 64 * 4 {
        return APP_SHIELD_64_RGBA.to_vec();
    }
    let mut pixels = vec![0u8; size * size * 4];
    for py in 0..size {
        let sy = (py * 64 / size.max(1)).min(63);
        for px in 0..size {
            let sx = (px * 64 / size.max(1)).min(63);
            let src_idx = (sy * 64 + sx) * 4;
            let dst_idx = (py * size + px) * 4;
            pixels[dst_idx..dst_idx + 4].copy_from_slice(&APP_SHIELD_64_RGBA[src_idx..src_idx + 4]);
        }
    }
    pixels
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

    #[test]
    fn renders_3d_app_and_tray_icons_with_non_empty_pixels() {
        let app_icon = super::render_3d_app_icon(64);
        assert_eq!(app_icon.len(), 64 * 64 * 4);
        assert_eq!(app_icon[3], 0);
        assert!(
            app_icon
                .as_chunks::<4>()
                .0
                .iter()
                .any(|px| px[3] == 255 && px[0] > 200)
        );

        #[cfg(windows)]
        {
            let tray_icon = super::tray::render_3d_tray_icon(64);
            assert_eq!(tray_icon.len(), 64 * 64 * 4);
            assert_eq!(tray_icon[3], 0);
            assert!(
                tray_icon
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|px| px[3] == 255 && px[0] > 200)
            );
        }
    }

    #[test]
    fn parses_magnet_names_and_aria2_progress_lines() {
        let magnet = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Ubuntu+24.04+Desktop.iso";
        assert!(super::is_torrent_source(magnet));
        assert_eq!(super::validate_torrent_source(magnet).unwrap(), magnet);
        assert_eq!(
            super::suggested_torrent_name(magnet),
            "Ubuntu 24.04 Desktop.iso"
        );
        assert_eq!(
            super::parse_aria2_progress("[#4a1b2c 10485760B/41943040B(25%) CN:6 SD:2 DL:2097152B]"),
            Some((10_485_760, Some(41_943_040)))
        );
        assert_eq!(
            super::parse_aria2_complete_path(
                "Download complete: C:/Downloads/Ubuntu 24.04 Desktop.iso"
            ),
            Some("C:/Downloads/Ubuntu 24.04 Desktop.iso".to_owned())
        );
        assert_eq!(
            super::parse_aria2_complete_path("Download complete: [MEMORY]ubuntu"),
            None
        );
    }
}
