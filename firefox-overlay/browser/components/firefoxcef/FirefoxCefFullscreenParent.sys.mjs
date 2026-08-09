/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

export class FirefoxCefDOMFullscreenParent extends JSWindowActorParent {
  fullscreenWindow = null;

  receiveMessage(message) {
    const browser = this.browsingContext.top.embedderElement;
    const window = browser?.documentGlobal;
    if (!window?.FullScreen?.firefoxCef) {
      return;
    }

    switch (message.name) {
      case "FirefoxCefFullscreen:Request":
        this.sendAsyncMessage("FirefoxCefFullscreen:Enter", {});
        break;
      case "FirefoxCefFullscreen:Entered":
        this.fullscreenWindow = window;
        window.FullScreen.enterDomFullscreen(this);
        break;
      case "FirefoxCefFullscreen:ExitRequest":
        this.sendAsyncMessage("FirefoxCefFullscreen:Exit", {});
        break;
      case "FirefoxCefFullscreen:Exited":
        window.FullScreen.cleanupDomFullscreen();
        this.fullscreenWindow = null;
        break;
    }
  }

  didDestroy() {
    this.fullscreenWindow?.FullScreen.cleanupDomFullscreen();
    this.fullscreenWindow = null;
  }
}
