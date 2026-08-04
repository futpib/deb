/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "FirefoxCefBridge.h"

#include <algorithm>
#include <cstdlib>
#include <cstring>
#include <strings.h>
#include <utility>

#include "XREChildData.h"
#include "mozilla/Bootstrap.h"
#include "mozilla/ClearOnShutdown.h"
#include "mozilla/ProcessType.h"
#include "mozilla/Services.h"
#include "mozilla/StaticMutex.h"
#include "mozilla/StaticPtr.h"
#include "nsIObserverService.h"
#include "nsString.h"
#include "nsThreadUtils.h"
#include "nsXPCOM.h"
#include "nsXULAppAPI.h"

namespace {

struct FirefoxCefCallbacks {
  size_t size;
  void* context;
  void (*onAfterCreated)(void* context, int32_t browserId,
                         uint64_t nativeWindow);
  void (*onLoadingStateChange)(void* context, int32_t browserId,
                               uint8_t loading);
  void (*onLoadError)(void* context, int32_t browserId, int32_t errorCode,
                      const char* errorText, const char* failedUrl);
  void (*onBeforeClose)(void* context, int32_t browserId);
};

mozilla::StaticMutex sMutex;
FirefoxCefCallbacks sCallbacks;
uint32_t sBrowserId;
uint32_t sInitialWidth = 2;
uint32_t sInitialHeight = 2;
uint64_t sNativeWindow;
nsCString sInitialUrl;
bool sRuntimeReady;
bool sAfterCreated;
bool sBeforeClose;

FirefoxCefCallbacks Callbacks(uint32_t* aBrowserId = nullptr) {
  mozilla::StaticMutexAutoLock lock(sMutex);
  if (aBrowserId) {
    *aBrowserId = sBrowserId;
  }
  return sCallbacks;
}

void MaybeFireAfterCreated() {
  FirefoxCefCallbacks callbacks;
  uint32_t browserId;
  uint64_t nativeWindow;
  {
    mozilla::StaticMutexAutoLock lock(sMutex);
    if (sAfterCreated || !sRuntimeReady || !sNativeWindow) {
      return;
    }
    sAfterCreated = true;
    callbacks = sCallbacks;
    browserId = sBrowserId;
    nativeWindow = sNativeWindow;
  }
  if (callbacks.onAfterCreated) {
    callbacks.onAfterCreated(callbacks.context, browserId, nativeWindow);
  }
}

void NotifyCommand(nsCString aCommand) {
  auto notify = [command = std::move(aCommand)]() {
    nsCOMPtr<nsIObserverService> observerService =
        mozilla::services::GetObserverService();
    if (!observerService) {
      return;
    }
    NS_ConvertUTF8toUTF16 data(command);
    observerService->NotifyObservers(nullptr, "firefox-cef-command",
                                     data.get());
  };
  if (NS_IsMainThread()) {
    notify();
    return;
  }
  NS_DispatchToMainThread(
      NS_NewRunnableFunction("FirefoxCefBridge::NotifyCommand",
                             std::move(notify)));
}

bool IsFlag(const char* aArgument, const char* aFlag) {
  if (!aArgument || *aArgument != '-') {
    return false;
  }
  ++aArgument;
  if (*aArgument == '-') {
    ++aArgument;
  }
  return strcasecmp(aArgument, aFlag) == 0;
}

}  // namespace

namespace mozilla {

static StaticRefPtr<FirefoxCefBridge> sSingleton;

NS_IMPL_ISUPPORTS(FirefoxCefBridge, nsIFirefoxCefBridge)

already_AddRefed<FirefoxCefBridge> FirefoxCefBridge::GetSingleton() {
  if (!sSingleton) {
    sSingleton = new FirefoxCefBridge;
    ClearOnShutdown(&sSingleton);
  }
  return do_AddRef(sSingleton);
}

NS_IMETHODIMP FirefoxCefBridge::GetBrowserId(uint32_t* aBrowserId) {
  StaticMutexAutoLock lock(sMutex);
  *aBrowserId = sBrowserId;
  return NS_OK;
}

NS_IMETHODIMP FirefoxCefBridge::GetInitialUrl(nsACString& aInitialUrl) {
  StaticMutexAutoLock lock(sMutex);
  aInitialUrl = sInitialUrl;
  return NS_OK;
}

NS_IMETHODIMP FirefoxCefBridge::GetInitialWidth(uint32_t* aInitialWidth) {
  StaticMutexAutoLock lock(sMutex);
  *aInitialWidth = sInitialWidth;
  return NS_OK;
}

NS_IMETHODIMP FirefoxCefBridge::GetInitialHeight(uint32_t* aInitialHeight) {
  StaticMutexAutoLock lock(sMutex);
  *aInitialHeight = sInitialHeight;
  return NS_OK;
}

NS_IMETHODIMP FirefoxCefBridge::RuntimeReady() {
  {
    StaticMutexAutoLock lock(sMutex);
    sRuntimeReady = true;
  }
  MaybeFireAfterCreated();
  return NS_OK;
}

NS_IMETHODIMP FirefoxCefBridge::LoadingStateChanged(bool aLoading) {
  uint32_t browserId;
  FirefoxCefCallbacks callbacks = Callbacks(&browserId);
  if (callbacks.onLoadingStateChange) {
    callbacks.onLoadingStateChange(callbacks.context, browserId, aLoading);
  }
  return NS_OK;
}

NS_IMETHODIMP FirefoxCefBridge::LoadError(int32_t aErrorCode,
                                          const nsACString& aErrorText,
                                          const nsACString& aFailedUrl) {
  uint32_t browserId;
  FirefoxCefCallbacks callbacks = Callbacks(&browserId);
  if (callbacks.onLoadError) {
    callbacks.onLoadError(callbacks.context, browserId, aErrorCode,
                          PromiseFlatCString(aErrorText).get(),
                          PromiseFlatCString(aFailedUrl).get());
  }
  return NS_OK;
}

NS_IMETHODIMP FirefoxCefBridge::BeforeClose() {
  FirefoxCefCallbacks callbacks;
  uint32_t browserId;
  {
    StaticMutexAutoLock lock(sMutex);
    if (sBeforeClose) {
      return NS_OK;
    }
    sBeforeClose = true;
    callbacks = sCallbacks;
    browserId = sBrowserId;
  }
  if (callbacks.onBeforeClose) {
    callbacks.onBeforeClose(callbacks.context, browserId);
  }
  return NS_OK;
}

}  // namespace mozilla

extern "C" NS_EXPORT void firefox_cef_gecko_set_callbacks(
    const FirefoxCefCallbacks* aCallbacks) {
  mozilla::StaticMutexAutoLock lock(sMutex);
  if (aCallbacks && aCallbacks->size >= sizeof(FirefoxCefCallbacks)) {
    sCallbacks = *aCallbacks;
  } else {
    sCallbacks = {};
  }
}

extern "C" NS_EXPORT int firefox_cef_gecko_configure(
    uint32_t aBrowserId, uint64_t aParentWindow, uint32_t aWidth,
    uint32_t aHeight, const char* aInitialUrl) {
  {
    mozilla::StaticMutexAutoLock lock(sMutex);
    sBrowserId = aBrowserId;
    sInitialWidth = std::max(aWidth, 2U);
    sInitialHeight = std::max(aHeight, 2U);
    sInitialUrl.Assign(aInitialUrl ? aInitialUrl : "about:blank");
    sNativeWindow = 0;
    sRuntimeReady = false;
    sAfterCreated = false;
    sBeforeClose = false;
  }
  nsAutoCString parent;
  parent.AppendInt(aParentWindow);
  return setenv("FIREFOX_CEF_PARENT_XID", parent.get(), 1) == 0;
}

extern "C" NS_EXPORT void firefox_cef_gecko_set_native_window(
    uint64_t aNativeWindow) {
  {
    mozilla::StaticMutexAutoLock lock(sMutex);
    sNativeWindow = aNativeWindow;
  }
  MaybeFireAfterCreated();
}

extern "C" NS_EXPORT int firefox_cef_gecko_navigate(const char* aUrl) {
  if (!aUrl) {
    return 0;
  }
  nsAutoCString command("navigate\t");
  command.Append(aUrl);
  NotifyCommand(std::move(command));
  return 1;
}

extern "C" NS_EXPORT int firefox_cef_gecko_reload() {
  NotifyCommand(nsCString("reload"));
  return 1;
}

extern "C" NS_EXPORT int firefox_cef_gecko_focus() {
  NotifyCommand(nsCString("focus"));
  return 1;
}

extern "C" NS_EXPORT int firefox_cef_gecko_close() {
  NotifyCommand(nsCString("close"));
  return 1;
}

extern "C" NS_EXPORT int firefox_cef_gecko_post_task(
    void (*aTask)(void*), void* aContext) {
  if (!aTask) {
    return 0;
  }
  nsresult result = NS_DispatchToMainThread(NS_NewRunnableFunction(
      "FirefoxCefBridge::PostTask", [aTask, aContext] { aTask(aContext); }));
  return NS_SUCCEEDED(result);
}

extern "C" NS_EXPORT int firefox_cef_gecko_run(int aArgc, char** aArgv,
                                                const char* aAppIni) {
  if (aArgc > 1 && IsFlag(aArgv[1], "contentproc")) {
    if (aArgc < 4) {
      return 3;
    }
    mozilla::SetGeckoProcessType(aArgv[--aArgc]);
    mozilla::SetGeckoChildID(aArgv[--aArgc]);
    mozilla::Bootstrap::UniquePtr bootstrap;
    mozilla::XRE_GetBootstrap(bootstrap);
    if (!bootstrap) {
      return 4;
    }
    bootstrap->NS_LogInit();
#ifdef MOZ_ENABLE_FORKSERVER
    if (mozilla::GetGeckoProcessType() == GeckoProcessType_ForkServer) {
      if (bootstrap->XRE_ForkServer(&aArgc, &aArgv)) {
        bootstrap->NS_LogTerm();
        return 0;
      }
    }
#endif
    XREChildData childData;
    nsresult result =
        bootstrap->XRE_InitChildProcess(aArgc, aArgv, &childData);
    bootstrap->NS_LogTerm();
    return NS_FAILED(result) ? 1 : 0;
  }

  if (!aAppIni) {
    return 2;
  }
  mozilla::Bootstrap::UniquePtr bootstrap;
  mozilla::XRE_GetBootstrap(bootstrap);
  if (!bootstrap) {
    return 4;
  }
  bootstrap->NS_LogInit();
  bootstrap->XRE_EnableSameExecutableForContentProc();
  mozilla::BootstrapConfig config{};
  config.appData = nullptr;
  config.appDataPath = aAppIni;
  int result = bootstrap->XRE_main(aArgc, aArgv, config);
  bootstrap->NS_LogTerm();
  return result;
}
