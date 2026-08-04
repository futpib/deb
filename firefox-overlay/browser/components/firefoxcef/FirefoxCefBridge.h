/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef mozilla_FirefoxCefBridge_h
#define mozilla_FirefoxCefBridge_h

#include "nsIFirefoxCefBridge.h"

namespace mozilla {

class FirefoxCefBridge final : public nsIFirefoxCefBridge {
 public:
  NS_DECL_THREADSAFE_ISUPPORTS
  NS_DECL_NSIFIREFOXCEFBRIDGE

  FirefoxCefBridge() = default;
  static already_AddRefed<FirefoxCefBridge> GetSingleton();

 private:
  ~FirefoxCefBridge() = default;
};

}  // namespace mozilla

#endif  // mozilla_FirefoxCefBridge_h
