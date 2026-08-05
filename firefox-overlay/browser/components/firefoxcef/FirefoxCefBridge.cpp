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
#include "mozilla/OriginAttributes.h"
#include "mozilla/ProcessType.h"
#include "mozilla/Services.h"
#include "mozilla/StaticMutex.h"
#include "mozilla/StaticPtr.h"
#include "nsIArray.h"
#include "nsArrayUtils.h"
#include "nsICookie.h"
#include "nsICookieManager.h"
#include "nsICookieNotification.h"
#include "nsICookieValidation.h"
#include "nsIObserverService.h"
#include "nsNetUtil.h"
#include "nsServiceManagerUtils.h"
#include "nsString.h"
#include "nsThreadUtils.h"
#include "nsXPCOM.h"
#include "nsXULAppAPI.h"

namespace {

struct FirefoxCefCookie {
  const char* name;
  const char* value;
  const char* domain;
  const char* path;
  const char* partitionKeyTopLevelSite;
  uint8_t secure;
  uint8_t httpOnly;
  uint8_t session;
  uint8_t partitioned;
  uint8_t partitionKeyHasCrossSiteAncestor;
  int64_t expiresMilliseconds;
  int64_t creationMicroseconds;
  int64_t lastAccessMicroseconds;
  int64_t updateMicroseconds;
  int32_t sameSite;
};

using FirefoxCefCookieVisitor = void (*)(void* context,
                                         const FirefoxCefCookie* cookie);
using FirefoxCefCookieCompletion = void (*)(void* context, uint8_t success);

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
  void (*onCookieChanged)(void* context, const FirefoxCefCookie* cookie,
                          uint8_t action);
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
bool sObservingCookies;

class CookieView {
 public:
  explicit CookieView(nsICookie* aCookie) {
    aCookie->GetName(mName);
    aCookie->GetValue(mValue);
    aCookie->GetHost(mDomain);
    aCookie->GetPath(mPath);
    bool secure = false;
    bool httpOnly = false;
    bool session = false;
    aCookie->GetIsSecure(&secure);
    aCookie->GetIsHttpOnly(&httpOnly);
    aCookie->GetIsSession(&session);
    mRaw.secure = secure;
    mRaw.httpOnly = httpOnly;
    mRaw.session = session;
    aCookie->GetExpiry(&mRaw.expiresMilliseconds);
    aCookie->GetCreationTime(&mRaw.creationMicroseconds);
    aCookie->GetLastAccessed(&mRaw.lastAccessMicroseconds);
    aCookie->GetUpdateTime(&mRaw.updateMicroseconds);
    aCookie->GetSameSite(&mRaw.sameSite);

    const mozilla::OriginAttributes& attributes =
        aCookie->OriginAttributesNative();
    if (!attributes.mPartitionKey.IsEmpty()) {
      nsAutoString scheme;
      nsAutoString domain;
      int32_t port = -1;
      bool ancestor = false;
      if (mozilla::OriginAttributes::ParsePartitionKey(
              attributes.mPartitionKey, scheme, domain, port, ancestor) &&
          mozilla::OriginAttributes::ExtractSiteFromPartitionKey(
              attributes.mPartitionKey, mPartitionKeyTopLevelSite)) {
        mRaw.partitioned = 1;
        mRaw.partitionKeyHasCrossSiteAncestor = ancestor;
      }
    }

    mPartitionKeyTopLevelSiteUtf8 =
        NS_ConvertUTF16toUTF8(mPartitionKeyTopLevelSite);
    mRaw.name = mName.get();
    mRaw.value = mValue.get();
    mRaw.domain = mDomain.get();
    mRaw.path = mPath.get();
    mRaw.partitionKeyTopLevelSite = mPartitionKeyTopLevelSiteUtf8.get();
  }

  const FirefoxCefCookie* Raw() const { return &mRaw; }

 private:
  FirefoxCefCookie mRaw{};
  nsCString mName;
  nsCString mValue;
  nsCString mDomain;
  nsCString mPath;
  nsString mPartitionKeyTopLevelSite;
  nsCString mPartitionKeyTopLevelSiteUtf8;
};

void VisitCookie(nsICookie* aCookie, FirefoxCefCookieVisitor aVisitor,
                 void* aContext) {
  if (!aCookie || !aVisitor) {
    return;
  }
  CookieView view(aCookie);
  aVisitor(aContext, view.Raw());
}

nsresult CookieUri(const FirefoxCefCookie* aCookie, nsIURI** aUri) {
  nsAutoCString host(aCookie->domain ? aCookie->domain : "");
  if (!host.IsEmpty() && host.First() == '.') {
    host.Cut(0, 1);
  }
  if (!host.IsEmpty() && host.FindChar(':') != kNotFound &&
      host.First() != '[') {
    host.Insert('[', 0);
    host.Append(']');
  }
  nsAutoCString spec(aCookie->secure ? "https://" : "http://");
  spec.Append(host);
  spec.Append(aCookie->path ? aCookie->path : "/");
  return NS_NewURI(aUri, spec);
}

bool CookieOriginAttributes(const FirefoxCefCookie* aCookie,
                            mozilla::OriginAttributes& aAttributes) {
  if (!aCookie->partitioned) {
    return true;
  }
  if (!aCookie->partitionKeyTopLevelSite ||
      !*aCookie->partitionKeyTopLevelSite) {
    return false;
  }
  nsCOMPtr<nsIURI> partitionUri;
  if (NS_FAILED(NS_NewURI(getter_AddRefs(partitionUri),
                         nsDependentCString(
                             aCookie->partitionKeyTopLevelSite)))) {
    return false;
  }
  aAttributes.SetPartitionKey(
      partitionUri, aCookie->partitionKeyHasCrossSiteAncestor != 0);
  return !aAttributes.mPartitionKey.IsEmpty();
}

void DispatchCookieCompletion(FirefoxCefCookieCompletion aCompletion,
                              void* aContext, bool aSuccess) {
  if (!aCompletion) {
    return;
  }
  NS_DispatchToMainThread(NS_NewRunnableFunction(
      "FirefoxCefBridge::CookieCompletion",
      [aCompletion, aContext, aSuccess] {
        aCompletion(aContext, static_cast<uint8_t>(aSuccess));
      }));
}

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

NS_IMPL_ISUPPORTS(FirefoxCefBridge, nsIFirefoxCefBridge, nsIObserver)

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
  nsCOMPtr<nsIObserverService> observerService =
      mozilla::services::GetObserverService();
  if (observerService && !sObservingCookies) {
    nsresult rv = observerService->AddObserver(this, "cookie-changed", false);
    if (NS_SUCCEEDED(rv)) {
      sObservingCookies = true;
    }
  }
  MaybeFireAfterCreated();
  return NS_OK;
}

NS_IMETHODIMP FirefoxCefBridge::Observe(nsISupports* aSubject,
                                        const char* aTopic,
                                        const char16_t* /* aData */) {
  if (strcmp(aTopic, "cookie-changed") != 0) {
    return NS_OK;
  }
  nsCOMPtr<nsICookieNotification> notification = do_QueryInterface(aSubject);
  if (!notification) {
    return NS_OK;
  }
  FirefoxCefCallbacks callbacks = Callbacks();
  if (!callbacks.onCookieChanged) {
    return NS_OK;
  }

  nsICookieNotification::Action action;
  notification->GetAction(&action);
  if (action == nsICookieNotification::COOKIES_BATCH_DELETED) {
    nsCOMPtr<nsIArray> cookies;
    notification->GetBatchDeletedCookies(getter_AddRefs(cookies));
    uint32_t length = 0;
    if (cookies) {
      cookies->GetLength(&length);
    }
    for (uint32_t index = 0; index < length; ++index) {
      nsCOMPtr<nsICookie> cookie = do_QueryElementAt(cookies, index);
      if (cookie) {
        CookieView view(cookie);
        callbacks.onCookieChanged(callbacks.context, view.Raw(),
                                  static_cast<uint8_t>(action));
      }
    }
    return NS_OK;
  }

  nsCOMPtr<nsICookie> cookie;
  notification->GetCookie(getter_AddRefs(cookie));
  if (cookie) {
    CookieView view(cookie);
    callbacks.onCookieChanged(callbacks.context, view.Raw(),
                              static_cast<uint8_t>(action));
  } else if (action == nsICookieNotification::ALL_COOKIES_CLEARED) {
    callbacks.onCookieChanged(callbacks.context, nullptr,
                              static_cast<uint8_t>(action));
  }
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

extern "C" NS_EXPORT int firefox_cef_gecko_visit_cookies(
    FirefoxCefCookieVisitor aVisitor, FirefoxCefCookieCompletion aCompletion,
    void* aContext) {
  if (!aVisitor || !aCompletion) {
    return 0;
  }
  nsresult rv = NS_DispatchToMainThread(NS_NewRunnableFunction(
      "FirefoxCefBridge::VisitCookies",
      [aVisitor, aCompletion, aContext] {
        nsCOMPtr<nsICookieManager> manager =
            do_GetService(NS_COOKIEMANAGER_CONTRACTID);
        nsTArray<RefPtr<nsICookie>> cookies;
        bool success = manager && NS_SUCCEEDED(manager->GetCookies(cookies));
        if (success) {
          for (const auto& cookie : cookies) {
            VisitCookie(cookie, aVisitor, aContext);
          }
        }
        aCompletion(aContext, static_cast<uint8_t>(success));
      }));
  return NS_SUCCEEDED(rv);
}

extern "C" NS_EXPORT int firefox_cef_gecko_set_cookie(
    const FirefoxCefCookie* aCookie, FirefoxCefCookieCompletion aCompletion,
    void* aContext) {
  if (!NS_IsMainThread() || !aCookie || !aCompletion || !aCookie->name ||
      !aCookie->value || !aCookie->domain || !aCookie->path) {
    return 0;
  }
  nsCOMPtr<nsICookieManager> manager =
      do_GetService(NS_COOKIEMANAGER_CONTRACTID);
  nsCOMPtr<nsIURI> cookieUri;
  mozilla::OriginAttributes attributes;
  bool success =
      manager && NS_SUCCEEDED(CookieUri(aCookie, getter_AddRefs(cookieUri))) &&
      CookieOriginAttributes(aCookie, attributes);
  if (success) {
    nsCOMPtr<nsICookieValidation> validation;
    nsICookie::schemeType scheme =
        aCookie->secure ? nsICookie::SCHEME_HTTPS : nsICookie::SCHEME_HTTP;
    success = NS_SUCCEEDED(manager->AddNative(
        cookieUri, nsDependentCString(aCookie->domain),
        nsDependentCString(aCookie->path), nsDependentCString(aCookie->name),
        nsDependentCString(aCookie->value), aCookie->secure != 0,
        aCookie->httpOnly != 0, aCookie->session != 0,
        aCookie->expiresMilliseconds, &attributes, aCookie->sameSite, scheme,
        aCookie->partitioned != 0, true, nullptr,
        getter_AddRefs(validation)));
  }
  DispatchCookieCompletion(aCompletion, aContext, success);
  return 1;
}

extern "C" NS_EXPORT int firefox_cef_gecko_delete_cookie(
    const FirefoxCefCookie* aCookie, FirefoxCefCookieCompletion aCompletion,
    void* aContext) {
  if (!NS_IsMainThread() || !aCookie || !aCompletion || !aCookie->name ||
      !aCookie->domain || !aCookie->path) {
    return 0;
  }
  nsCOMPtr<nsICookieManager> manager =
      do_GetService(NS_COOKIEMANAGER_CONTRACTID);
  mozilla::OriginAttributes attributes;
  bool success = manager && CookieOriginAttributes(aCookie, attributes);
  if (success) {
    success = NS_SUCCEEDED(manager->RemoveNative(
        nsDependentCString(aCookie->domain), nsDependentCString(aCookie->name),
        nsDependentCString(aCookie->path), &attributes, true, nullptr));
  }
  DispatchCookieCompletion(aCompletion, aContext, success);
  return 1;
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
