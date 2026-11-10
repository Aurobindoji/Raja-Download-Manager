# Raja

**RAJA** stands for **Rapid Adaptive Job Accelerator**. Raja is a Windows
download manager with a Rust download core, CLI, desktop UI,
Chrome/Edge context-menu extension, portable ZIP packaging, and an unsigned
Windows installer. Direct HTTP/HTTPS files and supported video pages can be
downloaded; code signing is not included.

## Initial decisions

- Implement a new download engine and use only code with compatible licensing.
- License this implementation under GPL-3.0-only (GNU GPL version 3).
- Keep planning logic deterministic and independent of I/O so it can be tested
  separately from download and storage behavior.

## Current status

The core includes a pure segment planner. It repeatedly assigns the upper half
of the largest remaining byte range, stopping when another split would
produce a range smaller than the configured minimum or leave no lower range
to schedule.

Persistent state uses SQLite in WAL mode with `synchronous=FULL`. The initial
schema tracks downloads, sources, segments, verified blocks, events, host
limits, and settings. Segment cursors may only be recorded after the matching
part-file bytes have been flushed to disk.

The Windows core creates sparse `.ndpart` files and holds an exclusive file
lock while a download owns one. Writes are serialized; each segment write must
start at its durable cursor, and SQLite advances only after the part file is
flushed successfully. Each durable write stores a SHA-256 block record in the
same SQLite transaction as its cursor. On restart, Raja verifies every saved
block before resuming; a corrupt block or legacy cursor without hash records
resets only that segment for safe redownload.

The HTTP client accepts validated `206 Partial Content` responses with an exact
`Content-Range`, identity encoding, and the expected body length. For servers
that ignore ranges, it supports a single full-file `200 OK` response only when
`Content-Length` is present and the transfer is one segment; it restarts that
segment from byte zero rather than combining old and new content. Both paths
stream data in bounded chunks, keep the final chunk uncommitted until the
response reaches EOF, and mark a segment complete only after the entire
response is verified. Transient connection/time-out failures and HTTP 408, 425,
429, and 5xx responses receive up to four retries with exponential backoff.
Validation failures, source changes, and local storage errors are not retried.
ETag/Last-Modified validators are persisted and used with `If-Range`; a
multi-segment download is refused if the server offers no validator, so
segments from different versions of a file are not combined.

A download is not marked complete until finalization verifies the whole part
file's size and optional SHA-256, atomically promotes it to a new filename in
the same directory without overwriting an existing file, and then records the
completed state in SQLite. On Windows, the part file receives a Mark of the
Web zone identifier before promotion. SQLite records the intended paths, size,
and SHA-256 before promotion; on restart, pending finalizations verify either
the part file or already-renamed final file and safely finish the database
update.

## Desktop UI

Build both executables and launch the GUI:

```powershell
cargo build --release --bins --locked
.\target\release\raja-ui.exe
```

The desktop UI shares the CLI's SQLite state. It has a dark menu and toolbar,
download-category sidebar, and a selectable downloads table with durable
progress. Categories are inferred from file extensions; unfinished and
completed downloads can also be filtered. The Add URL dialog accepts a
destination (with a native Save As dialog and a filename suggested from the
URL) and optionally verifies an expected SHA-256. Pause, resume, refresh, and
opening a completed download's location in Explorer are available for
selected downloads. Completed entries can be removed from history after
confirmation; this never deletes the downloaded file. Start Queue serially
resumes queued, paused, and failed downloads in oldest-first order; an error
pauses the queue. Stop Queue pauses its current item, and Stop All requests a
pause for every active download. The Scheduler can start the queue after
5–240 minutes, but Raja must remain open and Windows must stay awake; scheduled
times are not persisted across app exits. The Options dialog saves a default
download directory in Raja's SQLite settings. Time-left and transfer-rate
values are estimated from durable progress while the UI is open; they are
initially unknown and are not persisted. Last-attempt time is stored and shown
in UTC. Keep `raja-ui.exe` beside `raja-core.exe`.

## Browser integration

The included Chrome/Edge extension shows **Download this video** over detected
HTML5 video players and adds a context-menu item for links and pages. Clicking
the video button lists available resolutions up to 1080p; choosing one starts
video extraction to RAJA's default download folder. Choosing a link from the
context menu starts a direct download. The installer registers a browser
native-messaging bridge, so the browser does not open a "website wants to open
this application" prompt.

For an installer-based install, RAJA installs its native-messaging bridge and
starts in the Windows system tray. Closing the main window hides it in the
tray; use the tray menu to show or exit RAJA. The app starts automatically
when the current Windows user signs in. The browser extension itself still
has to be loaded in Chrome/Edge:

1. In Chrome open `chrome://extensions`, or in Edge open `edge://extensions`.
   Enable Developer mode, select **Load unpacked**, and choose the
   `browser-extension` folder under the Raja install directory.
2. Reload the extension on the browser extensions page and refresh the page.
   The **Download this video** button appears over a detected video player.
   Allow the extension's native-messaging permission if the browser asks.
   Click the button to choose a detected resolution. Right-click a link or
   page to use the context-menu option instead.

For a portable ZIP install, extract it to a folder that will stay in place and
run `Install-RajaBrowserIntegration.ps1` from that folder to register the
`raja:` protocol for the current Windows user. Then load its `browser-extension`
folder as an unpacked extension using the steps above. The no-prompt native
messaging bridge and tray startup are configured by the conventional
installer, not by the portable package; the portable extension falls back to
the `raja:` protocol and the browser may ask permission to open RAJA.

To remove integration, remove the extension in the browser. The installer
removes the protocol registration on uninstall if it still points to Raja;
for a portable install, run `Uninstall-RajaBrowserIntegration.ps1`. The
protocol handler can also be invoked by other applications, but Raja validates
the request and only opens its confirmation dialog.

### Video pages

The Add URL dialog can optionally extract a video page using
[yt-dlp](https://github.com/yt-dlp/yt-dlp/releases/latest). Install yt-dlp
separately and make `yt-dlp.exe` available on `PATH`; Raja does not bundle or
install it. `ffmpeg` on `PATH` is recommended for combining separate
audio/video streams. Raja asks yt-dlp to
download one item (not a playlist), continues a partial transfer when rerun,
prefers a combined format up to 1080p to avoid unexpectedly selecting very
large 4K files, and uses the filename stem you choose while letting yt-dlp
select the final extension. Stop All terminates active video extraction; rerun
the same URL and destination to continue when the site and extractor support
resuming.

Video extraction is performed by the separate yt-dlp program, so it is not
included in Raja's byte-range engine, but its URL, status, progress estimate,
and final path are recorded in Raja's SQLite download history. Video jobs can
be queued, resumed, forgotten from history, and stopped; Stop All and choosing
Exit from the system-tray menu terminate yt-dlp and its child processes so the
job can be resumed later. Closing the main window only hides RAJA in the tray.
The estimated size and downloaded-byte progress come from yt-dlp and may not
be available for every site. Site support can change; DRM-protected content is
not supported or bypassed. Only download content you are authorized to save
and follow the source site's terms. yt-dlp and ffmpeg have their own licenses
and are not included in the Raja installer or portable ZIP.

Create a portable Windows x64 ZIP after building all binaries:

```powershell
cargo build --release --bins --locked
pwsh -File scripts/package-windows.ps1
```

For a conventional per-user Windows installer, build the release binaries and
compile `installer\Raja.iss` with Inno Setup 6:

```powershell
& "$env:ProgramFiles(x86)\Inno Setup 6\ISCC.exe" /Otarget\installer installer\Raja.iss
```

The installer places Raja under
`%LOCALAPPDATA%\Programs\Raja`, offers optional Start Menu/desktop shortcuts,
and registers the `raja:` protocol for the current user directly through
Windows registry entries. If another application owns that protocol, setup
asks before replacing it. On uninstall, Raja removes the handler only if it
still points to Raja. Setup does not launch PowerShell. The installer is not
code-signed; Windows may show its normal publisher warning.

The archive is written to `target\package` and includes the app executables,
browser extension, per-user protocol install/uninstall scripts, this README,
and the GPL-3.0-only license. Extract the ZIP and run `raja-ui.exe`; keep
`raja-core.exe` and `raja-browser-host.exe` in the same folder. CI publishes the ZIP as the
`raja-windows-x64` workflow artifact. Push a `v*` tag to create a GitHub
Release containing the portable ZIP and setup executable. The tag must match
the Cargo version (currently `v0.1.0`).

## CLI

The CLI supports one HTTP/HTTPS file per invocation:

```powershell
cargo run -- download https://example.com/archive.zip D:\Downloads\archive.zip
cargo run -- download https://example.com/archive.zip D:\Downloads\archive.zip --sha256 <64-hex-digits>
cargo run -- list
cargo run -- pause 17
cargo run -- retry 17
cargo run -- forget 17
```

By default, the database is stored under `%LOCALAPPDATA%\Raja\raja.db` on
Windows. Pass `--database <path>` to choose another database location. Each startup recovers pending finalizations before starting a new download.
If the output has an unfinished Raja record and matching `.ndpart` file, the
CLI checks the URL, size, and saved server validators, then resumes from the
last durable cursor when the server supports ranges. Validator-backed files
larger than 1 MiB are split into up to four ranges and downloaded concurrently.
Full-response fallback restarts from byte zero after interruption and stays on
one connection. This prototype requires a non-empty file and a known
`Content-Length`. An optional SHA-256 requirement is persisted with the
download, enforced on resume even if the flag is omitted, and cannot be
silently replaced with a different hash. Active downloads report durable
progress and measured transfer rate; `list` shows each of the latest 100
downloads with its persisted byte progress. Failed transfers retain their
error message in the database event log and can be retried by issuing the same
download command or `retry <id>`; retry reuses the saved URL, destination, and
SHA-256 requirement. Use the ID from `list` with `pause`; the transfer stops
at its next durable write, and rerunning the original download command resumes it.
Use `forget <id>` to remove a completed download from Raja's history without
deleting the downloaded file; incomplete downloads cannot be forgotten.

## Development

```powershell
cargo test --workspace --locked
```

The supported product target is Windows 10/11. The planner itself has no
platform-specific dependencies, so its unit tests can run on any Rust host.
