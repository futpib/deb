/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

export class FirefoxCefDOMFullscreenChild extends JSWindowActorChild {
  receiveMessage(message) {
    const windowUtils = this.contentWindow?.windowUtils;
    if (!windowUtils) {
      this.sendAsyncMessage("FirefoxCefFullscreen:Exited", {});
      return;
    }

    switch (message.name) {
      case "FirefoxCefFullscreen:Enter":
        windowUtils.handleFullscreenRequests();
        this.sendAsyncMessage(
          this.document.fullscreenElement
            ? "FirefoxCefFullscreen:Entered"
            : "FirefoxCefFullscreen:Exited",
          {}
        );
        break;
      case "FirefoxCefFullscreen:Exit":
        if (this.document.fullscreenElement) {
          windowUtils.exitFullscreen();
        } else {
          this.sendAsyncMessage("FirefoxCefFullscreen:Exited", {});
        }
        break;
    }
  }

  handleEvent(event) {
    switch (event.type) {
      case "MozDOMFullscreen:Request":
        this.sendAsyncMessage("FirefoxCefFullscreen:Request", {});
        break;
      case "MozDOMFullscreen:Exit":
        this.sendAsyncMessage("FirefoxCefFullscreen:ExitRequest", {});
        break;
      case "MozDOMFullscreen:Exited":
        this.sendAsyncMessage("FirefoxCefFullscreen:Exited", {});
        break;
    }
  }
}
