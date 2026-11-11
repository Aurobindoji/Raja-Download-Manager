(() => {
  const hostId = "raja-download-floating-button";
  const existingHost = document.getElementById(hostId);
  if (existingHost) {
    existingHost.remove();
  }

  const host = document.createElement("div");
  host.id = hostId;
  const shadow = host.attachShadow({ mode: "closed" });
  const style = document.createElement("style");
  style.textContent = `
    :host {
      all: initial;
      position: fixed;
      left: 0;
      top: 0;
      z-index: 2147483647;
      display: none;
      font-family: "Segoe UI", Tahoma, Arial, sans-serif;
    }
    .floating-bar {
      display: inline-flex;
      align-items: stretch;
      border: 1px solid rgba(255, 255, 255, 0.45);
      border-radius: 5px;
      background: #e06a2b;
      box-shadow: 0 2px 8px rgba(0, 0, 0, 0.45);
      overflow: hidden;
      user-select: none;
    }
    .main-btn {
      box-sizing: border-box;
      display: inline-flex;
      align-items: center;
      gap: 7px;
      border: 0;
      padding: 6px 11px;
      background: transparent;
      color: #ffffff;
      font: 600 13px/1.2 "Segoe UI", Arial, sans-serif;
      cursor: pointer;
    }
    .main-btn:hover {
      background: #c9591d;
    }
    .play-icon {
      display: inline-block;
      width: 0;
      height: 0;
      border-top: 5px solid transparent;
      border-bottom: 5px solid transparent;
      border-left: 9px solid #63d471;
    }
    .bar-close-btn {
      box-sizing: border-box;
      display: inline-flex;
      align-items: center;
      justify-content: center;
      width: 24px;
      border: 0;
      border-left: 1px solid rgba(255, 255, 255, 0.3);
      padding: 0;
      background: rgba(0, 0, 0, 0.12);
      color: #ffffff;
      font: 700 12px/1 Arial, sans-serif;
      cursor: pointer;
    }
    .bar-close-btn:hover {
      background: #b83227;
      color: #ffffff;
    }
    .formats {
      position: relative;
      box-sizing: border-box;
      min-width: 460px;
      max-width: 640px;
      margin-top: 5px;
      padding: 6px 0 6px;
      border: 1px solid #555;
      border-radius: 4px;
      background: #222326;
      color: #f0f0f0;
      box-shadow: 0 4px 16px rgba(0, 0, 0, 0.65);
      font: 12.5px/1.35 "Segoe UI", Tahoma, Arial, sans-serif;
    }
    .formats[hidden] {
      display: none;
    }
    .popup-header {
      display: flex;
      align-items: center;
      justify-content: space-between;
      padding: 2px 8px 6px 14px;
      border-bottom: 1px solid #3c3c3e;
      margin-bottom: 3px;
    }
    .download-all-btn {
      border: 0;
      padding: 3px 8px;
      border-radius: 3px;
      background: transparent;
      color: #f0f0f0;
      text-align: left;
      font: 600 12.5px/1.2 "Segoe UI", Tahoma, Arial, sans-serif;
      cursor: pointer;
    }
    .download-all-btn:hover {
      background: #37373d;
    }
    .popup-close-btn {
      box-sizing: border-box;
      display: inline-flex;
      align-items: center;
      justify-content: center;
      width: 22px;
      height: 22px;
      border: 1px solid #555;
      border-radius: 3px;
      background: #2f3034;
      color: #e0e0e0;
      font: 700 12px/1 Arial, sans-serif;
      cursor: pointer;
    }
    .popup-close-btn:hover {
      background: #c42b1c;
      border-color: #e04332;
      color: #ffffff;
    }
    .quality-option {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 14px;
      width: 100%;
      border: 0;
      padding: 5px 14px 5px 22px;
      background: transparent;
      color: #f0f0f0;
      text-align: left;
      font: inherit;
      cursor: pointer;
      white-space: nowrap;
    }
    .quality-option:hover {
      background: #094771;
      color: #ffffff;
    }
    .quality-option:disabled {
      opacity: 0.75;
      cursor: default;
    }
    .opt-title {
      overflow: hidden;
      text-overflow: ellipsis;
      max-width: 270px;
    }
    .opt-meta {
      flex-shrink: 0;
      color: #e6e6e6;
    }
  `;

  const bar = document.createElement("div");
  bar.className = "floating-bar";

  const button = document.createElement("button");
  button.type = "button";
  button.className = "main-btn";
  button.setAttribute("aria-label", "Download this video with RAJA");
  const playIcon = document.createElement("span");
  playIcon.className = "play-icon";
  const btnLabel = document.createElement("span");
  btnLabel.textContent = "Download this video";
  button.append(playIcon, btnLabel);

  const closeBarBtn = document.createElement("button");
  closeBarBtn.type = "button";
  closeBarBtn.className = "bar-close-btn";
  closeBarBtn.textContent = "✕";
  closeBarBtn.title = "Close download button";

  bar.append(button, closeBarBtn);

  const formatsMenu = document.createElement("div");
  formatsMenu.className = "formats";
  formatsMenu.hidden = true;

  let dismissedForUrl = null;
  let prefetchedUrl = null;
  const urlFormatsCache = new Map();

  closeBarBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    dismissedForUrl = window.location.href;
    formatsMenu.hidden = true;
    host.style.display = "none";
  });

  function getFullVideoTitle() {
    const raw =
      document.querySelector("h1.ytd-watch-metadata yt-formatted-string")?.textContent ||
      document.querySelector('meta[property="og:title"]')?.getAttribute("content") ||
      document.title ||
      "Video";
    return raw.replace(/\s*-\s*YouTube$/i, "").trim() || "Video";
  }

  function getCleanVideoTitle() {
    const cleaned = getFullVideoTitle();
    return cleaned.length > 38 ? `${cleaned.slice(0, 38)}...` : cleaned;
  }

  function estimateFormatSizeBytes(height, durationSec, sizeByHeight) {
    if (sizeByHeight && sizeByHeight.has(height)) {
      return sizeByHeight.get(height);
    }
    const dur = Number.isFinite(durationSec) && durationSec > 0 ? durationSec : 240;
    const kbpsByHeight = {
      1080: 2150,
      720: 950,
      480: 540,
      360: 340,
      240: 210,
      144: 130
    };
    const kbps = kbpsByHeight[height] || 650;
    return Math.round((kbps * 1000 / 8) * dur);
  }

  function extractInPageFormats(activeVideoEl) {
    const heightsSet = new Set();
    const sizeByHeight = new Map();
    let hasHdr = false;

    try {
      const scripts = document.querySelectorAll("script");
      for (const script of scripts) {
        const text = script.textContent || "";
        if (!text.includes("adaptiveFormats") && !text.includes("ytInitialPlayerResponse")) {
          continue;
        }
        const heightMatches = text.matchAll(/"height"\s*:\s*(\d{3,4})/g);
        for (const match of heightMatches) {
          const h = Number(match[1]);
          if ([144, 240, 360, 480, 720, 1080].includes(h)) {
            heightsSet.add(h);
          }
        }
        const sizeMatches = text.matchAll(
          /"contentLength"\s*:\s*"(\d+)"[^{}]{0,260}?"height"\s*:\s*(\d{3,4})|"height"\s*:\s*(\d{3,4})[^{}]{0,260}?"contentLength"\s*:\s*"(\d+)"/g
        );
        for (const match of sizeMatches) {
          const len = Number(match[1] || match[4]);
          const h = Number(match[2] || match[3]);
          if ([144, 240, 360, 480, 720, 1080].includes(h) && len > 0) {
            const audioAdd = Math.round(len * 0.14);
            const total = len + audioAdd;
            if (!sizeByHeight.has(h) || total > sizeByHeight.get(h)) {
              sizeByHeight.set(h, total);
            }
          }
        }
        if (
          text.includes("COLOR_TRANSFER_SMPTEST2084") ||
          text.includes("COLOR_TRANSFER_ARIB_STD_B67") ||
          text.includes("HDR")
        ) {
          hasHdr = true;
        }
      }
    } catch {
      // Ignore script scan errors
    }

    if (activeVideoEl && activeVideoEl.videoHeight >= 144) {
      for (const h of [1080, 720, 480, 360, 240, 144]) {
        if (h <= Math.max(activeVideoEl.videoHeight, 1080)) {
          heightsSet.add(h);
        }
      }
    }

    if (heightsSet.size === 0) {
      for (const h of [1080, 720, 480, 360, 240, 144]) {
        heightsSet.add(h);
      }
    }

    const sortedHeights = Array.from(heightsSet)
      .filter((h) => Number.isInteger(h) && h >= 144 && h <= 1080)
      .sort((a, b) => b - a);

    return buildFormatEntries(sortedHeights, hasHdr, activeVideoEl?.duration, sizeByHeight);
  }

  function buildFormatEntries(heights, includeHdr = true, durationSec = 240, sizeByHeight = null) {
    const validHeights = heights
      .filter((h) => Number.isInteger(h) && h >= 144 && h <= 1080)
      .sort((a, b) => b - a);
    const list = [];
    for (const h of validHeights) {
      const hdTag = h >= 720 ? " HD" : "";
      const baseBytes = estimateFormatSizeBytes(h, durationSec, sizeByHeight);
      list.push({
        height: h,
        container: "MP4",
        label: `MP4 file, quality ${h}p${hdTag}`,
        fileSize: baseBytes
      });
      if (includeHdr) {
        list.push({
          height: h,
          container: "MKV",
          label: `MKV file, quality ${h}p${hdTag}, HDR`,
          fileSize: Math.round(baseBytes * 1.06)
        });
      }
    }
    return list;
  }

  // Sends a message to the extension service worker without ever leaving the
  // caller hanging: handles an invalidated extension context (after the
  // extension was updated/reloaded) and a missing response via a timeout.
  function sendRuntimeMessage(message, callback, timeoutMs = 20000) {
    let finished = false;
    const finish = (result) => {
      if (finished) return;
      finished = true;
      window.clearTimeout(timer);
      callback(result);
    };
    const timer = window.setTimeout(
      () => finish({ ok: false, message: "RAJA did not respond. Refresh the page." }),
      timeoutMs
    );
    try {
      if (!chrome.runtime?.id) {
        throw new Error("Extension context invalidated");
      }
      chrome.runtime.sendMessage(message, (response) => {
        const error = chrome.runtime.lastError;
        if (error) {
          finish({ ok: false, message: error.message || "Could not reach RAJA." });
          return;
        }
        finish(response || { ok: false, message: "RAJA did not respond." });
      });
    } catch (_error) {
      finish({ ok: false, message: "RAJA extension updated - refresh this page (F5)." });
    }
  }

  function prefetchFormatsInBackground(url) {
    if (!url || prefetchedUrl === url || urlFormatsCache.has(url)) {
      return;
    }
    prefetchedUrl = url;
    sendRuntimeMessage(
      { type: "raja-list-video-formats", url },
      (response) => {
        if (!response?.ok || !Array.isArray(response.heights)) {
          return;
        }
        const activeVideo = getActiveVideo();
        const entries = buildFormatEntries(response.heights, true, activeVideo?.video?.duration);
        if (entries.length > 0) {
          urlFormatsCache.set(url, entries);
          if (!formatsMenu.hidden && window.location.href === url) {
            renderFormatMenu(url, entries);
          }
        }
      }
    );
  }

  function startDownload(url, entry, rowEl, originalTitle, originalMeta) {
    const height = entry?.height || 1080;
    const container = entry?.container || "MP4";
    const description = entry?.label || `MP4 file, quality ${height}p`;
    const fileSize = entry?.fileSize || estimateFormatSizeBytes(height, 240, null);
    const fullTitle = getFullVideoTitle();

    if (rowEl) {
      rowEl.disabled = true;
      const metaSpan = rowEl.querySelector(".opt-meta");
      if (metaSpan) {
        metaSpan.textContent = `Opening in RAJA...`;
      }
    }
    sendRuntimeMessage(
      {
        type: "raja-download-page",
        url,
        maxHeight: height,
        title: fullTitle,
        container,
        description,
        fileSize
      },
      (result) => {
        const sendFailed = !result?.ok;
        if (rowEl) {
          const metaSpan = rowEl.querySelector(".opt-meta");
          if (metaSpan) {
            metaSpan.textContent = sendFailed
              ? result?.message || "Could not open RAJA"
              : `Opened in RAJA`;
          }
        }
        window.setTimeout(() => {
          formatsMenu.hidden = true;
          if (rowEl) {
            rowEl.disabled = false;
            const titleSpan = rowEl.querySelector(".opt-title");
            const metaSpan = rowEl.querySelector(".opt-meta");
            if (titleSpan) titleSpan.textContent = originalTitle;
            if (metaSpan) metaSpan.textContent = originalMeta;
          }
        }, sendFailed ? 4000 : 450);
      }
    );
  }

  function renderFormatMenu(url, entries) {
    formatsMenu.replaceChildren();
    const title = getCleanVideoTitle();

    const header = document.createElement("div");
    header.className = "popup-header";

    const downloadAll = document.createElement("button");
    downloadAll.type = "button";
    downloadAll.className = "download-all-btn";
    downloadAll.textContent = "Download all";
    downloadAll.addEventListener("click", () => {
      const topEntry = entries[0] || { height: 1080, container: "MP4", label: "MP4 file, quality 1080p HD" };
      startDownload(url, topEntry, null, "", "");
    });

    const closePopupBtn = document.createElement("button");
    closePopupBtn.type = "button";
    closePopupBtn.className = "popup-close-btn";
    closePopupBtn.textContent = "✕";
    closePopupBtn.title = "Close popup";
    closePopupBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      formatsMenu.hidden = true;
    });

    header.append(downloadAll, closePopupBtn);
    formatsMenu.append(header);

    entries.forEach((entry, index) => {
      const option = document.createElement("button");
      option.type = "button";
      option.className = "quality-option";

      const leftText = `${index + 1}.  ${title}`;
      const rightText = entry.label;

      const titleSpan = document.createElement("span");
      titleSpan.className = "opt-title";
      titleSpan.textContent = leftText;

      const metaSpan = document.createElement("span");
      metaSpan.className = "opt-meta";
      metaSpan.textContent = rightText;

      option.append(titleSpan, metaSpan);
      option.addEventListener("click", () => {
        startDownload(url, entry, option, leftText, rightText);
      });
      formatsMenu.append(option);
    });
  }

  button.addEventListener("click", (e) => {
    e.stopPropagation();
    if (!formatsMenu.hidden) {
      formatsMenu.hidden = true;
      return;
    }
    const url = window.location.href;
    const activeVideo = getActiveVideo();
    const entries = urlFormatsCache.get(url) || extractInPageFormats(activeVideo?.video);
    renderFormatMenu(url, entries);
    formatsMenu.hidden = false;
    prefetchFormatsInBackground(url);
  });

  document.addEventListener("click", (e) => {
    if (!formatsMenu.hidden && !e.composedPath().includes(host)) {
      formatsMenu.hidden = true;
    }
  });

  // "Grab all links with Raja": every distinct http(s) link and media source
  // on the page, for Raja's Link Grabber.
  chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
    if (message?.type !== "raja-collect-links") {
      return;
    }
    const seen = new Set();
    const links = [];
    const add = (value) => {
      if (!value || links.length >= 5000) return;
      let url;
      try {
        url = new URL(value, document.baseURI);
      } catch {
        return;
      }
      if (url.protocol !== "http:" && url.protocol !== "https:") return;
      url.hash = "";
      if (!seen.has(url.href)) {
        seen.add(url.href);
        links.push(url.href);
      }
    };
    document.querySelectorAll("a[href]").forEach((a) => add(a.getAttribute("href")));
    document
      .querySelectorAll("video[src], audio[src], source[src]")
      .forEach((media) => add(media.getAttribute("src")));
    sendResponse({ links });
  });

  shadow.append(style, bar, formatsMenu);
  document.documentElement.append(host);

  const videos = [];
  function getActiveVideo() {
    return videos
      .filter((video) => video.isConnected)
      .map((video) => ({ video, rect: video.getBoundingClientRect() }))
      .filter(
        ({ video, rect }) =>
          video.readyState > 0 &&
          rect.width >= 240 &&
          rect.height >= 135 &&
          rect.bottom > 0 &&
          rect.right > 0 &&
          rect.top < window.innerHeight &&
          rect.left < window.innerWidth
      )
      .sort((a, b) => b.rect.width * b.rect.height - a.rect.width * a.rect.height)[0];
  }

  const updatePosition = () => {
    if (dismissedForUrl === window.location.href) {
      host.style.display = "none";
      formatsMenu.hidden = true;
      return;
    }

    const activeVideo = getActiveVideo();
    if (!activeVideo) {
      host.style.display = "none";
      formatsMenu.hidden = true;
      return;
    }

    host.style.display = "block";
    host.style.left = `${Math.max(4, activeVideo.rect.left + 12)}px`;
    host.style.top = `${Math.max(4, activeVideo.rect.top + 12)}px`;
    prefetchFormatsInBackground(window.location.href);
  };

  const scanVideos = () => {
    for (const video of document.querySelectorAll("video")) {
      if (!videos.includes(video)) {
        videos.push(video);
        video.addEventListener("loadeddata", updatePosition);
        video.addEventListener("play", updatePosition);
      }
    }
    updatePosition();
  };

  window.addEventListener("scroll", updatePosition, true);
  window.addEventListener("resize", updatePosition);
  new MutationObserver(scanVideos).observe(document.documentElement, {
    childList: true,
    subtree: true
  });
  scanVideos();
  window.setInterval(scanVideos, 1200);
})();
