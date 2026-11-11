# RAJA Download Manager

**RAJA** (**Rapid Adaptive Job Accelerator**) is a fast, lightweight download manager for Windows 10/11, written in Rust. It pairs a crash-safe multi-connection HTTP/HTTPS engine with a classic dark desktop UI, video-page extraction, BitTorrent/magnet support and a Chrome/Edge/Brave browser extension.

## What makes the download engine different

Most download managers treat speed as the only goal. RAJA treats **"the file on disk is exactly the file on the server"** as a hard rule, and gets speed on top of that.

- **Multi-connection downloads.** A file is split into byte ranges that download in parallel (1–32 connections, 8 by default, at least 1 MB per segment). Splitting is used only when the server really supports ranges *and* gives a validator (ETag or Last-Modified); otherwise RAJA falls back to one safe connection instead of risking a mixed-up file. When a site redirects to mirrors, the redirect is resolved once and every connection uses that same mirror (also on resume).
- **Durable progress, not optimistic progress.** Every block is written to the part file and flushed to disk *before* its position is recorded. The download state lives in SQLite with WAL journaling and `synchronous = FULL`, so after a power cut or crash the shown progress never claims bytes that are not really on disk.
- **SHA-256 per block.** Each written block is hashed and the hash is stored. On resume, RAJA re-reads and re-checks those blocks; a segment whose data does not match is downloaded again rather than trusted.
- **Server-change detection.** Resumed ranges are requested with `If-Range` and the stored ETag/Last-Modified. If the file on the server has changed, the download stops with a clear error instead of stitching old and new bytes together.
- **Strict response checks.** `Content-Range` and `Content-Length` must match exactly what was asked for, and compressed (`Content-Encoding`) range replies are refused, so a misbehaving server or proxy cannot silently corrupt a segment.
- **Automatic retries.** Dropped connections, resets and timeouts are retried with exponential back-off (4 attempts per segment) without losing the bytes already saved. When a server says it is busy (HTTP 429/503), RAJA waits as long as the server asks (`Retry-After`, up to a minute) instead of hammering it.
- **Honest state after a crash.** Downloads that were running when Windows or RAJA shut down are shown as paused on the next start, never as still downloading.
- **Atomic finish.** The finished file is fully hashed (optionally checked against an expected SHA-256), then moved to its final name in one write-through rename. A finalization journal makes even a crash in the middle of this step recoverable on the next start; you never get a half-renamed file.
- **Sparse, locked part files.** The part file is pre-sized as a sparse file (no time wasted writing zeros) and locked while in use, so two downloads can never write into the same file.
- **Windows security marking.** Downloaded files get the standard "came from the internet" mark (Zone.Identifier), so Windows SmartScreen and Office Protected View treat them correctly.

## Features

- **Desktop UI:** resizable columns, categories (Compressed, Documents, Music, Programs, Video), queues, scheduler, system tray, and a right-click menu with `Open`, `Open with...`, `Open folder`, `Move/Rename`, `Redownload`, `Refresh download address`, `Remove` and `Properties`.
- **Themes:** View → Theme switches between Dark, Windows 11 Light, Aero Glass and Aero Glass Dark; the glass themes give the Windows 11 title bar a real acrylic (blurred glass) backdrop.
- **Progress window:** status, size, speed and time left, plus a per-connection view showing where each connection started and how far it has got.
- **Browser extension:** a **Download this video** quality picker (1080p–360p, MP4/MKV) over HTML5 video players, and a **Download with Raja** item in the right-click menu. Requests from the browser open a small download-info window without bringing up the whole app. It can also take over downloads the browser starts by itself (right-click the Raja toolbar icon to switch this off); if Raja cannot be reached, the browser keeps the download.
- **Video pages:** extraction through `yt-dlp` + `ffmpeg`, saved in the container you pick (MP4 or MKV), with the real size, speed and time left while video and audio streams download, and titles in any language (including Hindi) kept intact in the saved file name.
- **Torrents and magnet links:** through `aria2c`, with a file-tree picker to choose which files to download and their priority.
- **Updates:** Help → *Check for updates* (and a quiet check once a day) finds a newer release on GitHub, downloads the installer, checks its SHA-256 when GitHub publishes one, installs it silently and starts Raja again.

## Quick Start

### 1. Install or run
- **Installer:** run `Raja-0.2.8-Setup-x64.exe` (installs to `%LOCALAPPDATA%\Programs\Raja` and registers the browser native-messaging host).
- **Portable:** extract `Raja-0.2.8-windows-x64.zip` and run `raja-ui.exe`.

### 2. Load the browser extension (Chrome / Edge / Brave)
1. Open `chrome://extensions` (or `edge://extensions`) and turn on **Developer mode**.
2. Click **Load unpacked** and select `%LOCALAPPDATA%\Programs\Raja\browser-extension`.

### 3. Optional external tools (on `PATH`)
- [`yt-dlp`](https://github.com/yt-dlp/yt-dlp/releases/latest) and `ffmpeg` — for video pages.
- [`aria2c`](https://github.com/aria2/aria2/releases/latest) — for `.torrent` and `magnet:` downloads.

## Build from source

```powershell
cargo build --release --bins --locked
pwsh -File scripts/package-windows.ps1
& "$env:ProgramFiles(x86)\Inno Setup 6\ISCC.exe" /Otarget\installer installer\Raja.iss
```

## License

Licensed under **GPL-3.0-only** ([LICENSE](LICENSE)).
