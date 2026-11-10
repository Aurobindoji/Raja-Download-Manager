use std::error::Error;
use std::fmt;
use std::io::{self, Read};
use std::time::Duration;

use reqwest::blocking::{Client, Response};
use reqwest::header::{
    ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, ETAG, IF_RANGE,
    LAST_MODIFIED, RANGE,
};

use crate::part_file::{PartFile, PartFileError};
use crate::storage::{SourceValidators, Storage, StorageError};

const WRITE_BUFFER_SIZE: usize = 256 * 1024;
const MAX_RETRIES: u32 = 4;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub enum HttpRangeError {
    Request(reqwest::Error),
    Io(io::Error),
    Storage(StorageError),
    PartFile(PartFileError),
    InvalidUrl,
    UnsupportedUrlScheme,
    EmptyFileUnsupported,
    UnexpectedStatus(u16),
    MissingContentLength,
    MissingContentRange,
    InvalidContentRange,
    ContentRangeMismatch,
    SourceChanged,
    MissingSourceValidator,
    ContentLengthMismatch { expected: u64, actual: u64 },
    InvalidContentLength,
    BodyLengthMismatch { expected: u64, actual: u64 },
    BodyExceedsRange { expected: u64 },
    EncodedRangeResponse,
    SegmentNotReady,
    SegmentAlreadyComplete,
}

impl fmt::Display for HttpRangeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request(error) => write!(formatter, "HTTP request failed: {error}"),
            Self::Io(error) => write!(formatter, "HTTP response read failed: {error}"),
            Self::Storage(error) => write!(formatter, "download state error: {error}"),
            Self::PartFile(error) => write!(formatter, "part-file write failed: {error}"),
            Self::InvalidUrl => formatter.write_str("download URL is invalid"),
            Self::UnsupportedUrlScheme => {
                formatter.write_str("download URL must use HTTP or HTTPS")
            }
            Self::EmptyFileUnsupported => {
                formatter.write_str("zero-byte downloads are not supported yet")
            }
            Self::UnexpectedStatus(status) => {
                write!(
                    formatter,
                    "server did not honor the byte range (HTTP {status})"
                )
            }
            Self::MissingContentLength => {
                formatter.write_str("server omitted Content-Length for a full-file response")
            }
            Self::MissingContentRange => {
                formatter.write_str("server omitted the Content-Range response header")
            }
            Self::InvalidContentRange => {
                formatter.write_str("server returned an invalid Content-Range header")
            }
            Self::ContentRangeMismatch => {
                formatter.write_str("server returned a different byte range or file size")
            }
            Self::SourceChanged => {
                formatter.write_str("download source changed while the transfer was in progress")
            }
            Self::MissingSourceValidator => formatter.write_str(
                "server supplied no ETag or Last-Modified validator; multi-segment download is unsafe",
            ),
            Self::ContentLengthMismatch { expected, actual } => write!(
                formatter,
                "server declared {actual} response bytes; expected {expected}"
            ),
            Self::InvalidContentLength => {
                formatter.write_str("server returned an invalid Content-Length header")
            }
            Self::BodyLengthMismatch { expected, actual } => write!(
                formatter,
                "server sent {actual} response bytes; expected {expected}"
            ),
            Self::BodyExceedsRange { expected } => write!(
                formatter,
                "server sent more than the requested {expected} response bytes"
            ),
            Self::EncodedRangeResponse => {
                formatter.write_str("server encoded a byte-range response")
            }
            Self::SegmentNotReady => {
                formatter.write_str("segment cursor is outside its configured byte range")
            }
            Self::SegmentAlreadyComplete => formatter.write_str("segment is already complete"),
        }
    }
}

impl Error for HttpRangeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Request(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Storage(error) => Some(error),
            Self::PartFile(error) => Some(error),
            _ => None,
        }
    }
}

impl HttpRangeError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Request(error) => error.is_connect() || error.is_timeout(),
            Self::Io(error) => matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::Interrupted
                    | io::ErrorKind::NotConnected
                    | io::ErrorKind::TimedOut
                    | io::ErrorKind::UnexpectedEof
            ),
            Self::UnexpectedStatus(status) => {
                matches!(*status, 408 | 425 | 429) || (500..=599).contains(status)
            }
            _ => false,
        }
    }
}

impl From<StorageError> for HttpRangeError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<PartFileError> for HttpRangeError {
    fn from(error: PartFileError) -> Self {
        Self::PartFile(error)
    }
}

pub struct HttpRangeClient {
    client: Client,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
    pub size: u64,
    pub validators: SourceValidators,
    pub supports_ranges: bool,
}

impl HttpRangeClient {
    pub fn new() -> Result<Self, HttpRangeError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|error| HttpRangeError::Request(error.without_url()))?;
        Ok(Self { client })
    }

    pub fn probe(&self, url: &str) -> Result<RemoteFile, HttpRangeError> {
        let parsed_url = reqwest::Url::parse(url).map_err(|_| HttpRangeError::InvalidUrl)?;
        if !matches!(parsed_url.scheme(), "http" | "https") {
            return Err(HttpRangeError::UnsupportedUrlScheme);
        }
        let mut response = self
            .client
            .get(parsed_url)
            .header(RANGE, "bytes=0-0")
            .header(ACCEPT_ENCODING, "identity")
            .send()
            .map_err(|error| HttpRangeError::Request(error.without_url()))?;
        let supports_ranges = response.status().as_u16() == 206;
        let size = match response.status().as_u16() {
            206 => validate_content_range(&response, 0, 0, None)?,
            200 => {
                let content_length = response
                    .headers()
                    .get(CONTENT_LENGTH)
                    .ok_or(HttpRangeError::MissingContentLength)?;
                parse_unsigned_header(content_length)?
            }
            status => return Err(HttpRangeError::UnexpectedStatus(status)),
        };
        if size == 0 {
            return Err(HttpRangeError::EmptyFileUnsupported);
        }
        if let Some(content_encoding) = response.headers().get(CONTENT_ENCODING)
            && !content_encoding
                .to_str()
                .map_err(|_| HttpRangeError::EncodedRangeResponse)?
                .eq_ignore_ascii_case("identity")
        {
            return Err(HttpRangeError::EncodedRangeResponse);
        }
        if response.status().as_u16() == 206 {
            if let Some(content_length) = response.headers().get(CONTENT_LENGTH) {
                let actual = parse_unsigned_header(content_length)?;
                if actual != 1 {
                    return Err(HttpRangeError::ContentLengthMismatch {
                        expected: 1,
                        actual,
                    });
                }
            }
            let mut first_byte = [0; 1];
            response
                .read_exact(&mut first_byte)
                .map_err(HttpRangeError::Io)?;
        }
        let validators = response_validators(&response)?;
        Ok(RemoteFile {
            size,
            validators,
            supports_ranges,
        })
    }

    pub fn download_segment(
        &self,
        url: &str,
        part_file: &PartFile,
        storage: &mut Storage,
        segment_id: i64,
    ) -> Result<bool, HttpRangeError> {
        let mut retries = 0;
        loop {
            match self.download_segment_once(url, part_file, storage, segment_id) {
                Ok(ready_to_finalize) => return Ok(ready_to_finalize),
                Err(error) if error.is_retryable() && retries < MAX_RETRIES => {
                    let delay = RETRY_BASE_DELAY * (1 << retries);
                    std::thread::sleep(delay);
                    retries += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn download_segment_once(
        &self,
        url: &str,
        part_file: &PartFile,
        storage: &mut Storage,
        segment_id: i64,
    ) -> Result<bool, HttpRangeError> {
        let parsed_url = reqwest::Url::parse(url).map_err(|_| HttpRangeError::InvalidUrl)?;
        if !matches!(parsed_url.scheme(), "http" | "https") {
            return Err(HttpRangeError::UnsupportedUrlScheme);
        }

        let (segment_start, segment_end) = storage.segment_range(segment_id)?;
        let status = storage.segment_status(segment_id)?;
        if status == "complete" {
            return Err(HttpRangeError::SegmentAlreadyComplete);
        }
        let stored_validators = storage.source_validators(segment_id)?;
        let mut cursor = storage.segment_cursor(segment_id)?;
        if cursor == segment_end
            || (cursor > segment_start && stored_validators.if_range_value().is_none())
        {
            let (reset_start, _) = storage.reset_segment_for_redownload(segment_id)?;
            cursor = reset_start;
        }
        let total_size = storage.segment_download_size(segment_id)?;
        if cursor < segment_start || cursor > segment_end {
            return Err(HttpRangeError::SegmentNotReady);
        }
        let requested_end = segment_end - 1;
        let mut request = self
            .client
            .get(parsed_url)
            .header(RANGE, format!("bytes={cursor}-{requested_end}"))
            .header(ACCEPT_ENCODING, "identity");
        if let Some(if_range) = stored_validators.if_range_value() {
            request = request.header(IF_RANGE, if_range);
        }
        let mut response = request
            .send()
            .map_err(|error| HttpRangeError::Request(error.without_url()))?;

        if response.status().as_u16() == 200 {
            if segment_start != 0
                || total_size != Some(segment_end)
                || storage.segment_count(segment_id)? != 1
            {
                return Err(HttpRangeError::UnexpectedStatus(200));
            }
            if cursor != segment_start {
                cursor = storage.reset_segment_for_redownload(segment_id)?.0;
            }
        } else if response.status().as_u16() == 206 {
            validate_content_range(&response, cursor, requested_end, total_size)?;
        } else {
            return Err(HttpRangeError::UnexpectedStatus(response.status().as_u16()));
        }
        let response_validators = response_validators(&response)?;
        if !stored_validators.matches(&response_validators) {
            return Err(HttpRangeError::SourceChanged);
        }
        if storage.segment_count(segment_id)? > 1
            && response_validators.etag.is_none()
            && response_validators.last_modified.is_none()
        {
            return Err(HttpRangeError::MissingSourceValidator);
        }
        storage.save_source_validators(segment_id, &response_validators)?;
        if let Some(content_encoding) = response.headers().get(CONTENT_ENCODING)
            && !content_encoding
                .to_str()
                .map_err(|_| HttpRangeError::EncodedRangeResponse)?
                .eq_ignore_ascii_case("identity")
        {
            return Err(HttpRangeError::EncodedRangeResponse);
        }
        let expected_len = segment_end - cursor;
        if let Some(content_length) = response.headers().get(CONTENT_LENGTH) {
            let actual = parse_unsigned_header(content_length)?;
            if actual != expected_len {
                return Err(HttpRangeError::ContentLengthMismatch {
                    expected: expected_len,
                    actual,
                });
            }
        }

        let mut received = 0u64;
        let mut durable_cursor = cursor;
        let mut pending = Vec::new();
        let mut buffer = vec![0; WRITE_BUFFER_SIZE];
        loop {
            let read = response.read(&mut buffer).map_err(HttpRangeError::Io)?;
            if read == 0 {
                break;
            }
            let read_len = u64::try_from(read).map_err(|_| HttpRangeError::BodyExceedsRange {
                expected: expected_len,
            })?;
            received = received
                .checked_add(read_len)
                .ok_or(HttpRangeError::BodyExceedsRange {
                    expected: expected_len,
                })?;
            if received > expected_len {
                return Err(HttpRangeError::BodyExceedsRange {
                    expected: expected_len,
                });
            }

            if !pending.is_empty() {
                durable_cursor =
                    part_file.write_segment(storage, segment_id, durable_cursor, &pending)?;
            }
            pending.clear();
            pending.extend_from_slice(&buffer[..read]);
        }

        if received != expected_len {
            return Err(HttpRangeError::BodyLengthMismatch {
                expected: expected_len,
                actual: received,
            });
        }

        if !pending.is_empty() {
            durable_cursor =
                part_file.write_segment(storage, segment_id, durable_cursor, &pending)?;
        }
        if durable_cursor != segment_end {
            return Err(HttpRangeError::BodyLengthMismatch {
                expected: expected_len,
                actual: durable_cursor - cursor,
            });
        }

        storage.mark_segment_complete(segment_id)?;
        Ok(storage.download_ready_to_finalize(segment_id)?)
    }
}

fn validate_content_range(
    response: &Response,
    expected_start: u64,
    expected_end: u64,
    known_total: Option<u64>,
) -> Result<u64, HttpRangeError> {
    let value = response
        .headers()
        .get(CONTENT_RANGE)
        .ok_or(HttpRangeError::MissingContentRange)?
        .to_str()
        .map_err(|_| HttpRangeError::InvalidContentRange)?;
    let range = value
        .strip_prefix("bytes ")
        .ok_or(HttpRangeError::InvalidContentRange)?;
    let (positions, total) = range
        .split_once('/')
        .ok_or(HttpRangeError::InvalidContentRange)?;
    let (start, end) = positions
        .split_once('-')
        .ok_or(HttpRangeError::InvalidContentRange)?;
    let start = start
        .parse::<u64>()
        .map_err(|_| HttpRangeError::InvalidContentRange)?;
    let end = end
        .parse::<u64>()
        .map_err(|_| HttpRangeError::InvalidContentRange)?;
    let total = total
        .parse::<u64>()
        .map_err(|_| HttpRangeError::InvalidContentRange)?;
    if start > end || end >= total {
        return Err(HttpRangeError::InvalidContentRange);
    }
    if start != expected_start
        || end != expected_end
        || known_total.is_some_and(|known| known != total)
    {
        return Err(HttpRangeError::ContentRangeMismatch);
    }
    Ok(total)
}

fn response_validators(response: &Response) -> Result<SourceValidators, HttpRangeError> {
    Ok(SourceValidators {
        etag: response
            .headers()
            .get(ETAG)
            .map(|value| {
                value
                    .to_str()
                    .map(str::to_owned)
                    .map_err(|_| HttpRangeError::SourceChanged)
            })
            .transpose()?,
        last_modified: response
            .headers()
            .get(LAST_MODIFIED)
            .map(|value| {
                value
                    .to_str()
                    .map(str::to_owned)
                    .map_err(|_| HttpRangeError::SourceChanged)
            })
            .transpose()?,
    })
}

fn parse_unsigned_header(value: &reqwest::header::HeaderValue) -> Result<u64, HttpRangeError> {
    let value = value
        .to_str()
        .map_err(|_| HttpRangeError::InvalidContentLength)?;
    value
        .parse::<u64>()
        .map_err(|_| HttpRangeError::InvalidContentLength)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;

    use crate::part_file::PartFile;
    use crate::storage::{SourceValidators, Storage};

    use super::{HttpRangeClient, HttpRangeError};

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

    struct TestFiles {
        part_path: PathBuf,
        final_path: PathBuf,
        database_path: PathBuf,
    }

    impl TestFiles {
        fn new() -> Self {
            let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
            let prefix = format!("raja-http-test-{}-{id}", std::process::id());
            let temp = std::env::temp_dir();
            Self {
                part_path: temp.join(format!("{prefix}.ndpart")),
                final_path: temp.join(format!("{prefix}.bin")),
                database_path: temp.join(format!("{prefix}.db")),
            }
        }

        fn setup(&self, size: u64) -> (Storage, PartFile, i64) {
            let mut storage = Storage::open(&self.database_path).unwrap();
            let download_id = storage
                .create_download("http://127.0.0.1/test", "file.bin", "downloads", Some(size))
                .unwrap();
            let segment_id = storage.create_segment(download_id, 0, size).unwrap();
            let part_file = PartFile::create(&self.part_path, size).unwrap();
            (storage, part_file, segment_id)
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

    fn serve_once(
        status: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let headers = headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect::<String>();
        let status = status.to_owned();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut one_byte = [0; 1];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut one_byte).unwrap();
                request.push(one_byte[0]);
            }
            let response = format!("HTTP/1.1 {status}\r\n{headers}Connection: close\r\n\r\n");
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
            stream.flush().unwrap();
            String::from_utf8(request).unwrap()
        });
        (format!("http://{address}/file"), handle)
    }

    #[test]
    fn retries_temporary_server_failures_and_completes_the_segment() {
        let paths = TestFiles::new();
        let (mut storage, part_file, segment_id) = paths.setup(4);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
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
                requests.push(String::from_utf8(request).unwrap());
                if attempt == 0 {
                    stream
                        .write_all(
                            b"HTTP/1.1 503 Service Unavailable\r\n\
                              Content-Length: 0\r\n\
                              Connection: close\r\n\r\n",
                        )
                        .unwrap();
                } else {
                    stream
                        .write_all(
                            b"HTTP/1.1 206 Partial Content\r\n\
                              Content-Range: bytes 0-3/4\r\n\
                              Content-Length: 4\r\n\
                              Connection: close\r\n\r\n\
                              data",
                        )
                        .unwrap();
                }
                stream.flush().unwrap();
            }
            requests
        });

        assert!(
            HttpRangeClient::new()
                .unwrap()
                .download_segment(
                    &format!("http://{address}/file"),
                    &part_file,
                    &mut storage,
                    segment_id,
                )
                .unwrap()
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| {
            request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("Range: bytes=0-3"))
        }));
        assert_eq!(storage.segment_status(segment_id).unwrap(), "complete");
    }

    #[test]
    fn retries_only_transient_http_errors() {
        assert!(HttpRangeError::UnexpectedStatus(429).is_retryable());
        assert!(HttpRangeError::UnexpectedStatus(503).is_retryable());
        assert!(!HttpRangeError::UnexpectedStatus(404).is_retryable());
        assert!(!HttpRangeError::SourceChanged.is_retryable());
        assert!(!HttpRangeError::ContentRangeMismatch.is_retryable());
    }

    #[test]
    fn probes_servers_that_return_a_full_response_instead_of_ranges() {
        let (url, server) = serve_once(
            "200 OK",
            &[("Content-Length", "12"), ("ETag", "\"v1\"")],
            b"hello world!".to_vec(),
        );

        let remote = HttpRangeClient::new().unwrap().probe(&url).unwrap();
        server.join().unwrap();
        assert_eq!(remote.size, 12);
        assert_eq!(remote.validators.etag.as_deref(), Some("\"v1\""));
    }

    #[test]
    fn refuses_full_response_probes_without_a_content_length() {
        let (url, server) = serve_once("200 OK", &[], b"data".to_vec());
        assert!(matches!(
            HttpRangeClient::new().unwrap().probe(&url),
            Err(HttpRangeError::MissingContentLength)
        ));
        server.join().unwrap();
    }

    #[test]
    fn full_response_fallback_restarts_from_zero_before_completing() {
        let paths = TestFiles::new();
        let (mut storage, part_file, segment_id) = paths.setup(4);
        part_file
            .write_segment(&mut storage, segment_id, 0, b"old!")
            .unwrap();
        let (url, server) = serve_once(
            "200 OK",
            &[("Content-Length", "4"), ("ETag", "\"v1\"")],
            b"new!".to_vec(),
        );

        assert!(
            HttpRangeClient::new()
                .unwrap()
                .download_segment(&url, &part_file, &mut storage, segment_id)
                .unwrap()
        );
        server.join().unwrap();
        assert_eq!(storage.segment_cursor(segment_id).unwrap(), 4);
        assert_eq!(storage.segment_status(segment_id).unwrap(), "complete");
        part_file
            .finalize(&mut storage, segment_id, &paths.final_path, None)
            .unwrap();
        assert_eq!(fs::read(&paths.final_path).unwrap(), b"new!");
    }

    #[test]
    fn refuses_full_response_fallback_for_multiple_segments() {
        let paths = TestFiles::new();
        let mut storage = Storage::open(&paths.database_path).unwrap();
        let download_id = storage
            .create_download("http://127.0.0.1/test", "file.bin", "downloads", Some(4))
            .unwrap();
        let first_segment = storage.create_segment(download_id, 0, 2).unwrap();
        storage.create_segment(download_id, 2, 4).unwrap();
        let part_file = PartFile::create(&paths.part_path, 4).unwrap();
        let (url, server) = serve_once("200 OK", &[("Content-Length", "4")], b"data".to_vec());

        assert!(matches!(
            HttpRangeClient::new().unwrap().download_segment(
                &url,
                &part_file,
                &mut storage,
                first_segment,
            ),
            Err(HttpRangeError::UnexpectedStatus(200))
        ));
        server.join().unwrap();
        assert_eq!(storage.segment_cursor(first_segment).unwrap(), 0);
    }

    #[test]
    fn downloads_only_the_requested_range_and_completes_the_file() {
        let paths = TestFiles::new();
        let (mut storage, part_file, segment_id) = paths.setup(12);
        let (url, server) = serve_once(
            "206 Partial Content",
            &[("Content-Range", "bytes 0-11/12"), ("Content-Length", "12")],
            b"hello world!".to_vec(),
        );

        assert!(
            HttpRangeClient::new()
                .unwrap()
                .download_segment(&url, &part_file, &mut storage, segment_id)
                .unwrap()
        );
        let request = server.join().unwrap();
        assert!(
            request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("Range: bytes=0-11"))
        );
        assert_eq!(storage.segment_status(segment_id).unwrap(), "complete");
        assert_eq!(storage.download_status(segment_id).unwrap(), "queued");

        part_file
            .finalize(
                &mut storage,
                segment_id,
                &paths.final_path,
                Some("7509e5bda0c762d2bac7f90d758b5b2263fa01ccbc542ab5e3df163be08e6ca9"),
            )
            .unwrap();
        let bytes = fs::read(&paths.final_path).unwrap();
        assert_eq!(bytes, b"hello world!");
        assert!(!paths.part_path.exists());
        assert_eq!(storage.download_status(segment_id).unwrap(), "complete");
    }

    #[test]
    fn falls_back_when_servers_ignore_ranges() {
        let paths = TestFiles::new();
        let (mut storage, part_file, segment_id) = paths.setup(4);
        let (url, server) = serve_once("200 OK", &[("Content-Length", "4")], b"data".to_vec());

        assert!(
            HttpRangeClient::new()
                .unwrap()
                .download_segment(&url, &part_file, &mut storage, segment_id)
                .unwrap()
        );
        server.join().unwrap();
        assert_eq!(storage.segment_status(segment_id).unwrap(), "complete");
    }

    #[test]
    fn rejects_a_mismatched_content_range_without_writing_bytes() {
        let paths = TestFiles::new();
        let (mut storage, part_file, segment_id) = paths.setup(4);
        let (url, server) = serve_once(
            "206 Partial Content",
            &[("Content-Range", "bytes 1-4/5"), ("Content-Length", "4")],
            b"data".to_vec(),
        );

        assert!(matches!(
            HttpRangeClient::new().unwrap().download_segment(
                &url,
                &part_file,
                &mut storage,
                segment_id
            ),
            Err(HttpRangeError::ContentRangeMismatch)
        ));
        server.join().unwrap();
        assert_eq!(storage.segment_cursor(segment_id).unwrap(), 0);
    }

    #[test]
    fn incomplete_response_does_not_mark_the_segment_complete() {
        let paths = TestFiles::new();
        let size = 70_000u64;
        let (mut storage, part_file, segment_id) = paths.setup(size);
        let (url, server) = serve_once(
            "206 Partial Content",
            &[
                ("Content-Range", "bytes 0-69999/70000"),
                ("Content-Length", "70000"),
            ],
            vec![b'x'; 65_000],
        );

        let result = HttpRangeClient::new().unwrap().download_segment(
            &url,
            &part_file,
            &mut storage,
            segment_id,
        );
        assert!(result.is_err());
        server.join().unwrap();
        assert!(storage.segment_cursor(segment_id).unwrap() < size);
        assert_eq!(storage.segment_status(segment_id).unwrap(), "active");
    }

    #[test]
    fn redownloads_a_fully_written_but_unverified_segment_after_reopen() {
        let paths = TestFiles::new();
        let (mut storage, part_file, segment_id) = paths.setup(4);
        part_file
            .write_segment(&mut storage, segment_id, 0, b"old!")
            .unwrap();
        assert_eq!(storage.segment_cursor(segment_id).unwrap(), 4);
        assert_eq!(storage.segment_status(segment_id).unwrap(), "active");

        let (url, server) = serve_once(
            "206 Partial Content",
            &[("Content-Range", "bytes 0-3/4"), ("Content-Length", "4")],
            b"new!".to_vec(),
        );
        assert!(
            HttpRangeClient::new()
                .unwrap()
                .download_segment(&url, &part_file, &mut storage, segment_id)
                .unwrap()
        );
        server.join().unwrap();
        assert_eq!(storage.segment_status(segment_id).unwrap(), "complete");

        part_file
            .finalize(&mut storage, segment_id, &paths.final_path, None)
            .unwrap();
        assert_eq!(fs::read(&paths.final_path).unwrap(), b"new!");
    }

    #[test]
    fn downloads_multiple_segments_before_marking_the_file_complete() {
        let paths = TestFiles::new();
        let mut storage = Storage::open(&paths.database_path).unwrap();
        let download_id = storage
            .create_download("http://127.0.0.1/test", "file.bin", "downloads", Some(12))
            .unwrap();
        let first = storage.create_segment(download_id, 0, 6).unwrap();
        let second = storage.create_segment(download_id, 6, 12).unwrap();
        let part_file = PartFile::create(&paths.part_path, 12).unwrap();

        let (url, server) = serve_once(
            "206 Partial Content",
            &[
                ("Content-Range", "bytes 0-5/12"),
                ("Content-Length", "6"),
                ("ETag", "\"v1\""),
            ],
            b"hello ".to_vec(),
        );
        assert!(
            !HttpRangeClient::new()
                .unwrap()
                .download_segment(&url, &part_file, &mut storage, first)
                .unwrap()
        );
        server.join().unwrap();

        let (url, server) = serve_once(
            "206 Partial Content",
            &[
                ("Content-Range", "bytes 6-11/12"),
                ("Content-Length", "6"),
                ("ETag", "\"v1\""),
            ],
            b"world!".to_vec(),
        );
        assert!(
            HttpRangeClient::new()
                .unwrap()
                .download_segment(&url, &part_file, &mut storage, second)
                .unwrap()
        );
        server.join().unwrap();

        part_file
            .finalize(&mut storage, second, &paths.final_path, None)
            .unwrap();
        assert_eq!(fs::read(&paths.final_path).unwrap(), b"hello world!");
    }

    #[test]
    fn resumes_with_if_range_and_rejects_a_changed_etag() {
        let paths = TestFiles::new();
        let (mut storage, part_file, segment_id) = paths.setup(8);
        part_file
            .write_segment(&mut storage, segment_id, 0, b"old-")
            .unwrap();
        storage
            .save_source_validators(
                segment_id,
                &SourceValidators {
                    etag: Some("\"v1\"".to_owned()),
                    last_modified: None,
                },
            )
            .unwrap();
        let (url, server) = serve_once(
            "206 Partial Content",
            &[
                ("Content-Range", "bytes 4-7/8"),
                ("Content-Length", "4"),
                ("ETag", "\"v2\""),
            ],
            b"new!".to_vec(),
        );

        assert!(matches!(
            HttpRangeClient::new().unwrap().download_segment(
                &url,
                &part_file,
                &mut storage,
                segment_id
            ),
            Err(HttpRangeError::SourceChanged)
        ));
        let request = server.join().unwrap();
        assert!(
            request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("Range: bytes=4-7"))
        );
        assert!(request.lines().any(|line| {
            let (name, value) = line.split_once(':').unwrap_or_default();
            name.eq_ignore_ascii_case("if-range") && value.trim() == "\"v1\""
        }));
        assert_eq!(storage.segment_cursor(segment_id).unwrap(), 4);

        let (url, server) = serve_once(
            "206 Partial Content",
            &[
                ("Content-Range", "bytes 4-7/8"),
                ("Content-Length", "4"),
                ("ETag", "\"v1\""),
            ],
            b"new!".to_vec(),
        );
        assert!(
            HttpRangeClient::new()
                .unwrap()
                .download_segment(&url, &part_file, &mut storage, segment_id)
                .unwrap()
        );
        server.join().unwrap();
        drop(part_file);
        assert_eq!(fs::read(&paths.part_path).unwrap(), b"old-new!");
    }

    #[test]
    fn refuses_multi_segment_download_when_server_has_no_validator() {
        let paths = TestFiles::new();
        let mut storage = Storage::open(&paths.database_path).unwrap();
        let download_id = storage
            .create_download("http://127.0.0.1/test", "file.bin", "downloads", Some(8))
            .unwrap();
        let segment_id = storage.create_segment(download_id, 0, 4).unwrap();
        storage.create_segment(download_id, 4, 8).unwrap();
        let part_file = PartFile::create(&paths.part_path, 8).unwrap();
        let (url, server) = serve_once(
            "206 Partial Content",
            &[("Content-Range", "bytes 0-3/8"), ("Content-Length", "4")],
            b"data".to_vec(),
        );

        assert!(matches!(
            HttpRangeClient::new().unwrap().download_segment(
                &url,
                &part_file,
                &mut storage,
                segment_id
            ),
            Err(HttpRangeError::MissingSourceValidator)
        ));
        server.join().unwrap();
        assert_eq!(storage.segment_cursor(segment_id).unwrap(), 0);
    }
}
