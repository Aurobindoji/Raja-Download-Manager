(() => {
  const hostId = "raja-download-floating-button";
  if (document.getElementById(hostId)) {
    return;
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
    }
    button {
      box-sizing: border-box;
      display: inline-flex;
      align-items: center;
      gap: 7px;
      border: 1px solid rgba(255, 255, 255, 0.4);
      border-radius: 4px;
      padding: 7px 12px;
      background: #f26722;
      color: #fff;
      box-shadow: 0 2px 8px rgba(0, 0, 0, 0.4);
      font: 600 13px/1.2 Arial, sans-serif;
      cursor: pointer;
    }
    button::before {
      content: "▶";
      color: #63d471;
      font-size: 13px;
    }
    button:hover {
      background: #dc5514;
    }
    button:focus-visible {
      outline: 3px solid #fff;
      outline-offset: 3px;
    }
    button:disabled {
      cursor: wait;
      opacity: 0.8;
    }
    .formats {
      box-sizing: border-box;
      min-width: 190px;
      margin-top: 6px;
      padding: 8px;
      border: 1px solid #474747;
      border-radius: 4px;
      background: #202124;
      color: #fff;
      box-shadow: 0 3px 12px rgba(0, 0, 0, 0.45);
      font: 13px/1.35 Arial, sans-serif;
    }
    .formats[hidden] {
      display: none;
    }
    .formats-title {
      margin: 0 0 5px;
      padding: 2px 5px 6px;
      border-bottom: 1px solid #444;
      color: #d7d7d7;
      font-weight: 600;
    }
    .quality-option {
      display: block;
      width: 100%;
      border: 0;
      border-radius: 2px;
      padding: 6px 8px;
      background: transparent;
      color: #fff;
      text-align: left;
      font: inherit;
      cursor: pointer;
    }
    .quality-option:hover {
      background: #3a3b3d;
    }
    .formats-error {
      padding: 6px 5px;
      color: #ffb6a4;
    }
  `;

  const button = document.createElement("button");
  button.type = "button";
  button.textContent = "Download this video";
  button.setAttribute("aria-label", "Download this video with RAJA");
  const formatsMenu = document.createElement("div");
  formatsMenu.className = "formats";
  formatsMenu.hidden = true;
  button.addEventListener("click", () => {
    button.disabled = true;
    button.textContent = "Checking formats...";
    formatsMenu.replaceChildren();
    formatsMenu.hidden = false;
    const title = document.createElement("div");
    title.className = "formats-title";
    title.textContent = "Available video quality";
    formatsMenu.append(title);
    chrome.runtime.sendMessage(
      { type: "raja-list-video-formats", url: window.location.href },
      (response) => {
        const failed = chrome.runtime.lastError || !response?.ok;
        if (failed || !Array.isArray(response.heights) || response.heights.length === 0) {
          const error = document.createElement("div");
          error.className = "formats-error";
          error.textContent = response?.message || "Could not read available video formats.";
          formatsMenu.append(error);
          button.textContent = "Download this video";
          button.disabled = false;
          return;
        }

        button.textContent = "Download this video";
        button.disabled = false;
        for (const height of response.heights) {
          if (!Number.isInteger(height) || height < 144 || height > 1080) {
            continue;
          }
          const option = document.createElement("button");
          option.type = "button";
          option.className = "quality-option";
          option.textContent = `${height}p`;
          option.addEventListener("click", () => {
            option.disabled = true;
            option.textContent = `Sending ${height}p to RAJA...`;
            chrome.runtime.sendMessage(
              {
                type: "raja-download-page",
                url: window.location.href,
                maxHeight: height
              },
              (result) => {
                const sendFailed = chrome.runtime.lastError || !result?.ok;
                option.textContent = sendFailed
                  ? result?.message || "RAJA could not start this video."
                  : `Added ${height}p download to RAJA`;
                window.setTimeout(() => {
                  formatsMenu.hidden = true;
                  option.disabled = false;
                  option.textContent = `${height}p`;
                }, 2200);
              }
            );
          });
          formatsMenu.append(option);
        }
      }
    );
  });

  shadow.append(style, button, formatsMenu);
  document.documentElement.append(host);

  const videos = [];
  const updatePosition = () => {
    const activeVideo = videos
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

    if (!activeVideo) {
      host.style.display = "none";
      formatsMenu.hidden = true;
      return;
    }

    host.style.display = "block";
    host.style.left = `${Math.max(4, activeVideo.rect.left + 12)}px`;
    host.style.top = `${Math.max(4, activeVideo.rect.top + 12)}px`;
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
