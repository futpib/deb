/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

export class FirefoxCefCommandLineHandler {
  static classID = Components.ID("{921368d4-54ed-42b7-aa00-ba168284ade4}");
  static contractID =
    "@dual-engine-browser.local/firefox-cef-command-line;1";

  QueryInterface = ChromeUtils.generateQI([Ci.nsICommandLineHandler]);

  handle(commandLine) {
    if (!Services.env.exists("FIREFOX_CEF_PARENT_XID")) {
      return;
    }

    Cc["@mozilla.org/embedcomp/window-watcher;1"]
      .getService(Ci.nsIWindowWatcher)
      .openWindow(
        null,
        "chrome://firefoxcef/content/main.xhtml",
        "_blank",
        "chrome,dialog=no,all",
        commandLine
      );
    commandLine.preventDefault = true;
  }

  helpInfo = "";
}
