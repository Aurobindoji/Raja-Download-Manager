//! Link Grabber helpers: find every link in pasted text, name it, and check
//! the links (size, name, type) a few at a time in the background.

use std::sync::Mutex;
use std::sync::mpsc::Sender;

/// At most this many links are taken from one paste or page.
pub const MAX_LINKS: usize = 5000;
/// Links checked at the same time.
const CHECK_WORKERS: usize = 6;

/// Every distinct `http://` / `https://` link in `text`, in order. Trailing
/// punctuation that is usually part of the sentence, not the link, is cut.
pub fn extract_links(text: &str) -> Vec<String> {
    let mut links: Vec<String> = Vec::new();
    let mut rest = text;
    while links.len() < MAX_LINKS {
        let Some(start) = find_scheme(rest) else {
            break;
        };
        let candidate = &rest[start..];
        let end = candidate
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`'))
            .unwrap_or(candidate.len());
        let link = candidate[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}']);
        if link.len() > "https://".len() && !links.iter().any(|known| known == link) {
            links.push(link.to_owned());
        }
        rest = &candidate[end.max(1)..];
    }
    links
}

fn find_scheme(text: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    match (lower.find("http://"), lower.find("https://")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// A readable file name from the link itself: its last path part, decoded.
pub fn file_name_from_url(url: &str) -> String {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return "download".to_owned();
    };
    let last = parsed
        .path_segments()
        .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
        .map(percent_decode_lossy)
        .filter(|name| !name.trim().is_empty());
    last.or_else(|| parsed.host_str().map(str::to_owned))
        .unwrap_or_else(|| "download".to_owned())
}

fn percent_decode_lossy(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|pair| std::str::from_utf8(pair).ok())
            .and_then(|pair| u8::from_str_radix(pair, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Checks `links` with a few parallel workers; each result is sent as soon
/// as it is known, so the grabber list fills in while the user looks at it.
pub fn check_links<T: Send + 'static>(
    links: Vec<String>,
    check: fn(&str) -> T,
    results: Sender<(String, T)>,
) {
    std::thread::spawn(move || {
        let queue = Mutex::new(links.into_iter());
        std::thread::scope(|scope| {
            for _ in 0..CHECK_WORKERS {
                let results = results.clone();
                let queue = &queue;
                scope.spawn(move || {
                    loop {
                        let next = queue.lock().ok().and_then(|mut queue| queue.next());
                        let Some(link) = next else {
                            break;
                        };
                        let outcome = check(&link);
                        if results.send((link, outcome)).is_err() {
                            break;
                        }
                    }
                });
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_every_link_once_and_trims_sentence_punctuation() {
        let text = "Get it here: https://example.com/a.zip, or (http://mirror.test/b.iso).\n\
                    Again https://example.com/a.zip and \"https://x.test/c d\" end.";
        assert_eq!(
            extract_links(text),
            vec![
                "https://example.com/a.zip",
                "http://mirror.test/b.iso",
                "https://x.test/c",
            ]
        );
        assert!(extract_links("no links here, just https:// text").is_empty());
    }

    #[test]
    fn names_files_from_their_links() {
        assert_eq!(
            file_name_from_url("https://cdimage.kali.org/kali/kali-linux-2026-installer-amd64.iso"),
            "kali-linux-2026-installer-amd64.iso"
        );
        assert_eq!(
            file_name_from_url("https://example.com/files/My%20Report%202026.pdf?x=1"),
            "My Report 2026.pdf"
        );
        assert_eq!(file_name_from_url("https://example.com/"), "example.com");
    }

    #[test]
    fn checks_every_link_in_the_background() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let links: Vec<String> = (0..20).map(|i| format!("https://t.test/{i}")).collect();
        check_links(links, |link: &str| link.len(), sender);
        let mut seen: Vec<String> = receiver.iter().map(|(link, _)| link).collect();
        seen.sort();
        assert_eq!(seen.len(), 20);
    }
}
