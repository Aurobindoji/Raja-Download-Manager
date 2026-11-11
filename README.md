# RAJA Download Manager

**RAJA** (**Rapid Adaptive Job Accelerator**) is a fast, lightweight Windows 10/11 download manager built in Rust with an IDM-style dark desktop UI, multi-segment HTTP/HTTPS engine, video extractor (`yt-dlp`), BitTorrent/Magnet support (`aria2c`), and Chrome/Edge/Brave browser extension.

## Key Features

- **IDM-Style Desktop UI**: Resizable columns, category filters, download queues, scheduler, right-click context menu (`Open`, `Open with...`, `Open folder`, `Move/Rename`, `Redownload`, `Refresh download address`, `Remove`, `Properties`), and system tray integration.
- **Browser Extension**: Shows an instant **Download this video** format selector (`1080p`–`360p` MP4/MKV) over HTML5 video players and adds a **Download with Raja** right-click menu.
- **Torrent & Magnet Support**: Built-in `.torrent` file tree selector (`Torrent Contents`) with per-file selection and download priorities.
- **Safe Multi-Segment Core**: SQLite WAL state with SHA-256 block verification and atomic file promotion.

## Quick Start

### 1. Install or Run
- **Installer**: Run `Raja-0.2.0-Setup-x64.exe` (installs to `%LOCALAPPDATA%\Programs\Raja` and registers the browser native-messaging host).
- **Portable**: Extract `Raja-0.2.0-windows-x64.zip` and run `raja-ui.exe`.

### 2. Load Browser Extension (Chrome / Edge / Brave)
1. Open `chrome://extensions` (or `edge://extensions`) and enable **Developer mode**.
2. Click **Load unpacked** and select `%LOCALAPPDATA%\Programs\Raja\browser-extension`.

### 3. Optional External Tools (on `PATH`)
- [`yt-dlp`](https://github.com/yt-dlp/yt-dlp/releases/latest) & `ffmpeg` — for video page extraction.
- [`aria2c`](https://github.com/aria2/aria2/releases/latest) — for `.torrent` and `magnet:` downloads.

## Build from Source

```powershell
cargo build --release --bins --locked
pwsh -File scripts/package-windows.ps1
& "$env:ProgramFiles(x86)\Inno Setup 6\ISCC.exe" /Otarget\installer installer\Raja.iss
```

## License

Licensed under **GPL-3.0-only** ([LICENSE](LICENSE)).
