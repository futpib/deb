/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

const bridge = Cc[
  "@deb.local/firefox-cef-bridge;1"
].getService(Ci.nsIFirefoxCefBridge);

const browsers = new Map();
const windowParameters = new URLSearchParams(window.location.search);
const childBrowserId = Number(windowParameters.get("browserId"));
const startsRuntime = !Number.isInteger(childBrowserId) || childBrowserId <= 0;
const coordinator = startsRuntime;
const ownedBrowserId = coordinator ? bridge.browserId : childBrowserId;
const ownedInitialUrl = coordinator
  ? bridge.initialUrl
  : windowParameters.get("url") ?? "about:blank";
function loadUrl(browser, url) {
  browser.loadURI(Services.io.newURI(url), {
    triggeringPrincipal: Services.scriptSecurityManager.getSystemPrincipal(),
  });
}

function createBrowserElement(browserId) {
  const browser = document.createXULElement("browser");
  browser.id = `content-${browserId}`;
  browser.setAttribute("type", "content");
  browser.setAttribute("primary", "true");
  browser.setAttribute("remote", "true");
  browser.setAttribute("remoteType", "web");
  browser.setAttribute("maychangeremoteness", "true");
  browser.setAttribute("flex", "1");
  return browser;
}

function repaintBrowser(entry) {
  entry.browser.frameLoader?.requestUpdatePosition();
  window.windowUtils.updateLayerTree();
}

function presentBrowser(entry) {
  repaintBrowser(entry);
  entry.browser.focus();
}

function finishActivation(entry) {
  if (!entry.active || !entry.browser.isConnected) {
    return;
  }
  entry.browser.hidden = false;
  entry.browser.getBoundingClientRect();
  entry.browser.preserveLayers(false);
  entry.browser.docShellIsActive = true;
  const remoteTab = entry.browser.frameLoader?.remoteTab;
  if (remoteTab) {
    remoteTab.priorityHint = true;
  }
  entry.browser.renderLayers = true;
  presentBrowser(entry);
}

function activateBrowser(browser) {
  let selectedBrowser = browser;
  for (const entry of browsers.values()) {
    entry.active = entry.browser == browser;
    if (entry.active) {
      finishActivation(entry);
      selectedBrowser = entry.browser;
    } else {
      const remoteTab = entry.browser.frameLoader?.remoteTab;
      if (remoteTab) {
        remoteTab.priorityHint = false;
      }
    }
  }
  document.getElementById("browsers").selectedPanel = selectedBrowser;
}

function registerBrowser(browserId, browser, initialUrl) {
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
      if (
        !webProgress.isTopLevel ||
        !(stateFlags & Ci.nsIWebProgressListener.STATE_IS_NETWORK)
      ) {
        return;
      }
      if (stateFlags & Ci.nsIWebProgressListener.STATE_START) {
        bridge.loadingStateChanged(browserId, true);
      }
      if (stateFlags & Ci.nsIWebProgressListener.STATE_STOP) {
        bridge.loadingStateChanged(browserId, false);
        titleListener();
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
    bridge.titleChanged(
      browserId,
      browsers.get(browserId)?.browser.contentTitle ?? ""
    );
  };
  const crashListener = () => {
    const entry = browsers.get(browserId);
    if (entry) {
      entry.crashed = true;
    }
    bridge.browserCrashed(browserId, "Gecko content process terminated");
  };

  browser.webProgress.addProgressListener(
    progressListener,
    Ci.nsIWebProgress.NOTIFY_LOCATION |
      Ci.nsIWebProgress.NOTIFY_STATE_NETWORK
  );
  browser.addEventListener("pagetitlechanged", titleListener);
  browser.addEventListener("oop-browser-crashed", crashListener);
  const entry = {
    active: false,
    browser,
    browserId,
    crashListener,
    crashed: false,
    currentUrl: initialUrl,
    progressListener,
    titleListener,
  };
  browsers.set(browserId, entry);
  return entry;
}

function createBrowser(browserId, initialUrl) {
  if (!browserId || browsers.has(browserId)) {
    return;
  }

  const browser = createBrowserElement(browserId);
  document.getElementById("browsers").appendChild(browser);
  browser.getBoundingClientRect();
  registerBrowser(browserId, browser, initialUrl);

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
  entry.browser.removeEventListener("pagetitlechanged", entry.titleListener);
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
  entry.active = false;
  const remoteTab = entry.browser.frameLoader?.remoteTab;
  if (remoteTab) {
    remoteTab.priorityHint = false;
  }
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

const commandObserver = (subject, topic, command) => {
  const [name, browserIdText, ...arguments_] = command.split("\t");
  const browserId = Number.parseInt(browserIdText, 10);
  const entry = browsers.get(browserId);
  switch (name) {
    case "create":
      if (coordinator) {
        const url = arguments_.join("\t");
        const childUrl =
          `chrome://firefoxcef/content/main.xhtml?browserId=${browserId}` +
          `&url=${encodeURIComponent(url)}`;
        Services.ww.openWindow(
          null,
          childUrl,
          `_firefox_cef_${browserId}`,
          "chrome,dialog=no,resizable",
          null
        );
      }
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
    case "invalidate":
      if (entry) {
        repaintBrowser(entry);
      }
      break;
    case "close":
      if (entry) {
        closeBrowser(browserId, arguments_[0] == "1");
        if (!coordinator) {
          window.close();
        }
      }
      break;
    case "shutdown":
      window.close();
      break;
  }
};

window.addEventListener("load", () => {
  Services.obs.addObserver(commandObserver, "firefox-cef-command");

  const baseWindow = window.docShell.treeOwner.QueryInterface(Ci.nsIBaseWindow);
  bridge.attachWindow(ownedBrowserId, baseWindow);
  if (startsRuntime) {
    bridge.runtimeReady();
  }
  createBrowser(ownedBrowserId, ownedInitialUrl);
}, { once: true });

window.addEventListener("unload", () => {
  Services.obs.removeObserver(commandObserver, "firefox-cef-command");
  for (const browserId of [...browsers.keys()]) {
    closeBrowser(browserId, true);
  }
}, { once: true });
