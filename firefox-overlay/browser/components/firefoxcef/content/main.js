/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

const bridge = Cc[
  "@deb.local/firefox-cef-bridge;1"
].getService(Ci.nsIFirefoxCefBridge);

const browsers = new Map();

function loadUrl(browser, url) {
  browser.loadURI(Services.io.newURI(url), {
    triggeringPrincipal: Services.scriptSecurityManager.getSystemPrincipal(),
  });
}

function activateBrowser(browser) {
  for (const entry of browsers.values()) {
    const active = entry.browser == browser;
    entry.browser.docShellIsActive = active;
    entry.browser.hidden = !active;
  }
  document.getElementById("browsers").selectedPanel = browser;
}

function createBrowser(browserId, initialUrl) {
  if (!browserId || browsers.has(browserId)) {
    return;
  }

  const browser = document.createXULElement("browser");
  browser.id = `content-${browserId}`;
  browser.setAttribute("type", "content");
  browser.setAttribute("primary", "true");
  browser.setAttribute("remote", "true");
  browser.setAttribute("remoteType", "web");
  browser.setAttribute("maychangeremoteness", "true");
  browser.setAttribute("flex", "1");

  const progressListener = {
    QueryInterface: ChromeUtils.generateQI([
      "nsIWebProgressListener",
      "nsISupportsWeakReference",
    ]),

    onLocationChange(webProgress, request, location) {
      if (webProgress.isTopLevel && location) {
        const entry = browsers.get(browserId);
        if (entry) {
          entry.currentUrl = location.spec;
        }
        bridge.addressChanged(browserId, location.spec);
      }
    },

    onStateChange(webProgress, request, stateFlags, status) {
      if (!(stateFlags & Ci.nsIWebProgressListener.STATE_IS_NETWORK)) {
        return;
      }
      if (stateFlags & Ci.nsIWebProgressListener.STATE_START) {
        bridge.loadingStateChanged(browserId, true);
      }
      if (stateFlags & Ci.nsIWebProgressListener.STATE_STOP) {
        bridge.loadingStateChanged(browserId, false);
        if (!Components.isSuccessCode(status) && status != Cr.NS_BINDING_ABORTED) {
          const failedUrl = request?.URI?.spec ?? "";
          bridge.loadError(
            browserId,
            status,
            Components.Exception("", status).name,
            failedUrl
          );
        }
      }
    },
  };

  const titleListener = () => {
    bridge.titleChanged(browserId, browser.contentTitle ?? "");
  };
  const crashListener = () => {
    const entry = browsers.get(browserId);
    if (entry) {
      entry.crashed = true;
    }
    bridge.browserCrashed(browserId, "Gecko content process terminated");
  };

  document.getElementById("browsers").appendChild(browser);
  browser.webProgress.addProgressListener(
    progressListener,
    Ci.nsIWebProgress.NOTIFY_LOCATION |
      Ci.nsIWebProgress.NOTIFY_STATE_NETWORK
  );
  browser.addEventListener("DOMTitleChanged", titleListener, true);
  browser.addEventListener("oop-browser-crashed", crashListener);
  browsers.set(browserId, {
    browser,
    crashListener,
    crashed: false,
    currentUrl: initialUrl,
    progressListener,
    titleListener,
  });

  activateBrowser(browser);
  loadUrl(browser, initialUrl);
  browser.focus();
  bridge.browserReady(browserId);
}

function removeBrowser(browserId, notifyClose) {
  const entry = browsers.get(browserId);
  if (!entry) {
    return;
  }
  entry.browser.webProgress.removeProgressListener(entry.progressListener);
  entry.browser.removeEventListener("DOMTitleChanged", entry.titleListener, true);
  entry.browser.removeEventListener("oop-browser-crashed", entry.crashListener);
  entry.browser.remove();
  browsers.delete(browserId);
  if (notifyClose) {
    bridge.beforeClose(browserId);
  }
}

function replaceCrashedBrowser(browserId, url) {
  removeBrowser(browserId, false);
  createBrowser(browserId, url);
}

function setBrowserVisibility(browserId, visible) {
  const entry = browsers.get(browserId);
  if (!entry) {
    return;
  }
  if (visible) {
    activateBrowser(entry.browser);
    return;
  }
  entry.browser.docShellIsActive = false;
  entry.browser.hidden = true;
}

function closeBrowser(browserId, force) {
  const entry = browsers.get(browserId);
  if (!entry) {
    return;
  }
  if (!force && !entry.browser.permitUnload().permitUnload) {
    return;
  }
  removeBrowser(browserId, true);
}

window.addEventListener("load", () => {
  Services.obs.addObserver((subject, topic, command) => {
    const [name, browserIdText, ...arguments_] = command.split("\t");
    const browserId = Number.parseInt(browserIdText, 10);
    const entry = browsers.get(browserId);
    switch (name) {
      case "create":
        createBrowser(browserId, arguments_.join("\t"));
        break;
      case "navigate":
        if (entry) {
          const url = arguments_.join("\t");
          if (entry.crashed) {
            replaceCrashedBrowser(browserId, url);
          } else {
            loadUrl(entry.browser, url);
            activateBrowser(entry.browser);
            entry.browser.focus();
          }
        }
        break;
      case "reload":
        if (entry?.crashed) {
          replaceCrashedBrowser(browserId, entry.currentUrl);
        } else {
          entry?.browser.reload();
        }
        break;
      case "focus":
        if (entry) {
          window.focus();
          activateBrowser(entry.browser);
          entry.browser.focus();
        }
        break;
      case "visibility":
        setBrowserVisibility(browserId, arguments_[0] == "1");
        break;
      case "close":
        closeBrowser(browserId, arguments_[0] == "1");
        break;
      case "shutdown":
        window.close();
        break;
    }
  }, "firefox-cef-command");

  bridge.runtimeReady();
  createBrowser(bridge.browserId, bridge.initialUrl);
}, { once: true });

window.addEventListener("unload", () => {
  for (const browserId of [...browsers.keys()]) {
    closeBrowser(browserId, true);
  }
}, { once: true });
