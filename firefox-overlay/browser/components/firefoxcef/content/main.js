/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

const bridge = Cc[
  "@deb.local/firefox-cef-bridge;1"
].getService(Ci.nsIFirefoxCefBridge);
ChromeUtils.importESModule(
  "resource://gre/modules/ActorManagerParent.sys.mjs"
);
const { AddonManager } = ChromeUtils.importESModule(
  "resource://gre/modules/AddonManager.sys.mjs"
);

const browsers = new Map();
const tabsProgressListeners = new Set();
const devtoolsCommands = new Map();
const devtoolsOpenings = new Map();
const windowParameters = new URLSearchParams(window.location.search);
const childBrowserId = Number(windowParameters.get("browserId"));
const startsRuntime = !Number.isInteger(childBrowserId) || childBrowserId <= 0;
const coordinator = startsRuntime;
const ownedBrowserId = coordinator ? bridge.browserId : childBrowserId;
const ownedInitialUrl = coordinator
  ? bridge.initialUrl
  : windowParameters.get("url") ?? "about:blank";

window.gBrowserInit = {
  isAdoptingTab() {
    return false;
  },
};
window.gBrowser = {
  get tabs() {
    return [...browsers.values()].map(entry => entry.nativeTab);
  },
  get selectedTab() {
    return [...browsers.values()].find(entry => entry.active)?.nativeTab ?? null;
  },
  get selectedTabs() {
    const selectedTab = this.selectedTab;
    return selectedTab ? [selectedTab] : [];
  },
  get selectedBrowser() {
    return [...browsers.values()].find(entry => entry.active)?.browser ?? null;
  },
  addTabsProgressListener(listener) {
    tabsProgressListeners.add(listener);
  },
  removeTabsProgressListener(listener) {
    tabsProgressListeners.delete(listener);
  },
  getBrowserForTab(tab) {
    return tab?.linkedBrowser ?? null;
  },
  getIcon() {
    return "";
  },
  getTabForBrowser(browser) {
    return [...browsers.values()].find(entry => entry.browser == browser)
      ?.nativeTab ?? null;
  },
  getTabSharingState() {
    return {};
  },
};

async function loadDebExtensions() {
  const environment = Cc["@mozilla.org/process/environment;1"].getService(
    Ci.nsIEnvironment
  );
  const encodedPaths = environment.get("DEB_FIREFOX_EXTENSIONS");
  if (!encodedPaths) {
    return;
  }

  for (const path of JSON.parse(encodedPaths)) {
    const file = Cc["@mozilla.org/file/local;1"].createInstance(Ci.nsIFile);
    file.initWithPath(path);
    try {
      await AddonManager.installTemporaryAddon(file);
      console.info(`firefox-cef: loaded extension ${path}`);
    } catch (error) {
      console.error(`firefox-cef: failed to load extension ${path}`, error);
    }
  }
}

if (startsRuntime) {
  ChromeUtils.registerWindowActor("FirefoxCefDOMFullscreen", {
    parent: {
      esModuleURI:
        "resource:///modules/FirefoxCefFullscreenParent.sys.mjs",
    },
    child: {
      esModuleURI:
        "resource:///modules/FirefoxCefFullscreenChild.sys.mjs",
      events: {
        "MozDOMFullscreen:Request": {},
        "MozDOMFullscreen:Exit": {},
        "MozDOMFullscreen:Exited": {},
      },
    },
    allFrames: true,
  });
}

window.FullScreen = {
  firefoxCef: true,
  actor: null,
  active: false,

  enterDomFullscreen(actor) {
    this.actor = actor;
    if (!this.active) {
      this.active = true;
      document.documentElement.setAttribute("inDOMFullscreen", true);
      bridge.fullscreenChanged(ownedBrowserId, true);
    }
  },

  cleanupDomFullscreen() {
    if (this.active) {
      this.active = false;
      document.documentElement.removeAttribute("inDOMFullscreen");
      bridge.fullscreenChanged(ownedBrowserId, false);
    }
    this.actor = null;
  },

  exitDomFullscreen() {
    this.actor?.sendAsyncMessage("FirefoxCefFullscreen:Exit", {});
  },
};

function loadUrl(browser, url) {
  browser.loadURI(Services.io.newURI(url), {
    triggeringPrincipal: Services.scriptSecurityManager.getSystemPrincipal(),
  });
}

async function openDeveloperTools(entry) {
  const { require } = ChromeUtils.importESModule(
    "resource://devtools/shared/loader/Loader.sys.mjs"
  );
  const { CommandsFactory } = require(
    "resource://devtools/shared/commands/commands-factory.js"
  );
  const { gDevTools } = require(
    "resource://devtools/client/framework/devtools.js"
  );
  const { Toolbox } = require(
    "resource://devtools/client/framework/toolbox.js"
  );
  let commands = devtoolsCommands.get(entry.browserId);
  if (!commands) {
    commands = await CommandsFactory.forRemoteTab(entry.browser.browserId);
    devtoolsCommands.set(entry.browserId, commands);
    commands.client.once("closed").then(() => {
      devtoolsCommands.delete(entry.browserId);
    });
  }
  const createsWindow = !gDevTools.getToolboxForCommands(commands);
  if (createsWindow) {
    bridge.prepareDevToolsWindow();
  }
  try {
    return await gDevTools.showToolbox(commands, {
      toolId: "inspector",
      hostType: Toolbox.HostType.WINDOW,
      raise: true,
    });
  } catch (error) {
    if (createsWindow) {
      bridge.cancelDevToolsWindow();
    }
    throw error;
  }
}

function showDeveloperTools(entry) {
  const opening = devtoolsOpenings.get(entry.browserId);
  if (opening) {
    return opening.then(toolbox => toolbox.raise());
  }
  const pending = openDeveloperTools(entry).finally(() => {
    if (devtoolsOpenings.get(entry.browserId) === pending) {
      devtoolsOpenings.delete(entry.browserId);
    }
  });
  devtoolsOpenings.set(entry.browserId, pending);
  return pending;
}

function createBrowserElement(browserId) {
  const browser = document.createXULElement("browser");
  browser.id = `content-${browserId}`;
  browser.setAttribute("type", "content");
  browser.setAttribute("primary", "true");
  browser.setAttribute("remote", "true");
  browser.setAttribute("remoteType", "web");
  browser.setAttribute("messagemanagergroup", "browsers");
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
  const previousTab = window.gBrowser.selectedTab;
  let selectedBrowser = browser;
  for (const entry of browsers.values()) {
    entry.active = entry.browser == browser;
    entry.nativeTab.selected = entry.active;
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
  const selectedTab = window.gBrowser.selectedTab;
  if (selectedTab && selectedTab != previousTab) {
    selectedTab.dispatchEvent(new CustomEvent("TabSelect", { bubbles: true }));
  }
}

function registerBrowser(browserId, browser, initialUrl) {
  const nativeTab = document.createXULElement("box");
  nativeTab.id = `tab-${browserId}`;
  nativeTab.setAttribute("label", initialUrl);
  Object.defineProperties(nativeTab, {
    documentGlobal: { get: () => window },
    linkedBrowser: { value: browser },
    linkedPanel: { value: browser.id },
  });
  Object.assign(nativeTab, {
    _tPos: browsers.size,
    group: null,
    hidden: false,
    lastAccessed: Date.now(),
    multiselected: false,
    muteReason: null,
    muted: false,
    openerTab: null,
    pinned: false,
    selected: false,
    soundPlaying: false,
    splitview: null,
    successor: null,
    undiscardable: false,
    userContextId: 0,
  });
  document.getElementById("extension-tabs").appendChild(nativeTab);
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
          entry.nativeTab.setAttribute("label", entry.browser.contentTitle || location.spec);
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
        nativeTab.setAttribute("busy", "true");
        bridge.loadingStateChanged(browserId, true);
      }
      if (stateFlags & Ci.nsIWebProgressListener.STATE_STOP) {
        nativeTab.removeAttribute("busy");
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
    nativeTab,
    progressListener,
    titleListener,
  };
  browsers.set(browserId, entry);
  nativeTab.dispatchEvent(new CustomEvent("TabOpen", { bubbles: true }));
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
  entry.nativeTab.dispatchEvent(new CustomEvent("TabClose", { bubbles: true }));
  entry.nativeTab.remove();
  browsers.delete(browserId);
  let position = 0;
  for (const remaining of browsers.values()) {
    remaining.nativeTab._tPos = position++;
  }
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
    case "show-devtools":
      if (entry) {
        showDeveloperTools(entry).catch(error => {
          console.error("firefox-cef: opening developer tools failed", error);
        });
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
    case "exit-fullscreen":
      if (entry) {
        window.FullScreen.exitDomFullscreen();
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

window.addEventListener("load", async () => {
  Services.obs.addObserver(commandObserver, "firefox-cef-command");

  const baseWindow = window.docShell.treeOwner.QueryInterface(Ci.nsIBaseWindow);
  bridge.attachWindow(ownedBrowserId, baseWindow);
  if (startsRuntime) {
    bridge.runtimeReady();
  }
  createBrowser(ownedBrowserId, ownedInitialUrl);
  if (startsRuntime) {
    await loadDebExtensions();
  }
}, { once: true });

window.addEventListener("unload", () => {
  Services.obs.removeObserver(commandObserver, "firefox-cef-command");
  for (const browserId of [...browsers.keys()]) {
    closeBrowser(browserId, true);
  }
}, { once: true });
