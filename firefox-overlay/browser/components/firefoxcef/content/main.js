/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

const bridge = Cc[
  "@deb.local/firefox-cef-bridge;1"
].getService(Ci.nsIFirefoxCefBridge);

const progressListener = {
  QueryInterface: ChromeUtils.generateQI([
    "nsIWebProgressListener",
    "nsISupportsWeakReference",
  ]),

  onStateChange(webProgress, request, stateFlags, status) {
    if (!(stateFlags & Ci.nsIWebProgressListener.STATE_IS_NETWORK)) {
      return;
    }
    if (stateFlags & Ci.nsIWebProgressListener.STATE_START) {
      bridge.loadingStateChanged(true);
    }
    if (stateFlags & Ci.nsIWebProgressListener.STATE_STOP) {
      bridge.loadingStateChanged(false);
      if (!Components.isSuccessCode(status) && status != Cr.NS_BINDING_ABORTED) {
        const failedUrl = request?.URI?.spec ?? "";
        bridge.loadError(status, Components.Exception("", status).name, failedUrl);
      }
    }
  },
};

function loadUrl(browser, url) {
  browser.loadURI(Services.io.newURI(url), {
    triggeringPrincipal: Services.scriptSecurityManager.getSystemPrincipal(),
  });
}

window.addEventListener("load", () => {
  const browser = document.getElementById("content");
  browser.webProgress.addProgressListener(
    progressListener,
    Ci.nsIWebProgress.NOTIFY_STATE_NETWORK
  );

  Services.obs.addObserver((subject, topic, command) => {
    const [name, ...arguments_] = command.split("\t");
    switch (name) {
      case "navigate":
        loadUrl(browser, arguments_.join("\t"));
        browser.focus();
        break;
      case "reload":
        browser.reload();
        break;
      case "focus":
        window.focus();
        browser.focus();
        break;
      case "close":
        window.close();
        break;
    }
  }, "firefox-cef-command");

  bridge.runtimeReady();
  loadUrl(browser, bridge.initialUrl);
  browser.focus();
}, { once: true });

window.addEventListener("unload", () => bridge.beforeClose(), { once: true });
