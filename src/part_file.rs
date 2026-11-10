use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sha2::{Digest, Sha256};

use crate::storage::{Storage, StorageError};

#[derive(Debug)]
pub enum PartFileError {
    Io(io::Error),
    Lock(std::fs::TryLockError),
    Storage(StorageError),
    InvalidPath,
    InvalidWrite,
    InvalidExpectedHash,
    HashMismatch { expected: String, actual: String },
    WrongFileSize { expected: u64, actual: u64 },
    DownloadNotReady,
    DownloadPaused,
    DifferentDestinationDirectory,
    RecoveryConflict,
    MissingFinalizationFile,
    LockPoisoned,
}

impl fmt::Display for PartFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "part-file I/O error: {error}"),
            Self::Lock(error) => write!(formatter, "part-file is already owned: {error}"),
            Self::Storage(error) => write!(formatter, "part-file state error: {error}"),
            Self::InvalidPath => formatter.write_str("file path cannot be stored as Unicode text"),
            Self::InvalidWrite => formatter.write_str(
                "write must begin at the durable cursor and stay within the segment and file",
            ),
            Self::InvalidExpectedHash => {
                formatter.write_str("expected SHA-256 must contain exactly 64 hexadecimal digits")
            }
            Self::HashMismatch { expected, actual } => {
                write!(
                    formatter,
                    "SHA-256 mismatch: expected {expected}, got {actual}"
                )
            }
            Self::WrongFileSize { expected, actual } => {
                write!(
                    formatter,
                    "part file has size {actual}; expected {expected}"
                )
            }
            Self::DownloadNotReady => formatter.write_str("download is not ready for finalization"),
            Self::DownloadPaused => formatter.write_str("download was paused"),
            Self::DifferentDestinationDirectory => {
                formatter.write_str("part file and final file must be in the same directory")
            }
            Self::RecoveryConflict => {
                formatter.write_str("both the part file and final file exist during recovery")
            }
            Self::MissingFinalizationFile => {
                formatter.write_str("neither pending part file nor final file exists")
            }
            Self::LockPoisoned => formatter.write_str("part-file lock was poisoned"),
        }
    }
}

impl Error for PartFileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Lock(error) => Some(error),
            Self::Storage(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for PartFileError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<std::fs::TryLockError> for PartFileError {
    fn from(error: std::fs::TryLockError) -> Self {
        Self::Lock(error)
    }
}

impl From<StorageError> for PartFileError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

pub struct PartFile {
    file: Mutex<Option<File>>,
    path: PathBuf,
    total_size: u64,
}

impl PartFile {
    pub fn create(path: impl AsRef<Path>, total_size: u64) -> Result<Self, PartFileError> {
        let path = path.as_ref().to_owned();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.try_lock()?;
        mark_sparse(&file)?;
        file.set_len(total_size)?;

        Ok(Self {
            file: Mutex::new(Some(file)),
            path,
            total_size,
        })
    }

    pub fn open_existing(path: impl AsRef<Path>, total_size: u64) -> Result<Self, PartFileError> {
        let path = path.as_ref().to_owned();
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        file.try_lock()?;
        let actual_size = file.metadata()?.len();
        if actual_size != total_size {
            return Err(PartFileError::WrongFileSize {
                expected: total_size,
                actual: actual_size,
            });
        }
        mark_sparse(&file)?;

        Ok(Self {
            file: Mutex::new(Some(file)),
            path,
            total_size,
        })
    }

    /// Writes at the durable cursor, flushes the bytes, and only then advances SQLite.
    pub fn write_segment(
        &self,
        storage: &mut Storage,
        segment_id: i64,
        offset: u64,
        data: &[u8],
    ) -> Result<u64, PartFileError> {
        let (range_start, range_end) = storage.segment_range(segment_id)?;
        if storage.download_status(segment_id)? == "paused" {
            return Err(PartFileError::DownloadPaused);
        }
        let durable_cursor = storage.segment_cursor(segment_id)?;
        let data_len = u64::try_from(data.len()).map_err(|_| PartFileError::InvalidWrite)?;
        let new_cursor = offset
            .checked_add(data_len)
            .ok_or(PartFileError::InvalidWrite)?;
        if data.is_empty()
            || offset != durable_cursor
            || offset < range_start
            || new_cursor > range_end
            || new_cursor > self.total_size
        {
            return Err(PartFileError::InvalidWrite);
        }

        let mut file_guard = self.file.lock().map_err(|_| PartFileError::LockPoisoned)?;
        let file = file_guard.as_mut().ok_or(PartFileError::DownloadNotReady)?;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(data)?;
        file.sync_all()?;
        storage.record_durable_write(segment_id, offset, data)?;

        Ok(new_cursor)
    }

    pub fn verify_durable_blocks(
        &self,
        storage: &mut Storage,
        segment_ids: &[i64],
    ) -> Result<usize, PartFileError> {
        let mut file_guard = self.file.lock().map_err(|_| PartFileError::LockPoisoned)?;
        let file = file_guard.as_mut().ok_or(PartFileError::DownloadNotReady)?;
        let mut invalid_segments = Vec::new();
        let mut buffer = vec![0; 256 * 1024];

        for segment_id in segment_ids {
            let (range_start, _) = storage.segment_range(*segment_id)?;
            let cursor = storage.segment_cursor(*segment_id)?;
            let blocks = storage.durable_blocks(*segment_id)?;
            let mut covered_until = range_start;
            let mut valid = true;
            for (start, end, expected_hash) in blocks {
                if start != covered_until || end > cursor || start >= end {
                    valid = false;
                    break;
                }
                file.seek(SeekFrom::Start(start))?;
                let mut remaining = end - start;
                let mut hasher = Sha256::new();
                while remaining > 0 {
                    let chunk_len = remaining.min(buffer.len() as u64) as usize;
                    file.read_exact(&mut buffer[..chunk_len])?;
                    hasher.update(&buffer[..chunk_len]);
                    remaining -= chunk_len as u64;
                }
                let actual_hash: [u8; 32] = hasher.finalize().into();
                if encode_sha256(&actual_hash) != expected_hash {
                    valid = false;
                    break;
                }
                covered_until = end;
            }
            if covered_until != cursor {
                valid = false;
            }
            if !valid {
                invalid_segments.push(*segment_id);
            }
        }
        drop(file_guard);

        for segment_id in &invalid_segments {
            storage.reset_segment_for_redownload(*segment_id)?;
        }
        Ok(invalid_segments.len())
    }

    pub fn finalize(
        self,
        storage: &mut Storage,
        segment_id: i64,
        destination: impl AsRef<Path>,
        expected_sha256: Option<&str>,
    ) -> Result<[u8; 32], PartFileError> {
        if !storage.download_ready_to_finalize(segment_id)? {
            return Err(PartFileError::DownloadNotReady);
        }
        let destination = destination.as_ref();
        ensure_same_directory(&self.path, destination)?;
        let expected = expected_sha256.map(parse_sha256).transpose()?;
        if destination.exists() {
            return Err(PartFileError::Io(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "final destination already exists",
            )));
        }

        let mut file_guard = self.file.lock().map_err(|_| PartFileError::LockPoisoned)?;
        let file = file_guard.as_mut().ok_or(PartFileError::DownloadNotReady)?;
        let actual_size = file.metadata()?.len();
        if actual_size != self.total_size {
            return Err(PartFileError::WrongFileSize {
                expected: self.total_size,
                actual: actual_size,
            });
        }
        file.sync_all()?;
        file.seek(SeekFrom::Start(0))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; 256 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        let digest: [u8; 32] = hasher.finalize().into();
        let actual_hash = encode_sha256(&digest);
        if let Some(expected) = expected
            && expected != digest
        {
            return Err(PartFileError::HashMismatch {
                expected: encode_sha256(&expected),
                actual: actual_hash,
            });
        }

        let part_path = self.path.to_str().ok_or(PartFileError::InvalidPath)?;
        let final_path = destination.to_str().ok_or(PartFileError::InvalidPath)?;
        if !storage.prepare_finalization(
            segment_id,
            part_path,
            final_path,
            self.total_size,
            &actual_hash,
        )? {
            return Err(PartFileError::DownloadNotReady);
        }
        mark_of_the_web(&self.path)?;
        let file = file_guard.take().ok_or(PartFileError::DownloadNotReady)?;
        drop(file_guard);
        drop(file);
        atomic_promote(&self.path, destination)?;

        if !storage.mark_download_complete(segment_id)? {
            return Err(PartFileError::DownloadNotReady);
        }
        Ok(digest)
    }

    pub fn recover_pending_finalizations(storage: &mut Storage) -> Result<usize, PartFileError> {
        let pending = storage.pending_finalizations()?;
        let mut recovered = 0;
        for intent in pending {
            let part_path = PathBuf::from(&intent.part_path);
            let final_path = PathBuf::from(&intent.final_path);
            ensure_same_directory(&part_path, &final_path)?;
            let expected = parse_sha256(&intent.sha256)?;

            if final_path.exists() {
                if part_path.exists() {
                    return Err(PartFileError::RecoveryConflict);
                }
                verify_file(&final_path, intent.size, &expected)?;
            } else if part_path.exists() {
                let part_file = Self::open_existing(&part_path, intent.size)?;
                part_file.verify_contents(&expected)?;
                mark_of_the_web(&part_path)?;
                let file = part_file
                    .file
                    .into_inner()
                    .map_err(|_| PartFileError::LockPoisoned)?
                    .ok_or(PartFileError::DownloadNotReady)?;
                drop(file);
                atomic_promote(&part_path, &final_path)?;
            } else {
                return Err(PartFileError::MissingFinalizationFile);
            }

            if !storage.mark_download_complete(intent.segment_id)? {
                return Err(PartFileError::DownloadNotReady);
            }
            recovered += 1;
        }
        Ok(recovered)
    }

    fn verify_contents(&self, expected: &[u8; 32]) -> Result<(), PartFileError> {
        let mut file_guard = self.file.lock().map_err(|_| PartFileError::LockPoisoned)?;
        let file = file_guard.as_mut().ok_or(PartFileError::DownloadNotReady)?;
        verify_file_handle(file, self.total_size, expected)
    }
}

fn verify_file(
    path: &Path,
    expected_size: u64,
    expected_hash: &[u8; 32],
) -> Result<(), PartFileError> {
    let file = OpenOptions::new().read(true).open(path)?;
    verify_file_handle(&file, expected_size, expected_hash)
}

fn verify_file_handle(
    file: &File,
    expected_size: u64,
    expected_hash: &[u8; 32],
) -> Result<(), PartFileError> {
    let actual_size = file.metadata()?.len();
    if actual_size != expected_size {
        return Err(PartFileError::WrongFileSize {
            expected: expected_size,
            actual: actual_size,
        });
    }
    let mut file = file;
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 256 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual: [u8; 32] = hasher.finalize().into();
    if &actual != expected_hash {
        return Err(PartFileError::HashMismatch {
            expected: encode_sha256(expected_hash),
            actual: encode_sha256(&actual),
        });
    }
    Ok(())
}

fn ensure_same_directory(source: &Path, destination: &Path) -> Result<(), PartFileError> {
    let source_parent = fs::canonicalize(parent_or_current(source))?;
    let destination_parent = fs::canonicalize(parent_or_current(destination))?;
    if source_parent != destination_parent || destination.file_name().is_none() {
        return Err(PartFileError::DifferentDestinationDirectory);
    }
    Ok(())
}

fn parent_or_current(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn parse_sha256(value: &str) -> Result<[u8; 32], PartFileError> {
    if value.len() != 64 {
        return Err(PartFileError::InvalidExpectedHash);
    }
    let mut digest = [0; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        let pair = &value.as_bytes()[index * 2..index * 2 + 2];
        let high = hex_nibble(pair[0]).ok_or(PartFileError::InvalidExpectedHash)?;
        let low = hex_nibble(pair[1]).ok_or(PartFileError::InvalidExpectedHash)?;
        *byte = high << 4 | low;
    }
    Ok(digest)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn encode_sha256(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(windows)]
fn atomic_promote(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn mark_of_the_web(path: &Path) -> io::Result<()> {
    let mut stream_path = path.as_os_str().to_owned();
    stream_path.push(":Zone.Identifier");
    let mut stream = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(stream_path)?;
    stream.write_all(b"[ZoneTransfer]\r\nZoneId=3\r\n")?;
    stream.sync_all()
}

#[cfg(not(windows))]
fn mark_of_the_web(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn atomic_promote(source: &Path, destination: &Path) -> io::Result<()> {
    fs::hard_link(source, destination)?;
    fs::remove_file(source)
}

#[cfg(windows)]
fn mark_sparse(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;

    let mut bytes_returned = 0u32;
    let result = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            null(),
            0,
            null_mut(),
            0,
            &mut bytes_returned,
            null_mut(),
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
fn mark_sparse(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Seek, SeekFrom};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::storage::Storage;

    use super::{PartFile, PartFileError, encode_sha256, parse_sha256};

    static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(0);

    struct TestFiles {
        part_path: PathBuf,
        final_path: PathBuf,
        database_path: PathBuf,
    }

    impl TestFiles {
        fn new() -> Self {
            let id = NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed);
            let unique = format!("raja-part-test-{}-{id}", std::process::id());
            let temp = std::env::temp_dir();
            Self {
                part_path: temp.join(format!("{unique}.ndpart")),
                final_path: temp.join(format!("{unique}.bin")),
                database_path: temp.join(format!("{unique}.db")),
            }
        }

        fn storage(&self) -> Storage {
            Storage::open(&self.database_path).unwrap()
        }

        fn create_segment(&self, storage: &mut Storage) -> i64 {
            let download_id = storage
                .create_download(
                    "https://example.test/file",
                    "file.bin",
                    "downloads",
                    Some(12),
                )
                .unwrap();
            storage.create_segment(download_id, 0, 12).unwrap()
        }
    }

    impl Drop for TestFiles {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.part_path);
            let _ = fs::remove_file(&self.final_path);
            let _ = fs::remove_file(&self.database_path);
            let _ = fs::remove_file(self.database_path.with_extension("db-wal"));
            let _ = fs::remove_file(self.database_path.with_extension("db-shm"));
        }
    }

    #[test]
    fn flushes_written_bytes_before_persisting_the_cursor() {
        let paths = TestFiles::new();
        let mut storage = paths.storage();
        let segment_id = paths.create_segment(&mut storage);
        let part_file = PartFile::create(&paths.part_path, 12).unwrap();

        assert_eq!(
            part_file
                .write_segment(&mut storage, segment_id, 0, b"hello ")
                .unwrap(),
            6
        );
        assert_eq!(storage.segment_cursor(segment_id).unwrap(), 6);

        drop(part_file);
        let mut file = fs::File::open(&paths.part_path).unwrap();
        let mut bytes = [0; 6];
        file.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"hello ");
    }

    #[test]
    fn rejects_nonsequential_and_out_of_range_writes_without_advancing_cursor() {
        let paths = TestFiles::new();
        let mut storage = paths.storage();
        let segment_id = paths.create_segment(&mut storage);
        let part_file = PartFile::create(&paths.part_path, 12).unwrap();

        assert!(matches!(
            part_file.write_segment(&mut storage, segment_id, 1, b"x"),
            Err(PartFileError::InvalidWrite)
        ));
        assert!(matches!(
            part_file.write_segment(&mut storage, segment_id, 0, b"this is too long"),
            Err(PartFileError::InvalidWrite)
        ));
        assert_eq!(storage.segment_cursor(segment_id).unwrap(), 0);
    }

    #[test]
    fn refuses_to_write_after_a_download_is_paused() {
        let paths = TestFiles::new();
        let mut storage = paths.storage();
        let download_id = storage
            .create_download(
                "https://example.test/file",
                "file.bin",
                "downloads",
                Some(12),
            )
            .unwrap();
        let segment_id = storage.create_segment(download_id, 0, 12).unwrap();
        let part_file = PartFile::create(&paths.part_path, 12).unwrap();
        storage.mark_download_active(download_id).unwrap();
        storage.pause_download(download_id).unwrap();

        assert!(matches!(
            part_file.write_segment(&mut storage, segment_id, 0, b"hello "),
            Err(PartFileError::DownloadPaused)
        ));
        assert_eq!(storage.segment_cursor(segment_id).unwrap(), 0);
    }

    #[test]
    fn reopens_part_file_and_resumes_from_the_durable_cursor() {
        let paths = TestFiles::new();
        let segment_id = {
            let mut storage = paths.storage();
            let segment_id = paths.create_segment(&mut storage);
            let part_file = PartFile::create(&paths.part_path, 12).unwrap();
            part_file
                .write_segment(&mut storage, segment_id, 0, b"hello ")
                .unwrap();
            segment_id
        };

        let mut storage = paths.storage();
        let part_file = PartFile::open_existing(&paths.part_path, 12).unwrap();
        part_file
            .write_segment(&mut storage, segment_id, 6, b"world!")
            .unwrap();
        storage.mark_segment_complete(segment_id).unwrap();
        drop(part_file);

        let mut file = fs::File::open(&paths.part_path).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(&bytes[..12], b"hello world!");
        assert_eq!(storage.segment_cursor(segment_id).unwrap(), 12);
        assert_eq!(storage.segment_status(segment_id).unwrap(), "complete");
    }

    #[test]
    fn resets_a_segment_when_a_durable_chunk_hash_does_not_match() {
        let paths = TestFiles::new();
        let segment_id = {
            let mut storage = paths.storage();
            let segment_id = paths.create_segment(&mut storage);
            let part_file = PartFile::create(&paths.part_path, 12).unwrap();
            part_file
                .write_segment(&mut storage, segment_id, 0, b"hello ")
                .unwrap();
            segment_id
        };
        {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .open(&paths.part_path)
                .unwrap();
            file.seek(SeekFrom::Start(1)).unwrap();
            std::io::Write::write_all(&mut file, b"X").unwrap();
            file.sync_all().unwrap();
        }

        let mut storage = paths.storage();
        let part_file = PartFile::open_existing(&paths.part_path, 12).unwrap();
        assert_eq!(
            part_file
                .verify_durable_blocks(&mut storage, &[segment_id])
                .unwrap(),
            1
        );
        assert_eq!(storage.segment_cursor(segment_id).unwrap(), 0);
        assert_eq!(storage.segment_status(segment_id).unwrap(), "pending");
        assert!(storage.durable_blocks(segment_id).unwrap().is_empty());
    }

    #[test]
    fn resets_legacy_cursor_without_durable_hashes_for_safe_redownload() {
        let paths = TestFiles::new();
        let mut storage = paths.storage();
        let segment_id = paths.create_segment(&mut storage);
        let part_file = PartFile::create(&paths.part_path, 12).unwrap();
        storage.record_durable_cursor(segment_id, 6).unwrap();

        assert_eq!(
            part_file
                .verify_durable_blocks(&mut storage, &[segment_id])
                .unwrap(),
            1
        );
        assert_eq!(storage.segment_cursor(segment_id).unwrap(), 0);
    }

    #[test]
    fn refuses_to_open_a_part_file_while_another_owner_holds_it() {
        let paths = TestFiles::new();
        let _owner = PartFile::create(&paths.part_path, 12).unwrap();

        assert!(matches!(
            PartFile::open_existing(&paths.part_path, 12),
            Err(PartFileError::Lock(_))
        ));
    }

    #[test]
    fn verifies_sha256_and_promotes_before_marking_download_complete() {
        let paths = TestFiles::new();
        let mut storage = paths.storage();
        let segment_id = paths.create_segment(&mut storage);
        let part_file = PartFile::create(&paths.part_path, 12).unwrap();
        part_file
            .write_segment(&mut storage, segment_id, 0, b"hello world!")
            .unwrap();
        storage.mark_segment_complete(segment_id).unwrap();

        assert_eq!(storage.download_status(segment_id).unwrap(), "queued");
        let digest = part_file
            .finalize(
                &mut storage,
                segment_id,
                &paths.final_path,
                Some("7509e5bda0c762d2bac7f90d758b5b2263fa01ccbc542ab5e3df163be08e6ca9"),
            )
            .unwrap();

        assert_eq!(
            encode_sha256(&digest),
            "7509e5bda0c762d2bac7f90d758b5b2263fa01ccbc542ab5e3df163be08e6ca9"
        );
        assert_eq!(fs::read(&paths.final_path).unwrap(), b"hello world!");
        assert!(!paths.part_path.exists());
        assert_eq!(storage.download_status(segment_id).unwrap(), "complete");
        #[cfg(windows)]
        assert_eq!(
            fs::read_to_string(format!("{}:Zone.Identifier", paths.final_path.display())).unwrap(),
            "[ZoneTransfer]\r\nZoneId=3\r\n"
        );
    }

    #[test]
    fn recovers_crash_after_finalization_intent_before_rename() {
        let paths = TestFiles::new();
        let segment_id = {
            let mut storage = paths.storage();
            let segment_id = paths.create_segment(&mut storage);
            let part_file = PartFile::create(&paths.part_path, 12).unwrap();
            part_file
                .write_segment(&mut storage, segment_id, 0, b"hello world!")
                .unwrap();
            storage.mark_segment_complete(segment_id).unwrap();
            assert!(
                storage
                    .prepare_finalization(
                        segment_id,
                        paths.part_path.to_str().unwrap(),
                        paths.final_path.to_str().unwrap(),
                        12,
                        "7509e5bda0c762d2bac7f90d758b5b2263fa01ccbc542ab5e3df163be08e6ca9"
                    )
                    .unwrap()
            );
            segment_id
        };

        let mut storage = paths.storage();
        assert_eq!(
            PartFile::recover_pending_finalizations(&mut storage).unwrap(),
            1
        );
        assert!(!paths.part_path.exists());
        assert_eq!(fs::read(&paths.final_path).unwrap(), b"hello world!");
        assert_eq!(storage.download_status(segment_id).unwrap(), "complete");
        assert!(storage.pending_finalizations().unwrap().is_empty());
    }

    #[test]
    fn recovers_crash_after_rename_before_database_commit() {
        let paths = TestFiles::new();
        let segment_id;
        {
            let mut storage = paths.storage();
            segment_id = paths.create_segment(&mut storage);
            let part_file = PartFile::create(&paths.part_path, 12).unwrap();
            part_file
                .write_segment(&mut storage, segment_id, 0, b"hello world!")
                .unwrap();
            storage.mark_segment_complete(segment_id).unwrap();
            storage
                .prepare_finalization(
                    segment_id,
                    paths.part_path.to_str().unwrap(),
                    paths.final_path.to_str().unwrap(),
                    12,
                    "7509e5bda0c762d2bac7f90d758b5b2263fa01ccbc542ab5e3df163be08e6ca9",
                )
                .unwrap();
        }
        fs::rename(&paths.part_path, &paths.final_path).unwrap();

        let mut storage = paths.storage();
        assert_eq!(
            PartFile::recover_pending_finalizations(&mut storage).unwrap(),
            1
        );
        assert_eq!(fs::read(&paths.final_path).unwrap(), b"hello world!");
        assert_eq!(storage.download_status(segment_id).unwrap(), "complete");
    }

    #[test]
    fn refuses_recovery_if_the_final_file_hash_does_not_match() {
        let paths = TestFiles::new();
        let segment_id;
        {
            let mut storage = paths.storage();
            segment_id = paths.create_segment(&mut storage);
            let part_file = PartFile::create(&paths.part_path, 12).unwrap();
            part_file
                .write_segment(&mut storage, segment_id, 0, b"hello world!")
                .unwrap();
            storage.mark_segment_complete(segment_id).unwrap();
            storage
                .prepare_finalization(
                    segment_id,
                    paths.part_path.to_str().unwrap(),
                    paths.final_path.to_str().unwrap(),
                    12,
                    "7509e5bda0c762d2bac7f90d758b5b2263fa01ccbc542ab5e3df163be08e6ca9",
                )
                .unwrap();
        }
        fs::rename(&paths.part_path, &paths.final_path).unwrap();
        fs::write(&paths.final_path, b"tampereddata").unwrap();

        let mut storage = paths.storage();
        assert!(matches!(
            PartFile::recover_pending_finalizations(&mut storage),
            Err(PartFileError::HashMismatch { .. })
        ));
        assert_eq!(storage.download_status(segment_id).unwrap(), "queued");
    }

    #[test]
    fn hash_mismatch_keeps_part_file_and_does_not_mark_download_complete() {
        let paths = TestFiles::new();
        let mut storage = paths.storage();
        let segment_id = paths.create_segment(&mut storage);
        let part_file = PartFile::create(&paths.part_path, 12).unwrap();
        part_file
            .write_segment(&mut storage, segment_id, 0, b"hello world!")
            .unwrap();
        storage.mark_segment_complete(segment_id).unwrap();

        assert!(matches!(
            part_file.finalize(
                &mut storage,
                segment_id,
                &paths.final_path,
                Some(&"0".repeat(64))
            ),
            Err(PartFileError::HashMismatch { .. })
        ));
        assert!(paths.part_path.exists());
        assert!(!paths.final_path.exists());
        assert_eq!(storage.download_status(segment_id).unwrap(), "queued");
    }

    #[test]
    fn refuses_to_overwrite_an_existing_destination() {
        let paths = TestFiles::new();
        let mut storage = paths.storage();
        let segment_id = paths.create_segment(&mut storage);
        let part_file = PartFile::create(&paths.part_path, 12).unwrap();
        part_file
            .write_segment(&mut storage, segment_id, 0, b"hello world!")
            .unwrap();
        storage.mark_segment_complete(segment_id).unwrap();
        fs::write(&paths.final_path, b"keep existing").unwrap();

        assert!(matches!(
            part_file.finalize(&mut storage, segment_id, &paths.final_path, None),
            Err(PartFileError::Io(_))
        ));
        assert_eq!(fs::read(&paths.final_path).unwrap(), b"keep existing");
        assert!(paths.part_path.exists());
        assert_eq!(storage.download_status(segment_id).unwrap(), "queued");
    }

    #[test]
    fn refuses_finalization_before_all_segments_are_verified() {
        let paths = TestFiles::new();
        let mut storage = paths.storage();
        let segment_id = paths.create_segment(&mut storage);
        let part_file = PartFile::create(&paths.part_path, 12).unwrap();

        assert!(matches!(
            part_file.finalize(&mut storage, segment_id, &paths.final_path, None),
            Err(PartFileError::DownloadNotReady)
        ));
        assert!(paths.part_path.exists());
        assert!(!paths.final_path.exists());
    }

    #[test]
    fn rejects_non_ascii_expected_hash_without_panicking() {
        assert!(matches!(
            parse_sha256(&"é".repeat(32)),
            Err(PartFileError::InvalidExpectedHash)
        ));
    }
}
