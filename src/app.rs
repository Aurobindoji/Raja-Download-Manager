use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::http::HttpRangeClient;
use crate::part_file::PartFile;
use crate::storage::Storage;

const MAX_PARALLEL_SEGMENTS: usize = 4;
const MIN_PARALLEL_SEGMENT_SIZE: u64 = 1024 * 1024;

pub fn run_cli<I>(arguments: I) -> Result<(), Box<dyn Error>>
where
    I: IntoIterator<Item = OsString>,
{
    let mut arguments = arguments.into_iter();
    let Some(command) = arguments.next() else {
        print_usage();
        return Ok(());
    };
    if command == "--help" || command == "-h" || command == "help" {
        print_usage();
        return Ok(());
    }
    if command == "list" {
        let mut database_override = None;
        while let Some(option) = arguments.next() {
            if option != "--database" {
                return Err(invalid_input("unknown list option").into());
            }
            if database_override.is_some() {
                return Err(invalid_input("--database may only be specified once").into());
            }
            database_override =
                Some(PathBuf::from(arguments.next().ok_or_else(|| {
                    invalid_input("--database requires a file path")
                })?));
        }
        return list_downloads(database_override);
    }
    if command == "pause" {
        let download_id = arguments
            .next()
            .ok_or_else(|| invalid_input("missing download ID"))?
            .into_string()
            .map_err(|_| invalid_input("download ID must be valid Unicode"))?
            .parse::<i64>()
            .map_err(|_| invalid_input("download ID must be a positive integer"))?;
        if download_id <= 0 {
            return Err(invalid_input("download ID must be a positive integer").into());
        }
        let mut database_override = None;
        while let Some(option) = arguments.next() {
            if option != "--database" {
                return Err(invalid_input("unknown pause option").into());
            }
            if database_override.is_some() {
                return Err(invalid_input("--database may only be specified once").into());
            }
            database_override =
                Some(PathBuf::from(arguments.next().ok_or_else(|| {
                    invalid_input("--database requires a file path")
                })?));
        }
        return pause_download(download_id, database_override);
    }
    if command == "retry" {
        let download_id = arguments
            .next()
            .ok_or_else(|| invalid_input("missing download ID"))?
            .into_string()
            .map_err(|_| invalid_input("download ID must be valid Unicode"))?
            .parse::<i64>()
            .map_err(|_| invalid_input("download ID must be a positive integer"))?;
        if download_id <= 0 {
            return Err(invalid_input("download ID must be a positive integer").into());
        }
        let mut database_override = None;
        while let Some(option) = arguments.next() {
            if option != "--database" {
                return Err(invalid_input("unknown retry option").into());
            }
            if database_override.is_some() {
                return Err(invalid_input("--database may only be specified once").into());
            }
            database_override =
                Some(PathBuf::from(arguments.next().ok_or_else(|| {
                    invalid_input("--database requires a file path")
                })?));
        }
        return retry_download(download_id, database_override);
    }
    if command == "forget" {
        let download_id = arguments
            .next()
            .ok_or_else(|| invalid_input("missing download ID"))?
            .into_string()
            .map_err(|_| invalid_input("download ID must be valid Unicode"))?
            .parse::<i64>()
            .map_err(|_| invalid_input("download ID must be a positive integer"))?;
        if download_id <= 0 {
            return Err(invalid_input("download ID must be a positive integer").into());
        }
        let mut database_override = None;
        while let Some(option) = arguments.next() {
            if option != "--database" {
                return Err(invalid_input("unknown forget option").into());
            }
            if database_override.is_some() {
                return Err(invalid_input("--database may only be specified once").into());
            }
            database_override =
                Some(PathBuf::from(arguments.next().ok_or_else(|| {
                    invalid_input("--database requires a file path")
                })?));
        }
        return forget_download(download_id, database_override);
    }
    if command != "download" {
        return Err(invalid_input(
            "unknown command; expected `download`, `list`, `pause`, `retry`, or `forget`",
        )
        .into());
    }

    let url = arguments
        .next()
        .ok_or_else(|| invalid_input("missing download URL"))?
        .into_string()
        .map_err(|_| invalid_input("download URL must be valid Unicode"))?;
    let requested_output = PathBuf::from(
        arguments
            .next()
            .ok_or_else(|| invalid_input("missing output file path"))?,
    );
    let mut expected_sha256 = None;
    let mut database_override = None;
    while let Some(option) = arguments.next() {
        if option == "--sha256" {
            if expected_sha256.is_some() {
                return Err(invalid_input("--sha256 may only be specified once").into());
            }
            expected_sha256 = Some(
                arguments
                    .next()
                    .ok_or_else(|| invalid_input("--sha256 requires a 64-digit hash"))?
                    .into_string()
                    .map_err(|_| invalid_input("SHA-256 must be valid Unicode"))?,
            );
        } else if option == "--database" {
            if database_override.is_some() {
                return Err(invalid_input("--database may only be specified once").into());
            }
            database_override =
                Some(PathBuf::from(arguments.next().ok_or_else(|| {
                    invalid_input("--database requires a file path")
                })?));
        } else {
            return Err(invalid_input("unknown download option").into());
        }
    }

    download_one(
        &url,
        &requested_output,
        expected_sha256.as_deref(),
        database_override,
    )?;
    Ok(())
}

fn download_one(
    url: &str,
    requested_output: &Path,
    expected_sha256: Option<&str>,
    database_override: Option<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    let expected_sha256 = expected_sha256.map(normalize_sha256).transpose()?;
    let output_name = requested_output
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| invalid_input("output path must include a file name"))?
        .to_owned();
    let output_parent = parent_or_current(requested_output);
    fs::create_dir_all(output_parent)?;
    let output_parent = fs::canonicalize(output_parent)?;
    let final_path = output_parent.join(output_name);
    let part_path = append_suffix(&final_path, ".ndpart");

    let database_path = match database_override {
        Some(path) => path,
        None => default_database_path()?,
    };
    fs::create_dir_all(parent_or_current(&database_path))?;
    let mut storage = Storage::open(&database_path)?;
    let recovered = PartFile::recover_pending_finalizations(&mut storage)?;
    if recovered > 0 {
        eprintln!("Recovered {recovered} completed download(s).");
    }
    if final_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "output file already exists; choose a different name",
        )
        .into());
    }

    let client = HttpRangeClient::new()?;
    let final_name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_input("output file name must be valid Unicode"))?;
    let output_dir = output_parent
        .to_str()
        .ok_or_else(|| invalid_input("output directory must be valid Unicode"))?;
    // Entries made by the add box or Link Grabber store the folder as typed
    // ("C:\..." or "C:\...\"), the engine stores it canonical ("\\?\C:\...").
    // Accept every spelling, so a queued entry is continued, not duplicated.
    let plain_dir = output_dir.strip_prefix(r"\\?\").unwrap_or(output_dir);
    let with_separator = format!("{plain_dir}{}", std::path::MAIN_SEPARATOR);
    let mut existing = None;
    for folder in [output_dir, plain_dir, with_separator.as_str()] {
        existing = storage.find_resumable_download(folder, final_name)?;
        if existing.is_some() {
            break;
        }
    }
    // Resume from the same mirror the download started on (its copy has the
    // ETag the saved segments were checked against); otherwise follow the
    // link's redirect once and pin the result for every segment.
    let pinned = existing.as_ref().and_then(|existing| {
        storage
            .download_server_urls()
            .ok()
            .and_then(|urls| urls.get(&existing.id).cloned())
    });
    let remote = match pinned.as_deref().map(|pinned| client.probe(pinned)) {
        Some(Ok(remote)) => remote,
        _ => client.probe(url)?,
    };
    let (download_id, segment_ids, part_file, effective_expected_sha256) = if let Some(existing) =
        existing
    {
        if existing.source_url != url {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "an unfinished download for this output belongs to a different URL",
            )
            .into());
        }
        if existing.total_size != Some(remote.size) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the remote file size changed since the interrupted download",
            )
            .into());
        }

        let (segment_ids, part_file) = match existing.segment_id {
            Some(segment_id) => {
                if !part_path.exists() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "the saved download has no matching .ndpart file; refusing to recreate it",
                    )
                    .into());
                }
                storage.save_source_validators(segment_id, &remote.validators)?;
                let segment_ids = storage.segment_ids(existing.id)?;
                if segment_ids.len() > 1
                    && (!remote.supports_ranges || remote.validators.if_range_value().is_none())
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the saved multi-segment download requires byte ranges and a stable server validator",
                    )
                    .into());
                }
                (
                    segment_ids,
                    PartFile::open_existing(&part_path, remote.size)?,
                )
            }
            None => {
                if part_path.exists() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "an untracked .ndpart file exists for a download with no segment record",
                    )
                    .into());
                }
                storage.set_source_validators(existing.id, &remote.validators)?;
                let segment_ids = create_segments(&mut storage, existing.id, &remote);
                (segment_ids?, PartFile::create(&part_path, remote.size)?)
            }
        };
        let effective_expected_sha256 =
            storage.set_expected_sha256(existing.id, expected_sha256.as_deref())?;
        (
            existing.id,
            segment_ids,
            part_file,
            effective_expected_sha256,
        )
    } else {
        if part_path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "an untracked .ndpart file already exists; refusing to overwrite it",
            )
            .into());
        }
        let download_id =
            storage.create_download(url, final_name, output_dir, Some(remote.size))?;
        let effective_expected_sha256 =
            storage.set_expected_sha256(download_id, expected_sha256.as_deref())?;
        storage.set_source_validators(download_id, &remote.validators)?;
        let segment_ids = create_segments(&mut storage, download_id, &remote)?;
        let part_file = PartFile::create(&part_path, remote.size)?;
        (
            download_id,
            segment_ids,
            part_file,
            effective_expected_sha256,
        )
    };

    storage.set_download_server_url(download_id, &remote.final_url)?;
    let segments_restarted = part_file.verify_durable_blocks(&mut storage, &segment_ids)?;
    if segments_restarted > 0 {
        eprintln!(
            "Restarted {segments_restarted} segment(s) whose saved data failed integrity verification."
        );
    }
    storage.mark_download_active(download_id)?;
    let segment_id = segment_ids
        .last()
        .copied()
        .ok_or_else(|| invalid_input("download has no segments to finalize"))?;
    let mut unfinished_segments = 0;
    let mut has_durable_progress = false;
    for segment_id in &segment_ids {
        if storage.segment_status(*segment_id)? != "complete" {
            unfinished_segments += 1;
            let (start, _) = storage.segment_range(*segment_id)?;
            has_durable_progress |= storage.segment_cursor(*segment_id)? > start;
        }
    }
    if unfinished_segments == 0 {
        eprintln!("Finishing verified download...");
    } else {
        let max_parallel = configured_max_parallel_segments(&storage);
        eprintln!(
            "{} {} bytes across {} connection(s)...",
            if has_durable_progress {
                "Resuming"
            } else {
                "Downloading"
            },
            remote.size,
            unfinished_segments.min(max_parallel),
        );
        let progress_worker = spawn_progress_reporter(&database_path, download_id)?;
        let download_result = download_segments(
            &client,
            &remote.final_url,
            &part_file,
            &database_path,
            &mut storage,
            &segment_ids,
        );
        let progress_result = progress_worker.stop();
        if let Err(error) = download_result {
            progress_result?;
            if storage.download_status(segment_id)? == "paused" {
                eprintln!(
                    "Download {download_id} paused; run the same download command to resume."
                );
                return Ok(());
            }
            storage.mark_download_failed(download_id, &error.to_string())?;
            return Err(error);
        }
        progress_result?;
    }
    if storage.download_status(segment_id)? == "paused" {
        eprintln!("Download {download_id} paused; run the same download command to resume.");
        return Ok(());
    }
    let digest = match part_file.finalize(
        &mut storage,
        segment_id,
        &final_path,
        effective_expected_sha256.as_deref(),
    ) {
        Ok(digest) => digest,
        Err(error) => {
            storage.mark_download_failed(download_id, &error.to_string())?;
            return Err(error.into());
        }
    };
    println!(
        "Saved {} (SHA-256: {})",
        final_path.display(),
        encode_sha256(&digest)
    );
    Ok(())
}

fn list_downloads(database_override: Option<PathBuf>) -> Result<(), Box<dyn Error>> {
    let database_path = match database_override {
        Some(path) => path,
        None => default_database_path()?,
    };
    fs::create_dir_all(parent_or_current(&database_path))?;
    let mut storage = Storage::open(&database_path)?;
    let recovered = PartFile::recover_pending_finalizations(&mut storage)?;
    if recovered > 0 {
        eprintln!("Recovered {recovered} completed download(s).");
    }
    let downloads = storage.list_downloads()?;
    if downloads.is_empty() {
        println!("No downloads found.");
        return Ok(());
    }
    println!(
        "{:<6} {:<9} {:>14} {:>14}  FILE",
        "ID", "STATUS", "PROGRESS", "SIZE (BYTES)"
    );
    for download in downloads {
        let size = download
            .total_size
            .map_or_else(|| "unknown".to_owned(), |size| size.to_string());
        let progress = download.total_size.map_or_else(
            || format!("{} bytes", download.durable_bytes),
            |total| format!("{:.1}%", percentage(download.durable_bytes, total)),
        );
        println!(
            "{:<6} {:<9} {:>14} {:>14}  {}",
            download.id, download.status, progress, size, download.file_name
        );
    }
    Ok(())
}

struct ProgressReporter {
    stop: Arc<AtomicBool>,
    worker: std::thread::JoinHandle<Result<(), String>>,
}

impl ProgressReporter {
    fn stop(self) -> Result<(), Box<dyn Error>> {
        self.stop.store(true, Ordering::Release);
        self.worker.thread().unpark();
        match self.worker.join() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(io::Error::other(error).into()),
            Err(_) => Err(io::Error::other("progress reporter panicked").into()),
        }
    }
}

fn spawn_progress_reporter(
    database_path: &Path,
    download_id: i64,
) -> Result<ProgressReporter, Box<dyn Error>> {
    let storage = Storage::open(database_path)?;
    let (initial_bytes, total_size) = storage.download_progress(download_id)?;
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let worker = std::thread::spawn(move || {
        let mut previous_bytes = initial_bytes;
        let mut previous_time = Instant::now();
        while !worker_stop.load(Ordering::Acquire) {
            std::thread::park_timeout(Duration::from_millis(500));
            if worker_stop.load(Ordering::Acquire) {
                break;
            }
            let (durable_bytes, _) = storage
                .download_progress(download_id)
                .map_err(|error| error.to_string())?;
            if durable_bytes == previous_bytes {
                continue;
            }
            let now = Instant::now();
            let elapsed = now.duration_since(previous_time).as_secs_f64();
            let rate = if elapsed > 0.0 {
                durable_bytes.saturating_sub(previous_bytes) as f64 / elapsed
            } else {
                0.0
            };
            eprintln!(
                "Progress: {:.1}% ({} / {} bytes; {:.2} MiB/s)",
                percentage(durable_bytes, total_size),
                durable_bytes,
                total_size,
                rate / (1024.0 * 1024.0),
            );
            previous_bytes = durable_bytes;
            previous_time = now;
        }
        Ok(())
    });
    Ok(ProgressReporter { stop, worker })
}

fn percentage(bytes: u64, total: u64) -> f64 {
    if total == 0 {
        100.0
    } else {
        (bytes.min(total) as f64 / total as f64) * 100.0
    }
}

fn pause_download(
    download_id: i64,
    database_override: Option<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    let database_path = match database_override {
        Some(path) => path,
        None => default_database_path()?,
    };
    let mut storage = Storage::open(&database_path)?;
    storage.pause_download(download_id)?;
    println!("Paused download {download_id}.");
    Ok(())
}

fn retry_download(
    download_id: i64,
    database_override: Option<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    let database_path = match database_override {
        Some(path) => path,
        None => default_database_path()?,
    };
    let storage = Storage::open(&database_path)?;
    let download = storage.retry_download(download_id)?;
    let output_path = Path::new(&download.output_dir).join(download.file_name);
    download_one(
        &download.source_url,
        &output_path,
        download.expected_sha256.as_deref(),
        Some(database_path),
    )
}

fn forget_download(
    download_id: i64,
    database_override: Option<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    let database_path = match database_override {
        Some(path) => path,
        None => default_database_path()?,
    };
    let mut storage = Storage::open(&database_path)?;
    storage.forget_completed_download(download_id)?;
    println!("Removed download {download_id} from Raja history; the downloaded file was kept.");
    Ok(())
}

fn configured_max_parallel_segments(storage: &Storage) -> usize {
    storage
        .setting("max_connections")
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| (1..=32).contains(&n))
        .unwrap_or(MAX_PARALLEL_SEGMENTS)
}

fn create_segments(
    storage: &mut Storage,
    download_id: i64,
    remote: &crate::http::RemoteFile,
) -> Result<Vec<i64>, Box<dyn Error>> {
    let can_parallelize = remote.supports_ranges && remote.validators.if_range_value().is_some();
    let max_parallel = configured_max_parallel_segments(storage);
    let ranges = segment_ranges_with_max(remote.size, can_parallelize, max_parallel);
    ranges
        .into_iter()
        .map(|(start, end)| {
            storage
                .create_segment(download_id, start, end)
                .map_err(Into::into)
        })
        .collect()
}

#[cfg(test)]
fn segment_ranges(total_size: u64, can_parallelize: bool) -> Vec<(u64, u64)> {
    segment_ranges_with_max(total_size, can_parallelize, MAX_PARALLEL_SEGMENTS)
}

fn segment_ranges_with_max(
    total_size: u64,
    can_parallelize: bool,
    max_parallel: usize,
) -> Vec<(u64, u64)> {
    let max_parallel = max_parallel.clamp(1, 32);
    if !can_parallelize || max_parallel <= 1 || total_size < MIN_PARALLEL_SEGMENT_SIZE * 2 {
        return vec![(0, total_size)];
    }
    let count = (total_size / MIN_PARALLEL_SEGMENT_SIZE).min(max_parallel as u64) as usize;
    let base_size = total_size / count as u64;
    let remainder = total_size % count as u64;
    let mut start = 0;
    (0..count)
        .map(|index| {
            let end = start + base_size + u64::from((index as u64) < remainder);
            let range = (start, end);
            start = end;
            range
        })
        .collect()
}

fn download_segments(
    client: &HttpRangeClient,
    url: &str,
    part_file: &PartFile,
    database_path: &Path,
    storage: &mut Storage,
    segment_ids: &[i64],
) -> Result<(), Box<dyn Error>> {
    let mut pending = Vec::new();
    for segment_id in segment_ids {
        if storage.segment_status(*segment_id)? != "complete" {
            pending.push(*segment_id);
        }
    }
    let max_parallel = configured_max_parallel_segments(storage);
    for batch in pending.chunks(max_parallel) {
        if batch.len() == 1 {
            client.download_segment(url, part_file, storage, batch[0])?;
            continue;
        }
        let results = std::thread::scope(|scope| {
            let handles = batch
                .iter()
                .copied()
                .map(|segment_id| {
                    let url = url.to_owned();
                    let database_path = database_path.to_owned();
                    scope.spawn(move || {
                        let mut worker_storage =
                            Storage::open(database_path).map_err(|error| error.to_string())?;
                        let worker_client =
                            HttpRangeClient::new().map_err(|error| error.to_string())?;
                        worker_client
                            .download_segment(&url, part_file, &mut worker_storage, segment_id)
                            .map(|_| ())
                            .map_err(|error| error.to_string())
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(std::thread::ScopedJoinHandle::join)
                .collect::<Vec<_>>()
        });
        for result in results {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return Err(io::Error::other(error).into()),
                Err(_) => return Err(io::Error::other("a download worker panicked").into()),
            }
        }
    }
    Ok(())
}

pub fn default_database_path() -> io::Result<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("APPDATA"))
        .ok_or_else(|| invalid_input("LOCALAPPDATA is not set; pass --database explicitly"))?;

    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .or_else(|| {
            std::env::var_os("HOME").map(|home| {
                let mut path = PathBuf::from(home);
                path.push(".local");
                path.push("share");
                path.into_os_string()
            })
        })
        .ok_or_else(|| invalid_input("no user data directory is available"))?;

    Ok(PathBuf::from(base).join("Raja").join("raja.db"))
}

fn parent_or_current(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn encode_sha256(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn invalid_input(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.to_owned())
}

fn normalize_sha256(value: &str) -> io::Result<String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid_input(
            "expected SHA-256 must contain exactly 64 hexadecimal digits",
        ));
    }
    Ok(value.to_ascii_lowercase())
}

fn print_usage() {
    println!(
        "Raja download manager\n\
         Usage:\n\
         \x20 raja-core download <HTTP-or-HTTPS-URL> <output-file> [--sha256 <hex>] [--database <path>]\n\
         \x20 raja-core list [--database <path>]\n\
         \x20 raja-core pause <download-id> [--database <path>]\n\
         \x20 raja-core retry <download-id> [--database <path>]\n\
         \x20 raja-core forget <download-id> [--database <path>]\n\
         \x20 raja-core --help"
    );
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use sha2::{Digest, Sha256};

    use crate::part_file::PartFile;
    use crate::storage::{SourceValidators, Storage};

    use super::{MAX_PARALLEL_SEGMENTS, MIN_PARALLEL_SEGMENT_SIZE, run_cli, segment_ranges};

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

    struct TestFiles {
        root: PathBuf,
    }

    impl TestFiles {
        fn new() -> Self {
            let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
            Self {
                root: std::env::temp_dir()
                    .join(format!("raja-cli-test-{}-{id}", std::process::id())),
            }
        }

        fn output(&self) -> PathBuf {
            self.root.join("downloads").join("file.bin")
        }

        fn database(&self) -> PathBuf {
            self.root.join("state").join("raja.db")
        }
    }

    impl Drop for TestFiles {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn serve_probe_and_download() -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut byte = [0; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                let range = request
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("range")
                            .then(|| value.trim().to_owned())
                    })
                    .unwrap();
                let (content_range, body): (&str, &[u8]) = match range.as_str() {
                    "bytes=0-0" => ("bytes 0-0/12", b"h"),
                    "bytes=0-11" => ("bytes 0-11/12", b"hello world!"),
                    other => panic!("unexpected byte range: {other}"),
                };
                let response = format!(
                    "HTTP/1.1 206 Partial Content\r\n\
                     Content-Range: {content_range}\r\n\
                     Content-Length: {}\r\n\
                     ETag: \"test-v1\"\r\n\
                     Connection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.write_all(body).unwrap();
                stream.flush().unwrap();
                requests.push(request);
            }
            requests
        });
        (format!("http://{address}/file"), handle)
    }

    fn serve_without_range_support() -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut byte = [0; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                requests.push(String::from_utf8(request).unwrap());
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\n\
                          Content-Length: 12\r\n\
                          ETag: \"test-v1\"\r\n\
                          Connection: close\r\n\r\n\
                          hello world!",
                    )
                    .unwrap();
                stream.flush().unwrap();
            }
            requests
        });
        (format!("http://{address}/file"), handle)
    }

    fn serve_failed_download() -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut byte = [0; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let response = if attempt == 0 {
                    b"HTTP/1.1 206 Partial Content\r\n\
                      Content-Range: bytes 0-0/4\r\n\
                      Content-Length: 1\r\n\
                      ETag: \"failure-v1\"\r\n\
                      Connection: close\r\n\r\nx"
                        .as_slice()
                } else {
                    b"HTTP/1.1 404 Not Found\r\n\
                      Content-Length: 0\r\n\
                      Connection: close\r\n\r\n"
                        .as_slice()
                };
                stream.write_all(response).unwrap();
                stream.flush().unwrap();
            }
        });
        (format!("http://{address}/file"), handle)
    }

    fn serve_range_download(body: Vec<u8>) -> (String, thread::JoinHandle<(Vec<String>, bool)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let total_size = body.len();
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            let accept_with_timeout = |timeout| {
                let deadline = Instant::now() + timeout;
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            stream.set_nonblocking(false).unwrap();
                            return Some(stream);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                return None;
                            }
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("test server accept failed: {error}"),
                    }
                }
            };
            let read_request = |stream: &TcpStream| {
                let mut stream = stream;
                let mut request = Vec::new();
                let mut byte = [0; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                String::from_utf8(request).unwrap()
            };
            let send_response = |mut stream: TcpStream, request: String| {
                let range = request
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        if name.eq_ignore_ascii_case("range") {
                            Some(value.trim().strip_prefix("bytes=")?.to_owned())
                        } else {
                            None
                        }
                    })
                    .unwrap();
                let (start, end) = range.split_once('-').unwrap();
                let start = start.parse::<usize>().unwrap();
                let end = end.parse::<usize>().unwrap();
                let response_body = &body[start..=end];
                let response = format!(
                    "HTTP/1.1 206 Partial Content\r\n\
                     Content-Range: bytes {start}-{end}/{total_size}\r\n\
                     Content-Length: {}\r\n\
                     ETag: \"parallel-v1\"\r\n\
                     Connection: close\r\n\r\n",
                    response_body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.write_all(response_body).unwrap();
                stream.flush().unwrap();
                request
            };

            let probe_stream = accept_with_timeout(Duration::from_secs(5)).unwrap();
            let probe_request = read_request(&probe_stream);
            assert!(request_has_header(&probe_request, "range", "bytes=0-0"));
            let probe_response = format!(
                "HTTP/1.1 206 Partial Content\r\n\
                 Content-Range: bytes 0-0/{total_size}\r\n\
                 Content-Length: 1\r\n\
                 ETag: \"parallel-v1\"\r\n\
                 Connection: close\r\n\r\n"
            );
            let mut probe_stream = probe_stream;
            probe_stream.write_all(probe_response.as_bytes()).unwrap();
            probe_stream.write_all(&body[0..1]).unwrap();
            probe_stream.flush().unwrap();
            requests.push(probe_request);

            let first_stream = accept_with_timeout(Duration::from_secs(5)).unwrap();
            let second_stream = accept_with_timeout(Duration::from_secs(3));
            let simultaneous = second_stream.is_some();
            let mut streams = vec![first_stream];
            if let Some(second_stream) = second_stream {
                streams.push(second_stream);
            } else {
                let first_request = read_request(&streams[0]);
                requests.push(send_response(streams.pop().unwrap(), first_request));
                streams.push(accept_with_timeout(Duration::from_secs(5)).unwrap());
            }
            for stream in streams {
                let request = read_request(&stream);
                requests.push(send_response(stream, request));
            }
            (requests, simultaneous)
        });
        (format!("http://{address}/file"), handle)
    }

    fn request_has_header(request: &str, name: &str, value: &str) -> bool {
        request.lines().any(|line| {
            let Some((header_name, header_value)) = line.split_once(':') else {
                return false;
            };
            header_name.eq_ignore_ascii_case(name) && header_value.trim() == value
        })
    }

    #[test]
    fn cli_downloads_and_finalizes_a_file_end_to_end() {
        let paths = TestFiles::new();
        let (url, server) = serve_probe_and_download();
        run_cli([
            "download".into(),
            url.into(),
            paths.output().into_os_string(),
            "--database".into(),
            paths.database().into_os_string(),
            "--sha256".into(),
            "7509e5bda0c762d2bac7f90d758b5b2263fa01ccbc542ab5e3df163be08e6ca9".into(),
        ])
        .unwrap();

        let requests = server.join().unwrap();
        assert!(request_has_header(&requests[0], "range", "bytes=0-0"));
        assert!(request_has_header(&requests[1], "range", "bytes=0-11"));
        assert!(request_has_header(&requests[1], "if-range", "\"test-v1\""));
        assert_eq!(fs::read(paths.output()).unwrap(), b"hello world!");
        assert!(paths.database().exists());
    }

    #[test]
    fn cli_downloads_from_a_server_that_ignores_range_requests() {
        let paths = TestFiles::new();
        let (url, server) = serve_without_range_support();
        run_cli([
            "download".into(),
            url.into(),
            paths.output().into_os_string(),
            "--database".into(),
            paths.database().into_os_string(),
        ])
        .unwrap();

        let requests = server.join().unwrap();
        assert!(request_has_header(&requests[0], "range", "bytes=0-0"));
        assert!(request_has_header(&requests[1], "range", "bytes=0-11"));
        assert_eq!(fs::read(paths.output()).unwrap(), b"hello world!");
    }

    #[test]
    fn cli_records_failures_and_lists_persisted_downloads() {
        let paths = TestFiles::new();
        let (url, server) = serve_failed_download();
        assert!(
            run_cli([
                "download".into(),
                url.into(),
                paths.output().into_os_string(),
                "--database".into(),
                paths.database().into_os_string(),
            ])
            .is_err()
        );
        server.join().unwrap();

        let storage = Storage::open(paths.database()).unwrap();
        let downloads = storage.list_downloads().unwrap();
        assert_eq!(downloads.len(), 1);
        assert_eq!(downloads[0].status, "failed");
        assert_eq!(downloads[0].file_name, "file.bin");

        run_cli([
            "list".into(),
            "--database".into(),
            paths.database().into_os_string(),
        ])
        .unwrap();
    }

    #[test]
    fn cli_pauses_an_active_download_for_later_resume() {
        let paths = TestFiles::new();
        fs::create_dir_all(paths.database().parent().unwrap()).unwrap();
        let mut storage = Storage::open(paths.database()).unwrap();
        let download_id = storage
            .create_download(
                "https://example.test/file",
                "file.bin",
                paths.output().parent().unwrap().to_str().unwrap(),
                Some(12),
            )
            .unwrap();
        storage.mark_download_active(download_id).unwrap();

        run_cli([
            "pause".into(),
            download_id.to_string().into(),
            "--database".into(),
            paths.database().into_os_string(),
        ])
        .unwrap();

        assert_eq!(storage.list_downloads().unwrap()[0].status, "paused");
    }

    #[test]
    fn cli_retries_a_failed_download_using_saved_url_destination_and_hash() {
        let paths = TestFiles::new();
        let output_path = paths.output();
        fs::create_dir_all(output_path.parent().unwrap()).unwrap();
        fs::create_dir_all(paths.database().parent().unwrap()).unwrap();
        let output_dir = fs::canonicalize(output_path.parent().unwrap()).unwrap();
        let (url, server) = serve_probe_and_download();
        let download_id = {
            let mut storage = Storage::open(paths.database()).unwrap();
            let download_id = storage
                .create_download(&url, "file.bin", output_dir.to_str().unwrap(), Some(12))
                .unwrap();
            storage
                .set_expected_sha256(
                    download_id,
                    Some("7509e5bda0c762d2bac7f90d758b5b2263fa01ccbc542ab5e3df163be08e6ca9"),
                )
                .unwrap();
            storage.mark_download_active(download_id).unwrap();
            storage
                .mark_download_failed(download_id, "temporary connection failure")
                .unwrap();
            download_id
        };

        run_cli([
            "retry".into(),
            download_id.to_string().into(),
            "--database".into(),
            paths.database().into_os_string(),
        ])
        .unwrap();
        let requests = server.join().unwrap();
        assert!(request_has_header(&requests[1], "if-range", "\"test-v1\""));
        assert_eq!(fs::read(paths.output()).unwrap(), b"hello world!");
        assert_eq!(
            Storage::open(paths.database())
                .unwrap()
                .list_downloads()
                .unwrap()[0]
                .status,
            "complete"
        );
    }

    #[test]
    fn cli_downloads_large_validator_backed_files_in_parallel_ranges() {
        let paths = TestFiles::new();
        let body = (0..MIN_PARALLEL_SEGMENT_SIZE * 2)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let (url, server) = serve_range_download(body.clone());
        run_cli([
            "download".into(),
            url.into(),
            paths.output().into_os_string(),
            "--database".into(),
            paths.database().into_os_string(),
        ])
        .unwrap();

        let (requests, simultaneous) = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(
            simultaneous,
            "both segment connections should be open together"
        );
        assert!(request_has_header(&requests[0], "range", "bytes=0-0"));
        let ranges = requests
            .iter()
            .skip(1)
            .map(|request| {
                request
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("range")
                            .then(|| value.trim().to_owned())
                    })
                    .unwrap()
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            ranges,
            [
                format!("bytes=0-{}", MIN_PARALLEL_SEGMENT_SIZE - 1),
                format!(
                    "bytes={}-{}",
                    MIN_PARALLEL_SEGMENT_SIZE,
                    MIN_PARALLEL_SEGMENT_SIZE * 2 - 1
                ),
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(fs::read(paths.output()).unwrap(), body);
        assert_eq!(MAX_PARALLEL_SEGMENTS, 4);
    }

    #[test]
    fn cli_resumes_a_partially_downloaded_parallel_range() {
        let paths = TestFiles::new();
        let size = MIN_PARALLEL_SEGMENT_SIZE * 2;
        let split = MIN_PARALLEL_SEGMENT_SIZE;
        let persisted = 64;
        let body = (0..size)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let output_path = paths.output();
        fs::create_dir_all(output_path.parent().unwrap()).unwrap();
        let output_dir = fs::canonicalize(output_path.parent().unwrap()).unwrap();
        fs::create_dir_all(paths.database().parent().unwrap()).unwrap();
        let part_path = output_dir.join("file.bin.ndpart");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let url = format!("http://{address}/file");
        let expected_range = format!("bytes={}-{}", split + persisted, size - 1);
        let server_expected_range = expected_range.clone();
        let server_body = body.clone();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut byte = [0; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                if attempt == 0 {
                    stream
                        .write_all(
                            format!(
                                "HTTP/1.1 206 Partial Content\r\n\
                                 Content-Range: bytes 0-0/{size}\r\n\
                                 Content-Length: 1\r\n\
                                 ETag: \"resume-v1\"\r\n\
                                 Connection: close\r\n\r\n"
                            )
                            .as_bytes(),
                        )
                        .unwrap();
                    stream.write_all(&server_body[0..1]).unwrap();
                } else {
                    assert!(request_has_header(
                        &request,
                        "range",
                        &server_expected_range
                    ));
                    stream
                        .write_all(
                            format!(
                                "HTTP/1.1 206 Partial Content\r\n\
                                 Content-Range: bytes {}-{}/{size}\r\n\
                                 Content-Length: {}\r\n\
                                 ETag: \"resume-v1\"\r\n\
                                 Connection: close\r\n\r\n",
                                split + persisted,
                                size - 1,
                                size - split - persisted,
                            )
                            .as_bytes(),
                        )
                        .unwrap();
                    stream
                        .write_all(&server_body[(split + persisted) as usize..])
                        .unwrap();
                }
                stream.flush().unwrap();
                requests.push(request);
            }
            requests
        });

        {
            let mut storage = Storage::open(paths.database()).unwrap();
            let download_id = storage
                .create_download(&url, "file.bin", output_dir.to_str().unwrap(), Some(size))
                .unwrap();
            storage
                .set_source_validators(
                    download_id,
                    &SourceValidators {
                        etag: Some("\"resume-v1\"".to_owned()),
                        last_modified: None,
                    },
                )
                .unwrap();
            let first = storage.create_segment(download_id, 0, split).unwrap();
            let second = storage.create_segment(download_id, split, size).unwrap();
            let part = PartFile::create(&part_path, size).unwrap();
            part.write_segment(&mut storage, first, 0, &body[..split as usize])
                .unwrap();
            storage.mark_segment_complete(first).unwrap();
            part.write_segment(
                &mut storage,
                second,
                split,
                &body[split as usize..(split + persisted) as usize],
            )
            .unwrap();
        }

        run_cli([
            "download".into(),
            url.into(),
            output_path.into_os_string(),
            "--database".into(),
            paths.database().into_os_string(),
        ])
        .unwrap();
        let requests = server.join().unwrap();
        assert!(request_has_header(&requests[1], "range", &expected_range));
        assert_eq!(fs::read(paths.output()).unwrap(), body);
    }

    #[test]
    fn segment_planning_is_contiguous_and_respects_single_connection_modes() {
        assert_eq!(segment_ranges(12, false), [(0, 12)]);
        assert_eq!(
            segment_ranges(MIN_PARALLEL_SEGMENT_SIZE, true),
            [(0, MIN_PARALLEL_SEGMENT_SIZE)]
        );

        let ranges = segment_ranges(MIN_PARALLEL_SEGMENT_SIZE * 5, true);
        assert_eq!(ranges.len(), MAX_PARALLEL_SEGMENTS);
        assert_eq!(ranges.first().unwrap().0, 0);
        assert_eq!(ranges.last().unwrap().1, MIN_PARALLEL_SEGMENT_SIZE * 5);
        assert!(ranges.windows(2).all(|pair| pair[0].1 == pair[1].0));
    }

    #[test]
    fn cli_startup_recovers_pending_finalization_before_new_download() {
        let paths = TestFiles::new();
        let recovery_part = paths.root.join("recovery.bin.ndpart");
        let recovery_output = paths.root.join("recovery.bin");
        fs::create_dir_all(paths.root.join("state")).unwrap();
        let recovered_segment_id = {
            let mut storage = Storage::open(paths.database()).unwrap();
            let download_id = storage
                .create_download(
                    "https://example.test/recovery",
                    "recovery.bin",
                    paths.root.to_str().unwrap(),
                    Some(12),
                )
                .unwrap();
            let segment_id = storage.create_segment(download_id, 0, 12).unwrap();
            let part = PartFile::create(&recovery_part, 12).unwrap();
            part.write_segment(&mut storage, segment_id, 0, b"recover-file")
                .unwrap();
            storage.mark_segment_complete(segment_id).unwrap();
            let hash = Sha256::digest(b"recover-file");
            let expected: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
            storage
                .prepare_finalization(
                    segment_id,
                    recovery_part.to_str().unwrap(),
                    recovery_output.to_str().unwrap(),
                    12,
                    &expected,
                )
                .unwrap();
            segment_id
        };
        let (url, server) = serve_probe_and_download();

        run_cli([
            "download".into(),
            url.into(),
            paths.output().into_os_string(),
            "--database".into(),
            paths.database().into_os_string(),
        ])
        .unwrap();
        server.join().unwrap();

        let storage = Storage::open(paths.database()).unwrap();
        assert_eq!(
            storage.download_status(recovered_segment_id).unwrap(),
            "complete"
        );
        assert_eq!(fs::read(recovery_output).unwrap(), b"recover-file");
    }

    #[test]
    fn cli_resumes_existing_part_file_from_the_persisted_cursor() {
        let paths = TestFiles::new();
        let output_path = paths.output();
        fs::create_dir_all(output_path.parent().unwrap()).unwrap();
        let output_dir = fs::canonicalize(output_path.parent().unwrap()).unwrap();
        fs::create_dir_all(paths.database().parent().unwrap()).unwrap();
        let part_path = output_dir.join("file.bin.ndpart");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let url = format!("http://{address}/file");
        {
            let mut storage = Storage::open(paths.database()).unwrap();
            let download_id = storage
                .create_download(&url, "file.bin", output_dir.to_str().unwrap(), Some(12))
                .unwrap();
            let segment_id = storage.create_segment(download_id, 0, 12).unwrap();
            storage
                .set_source_validators(
                    download_id,
                    &SourceValidators {
                        etag: Some("\"test-v1\"".to_owned()),
                        last_modified: None,
                    },
                )
                .unwrap();
            let part = PartFile::create(&part_path, 12).unwrap();
            part.write_segment(&mut storage, segment_id, 0, b"hell")
                .unwrap();
        }

        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for expected_range in ["bytes=0-0", "bytes=4-11"] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut byte = [0; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request_has_header(&request, "range", expected_range));
                let (content_range, body): (&str, &[u8]) = match expected_range {
                    "bytes=0-0" => ("bytes 0-0/12", b"h"),
                    "bytes=4-11" => ("bytes 4-11/12", b"o world!"),
                    _ => unreachable!(),
                };
                let response = format!(
                    "HTTP/1.1 206 Partial Content\r\n\
                     Content-Range: {content_range}\r\n\
                     Content-Length: {}\r\n\
                     ETag: \"test-v1\"\r\n\
                     Connection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.write_all(body).unwrap();
                stream.flush().unwrap();
                requests.push(request);
            }
            requests
        });

        run_cli([
            "download".into(),
            url.into(),
            output_path.into_os_string(),
            "--database".into(),
            paths.database().into_os_string(),
        ])
        .unwrap();

        let requests = server.join().unwrap();
        assert!(request_has_header(&requests[1], "if-range", "\"test-v1\""));
        assert_eq!(fs::read(paths.output()).unwrap(), b"hello world!");
    }

    #[test]
    fn cli_keeps_the_saved_sha256_requirement_when_resuming_without_the_flag() {
        let paths = TestFiles::new();
        let output_path = paths.output();
        fs::create_dir_all(output_path.parent().unwrap()).unwrap();
        let output_dir = fs::canonicalize(output_path.parent().unwrap()).unwrap();
        fs::create_dir_all(paths.database().parent().unwrap()).unwrap();
        let part_path = output_dir.join("file.bin.ndpart");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let url = format!("http://{address}/file");
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for expected_range in ["bytes=0-0", "bytes=4-11"] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut byte = [0; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request_has_header(&request, "range", expected_range));
                let (content_range, body): (&str, &[u8]) = match expected_range {
                    "bytes=0-0" => ("bytes 0-0/12", b"h"),
                    "bytes=4-11" => ("bytes 4-11/12", b"o world!"),
                    _ => unreachable!(),
                };
                let response = format!(
                    "HTTP/1.1 206 Partial Content\r\n\
                     Content-Range: {content_range}\r\n\
                     Content-Length: {}\r\n\
                     ETag: \"test-v1\"\r\n\
                     Connection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.write_all(body).unwrap();
                stream.flush().unwrap();
                requests.push(request);
            }
            requests
        });
        let download_id = {
            let mut storage = Storage::open(paths.database()).unwrap();
            let download_id = storage
                .create_download(&url, "file.bin", output_dir.to_str().unwrap(), Some(12))
                .unwrap();
            storage
                .set_expected_sha256(download_id, Some(&"0".repeat(64)))
                .unwrap();
            let segment_id = storage.create_segment(download_id, 0, 12).unwrap();
            storage
                .set_source_validators(
                    download_id,
                    &SourceValidators {
                        etag: Some("\"test-v1\"".to_owned()),
                        last_modified: None,
                    },
                )
                .unwrap();
            let part = PartFile::create(&part_path, 12).unwrap();
            part.write_segment(&mut storage, segment_id, 0, b"hell")
                .unwrap();
            download_id
        };

        assert!(
            run_cli([
                "download".into(),
                url.into(),
                output_path.into_os_string(),
                "--database".into(),
                paths.database().into_os_string(),
            ])
            .is_err()
        );
        server.join().unwrap();

        let storage = Storage::open(paths.database()).unwrap();
        assert_eq!(storage.list_downloads().unwrap()[0].status, "failed");
        assert!(!paths.output().exists());
        assert!(part_path.exists());
        assert_eq!(
            storage
                .find_resumable_download(
                    fs::canonicalize(paths.output().parent().unwrap())
                        .unwrap()
                        .to_str()
                        .unwrap(),
                    "file.bin"
                )
                .unwrap()
                .unwrap()
                .id,
            download_id
        );
    }

    #[test]
    fn cli_rejects_unknown_commands() {
        assert!(run_cli(["unknown".into()]).is_err());
    }
}
