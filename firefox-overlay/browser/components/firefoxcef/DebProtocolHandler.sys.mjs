/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

const NEW_TAB_URL = /^deb:\/\/new-tab\/?(?:[?#].*)?$/;

export class DebProtocolHandler {
  static classID = Components.ID("{a0530e46-98e6-4a17-8ce8-b446b404912e}");
  static contractID = "@mozilla.org/network/protocol;1?name=deb";

  scheme = "deb";
  defaultPort = -1;
  protocolFlags =
    Ci.nsIProtocolHandler.URI_STD |
    Ci.nsIProtocolHandler.URI_IS_LOCAL_RESOURCE |
    Ci.nsIProtocolHandler.URI_IS_POTENTIALLY_TRUSTWORTHY |
    Ci.nsIProtocolHandler.URI_IS_UI_RESOURCE;

  allowPort() {
    return false;
  }

  newChannel(uri, loadInfo) {
    if (!NEW_TAB_URL.test(uri.spec)) {
      throw Components.Exception(
        `Unknown deb internal page: ${uri.spec}`,
        Cr.NS_ERROR_FILE_NOT_FOUND
      );
    }

    const pageUri = Services.io.newURI(
      "chrome://firefoxcef/content/deb-new-tab.html"
    );
    const channel = Services.io.newChannelFromURIWithLoadInfo(pageUri, loadInfo);
    channel.originalURI = uri;
    channel.owner = Services.scriptSecurityManager.createContentPrincipal(uri, {});
    return channel;
  }

  QueryInterface = ChromeUtils.generateQI(["nsIProtocolHandler"]);
}
