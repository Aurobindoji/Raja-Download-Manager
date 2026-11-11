const MENU_ID = "raja-download";
const CAPTURE_MENU_ID = "raja-capture-downloads";
const CAPTURE_KEY = "captureDownloads";
// Browser downloads smaller than this stay in the browser (icons, tiny files).
const MIN_CAPTURE_BYTES = 256 * 1024;
const formatCache = new Map();
// Links handed back to the browser because Raja could not take them.
const returnedToBrowser = new Set();

function createMenus() {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({
      id: MENU_ID,
      title: "Download with Raja",
      contexts: ["link", "selection", "page"]
    });
    chrome.storage.local.get({ [CAPTURE_KEY]: true }, (stored) => {
      // Right-click the Raja toolbar icon to switch this on or off.
      chrome.contextMenus.create({
        id: CAPTURE_MENU_ID,
        title: "Let Raja take over browser downloads",
        type: "checkbox",
        checked: stored[CAPTURE_KEY] !== false,
        contexts: ["action"]
      });
    });
  });
}

chrome.runtime.onInstalled.addListener(createMenus);
chrome.runtime.onStartup.addListener(createMenus);

chrome.contextMenus.onClicked.addListener((info) => {
  if (info.menuItemId === CAPTURE_MENU_ID) {
    chrome.storage.local.set({ [CAPTURE_KEY]: Boolean(info.checked) });
    return;
  }
  if (info.menuItemId !== MENU_ID) {
    return;
  }

  const url = info.linkUrl || info.selectionText || info.pageUrl;
  const kind = info.linkUrl ? "http" : "video";
  sendToRaja(url, kind);
});

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (
    message?.type !== "raja-download-page" &&
    message?.type !== "raja-download-link" &&
    message?.type !== "raja-list-video-formats"
  ) {
    return;
  }

  if (message.type === "raja-list-video-formats") {
    queryVideoFormats(message.url, sendResponse);
    return true;
  }

  const kind = message.type === "raja-download-link" ? "http" : "video";
  sendToRaja(message.url, kind, sendResponse, {
    maxHeight: message.maxHeight,
    title: message.title,
    container: message.container,
    description: message.description,
    fileSize: message.fileSize
  });
  return true;
});

// Take over downloads the browser starts by itself (like IDM): cancel the
// browser's copy and hand link, file name and size to Raja. If Raja cannot be
// reached, the browser downloads it after all.
chrome.downloads.onDeterminingFilename.addListener((item, suggest) => {
  suggest();
  const url = item.finalUrl || item.url;
  if (returnedToBrowser.has(url)) {
    returnedToBrowser.delete(url);
    return;
  }
  if (
    !parseHttpUrl(url) ||
    item.byExtensionId === chrome.runtime.id ||
    (item.totalBytes > 0 && item.totalBytes < MIN_CAPTURE_BYTES)
  ) {
    return;
  }
  chrome.storage.local.get({ [CAPTURE_KEY]: true }, (stored) => {
    if (stored[CAPTURE_KEY] === false) {
      return;
    }
    chrome.downloads.cancel(item.id, () => {
      chrome.downloads.erase({ id: item.id });
      const fileName = (item.filename || "").split(/[\\/]/).pop();
      sendToRaja(
        url,
        "http",
        (result) => {
          if (!result?.ok) {
            returnedToBrowser.add(url);
            chrome.downloads.download({ url });
          }
        },
        {
          fileName,
          fileSize: item.totalBytes > 0 ? item.totalBytes : undefined,
          noProtocolFallback: true
        }
      );
    });
  });
});

function parseHttpUrl(candidate) {
  let url;
  try {
    url = new URL(candidate);
  } catch {
    return null;
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    return null;
  }
  return url;
}

function queryVideoFormats(candidate, callback) {
  const url = parseHttpUrl(candidate);
  if (!url) {
    callback({ ok: false, message: "Only HTTP or HTTPS pages are supported." });
    return;
  }

  const cached = formatCache.get(url.href);
  if (cached && Date.now() - cached.timestamp < 5 * 60 * 1000) {
    callback(cached.response);
    return;
  }

  chrome.runtime.sendNativeMessage(
    "com.raja.browser",
    { action: "list_formats", url: url.href },
    (response) => {
      if (chrome.runtime.lastError) {
        callback({ ok: false, message: chrome.runtime.lastError.message });
        return;
      }
      const result = response || { ok: false, message: "RAJA did not return video formats." };
      if (result.ok) {
        formatCache.set(url.href, { timestamp: Date.now(), response: result });
      }
      callback(result);
    }
  );
}

function sendToRaja(candidate, kind, callback = () => {}, meta = {}) {
  const url = parseHttpUrl(candidate);
  if (!url) {
    callback({ ok: false, message: "Only HTTP or HTTPS links are supported." });
    return;
  }

  chrome.runtime.sendNativeMessage(
    "com.raja.browser",
    {
      action: "download",
      url: url.href,
      kind,
      max_height: meta.maxHeight,
      title: meta.title,
      container: meta.container,
      description: meta.description,
      file_size: meta.fileSize,
      file_name: meta.fileName
    },
    (response) => {
      if (chrome.runtime.lastError && meta.noProtocolFallback) {
        callback({ ok: false, message: chrome.runtime.lastError.message });
        return;
      }
      if (chrome.runtime.lastError) {
        const handoff = new URL("raja://download");
        handoff.searchParams.set("url", url.href);
        chrome.tabs.create({ url: handoff.href }, () => {
          callback({
            ok: !chrome.runtime.lastError,
            message: chrome.runtime.lastError?.message
          });
        });
        return;
      }
      callback(response || { ok: false, message: "RAJA did not accept the request." });
    }
  );
}
