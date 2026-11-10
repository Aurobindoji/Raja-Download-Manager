use std::error::Error;
use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: i64 = 5;
type RetryDownloadDetails = (String, String, String, Option<String>, String, bool, String);
const SCHEMA: &str = "
CREATE TABLE downloads (
    id            INTEGER PRIMARY KEY,
    source_url    TEXT NOT NULL CHECK (length(source_url) > 0),
    file_name     TEXT NOT NULL CHECK (length(file_name) > 0),
    output_dir    TEXT NOT NULL CHECK (length(output_dir) > 0),
    total_size    INTEGER CHECK (total_size IS NULL OR total_size >= 0),
    expected_sha256 TEXT CHECK (expected_sha256 IS NULL OR length(expected_sha256) = 64),
    download_kind TEXT NOT NULL DEFAULT 'http'
                  CHECK (download_kind IN ('http', 'video')),
    external_bytes INTEGER NOT NULL DEFAULT 0 CHECK (external_bytes >= 0),
    status        TEXT NOT NULL DEFAULT 'queued'
                  CHECK (status IN ('queued', 'active', 'paused', 'complete', 'failed')),
    category      TEXT,
    queue_position INTEGER,
    created_at    INTEGER NOT NULL,
    completed_at  INTEGER,
    last_attempt_at INTEGER,
    finalize_pending INTEGER NOT NULL DEFAULT 0 CHECK (finalize_pending IN (0, 1)),
    part_path     TEXT,
    final_path    TEXT,
    final_size    INTEGER CHECK (final_size IS NULL OR final_size >= 0),
    final_sha256  TEXT
);

CREATE INDEX downloads_status_queue ON downloads(status, queue_position);

CREATE TABLE sources (
    download_id   INTEGER NOT NULL REFERENCES downloads(id) ON DELETE CASCADE,
    ordinal       INTEGER NOT NULL CHECK (ordinal >= 0),
    url           TEXT NOT NULL CHECK (length(url) > 0),
    etag          TEXT,
    last_modified TEXT,
    PRIMARY KEY (download_id, ordinal)
);

CREATE TABLE segments (
    id            INTEGER PRIMARY KEY,
    download_id   INTEGER NOT NULL REFERENCES downloads(id) ON DELETE CASCADE,
    range_start   INTEGER NOT NULL CHECK (range_start >= 0),
    range_end     INTEGER NOT NULL CHECK (range_end > range_start),
    cursor        INTEGER NOT NULL,
    status        TEXT NOT NULL DEFAULT 'pending'
                  CHECK (status IN ('pending', 'active', 'complete')),
    CHECK (cursor >= range_start AND cursor <= range_end),
    UNIQUE (download_id, range_start, range_end)
);

CREATE INDEX segments_download ON segments(download_id, range_start);

CREATE TRIGGER segments_no_overlap
BEFORE INSERT ON segments
WHEN EXISTS (
    SELECT 1 FROM segments
    WHERE download_id = NEW.download_id
      AND range_start < NEW.range_end
      AND range_end > NEW.range_start
)
BEGIN
    SELECT RAISE(ABORT, 'segment ranges overlap');
END;

CREATE TABLE blocks (
    segment_id    INTEGER NOT NULL REFERENCES segments(id) ON DELETE CASCADE,
    block_index   INTEGER NOT NULL CHECK (block_index >= 0),
    range_start   INTEGER NOT NULL CHECK (range_start >= 0),
    range_end     INTEGER NOT NULL CHECK (range_end > range_start),
    hash          TEXT NOT NULL CHECK (length(hash) > 0),
    PRIMARY KEY (segment_id, block_index)
);

CREATE TABLE events (
    id            INTEGER PRIMARY KEY,
    download_id   INTEGER REFERENCES downloads(id) ON DELETE CASCADE,
    event_type    TEXT NOT NULL CHECK (length(event_type) > 0),
    message       TEXT NOT NULL,
    created_at    INTEGER NOT NULL
);

CREATE INDEX events_download ON events(download_id, created_at);

CREATE TABLE hosts (
    host              TEXT PRIMARY KEY,
    max_connections   INTEGER CHECK (max_connections IS NULL OR max_connections > 0),
    max_request_size  INTEGER CHECK (max_request_size IS NULL OR max_request_size > 0),
    updated_at        INTEGER NOT NULL
);

CREATE TABLE settings (
    key   TEXT PRIMARY KEY CHECK (length(key) > 0),
    value TEXT NOT NULL
);
";

#[derive(Debug)]
pub enum StorageError {
    Database(rusqlite::Error),
    UnsupportedSchemaVersion(i64),
    UnsupportedJournalMode(String),
    ValueOutOfRange(&'static str),
    InvalidStoredInteger(&'static str),
    InvalidField(&'static str),
    InvalidRange,
    DownloadNotFound(i64),
    DownloadNotForgettable(i64),
    DownloadNotRetryable(i64),
    DownloadNotActive(i64),
    InvalidExpectedHash,
    ExpectedHashConflict(i64),
    RangeExceedsDownloadSize,
    InvalidCursor,
    CursorRegression,
    SegmentNotFound(i64),
    SegmentNotComplete(i64),
    FinalizationConflict(i64),
    InvalidFinalization,
    MultipleDownloadsForOutput,
    SourceChanged,
    ClockBeforeUnixEpoch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RetryDownload {
    pub source_url: String,
    pub file_name: String,
    pub output_dir: String,
    pub expected_sha256: Option<String>,
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "database error: {error}"),
            Self::UnsupportedSchemaVersion(version) => {
                write!(
                    formatter,
                    "database schema version {version} is newer than supported"
                )
            }
            Self::UnsupportedJournalMode(mode) => {
                write!(
                    formatter,
                    "SQLite did not enable WAL mode (reported {mode})"
                )
            }
            Self::ValueOutOfRange(field) => {
                write!(
                    formatter,
                    "{field} exceeds SQLite's supported integer range"
                )
            }
            Self::InvalidStoredInteger(field) => {
                write!(formatter, "database contains an invalid {field} value")
            }
            Self::InvalidField(field) => write!(formatter, "{field} must not be empty"),
            Self::InvalidRange => formatter.write_str("segment range must be non-empty"),
            Self::DownloadNotFound(id) => write!(formatter, "download {id} was not found"),
            Self::DownloadNotForgettable(id) => {
                write!(
                    formatter,
                    "download {id} is not complete and cannot be removed from history"
                )
            }
            Self::DownloadNotRetryable(id) => {
                write!(formatter, "download {id} is complete or cannot be retried")
            }
            Self::DownloadNotActive(id) => {
                write!(
                    formatter,
                    "download {id} is not active and cannot be paused"
                )
            }
            Self::InvalidExpectedHash => {
                formatter.write_str("expected SHA-256 must contain exactly 64 hexadecimal digits")
            }
            Self::ExpectedHashConflict(id) => {
                write!(
                    formatter,
                    "download {id} already has a different expected SHA-256"
                )
            }
            Self::RangeExceedsDownloadSize => {
                formatter.write_str("segment range exceeds the known download size")
            }
            Self::InvalidCursor => formatter.write_str("segment cursor is outside its byte range"),
            Self::CursorRegression => {
                formatter.write_str("durable segment cursor cannot move backwards")
            }
            Self::SegmentNotFound(id) => write!(formatter, "segment {id} was not found"),
            Self::SegmentNotComplete(id) => {
                write!(formatter, "segment {id} has not been fully verified")
            }
            Self::FinalizationConflict(id) => {
                write!(
                    formatter,
                    "download {id} already has a different pending finalization"
                )
            }
            Self::InvalidFinalization => {
                formatter.write_str("download has no valid pending finalization record")
            }
            Self::MultipleDownloadsForOutput => {
                formatter.write_str("multiple unfinished downloads target the same output file")
            }
            Self::SourceChanged => {
                formatter.write_str("download source validators changed during transfer")
            }
            Self::ClockBeforeUnixEpoch => {
                formatter.write_str("system clock is earlier than the Unix epoch")
            }
        }
    }
}

impl Error for StorageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for StorageError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

pub struct Storage {
    connection: Connection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadSummary {
    pub id: i64,
    pub file_name: String,
    pub output_path: std::path::PathBuf,
    pub status: String,
    pub download_kind: String,
    pub source_url: String,
    pub total_size: Option<u64>,
    pub last_attempt_at: Option<i64>,
    pub durable_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingFinalization {
    pub segment_id: i64,
    pub part_path: String,
    pub final_path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ResumableDownload {
    pub id: i64,
    pub source_url: String,
    pub total_size: Option<u64>,
    pub segment_id: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceValidators {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

impl SourceValidators {
    pub(crate) fn if_range_value(&self) -> Option<&str> {
        self.etag
            .as_deref()
            .filter(|etag| !etag.trim_start().starts_with("W/"))
            .or(self.last_modified.as_deref())
    }

    pub(crate) fn matches(&self, response: &Self) -> bool {
        self.etag
            .as_ref()
            .is_none_or(|etag| response.etag.as_ref() == Some(etag))
            && self
                .last_modified
                .as_ref()
                .is_none_or(|modified| response.last_modified.as_ref() == Some(modified))
    }
}

impl Storage {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;

        let journal_mode: String =
            connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            return Err(StorageError::UnsupportedJournalMode(journal_mode));
        }
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;

        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 =
            transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => {
                transaction.execute_batch(SCHEMA)?;
                transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            1 => {
                transaction.execute_batch(
                    "ALTER TABLE downloads ADD COLUMN finalize_pending INTEGER NOT NULL DEFAULT 0
                        CHECK (finalize_pending IN (0, 1));
                     ALTER TABLE downloads ADD COLUMN part_path TEXT;
                     ALTER TABLE downloads ADD COLUMN final_path TEXT;
                     ALTER TABLE downloads ADD COLUMN final_size INTEGER
                        CHECK (final_size IS NULL OR final_size >= 0);
                     ALTER TABLE downloads ADD COLUMN final_sha256 TEXT;
                     ALTER TABLE downloads ADD COLUMN last_attempt_at INTEGER;
                     ALTER TABLE downloads ADD COLUMN download_kind TEXT NOT NULL DEFAULT 'http'
                        CHECK (download_kind IN ('http', 'video'));
                     ALTER TABLE downloads ADD COLUMN external_bytes INTEGER NOT NULL DEFAULT 0
                        CHECK (external_bytes >= 0);",
                )?;
                transaction.execute_batch(
                    "ALTER TABLE downloads ADD COLUMN expected_sha256 TEXT
                        CHECK (expected_sha256 IS NULL OR length(expected_sha256) = 64);",
                )?;
                transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            2 => {
                transaction.execute_batch(
                    "ALTER TABLE downloads ADD COLUMN expected_sha256 TEXT
                        CHECK (expected_sha256 IS NULL OR length(expected_sha256) = 64);
                     ALTER TABLE downloads ADD COLUMN last_attempt_at INTEGER;
                     ALTER TABLE downloads ADD COLUMN download_kind TEXT NOT NULL DEFAULT 'http'
                        CHECK (download_kind IN ('http', 'video'));
                     ALTER TABLE downloads ADD COLUMN external_bytes INTEGER NOT NULL DEFAULT 0
                        CHECK (external_bytes >= 0);",
                )?;
                transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            3 => {
                transaction.execute_batch(
                    "ALTER TABLE downloads ADD COLUMN last_attempt_at INTEGER;
                         ALTER TABLE downloads ADD COLUMN download_kind TEXT NOT NULL DEFAULT 'http'
                            CHECK (download_kind IN ('http', 'video'));
                         ALTER TABLE downloads ADD COLUMN external_bytes INTEGER NOT NULL DEFAULT 0
                            CHECK (external_bytes >= 0);",
                )?;
                transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            4 => {
                transaction.execute_batch(
                    "ALTER TABLE downloads ADD COLUMN download_kind TEXT NOT NULL DEFAULT 'http'
                        CHECK (download_kind IN ('http', 'video'));
                     ALTER TABLE downloads ADD COLUMN external_bytes INTEGER NOT NULL DEFAULT 0
                        CHECK (external_bytes >= 0);",
                )?;
                transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            SCHEMA_VERSION => {}
            newer => return Err(StorageError::UnsupportedSchemaVersion(newer)),
        }
        transaction.commit()?;

        Ok(Self { connection })
    }

    pub fn create_download(
        &mut self,
        source_url: &str,
        file_name: &str,
        output_dir: &str,
        total_size: Option<u64>,
    ) -> Result<i64, StorageError> {
        validate_nonempty(source_url, "source URL")?;
        validate_nonempty(file_name, "file name")?;
        validate_nonempty(output_dir, "output directory")?;
        let total_size = total_size
            .map(|size| to_sqlite_integer(size, "download size"))
            .transpose()?;
        let created_at = unix_timestamp()?;

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO downloads (source_url, file_name, output_dir, total_size, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![source_url, file_name, output_dir, total_size, created_at],
        )?;
        let download_id = transaction.last_insert_rowid();
        transaction.execute(
            "INSERT INTO sources (download_id, ordinal, url) VALUES (?1, 0, ?2)",
            params![download_id, source_url],
        )?;
        transaction.commit()?;

        Ok(download_id)
    }

    pub fn create_video_download(
        &mut self,
        source_url: &str,
        file_name: &str,
        output_dir: &str,
    ) -> Result<i64, StorageError> {
        validate_nonempty(source_url, "source URL")?;
        validate_nonempty(file_name, "file name")?;
        validate_nonempty(output_dir, "output directory")?;
        let created_at = unix_timestamp()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO downloads (
                source_url, file_name, output_dir, download_kind, created_at
             ) VALUES (?1, ?2, ?3, 'video', ?4)",
            params![source_url, file_name, output_dir, created_at],
        )?;
        let download_id = transaction.last_insert_rowid();
        transaction.execute(
            "INSERT INTO sources (download_id, ordinal, url) VALUES (?1, 0, ?2)",
            params![download_id, source_url],
        )?;
        transaction.commit()?;
        Ok(download_id)
    }

    pub fn update_video_download(
        &mut self,
        download_id: i64,
        status: &str,
        downloaded_bytes: Option<u64>,
        total_bytes: Option<u64>,
        message: Option<&str>,
    ) -> Result<(), StorageError> {
        if !matches!(status, "active" | "paused" | "failed" | "complete") {
            return Err(StorageError::InvalidField("video download status"));
        }
        let downloaded_bytes = downloaded_bytes
            .map(|bytes| to_sqlite_integer(bytes, "downloaded bytes"))
            .transpose()?;
        let total_bytes = total_bytes
            .map(|size| to_sqlite_integer(size, "download size"))
            .transpose()?;
        let now = unix_timestamp()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_status: Option<String> = transaction
            .query_row(
                "SELECT status FROM downloads
                 WHERE id = ?1 AND download_kind = 'video'",
                [download_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(current_status) = current_status else {
            return Err(StorageError::DownloadNotFound(download_id));
        };
        let valid_transition = match status {
            "active" => current_status != "complete",
            "paused" => current_status == "active",
            "failed" => current_status != "complete",
            "complete" => current_status == "active",
            _ => false,
        };
        if !valid_transition {
            return Err(StorageError::InvalidField("video download transition"));
        }
        let updated = transaction.execute(
            "UPDATE downloads
             SET status = ?1, external_bytes = COALESCE(?2, external_bytes),
                 total_size = COALESCE(?3, total_size),
                 last_attempt_at = CASE
                     WHEN ?1 = 'active' AND status != 'active' THEN ?4
                     ELSE last_attempt_at END,
                 completed_at = CASE WHEN ?1 = 'complete' THEN ?4 ELSE completed_at END
             WHERE id = ?5 AND download_kind = 'video'",
            params![status, downloaded_bytes, total_bytes, now, download_id],
        )?;
        if updated == 0 {
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM downloads WHERE id = ?1 AND download_kind = 'video')",
                [download_id],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(StorageError::DownloadNotFound(download_id));
            }
        }
        if let Some(message) = message {
            transaction.execute(
                "INSERT INTO events (download_id, event_type, message, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![download_id, status, message, now],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn update_video_output_path(
        &mut self,
        download_id: i64,
        output_path: &Path,
    ) -> Result<(), StorageError> {
        let file_name = output_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(StorageError::InvalidField("video output file name"))?;
        let output_dir = output_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_str()
            .ok_or(StorageError::InvalidField("video output directory"))?;
        validate_nonempty(file_name, "video output file name")?;
        validate_nonempty(output_dir, "video output directory")?;
        let updated = self.connection.execute(
            "UPDATE downloads SET file_name = ?1, output_dir = ?2
             WHERE id = ?3 AND download_kind = 'video'",
            params![file_name, output_dir, download_id],
        )?;
        if updated == 0 {
            return Err(StorageError::DownloadNotFound(download_id));
        }
        Ok(())
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>, StorageError> {
        validate_nonempty(key, "setting key")?;
        self.connection
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(StorageError::from)
    }

    pub fn set_setting(&mut self, key: &str, value: &str) -> Result<(), StorageError> {
        validate_nonempty(key, "setting key")?;
        validate_nonempty(value, "setting value")?;
        self.connection.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn list_downloads(&self) -> Result<Vec<DownloadSummary>, StorageError> {
        let mut statement = self.connection.prepare(
            "SELECT downloads.id, downloads.file_name, downloads.output_dir,
                    downloads.status,
                    downloads.download_kind, downloads.source_url,
                    downloads.total_size,
                    downloads.last_attempt_at,
                    CASE WHEN downloads.download_kind = 'video' THEN downloads.external_bytes
                         ELSE COALESCE((
                             SELECT sum(segments.cursor - segments.range_start)
                             FROM segments WHERE segments.download_id = downloads.id
                         ), 0) END
             FROM downloads ORDER BY downloads.id DESC LIMIT 100",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, i64>(8)?,
            ))
        })?;
        rows.map(|row| {
            let (
                id,
                file_name,
                output_dir,
                status,
                download_kind,
                source_url,
                total_size,
                last_attempt_at,
                durable_bytes,
            ) = row?;
            Ok(DownloadSummary {
                id,
                output_path: Path::new(&output_dir).join(&file_name),
                file_name,
                status,
                download_kind,
                source_url,
                total_size: total_size
                    .map(|size| from_sqlite_integer(size, "download size"))
                    .transpose()?,
                last_attempt_at,
                durable_bytes: from_sqlite_integer(durable_bytes, "durable bytes")?,
            })
        })
        .collect()
    }

    pub fn forget_completed_download(&mut self, download_id: i64) -> Result<(), StorageError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = transaction.execute(
            "DELETE FROM downloads
             WHERE id = ?1 AND status = 'complete' AND finalize_pending = 0",
            [download_id],
        )?;
        if deleted == 0 {
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM downloads WHERE id = ?1)",
                [download_id],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(StorageError::DownloadNotFound(download_id));
            }
            return Err(StorageError::DownloadNotForgettable(download_id));
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn retry_download(&self, download_id: i64) -> Result<RetryDownload, StorageError> {
        let details: Option<RetryDownloadDetails> = self
            .connection
            .query_row(
                "SELECT source_url, file_name, output_dir, expected_sha256, status,
                        finalize_pending, download_kind
                 FROM downloads WHERE id = ?1",
                [download_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            source_url,
            file_name,
            output_dir,
            expected_sha256,
            status,
            pending,
            download_kind,
        )) = details
        else {
            return Err(StorageError::DownloadNotFound(download_id));
        };
        if status == "complete" || pending || download_kind != "http" {
            return Err(StorageError::DownloadNotRetryable(download_id));
        }
        Ok(RetryDownload {
            source_url,
            file_name,
            output_dir,
            expected_sha256,
        })
    }

    pub(crate) fn download_progress(&self, download_id: i64) -> Result<(u64, u64), StorageError> {
        let progress: Option<(Option<i64>, i64)> = self
            .connection
            .query_row(
                "SELECT downloads.total_size,
                        COALESCE((
                            SELECT sum(segments.cursor - segments.range_start)
                            FROM segments WHERE segments.download_id = downloads.id
                        ), 0)
                 FROM downloads WHERE downloads.id = ?1",
                [download_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((total_size, durable_bytes)) = progress else {
            return Err(StorageError::DownloadNotFound(download_id));
        };
        Ok((
            from_sqlite_integer(durable_bytes, "durable bytes")?,
            from_sqlite_integer(
                total_size.ok_or(StorageError::InvalidStoredInteger("download size"))?,
                "download size",
            )?,
        ))
    }

    pub(crate) fn set_expected_sha256(
        &mut self,
        download_id: i64,
        expected_sha256: Option<&str>,
    ) -> Result<Option<String>, StorageError> {
        let requested = expected_sha256
            .map(|hash| {
                if !is_sha256_hex(hash) {
                    return Err(StorageError::InvalidExpectedHash);
                }
                Ok(hash.to_ascii_lowercase())
            })
            .transpose()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored: Option<String> = transaction
            .query_row(
                "SELECT expected_sha256 FROM downloads WHERE id = ?1",
                [download_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StorageError::DownloadNotFound(download_id))?;
        if let (Some(stored), Some(requested)) = (&stored, &requested)
            && stored != requested
        {
            return Err(StorageError::ExpectedHashConflict(download_id));
        }
        let effective = stored.or(requested);
        if let Some(hash) = &effective {
            transaction.execute(
                "UPDATE downloads SET expected_sha256 = ?1
                 WHERE id = ?2 AND expected_sha256 IS NULL",
                params![hash, download_id],
            )?;
        }
        transaction.commit()?;
        Ok(effective)
    }

    pub(crate) fn mark_download_active(&mut self, download_id: i64) -> Result<(), StorageError> {
        let attempted_at = unix_timestamp()?;
        let updated = self.connection.execute(
            "UPDATE downloads SET status = 'active', last_attempt_at = ?1
             WHERE id = ?2 AND download_kind = 'http'
               AND status != 'complete' AND finalize_pending = 0",
            params![attempted_at, download_id],
        )?;
        if updated == 0 {
            let exists: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM downloads WHERE id = ?1)",
                [download_id],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(StorageError::DownloadNotFound(download_id));
            }
        }
        Ok(())
    }

    pub(crate) fn pause_download(&mut self, download_id: i64) -> Result<(), StorageError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let updated = transaction.execute(
            "UPDATE downloads SET status = 'paused'
             WHERE id = ?1 AND download_kind = 'http' AND status = 'active'",
            [download_id],
        )?;
        if updated == 0 {
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM downloads WHERE id = ?1)",
                [download_id],
                |row| row.get(0),
            )?;
            return if exists {
                Err(StorageError::DownloadNotActive(download_id))
            } else {
                Err(StorageError::DownloadNotFound(download_id))
            };
        }
        transaction.execute(
            "INSERT INTO events (download_id, event_type, message, created_at)
             VALUES (?1, 'paused', 'Download paused by user', ?2)",
            params![download_id, unix_timestamp()?],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn mark_download_failed(
        &mut self,
        download_id: i64,
        message: &str,
    ) -> Result<(), StorageError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let updated = transaction.execute(
            "UPDATE downloads SET status = 'failed'
             WHERE id = ?1 AND status != 'complete'",
            [download_id],
        )?;
        if updated == 0 {
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM downloads WHERE id = ?1)",
                [download_id],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(StorageError::DownloadNotFound(download_id));
            }
            return Ok(());
        }
        transaction.execute(
            "INSERT INTO events (download_id, event_type, message, created_at)
             VALUES (?1, 'failed', ?2, ?3)",
            params![download_id, message, unix_timestamp()?],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn find_resumable_download(
        &self,
        output_dir: &str,
        file_name: &str,
    ) -> Result<Option<ResumableDownload>, StorageError> {
        let mut statement = self.connection.prepare(
            "SELECT downloads.id, downloads.source_url, downloads.total_size,
                    (SELECT segments.id FROM segments
                     WHERE segments.download_id = downloads.id
                     ORDER BY segments.range_start LIMIT 1)
             FROM downloads
             WHERE downloads.output_dir = ?1
               AND downloads.file_name = ?2
               AND downloads.download_kind = 'http'
               AND downloads.status != 'complete'
               AND downloads.finalize_pending = 0
             ORDER BY downloads.id DESC
             LIMIT 2",
        )?;
        let rows = statement.query_map(params![output_dir, file_name], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })?;
        let mut matches = rows.collect::<Result<Vec<_>, _>>()?;
        if matches.len() > 1 {
            return Err(StorageError::MultipleDownloadsForOutput);
        }
        let Some((id, source_url, total_size, segment_id)) = matches.pop() else {
            return Ok(None);
        };
        Ok(Some(ResumableDownload {
            id,
            source_url,
            total_size: total_size
                .map(|size| from_sqlite_integer(size, "download size"))
                .transpose()?,
            segment_id,
        }))
    }

    pub(crate) fn set_source_validators(
        &mut self,
        download_id: i64,
        validators: &SourceValidators,
    ) -> Result<(), StorageError> {
        let updated = self.connection.execute(
            "UPDATE sources SET etag = COALESCE(etag, ?1),
                                last_modified = COALESCE(last_modified, ?2)
             WHERE download_id = ?3 AND ordinal = 0",
            params![validators.etag, validators.last_modified, download_id],
        )?;
        if updated == 0 {
            return Err(StorageError::DownloadNotFound(download_id));
        }
        Ok(())
    }

    pub fn create_segment(
        &mut self,
        download_id: i64,
        range_start: u64,
        range_end: u64,
    ) -> Result<i64, StorageError> {
        if range_start >= range_end {
            return Err(StorageError::InvalidRange);
        }
        let range_start = to_sqlite_integer(range_start, "segment start")?;
        let range_end = to_sqlite_integer(range_end, "segment end")?;
        let total_size: Option<i64> = self
            .connection
            .query_row(
                "SELECT total_size FROM downloads WHERE id = ?1",
                [download_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StorageError::DownloadNotFound(download_id))?;
        if total_size.is_some_and(|size| range_end > size) {
            return Err(StorageError::RangeExceedsDownloadSize);
        }

        self.connection.execute(
            "INSERT INTO segments (download_id, range_start, range_end, cursor)
             VALUES (?1, ?2, ?3, ?2)",
            params![download_id, range_start, range_end],
        )?;

        Ok(self.connection.last_insert_rowid())
    }

    pub(crate) fn segment_ids(&self, download_id: i64) -> Result<Vec<i64>, StorageError> {
        let mut statement = self
            .connection
            .prepare("SELECT id FROM segments WHERE download_id = ?1 ORDER BY range_start")?;
        let rows = statement.query_map([download_id], |row| row.get(0))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    /// Record a cursor only after the corresponding part-file bytes are flushed to disk.
    #[cfg(test)]
    pub(crate) fn record_durable_cursor(
        &mut self,
        segment_id: i64,
        cursor: u64,
    ) -> Result<(), StorageError> {
        let cursor_value = to_sqlite_integer(cursor, "segment cursor")?;
        let range: Option<(i64, i64, i64)> = self
            .connection
            .query_row(
                "SELECT range_start, range_end, cursor FROM segments WHERE id = ?1",
                [segment_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((range_start, range_end, current_cursor)) = range else {
            return Err(StorageError::SegmentNotFound(segment_id));
        };
        let range_start = from_sqlite_integer(range_start, "segment start")?;
        let range_end = from_sqlite_integer(range_end, "segment end")?;
        let current_cursor = from_sqlite_integer(current_cursor, "segment cursor")?;
        if cursor < range_start || cursor > range_end {
            return Err(StorageError::InvalidCursor);
        }
        if cursor < current_cursor {
            return Err(StorageError::CursorRegression);
        }

        self.connection.execute(
            "UPDATE segments
             SET cursor = ?1, status = 'active'
             WHERE id = ?2",
            params![cursor_value, segment_id],
        )?;
        Ok(())
    }

    pub(crate) fn record_durable_write(
        &mut self,
        segment_id: i64,
        offset: u64,
        data: &[u8],
    ) -> Result<u64, StorageError> {
        if data.is_empty() {
            return Err(StorageError::InvalidCursor);
        }
        let length = u64::try_from(data.len()).map_err(|_| StorageError::InvalidCursor)?;
        let cursor = offset
            .checked_add(length)
            .ok_or(StorageError::InvalidCursor)?;
        let offset_value = to_sqlite_integer(offset, "block start")?;
        let cursor_value = to_sqlite_integer(cursor, "segment cursor")?;
        let digest = Sha256::digest(data);
        let sha256: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let segment: Option<(i64, i64, i64)> = transaction
            .query_row(
                "SELECT range_start, range_end, cursor FROM segments WHERE id = ?1",
                [segment_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((range_start, range_end, current_cursor)) = segment else {
            return Err(StorageError::SegmentNotFound(segment_id));
        };
        if offset_value != current_cursor {
            return Err(if offset_value < current_cursor {
                StorageError::CursorRegression
            } else {
                StorageError::InvalidCursor
            });
        }
        if offset_value < range_start || cursor_value > range_end {
            return Err(StorageError::InvalidCursor);
        }
        transaction.execute(
            "INSERT INTO blocks (segment_id, block_index, range_start, range_end, hash)
             VALUES (?1, ?2, ?2, ?3, ?4)
             ON CONFLICT(segment_id, block_index) DO UPDATE SET
                 range_start = excluded.range_start,
                 range_end = excluded.range_end,
                 hash = excluded.hash",
            params![segment_id, offset_value, cursor_value, sha256],
        )?;
        transaction.execute(
            "UPDATE segments SET cursor = ?1, status = 'active' WHERE id = ?2",
            params![cursor_value, segment_id],
        )?;
        transaction.commit()?;
        Ok(cursor)
    }

    pub(crate) fn durable_blocks(
        &self,
        segment_id: i64,
    ) -> Result<Vec<(u64, u64, String)>, StorageError> {
        let mut statement = self.connection.prepare(
            "SELECT range_start, range_end, hash FROM blocks
             WHERE segment_id = ?1 ORDER BY range_start",
        )?;
        let rows = statement.query_map([segment_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.map(|row| {
            let (start, end, hash) = row?;
            Ok((
                from_sqlite_integer(start, "block start")?,
                from_sqlite_integer(end, "block end")?,
                hash,
            ))
        })
        .collect()
    }

    pub(crate) fn mark_segment_complete(&mut self, segment_id: i64) -> Result<(), StorageError> {
        let updated = self.connection.execute(
            "UPDATE segments SET status = 'complete'
             WHERE id = ?1 AND cursor = range_end",
            [segment_id],
        )?;
        if updated == 0 {
            let exists: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM segments WHERE id = ?1)",
                [segment_id],
                |row| row.get(0),
            )?;
            return if exists {
                Err(StorageError::SegmentNotComplete(segment_id))
            } else {
                Err(StorageError::SegmentNotFound(segment_id))
            };
        }
        Ok(())
    }

    pub(crate) fn reset_segment_for_redownload(
        &mut self,
        segment_id: i64,
    ) -> Result<(u64, u64), StorageError> {
        let range: Option<(i64, i64)> = self
            .connection
            .query_row(
                "SELECT range_start, range_end FROM segments WHERE id = ?1",
                [segment_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((start, end)) = range else {
            return Err(StorageError::SegmentNotFound(segment_id));
        };
        self.connection
            .execute("DELETE FROM blocks WHERE segment_id = ?1", [segment_id])?;
        self.connection.execute(
            "UPDATE segments SET cursor = range_start, status = 'pending' WHERE id = ?1",
            [segment_id],
        )?;
        Ok((
            from_sqlite_integer(start, "segment start")?,
            from_sqlite_integer(end, "segment end")?,
        ))
    }

    pub(crate) fn download_ready_to_finalize(&self, segment_id: i64) -> Result<bool, StorageError> {
        download_ready_to_finalize(&self.connection, segment_id)
    }

    pub(crate) fn prepare_finalization(
        &mut self,
        segment_id: i64,
        part_path: &str,
        final_path: &str,
        size: u64,
        sha256: &str,
    ) -> Result<bool, StorageError> {
        validate_nonempty(part_path, "part-file path")?;
        validate_nonempty(final_path, "final-file path")?;
        if !is_sha256_hex(sha256) {
            return Err(StorageError::InvalidFinalization);
        }
        let size = to_sqlite_integer(size, "final file size")?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !download_ready_to_finalize(&transaction, segment_id)? {
            return Ok(false);
        }

        let download_id: i64 = transaction.query_row(
            "SELECT download_id FROM segments WHERE id = ?1",
            [segment_id],
            |row| row.get(0),
        )?;
        let current: (
            i64,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<String>,
        ) = transaction.query_row(
            "SELECT finalize_pending, part_path, final_path, final_size, final_sha256
                 FROM downloads WHERE id = ?1",
            [download_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        if current.0 != 0 {
            let same_intent = current.1.as_deref() == Some(part_path)
                && current.2.as_deref() == Some(final_path)
                && current.3 == Some(size)
                && current.4.as_deref() == Some(sha256);
            return if same_intent {
                Ok(true)
            } else {
                Err(StorageError::FinalizationConflict(download_id))
            };
        }
        let total_size: Option<i64> = transaction.query_row(
            "SELECT total_size FROM downloads WHERE id = ?1",
            [download_id],
            |row| row.get(0),
        )?;
        if total_size.is_some_and(|total| total != size) {
            return Err(StorageError::InvalidFinalization);
        }
        transaction.execute(
            "UPDATE downloads SET finalize_pending = 1, part_path = ?1, final_path = ?2,
                                  final_size = ?3, final_sha256 = ?4
             WHERE id = ?5",
            params![part_path, final_path, size, sha256, download_id],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub(crate) fn pending_finalizations(&self) -> Result<Vec<PendingFinalization>, StorageError> {
        let mut statement = self.connection.prepare(
            "SELECT (
                SELECT segments.id FROM segments
                WHERE segments.download_id = downloads.id
                ORDER BY segments.range_start LIMIT 1
             ), downloads.part_path, downloads.final_path, downloads.final_size,
                downloads.final_sha256
             FROM downloads
             WHERE downloads.finalize_pending = 1
             ORDER BY downloads.id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, Option<i64>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;

        rows.map(|row| {
            let (segment_id, part_path, final_path, size, sha256) = row?;
            Ok(PendingFinalization {
                segment_id: segment_id.ok_or(StorageError::InvalidFinalization)?,
                part_path: part_path.ok_or(StorageError::InvalidFinalization)?,
                final_path: final_path.ok_or(StorageError::InvalidFinalization)?,
                size: from_sqlite_integer(
                    size.ok_or(StorageError::InvalidFinalization)?,
                    "final file size",
                )?,
                sha256: sha256.ok_or(StorageError::InvalidFinalization)?,
            })
        })
        .collect()
    }

    pub(crate) fn mark_download_complete(&mut self, segment_id: i64) -> Result<bool, StorageError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let download_id: i64 = transaction
            .query_row(
                "SELECT download_id FROM segments WHERE id = ?1",
                [segment_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StorageError::SegmentNotFound(segment_id))?;
        let finalize_pending: bool = transaction.query_row(
            "SELECT finalize_pending FROM downloads WHERE id = ?1",
            [download_id],
            |row| row.get(0),
        )?;
        if !finalize_pending {
            return Err(StorageError::InvalidFinalization);
        }
        let ready = download_ready_to_finalize(&transaction, segment_id)?;
        if !ready {
            return Ok(false);
        }
        transaction.execute(
            "UPDATE downloads SET status = 'complete', completed_at = ?1,
                                  finalize_pending = 0
             WHERE id = ?2",
            params![unix_timestamp()?, download_id],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn segment_cursor(&self, segment_id: i64) -> Result<u64, StorageError> {
        let cursor: Option<i64> = self
            .connection
            .query_row(
                "SELECT cursor FROM segments WHERE id = ?1",
                [segment_id],
                |row| row.get(0),
            )
            .optional()?;
        cursor
            .map(|value| from_sqlite_integer(value, "segment cursor"))
            .transpose()?
            .ok_or(StorageError::SegmentNotFound(segment_id))
    }

    pub fn segment_range(&self, segment_id: i64) -> Result<(u64, u64), StorageError> {
        let range: Option<(i64, i64)> = self
            .connection
            .query_row(
                "SELECT range_start, range_end FROM segments WHERE id = ?1",
                [segment_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (start, end) = range.ok_or(StorageError::SegmentNotFound(segment_id))?;
        Ok((
            from_sqlite_integer(start, "segment start")?,
            from_sqlite_integer(end, "segment end")?,
        ))
    }

    pub(crate) fn segment_download_size(
        &self,
        segment_id: i64,
    ) -> Result<Option<u64>, StorageError> {
        let total_size: Option<Option<i64>> = self
            .connection
            .query_row(
                "SELECT downloads.total_size
                 FROM segments JOIN downloads ON downloads.id = segments.download_id
                 WHERE segments.id = ?1",
                [segment_id],
                |row| row.get(0),
            )
            .optional()?;
        total_size
            .ok_or(StorageError::SegmentNotFound(segment_id))?
            .map(|size| from_sqlite_integer(size, "download size"))
            .transpose()
    }

    pub(crate) fn segment_count(&self, segment_id: i64) -> Result<u64, StorageError> {
        let count: i64 = self.connection.query_row(
            "SELECT count(*) FROM segments
             WHERE download_id = (SELECT download_id FROM segments WHERE id = ?1)",
            [segment_id],
            |row| row.get(0),
        )?;
        if count == 0 {
            return Err(StorageError::SegmentNotFound(segment_id));
        }
        from_sqlite_integer(count, "segment count")
    }

    pub(crate) fn source_validators(
        &self,
        segment_id: i64,
    ) -> Result<SourceValidators, StorageError> {
        let validators: Option<(Option<String>, Option<String>)> = self
            .connection
            .query_row(
                "SELECT sources.etag, sources.last_modified
                 FROM sources JOIN segments ON segments.download_id = sources.download_id
                 WHERE segments.id = ?1 AND sources.ordinal = 0",
                [segment_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (etag, last_modified) = validators.ok_or(StorageError::SegmentNotFound(segment_id))?;
        Ok(SourceValidators {
            etag,
            last_modified,
        })
    }

    pub(crate) fn save_source_validators(
        &mut self,
        segment_id: i64,
        validators: &SourceValidators,
    ) -> Result<(), StorageError> {
        let current = self.source_validators(segment_id)?;
        if !current.matches(validators) {
            return Err(StorageError::SourceChanged);
        }
        self.connection.execute(
            "UPDATE sources SET etag = COALESCE(etag, ?1),
                                last_modified = COALESCE(last_modified, ?2)
             WHERE download_id = (SELECT download_id FROM segments WHERE id = ?3)
               AND ordinal = 0",
            params![validators.etag, validators.last_modified, segment_id],
        )?;
        Ok(())
    }

    pub fn segment_status(&self, segment_id: i64) -> Result<String, StorageError> {
        self.connection
            .query_row(
                "SELECT status FROM segments WHERE id = ?1",
                [segment_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StorageError::SegmentNotFound(segment_id))
    }

    pub fn download_status(&self, segment_id: i64) -> Result<String, StorageError> {
        self.connection
            .query_row(
                "SELECT downloads.status
                 FROM downloads JOIN segments ON segments.download_id = downloads.id
                 WHERE segments.id = ?1",
                [segment_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StorageError::SegmentNotFound(segment_id))
    }
}

fn download_ready_to_finalize(
    connection: &rusqlite::Connection,
    segment_id: i64,
) -> Result<bool, StorageError> {
    let segment: Option<(i64, String)> = connection
        .query_row(
            "SELECT download_id, status FROM segments WHERE id = ?1",
            [segment_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (download_id, status) = segment.ok_or(StorageError::SegmentNotFound(segment_id))?;
    if status != "complete" {
        return Ok(false);
    }

    let incomplete_segments: i64 = connection.query_row(
        "SELECT count(*) FROM segments WHERE download_id = ?1 AND status != 'complete'",
        [download_id],
        |row| row.get(0),
    )?;
    if incomplete_segments != 0 {
        return Ok(false);
    }
    let total_size: Option<i64> = connection.query_row(
        "SELECT total_size FROM downloads WHERE id = ?1",
        [download_id],
        |row| row.get(0),
    )?;
    let (segment_count, first_byte, final_byte, covered_bytes): (i64, i64, i64, i64) = connection
        .query_row(
        "SELECT count(*), COALESCE(min(range_start), 0),
                    COALESCE(max(range_end), 0),
                    COALESCE(sum(range_end - range_start), 0)
             FROM segments WHERE download_id = ?1",
        [download_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    Ok(segment_count > 0
        && first_byte == 0
        && covered_bytes == final_byte
        && total_size.is_none_or(|size| final_byte == size))
}

fn validate_nonempty(value: &str, field: &'static str) -> Result<(), StorageError> {
    if value.trim().is_empty() {
        return Err(StorageError::InvalidField(field));
    }
    Ok(())
}

fn to_sqlite_integer(value: u64, field: &'static str) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::ValueOutOfRange(field))
}

fn from_sqlite_integer(value: i64, field: &'static str) -> Result<u64, StorageError> {
    u64::try_from(value).map_err(|_| StorageError::InvalidStoredInteger(field))
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn unix_timestamp() -> Result<i64, StorageError> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StorageError::ClockBeforeUnixEpoch)?
        .as_secs();
    i64::try_from(seconds).map_err(|_| StorageError::ValueOutOfRange("timestamp"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use rusqlite::OptionalExtension;

    use super::{SCHEMA_VERSION, Storage, StorageError};

    static NEXT_DATABASE_ID: AtomicU64 = AtomicU64::new(0);

    struct TestDatabase(PathBuf);

    impl TestDatabase {
        fn new() -> Self {
            let id = NEXT_DATABASE_ID.fetch_add(1, Ordering::Relaxed);
            Self(
                std::env::temp_dir()
                    .join(format!("raja-storage-test-{}-{id}.db", std::process::id())),
            )
        }
    }

    impl Drop for TestDatabase {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
            let _ = fs::remove_file(self.0.with_extension("db-wal"));
            let _ = fs::remove_file(self.0.with_extension("db-shm"));
        }
    }

    #[test]
    fn creates_wal_database_with_full_synchronous_mode_and_schema() {
        let database = TestDatabase::new();
        let storage = Storage::open(&database.0).unwrap();
        let journal_mode: String = storage
            .connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        let synchronous: i64 = storage
            .connection
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        let version: i64 = storage
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let settings_table: Option<String> = storage
            .connection
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'settings'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap();

        assert_eq!(journal_mode, "wal");
        assert_eq!(synchronous, 2);
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(settings_table.as_deref(), Some("settings"));
    }

    #[test]
    fn persists_application_settings_across_database_reopen() {
        let database = TestDatabase::new();
        {
            let mut storage = Storage::open(&database.0).unwrap();
            assert_eq!(storage.setting("download_directory").unwrap(), None);
            storage
                .set_setting("download_directory", "C:\\Downloads")
                .unwrap();
            storage
                .set_setting("download_directory", "D:\\Files")
                .unwrap();
        }

        let storage = Storage::open(&database.0).unwrap();
        assert_eq!(
            storage.setting("download_directory").unwrap(),
            Some("D:\\Files".to_owned())
        );
    }

    #[test]
    fn persists_download_activity_failure_and_list_summary() {
        let database = TestDatabase::new();
        let mut storage = Storage::open(&database.0).unwrap();
        let download_id = storage
            .create_download(
                "https://example.test/file",
                "file.bin",
                "downloads",
                Some(123),
            )
            .unwrap();

        storage.mark_download_active(download_id).unwrap();
        let active = storage.list_downloads().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].status, "active");
        assert_eq!(active[0].file_name, "file.bin");
        assert_eq!(
            active[0].output_path,
            PathBuf::from("downloads").join("file.bin")
        );
        assert_eq!(active[0].total_size, Some(123));
        assert!(active[0].last_attempt_at.is_some());
        assert_eq!(active[0].durable_bytes, 0);
        let segment_id = storage.create_segment(download_id, 0, 123).unwrap();
        storage.record_durable_cursor(segment_id, 23).unwrap();
        assert_eq!(storage.download_progress(download_id).unwrap(), (23, 123));
        assert_eq!(storage.list_downloads().unwrap()[0].durable_bytes, 23);

        storage
            .mark_download_failed(download_id, "temporary test failure")
            .unwrap();
        let failed = storage.list_downloads().unwrap();
        assert_eq!(failed[0].status, "failed");
        let event: (String, String) = storage
            .connection
            .query_row(
                "SELECT event_type, message FROM events WHERE download_id = ?1",
                [download_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            event,
            ("failed".to_owned(), "temporary test failure".to_owned())
        );
        assert!(
            storage
                .find_resumable_download("downloads", "file.bin")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn persists_video_download_progress_and_completion_in_history() {
        let database = TestDatabase::new();
        let mut storage = Storage::open(&database.0).unwrap();
        let download_id = storage
            .create_video_download("https://example.test/watch?v=clip", "clip.mp4", "videos")
            .unwrap();
        storage
            .update_video_download(download_id, "active", None, None, None)
            .unwrap();
        storage
            .update_video_download(download_id, "active", Some(250), Some(1_000), None)
            .unwrap();
        let progress = storage.list_downloads().unwrap();
        assert_eq!(progress[0].download_kind, "video");
        assert_eq!(progress[0].source_url, "https://example.test/watch?v=clip");
        assert_eq!(progress[0].status, "active");
        assert_eq!(progress[0].total_size, Some(1_000));
        assert_eq!(progress[0].durable_bytes, 250);
        assert!(progress[0].last_attempt_at.is_some());

        storage
            .update_video_output_path(download_id, Path::new("videos/clip.webm"))
            .unwrap();
        storage
            .update_video_download(
                download_id,
                "complete",
                None,
                None,
                Some("Video extraction completed"),
            )
            .unwrap();
        let completed = storage.list_downloads().unwrap();
        assert_eq!(completed[0].status, "complete");
        assert_eq!(completed[0].durable_bytes, 250);
        assert_eq!(completed[0].file_name, "clip.webm");
        assert_eq!(completed[0].output_path, Path::new("videos/clip.webm"));
        storage.forget_completed_download(download_id).unwrap();
        assert!(storage.list_downloads().unwrap().is_empty());
    }

    #[test]
    fn video_jobs_cannot_be_retried_by_the_http_cli_engine() {
        let database = TestDatabase::new();
        let mut storage = Storage::open(&database.0).unwrap();
        let download_id = storage
            .create_video_download("https://example.test/watch", "clip.mp4", "videos")
            .unwrap();
        assert!(matches!(
            storage.retry_download(download_id),
            Err(StorageError::DownloadNotRetryable(id)) if id == download_id
        ));
        assert!(matches!(
            storage.pause_download(download_id),
            Err(StorageError::DownloadNotActive(id)) if id == download_id
        ));
    }

    #[test]
    fn refuses_to_retry_completed_downloads() {
        let database = TestDatabase::new();
        let mut storage = Storage::open(&database.0).unwrap();
        let download_id = storage
            .create_download(
                "https://example.test/file",
                "file.bin",
                "downloads",
                Some(123),
            )
            .unwrap();
        let segment_id = storage.create_segment(download_id, 0, 123).unwrap();
        storage.record_durable_cursor(segment_id, 123).unwrap();
        storage.mark_segment_complete(segment_id).unwrap();
        storage
            .prepare_finalization(segment_id, "file.ndpart", "file.bin", 123, &"0".repeat(64))
            .unwrap();
        storage.mark_download_complete(segment_id).unwrap();

        assert!(matches!(
            storage.retry_download(download_id),
            Err(StorageError::DownloadNotRetryable(id)) if id == download_id
        ));
    }

    #[test]
    fn forgetting_a_completed_download_keeps_the_downloaded_file() {
        let database = TestDatabase::new();
        let output_file = database.0.with_extension("downloaded");
        fs::write(&output_file, b"verified output").unwrap();
        let mut storage = Storage::open(&database.0).unwrap();
        let download_id = storage
            .create_download(
                "https://example.test/file",
                output_file.file_name().unwrap().to_str().unwrap(),
                output_file.parent().unwrap().to_str().unwrap(),
                Some(15),
            )
            .unwrap();
        storage
            .connection
            .execute(
                "UPDATE downloads SET status = 'complete' WHERE id = ?1",
                [download_id],
            )
            .unwrap();

        storage.forget_completed_download(download_id).unwrap();

        assert!(storage.list_downloads().unwrap().is_empty());
        assert_eq!(fs::read(&output_file).unwrap(), b"verified output");
        fs::remove_file(output_file).unwrap();
    }

    #[test]
    fn refuses_to_forget_incomplete_or_missing_downloads() {
        let database = TestDatabase::new();
        let mut storage = Storage::open(&database.0).unwrap();
        let download_id = storage
            .create_download(
                "https://example.test/file",
                "file.bin",
                "downloads",
                Some(15),
            )
            .unwrap();

        assert!(matches!(
            storage.forget_completed_download(download_id),
            Err(StorageError::DownloadNotForgettable(id)) if id == download_id
        ));
        assert!(matches!(
            storage.forget_completed_download(download_id + 1),
            Err(StorageError::DownloadNotFound(id)) if id == download_id + 1
        ));
    }

    #[test]
    fn preserves_expected_hash_and_rejects_conflicting_resume_values() {
        let database = TestDatabase::new();
        let mut storage = Storage::open(&database.0).unwrap();
        let download_id = storage
            .create_download(
                "https://example.test/file",
                "file.bin",
                "downloads",
                Some(123),
            )
            .unwrap();
        let stored = storage
            .set_expected_sha256(download_id, Some(&"A".repeat(64)))
            .unwrap();
        assert_eq!(stored.as_deref(), Some("a".repeat(64).as_str()));
        assert_eq!(
            storage
                .set_expected_sha256(download_id, None)
                .unwrap()
                .as_deref(),
            Some("a".repeat(64).as_str())
        );
        assert!(matches!(
            storage.set_expected_sha256(download_id, Some(&"b".repeat(64))),
            Err(StorageError::ExpectedHashConflict(id)) if id == download_id
        ));
        assert!(matches!(
            storage.set_expected_sha256(download_id, Some("not-a-hash")),
            Err(StorageError::InvalidExpectedHash)
        ));
        drop(storage);

        let mut storage = Storage::open(&database.0).unwrap();
        assert_eq!(
            storage
                .set_expected_sha256(download_id, None)
                .unwrap()
                .as_deref(),
            Some("a".repeat(64).as_str())
        );
    }

    #[test]
    fn pauses_only_active_downloads_and_records_the_event() {
        let database = TestDatabase::new();
        let mut storage = Storage::open(&database.0).unwrap();
        let download_id = storage
            .create_download(
                "https://example.test/file",
                "file.bin",
                "downloads",
                Some(123),
            )
            .unwrap();

        assert!(matches!(
            storage.pause_download(download_id),
            Err(StorageError::DownloadNotActive(id)) if id == download_id
        ));
        storage.mark_download_active(download_id).unwrap();
        storage.pause_download(download_id).unwrap();

        let downloads = storage.list_downloads().unwrap();
        assert_eq!(downloads[0].status, "paused");
        let event: (String, String) = storage
            .connection
            .query_row(
                "SELECT event_type, message FROM events WHERE download_id = ?1",
                [download_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            event,
            ("paused".to_owned(), "Download paused by user".to_owned())
        );
    }

    #[test]
    fn migrates_version_one_downloads_table_to_finalization_schema() {
        let database = TestDatabase::new();
        let connection = rusqlite::Connection::open(&database.0).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE downloads (id INTEGER PRIMARY KEY);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        drop(connection);

        let storage = Storage::open(&database.0).unwrap();
        let version: i64 = storage
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let column_exists: bool = storage
            .connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM pragma_table_info('downloads')
                    WHERE name = 'finalize_pending'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert!(column_exists);
    }

    #[test]
    fn migrates_version_two_downloads_to_persist_expected_hashes() {
        let database = TestDatabase::new();
        let connection = rusqlite::Connection::open(&database.0).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE downloads (id INTEGER PRIMARY KEY);
                 PRAGMA user_version = 2;",
            )
            .unwrap();
        drop(connection);

        let storage = Storage::open(&database.0).unwrap();
        let version: i64 = storage
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let column_exists: bool = storage
            .connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM pragma_table_info('downloads')
                    WHERE name = 'expected_sha256'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert!(column_exists);
    }

    #[test]
    fn migrates_version_three_downloads_to_persist_last_attempt_time() {
        let database = TestDatabase::new();
        let connection = rusqlite::Connection::open(&database.0).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE downloads (id INTEGER PRIMARY KEY);
                 PRAGMA user_version = 3;",
            )
            .unwrap();
        drop(connection);

        let storage = Storage::open(&database.0).unwrap();
        let version: i64 = storage
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let column_exists: bool = storage
            .connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM pragma_table_info('downloads')
                    WHERE name = 'last_attempt_at'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert!(column_exists);
    }

    #[test]
    fn migrates_version_four_downloads_to_video_job_schema() {
        let database = TestDatabase::new();
        let connection = rusqlite::Connection::open(&database.0).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE downloads (id INTEGER PRIMARY KEY);
                 INSERT INTO downloads VALUES (1);
                 PRAGMA user_version = 4;",
            )
            .unwrap();
        drop(connection);

        let storage = Storage::open(&database.0).unwrap();
        let version: i64 = storage
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let kind_exists: bool = storage
            .connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM pragma_table_info('downloads')
                    WHERE name = 'download_kind'
                )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let legacy_defaults: (String, i64) = storage
            .connection
            .query_row(
                "SELECT download_kind, external_bytes FROM downloads WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert!(kind_exists);
        assert_eq!(legacy_defaults, ("http".to_owned(), 0));
    }

    #[test]
    fn persists_download_and_durable_segment_cursor_across_reopen() {
        let database = TestDatabase::new();
        let segment_id = {
            let mut storage = Storage::open(&database.0).unwrap();
            let download_id = storage
                .create_download(
                    "https://example.test/file",
                    "file.bin",
                    "downloads",
                    Some(100),
                )
                .unwrap();
            let segment_id = storage.create_segment(download_id, 0, 100).unwrap();
            storage.record_durable_cursor(segment_id, 60).unwrap();
            segment_id
        };

        let storage = Storage::open(&database.0).unwrap();
        assert_eq!(storage.segment_cursor(segment_id).unwrap(), 60);
        assert_eq!(storage.segment_status(segment_id).unwrap(), "active");
    }

    #[test]
    fn download_is_complete_only_after_every_segment_is_complete() {
        let database = TestDatabase::new();
        let mut storage = Storage::open(&database.0).unwrap();
        let download_id = storage
            .create_download(
                "https://example.test/file",
                "file.bin",
                "downloads",
                Some(10),
            )
            .unwrap();
        let first = storage.create_segment(download_id, 0, 5).unwrap();
        let second = storage.create_segment(download_id, 5, 10).unwrap();
        storage.record_durable_cursor(first, 5).unwrap();
        storage.mark_segment_complete(first).unwrap();

        assert!(!storage.download_ready_to_finalize(first).unwrap());
        storage.record_durable_cursor(second, 10).unwrap();
        storage.mark_segment_complete(second).unwrap();
        assert!(
            storage
                .prepare_finalization(
                    second,
                    "file.ndpart",
                    "file.bin",
                    10,
                    "0".repeat(64).as_str()
                )
                .unwrap()
        );
        assert!(storage.mark_download_complete(second).unwrap());

        let status: String = storage
            .connection
            .query_row(
                "SELECT status FROM downloads WHERE id = ?1",
                [download_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "complete");
    }

    #[test]
    fn refuses_to_complete_a_download_with_gaps_between_segments() {
        let database = TestDatabase::new();
        let mut storage = Storage::open(&database.0).unwrap();
        let download_id = storage
            .create_download(
                "https://example.test/file",
                "file.bin",
                "downloads",
                Some(10),
            )
            .unwrap();
        let first = storage.create_segment(download_id, 0, 4).unwrap();
        let last = storage.create_segment(download_id, 5, 10).unwrap();
        storage.record_durable_cursor(first, 4).unwrap();
        storage.mark_segment_complete(first).unwrap();
        storage.record_durable_cursor(last, 10).unwrap();
        storage.mark_segment_complete(last).unwrap();

        assert!(!storage.download_ready_to_finalize(last).unwrap());
        let status: String = storage
            .connection
            .query_row(
                "SELECT status FROM downloads WHERE id = ?1",
                [download_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "queued");
    }

    #[test]
    fn completes_segment_when_durable_cursor_reaches_its_end() {
        let database = TestDatabase::new();
        let mut storage = Storage::open(&database.0).unwrap();
        let download_id = storage
            .create_download(
                "https://example.test/file",
                "file.bin",
                "downloads",
                Some(10),
            )
            .unwrap();
        let segment_id = storage.create_segment(download_id, 5, 10).unwrap();

        storage.record_durable_cursor(segment_id, 10).unwrap();
        assert_eq!(storage.segment_status(segment_id).unwrap(), "active");
        storage.mark_segment_complete(segment_id).unwrap();

        assert_eq!(storage.segment_cursor(segment_id).unwrap(), 10);
        assert_eq!(storage.segment_status(segment_id).unwrap(), "complete");
    }

    #[test]
    fn incomplete_segment_can_be_reset_to_redownload_after_a_crash_window() {
        let database = TestDatabase::new();
        let mut storage = Storage::open(&database.0).unwrap();
        let download_id = storage
            .create_download(
                "https://example.test/file",
                "file.bin",
                "downloads",
                Some(10),
            )
            .unwrap();
        let segment_id = storage.create_segment(download_id, 2, 8).unwrap();
        storage.record_durable_cursor(segment_id, 8).unwrap();
        assert_eq!(storage.segment_status(segment_id).unwrap(), "active");

        assert_eq!(
            storage.reset_segment_for_redownload(segment_id).unwrap(),
            (2, 8)
        );
        assert_eq!(storage.segment_cursor(segment_id).unwrap(), 2);
        assert_eq!(storage.segment_status(segment_id).unwrap(), "pending");
    }

    #[test]
    fn rejects_invalid_or_regressing_cursors() {
        let database = TestDatabase::new();
        let mut storage = Storage::open(&database.0).unwrap();
        let download_id = storage
            .create_download(
                "https://example.test/file",
                "file.bin",
                "downloads",
                Some(10),
            )
            .unwrap();
        let segment_id = storage.create_segment(download_id, 5, 10).unwrap();
        storage.record_durable_cursor(segment_id, 8).unwrap();

        assert!(matches!(
            storage.record_durable_cursor(segment_id, 4),
            Err(StorageError::InvalidCursor)
        ));
        assert!(matches!(
            storage.record_durable_cursor(segment_id, 7),
            Err(StorageError::CursorRegression)
        ));
        assert_eq!(storage.segment_cursor(segment_id).unwrap(), 8);
    }

    #[test]
    fn rejects_overlapping_or_out_of_bounds_segments() {
        let database = TestDatabase::new();
        let mut storage = Storage::open(&database.0).unwrap();
        let download_id = storage
            .create_download(
                "https://example.test/file",
                "file.bin",
                "downloads",
                Some(100),
            )
            .unwrap();
        storage.create_segment(download_id, 0, 50).unwrap();
        storage.create_segment(download_id, 50, 100).unwrap();

        assert!(matches!(
            storage.create_segment(download_id, 49, 75),
            Err(StorageError::Database(_))
        ));
        assert!(matches!(
            storage.create_segment(download_id, 90, 101),
            Err(StorageError::RangeExceedsDownloadSize)
        ));
        assert!(matches!(
            storage.create_segment(download_id + 1, 0, 1),
            Err(StorageError::DownloadNotFound(_))
        ));
    }

    #[test]
    fn rejects_a_database_schema_newer_than_this_build() {
        let database = TestDatabase::new();
        let connection = rusqlite::Connection::open(&database.0).unwrap();
        connection
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(connection);

        assert!(matches!(
            Storage::open(&database.0),
            Err(StorageError::UnsupportedSchemaVersion(version))
                if version == SCHEMA_VERSION + 1
        ));
    }

    #[test]
    fn persists_pending_finalization_intent_for_restart_recovery() {
        let database = TestDatabase::new();
        let segment_id = {
            let mut storage = Storage::open(&database.0).unwrap();
            let download_id = storage
                .create_download(
                    "https://example.test/file",
                    "file.bin",
                    "downloads",
                    Some(10),
                )
                .unwrap();
            let segment_id = storage.create_segment(download_id, 0, 10).unwrap();
            storage.record_durable_cursor(segment_id, 10).unwrap();
            storage.mark_segment_complete(segment_id).unwrap();
            assert!(
                storage
                    .prepare_finalization(
                        segment_id,
                        "file.ndpart",
                        "file.bin",
                        10,
                        "a".repeat(64).as_str()
                    )
                    .unwrap()
            );
            segment_id
        };

        let storage = Storage::open(&database.0).unwrap();
        let pending = storage.pending_finalizations().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].segment_id, segment_id);
        assert_eq!(pending[0].part_path, "file.ndpart");
        assert_eq!(pending[0].final_path, "file.bin");
        assert_eq!(pending[0].size, 10);
        assert_eq!(pending[0].sha256, "a".repeat(64));
    }
}
