const MENU_ID = "raja-download";

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({
      id: MENU_ID,
      title: "Download with Raja",
      contexts: ["link", "selection", "page"]
    });
  });
});

chrome.contextMenus.onClicked.addListener((info) => {
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
  sendToRaja(message.url, kind, sendResponse, message.maxHeight);
  return true;
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

  chrome.runtime.sendNativeMessage(
    "com.raja.browser",
    { action: "list_formats", url: url.href },
    (response) => {
      if (chrome.runtime.lastError) {
        callback({ ok: false, message: chrome.runtime.lastError.message });
        return;
      }
      callback(response || { ok: false, message: "RAJA did not return video formats." });
    }
  );
}

function sendToRaja(candidate, kind, callback = () => {}, maxHeight) {
  const url = parseHttpUrl(candidate);
  if (!url) {
    callback({ ok: false, message: "Only HTTP or HTTPS links are supported." });
    return;
  }

  chrome.runtime.sendNativeMessage(
    "com.raja.browser",
    { action: "download", url: url.href, kind, max_height: maxHeight },
    (response) => {
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
