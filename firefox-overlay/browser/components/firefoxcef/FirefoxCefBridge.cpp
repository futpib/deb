/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "FirefoxCefBridge.h"

#include <algorithm>
#include <cmath>
#include <cstdint>
#include <cstring>
#include <map>
#include <strings.h>
#include <unordered_set>
#include <utility>
#include <vector>

#include "XREChildData.h"
#include "InputData.h"
#include "mozilla/Bootstrap.h"
#include "mozilla/ClearOnShutdown.h"
#include "mozilla/MouseEvents.h"
#include "mozilla/OriginAttributes.h"
#include "mozilla/ProcessType.h"
#include "mozilla/Services.h"
#include "mozilla/StaticMutex.h"
#include "mozilla/StaticPtr.h"
#include "mozilla/TextEvents.h"
#include "mozilla/TouchEvents.h"
#include "mozilla/dom/MouseEventBinding.h"
#include "mozilla/dom/WheelEventBinding.h"
#include "nsArrayUtils.h"
#include "nsIArray.h"
#include "nsIBaseWindow.h"
#include "nsICookie.h"
#include "nsICookieManager.h"
#include "nsICookieNotification.h"
#include "nsICookieValidation.h"
#include "nsIObserverService.h"
#include "nsIWidget.h"
#include "nsNetUtil.h"
#include "nsServiceManagerUtils.h"
#include "nsString.h"
#include "nsThreadUtils.h"
#include "nsXPCOM.h"
#include "nsXULAppAPI.h"

namespace {

struct FirefoxCefCookie {
  const char *name;
  const char *value;
  const char *domain;
  const char *path;
  const char *partitionKeyTopLevelSite;
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

struct FirefoxCefPlane {
  uint32_t stride;
  uint64_t offset;
  uint64_t size;
  int fd;
};

using FirefoxCefCookieVisitor = void (*)(void *context,
                                         const FirefoxCefCookie *cookie);
using FirefoxCefCookieCompletion = void (*)(void *context, uint8_t success);

struct FirefoxCefCallbacks {
  size_t size;
  void *context;
  void (*onAfterCreated)(void *context, int32_t browserId,
                         uint64_t nativeWindow);
  void (*onAddressChange)(void *context, int32_t browserId, const char *url);
  void (*onTitleChange)(void *context, int32_t browserId, const char *title);
  void (*onLoadingStateChange)(void *context, int32_t browserId,
                               uint8_t loading);
  void (*onLoadError)(void *context, int32_t browserId, int32_t errorCode,
                      const char *errorText, const char *failedUrl);
  void (*onBrowserCrashed)(void *context, int32_t browserId,
                           const char *reason);
  void (*onBeforeClose)(void *context, int32_t browserId);
  void (*onCookieChanged)(void *context, const FirefoxCefCookie *cookie,
                          uint8_t action);
  void (*onAcceleratedFrame)(void *context, int32_t browserId, uint64_t frameId,
                             uint32_t width, uint32_t height, uint64_t modifier,
                             const FirefoxCefPlane *planes, size_t planeCount,
                             int fenceFd);
  void (*onCursorChange)(void *context, int32_t browserId,
                         uint32_t cefCursorType);
  void (*onFullscreenChange)(void *context, int32_t browserId,
                             uint8_t fullscreen);
};

mozilla::StaticMutex sMutex;
FirefoxCefCallbacks sCallbacks;
struct BrowserConfig {
  uint64_t parentWindow;
  uint32_t width;
  uint32_t height;
  nsCString url;
};
uint32_t sBrowserId;
uint32_t sInitialWidth = 2;
uint32_t sInitialHeight = 2;
nsCString sInitialUrl;
bool sRuntimeReady;
bool sObservingCookies;
std::map<uint32_t, BrowserConfig> sConfiguredBrowsers;
std::map<uint32_t, nsCOMPtr<nsIWidget>> sWidgets;
std::map<nsIWidget *, uint32_t> sWidgetBrowsers;
std::unordered_set<uint32_t> sReadyBrowsers;
std::unordered_set<uint32_t> sAfterCreatedBrowsers;
std::unordered_set<uint32_t> sBeforeCloseBrowsers;
std::map<uint64_t, mozilla::MultiTouchInput> sTouchStates;

class CookieView {
public:
  explicit CookieView(nsICookie *aCookie) {
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

    const mozilla::OriginAttributes &attributes =
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

  const FirefoxCefCookie *Raw() const { return &mRaw; }

private:
  FirefoxCefCookie mRaw{};
  nsCString mName;
  nsCString mValue;
  nsCString mDomain;
  nsCString mPath;
  nsString mPartitionKeyTopLevelSite;
  nsCString mPartitionKeyTopLevelSiteUtf8;
};

void VisitCookie(nsICookie *aCookie, FirefoxCefCookieVisitor aVisitor,
                 void *aContext) {
  if (!aCookie || !aVisitor) {
    return;
  }
  CookieView view(aCookie);
  aVisitor(aContext, view.Raw());
}

nsresult CookieUri(const FirefoxCefCookie *aCookie, nsIURI **aUri) {
  nsAutoCString host(aCookie->domain ? aCookie->domain : "");
  if (!host.IsEmpty() && host.First() == '.') {
    host.Cut(0, 1);
  }
  if (!host.IsEmpty() && host.FindChar(':') != kNotFound &&
      host.First() != '[') {
    host.Insert('[', 0);
    host.Append(']');
  }
  // The bridge carries cookie state, not the URL that originally set it. Use a
  // secure source context without changing the cookie's Secure attribute.
  nsAutoCString spec("https://");
  spec.Append(host);
  spec.Append(aCookie->path ? aCookie->path : "/");
  return NS_NewURI(aUri, spec);
}

bool CookieOriginAttributes(const FirefoxCefCookie *aCookie,
                            mozilla::OriginAttributes &aAttributes) {
  if (!aCookie->partitioned) {
    return true;
  }
  if (!aCookie->partitionKeyTopLevelSite ||
      !*aCookie->partitionKeyTopLevelSite) {
    return false;
  }
  nsCOMPtr<nsIURI> partitionUri;
  if (NS_FAILED(
          NS_NewURI(getter_AddRefs(partitionUri),
                    nsDependentCString(aCookie->partitionKeyTopLevelSite)))) {
    return false;
  }
  aAttributes.SetPartitionKey(partitionUri,
                              aCookie->partitionKeyHasCrossSiteAncestor != 0);
  return !aAttributes.mPartitionKey.IsEmpty();
}

void DispatchCookieCompletion(FirefoxCefCookieCompletion aCompletion,
                              void *aContext, bool aSuccess) {
  if (!aCompletion) {
    return;
  }
  NS_DispatchToMainThread(NS_NewRunnableFunction(
      "FirefoxCefBridge::CookieCompletion", [aCompletion, aContext, aSuccess] {
        aCompletion(aContext, static_cast<uint8_t>(aSuccess));
      }));
}

FirefoxCefCallbacks Callbacks() {
  mozilla::StaticMutexAutoLock lock(sMutex);
  return sCallbacks;
}

nsCOMPtr<nsIWidget> BrowserWidget(uint32_t aBrowserId) {
  mozilla::StaticMutexAutoLock lock(sMutex);
  auto entry = sWidgets.find(aBrowserId);
  return entry == sWidgets.end() ? nullptr : entry->second;
}

mozilla::Modifiers GeckoModifiers(uint32_t aModifiers) {
  mozilla::Modifiers modifiers = mozilla::MODIFIER_NONE;
  if (aModifiers & 2) {
    modifiers |= mozilla::MODIFIER_SHIFT;
  }
  if (aModifiers & 4) {
    modifiers |= mozilla::MODIFIER_CONTROL;
  }
  if (aModifiers & 8) {
    modifiers |= mozilla::MODIFIER_ALT;
  }
  if (aModifiers & 128) {
    modifiers |= mozilla::MODIFIER_META;
  }
  if (aModifiers & 1) {
    modifiers |= mozilla::MODIFIER_CAPSLOCK;
  }
  if (aModifiers & 256) {
    modifiers |= mozilla::MODIFIER_NUMLOCK;
  }
  if (aModifiers & 4096) {
    modifiers |= mozilla::MODIFIER_ALTGRAPH;
  }
  return modifiers;
}

int16_t GeckoButtons(uint32_t aModifiers) {
  int16_t buttons = mozilla::MouseButtonsFlag::eNoButtons;
  if (aModifiers & 16) {
    buttons |= mozilla::MouseButtonsFlag::ePrimaryFlag;
  }
  if (aModifiers & 32) {
    buttons |= mozilla::MouseButtonsFlag::eMiddleFlag;
  }
  if (aModifiers & 64) {
    buttons |= mozilla::MouseButtonsFlag::eSecondaryFlag;
  }
  return buttons;
}

mozilla::MouseButton GeckoButton(uint32_t aButton) {
  switch (aButton) {
  case 0:
    return mozilla::MouseButton::ePrimary;
  case 1:
    return mozilla::MouseButton::eMiddle;
  case 2:
    return mozilla::MouseButton::eSecondary;
  default:
    return mozilla::MouseButton::eNotPressed;
  }
}

uint32_t ToCefCursorType(nsCursor aCursor) {
  switch (aCursor) {
  case eCursor_wait:
    return 4;
  case eCursor_select:
    return 3;
  case eCursor_hyperlink:
    return 2;
  case eCursor_n_resize:
    return 7;
  case eCursor_s_resize:
    return 10;
  case eCursor_w_resize:
    return 13;
  case eCursor_e_resize:
    return 6;
  case eCursor_nw_resize:
    return 9;
  case eCursor_se_resize:
    return 11;
  case eCursor_ne_resize:
    return 8;
  case eCursor_sw_resize:
    return 12;
  case eCursor_crosshair:
    return 1;
  case eCursor_move:
  case eCursor_all_scroll:
    return 29;
  case eCursor_help:
    return 5;
  case eCursor_copy:
    return 36;
  case eCursor_alias:
    return 33;
  case eCursor_context_menu:
    return 32;
  case eCursor_cell:
    return 31;
  case eCursor_grab:
    return 41;
  case eCursor_grabbing:
    return 42;
  case eCursor_spinning:
    return 34;
  case eCursor_zoom_in:
    return 39;
  case eCursor_zoom_out:
    return 40;
  case eCursor_not_allowed:
    return 38;
  case eCursor_col_resize:
    return 18;
  case eCursor_row_resize:
    return 19;
  case eCursor_no_drop:
    return 35;
  case eCursor_vertical_text:
    return 30;
  case eCursor_nesw_resize:
    return 16;
  case eCursor_nwse_resize:
    return 17;
  case eCursor_ns_resize:
    return 14;
  case eCursor_ew_resize:
    return 15;
  case eCursor_none:
    return 37;
  case eCursor_standard:
  case eCursorCount:
    return 0;
  }
  return 0;
}

void DispatchMouseMove(uint32_t aBrowserId, int32_t aX, int32_t aY,
                       uint32_t aModifiers, bool aLeaving) {
  nsCOMPtr<nsIWidget> widget = BrowserWidget(aBrowserId);
  if (!widget) {
    return;
  }
  mozilla::WidgetMouseEvent event(
      true, aLeaving ? mozilla::eMouseExitFromWidget : mozilla::eMouseMove,
      widget, mozilla::WidgetMouseEvent::eReal);
  event.mRefPoint = mozilla::LayoutDeviceIntPoint(aX, aY);
  event.mButtons = GeckoButtons(aModifiers);
  event.mModifiers = GeckoModifiers(aModifiers);
  event.mInputSource = mozilla::dom::MouseEvent_Binding::MOZ_SOURCE_MOUSE;
  if (aLeaving) {
    event.mExitFrom =
        mozilla::Some(mozilla::WidgetMouseEvent::ePlatformTopLevel);
  }
  widget->DispatchInputEvent(&event);
}

void DispatchMouseClick(uint32_t aBrowserId, int32_t aX, int32_t aY,
                        uint32_t aModifiers, uint32_t aButton, bool aMouseUp,
                        int32_t aClickCount) {
  nsCOMPtr<nsIWidget> widget = BrowserWidget(aBrowserId);
  mozilla::MouseButton button = GeckoButton(aButton);
  if (!widget || button == mozilla::MouseButton::eNotPressed) {
    return;
  }
  mozilla::WidgetMouseEvent event(
      true, aMouseUp ? mozilla::eMouseUp : mozilla::eMouseDown, widget,
      mozilla::WidgetMouseEvent::eReal);
  event.mRefPoint = mozilla::LayoutDeviceIntPoint(aX, aY);
  event.mButton = button;
  event.mButtons = GeckoButtons(aModifiers);
  const int16_t changedButton = mozilla::MouseButtonsFlagToChange(button);
  if (aMouseUp) {
    event.mButtons &= ~changedButton;
  } else {
    event.mButtons |= changedButton;
  }
  event.mClickCount = static_cast<uint32_t>(aClickCount);
  event.mModifiers = GeckoModifiers(aModifiers);
  event.mInputSource = mozilla::dom::MouseEvent_Binding::MOZ_SOURCE_MOUSE;
  widget->DispatchInputEvent(&event);
}

void DispatchMouseWheel(uint32_t aBrowserId, int32_t aX, int32_t aY,
                        uint32_t aModifiers, int32_t aDeltaX, int32_t aDeltaY) {
  nsCOMPtr<nsIWidget> widget = BrowserWidget(aBrowserId);
  if (!widget) {
    return;
  }
  mozilla::WidgetWheelEvent event(true, mozilla::eWheel, widget);
  event.mRefPoint = mozilla::LayoutDeviceIntPoint(aX, aY);
  event.mModifiers = GeckoModifiers(aModifiers);
  event.mButtons = GeckoButtons(aModifiers);
  if (aModifiers & (1 << 14)) {
    event.mDeltaMode = mozilla::dom::WheelEvent_Binding::DOM_DELTA_PIXEL;
    event.mDeltaX = -static_cast<double>(aDeltaX);
    event.mDeltaY = -static_cast<double>(aDeltaY);
    event.mIsNoLineOrPageDelta = true;
  } else {
    event.mDeltaMode = mozilla::dom::WheelEvent_Binding::DOM_DELTA_LINE;
    event.mDeltaX = event.mLineOrPageDeltaX =
        -static_cast<double>(aDeltaX) / 40.0;
    event.mDeltaY = event.mLineOrPageDeltaY =
        -static_cast<double>(aDeltaY) / 40.0;
    event.mWheelTicksX = -static_cast<double>(aDeltaX) / 120.0;
    event.mWheelTicksY = -static_cast<double>(aDeltaY) / 120.0;
  }
  event.mInputSource = mozilla::dom::MouseEvent_Binding::MOZ_SOURCE_MOUSE;
  widget->DispatchInputEvent(&event);
}

void DispatchTouch(uint32_t aBrowserId, int32_t aId, float aX, float aY,
                   float aRadiusX, float aRadiusY, float aRotationAngle,
                   float aPressure, uint32_t aEventType,
                   uint32_t aModifiers, uint32_t aPointerType) {
  nsCOMPtr<nsIWidget> widget = BrowserWidget(aBrowserId);
  if (!widget) {
    return;
  }

  const uint16_t inputSource =
      aPointerType == 0
          ? mozilla::dom::MouseEvent_Binding::MOZ_SOURCE_TOUCH
          : mozilla::dom::MouseEvent_Binding::MOZ_SOURCE_PEN;
  const uint64_t stateKey =
      (static_cast<uint64_t>(aBrowserId) << 8) | inputSource;
  auto [stateEntry, inserted] = sTouchStates.try_emplace(stateKey);
  mozilla::MultiTouchInput &state = stateEntry->second;
  state.modifiers = GeckoModifiers(aModifiers);
  state.mInputSource = inputSource;

  TouchPointerState pointerState;
  switch (aEventType) {
  case 0:
    pointerState = TOUCH_REMOVE;
    break;
  case 1:
  case 2:
    pointerState = TOUCH_CONTACT;
    break;
  case 3:
    pointerState = TOUCH_CANCEL;
    break;
  default:
    return;
  }

  mozilla::MultiTouchInput input = mozilla::UpdateSynthesizedTouchState(
      &state, mozilla::TimeStamp::Now(), static_cast<uint32_t>(aId),
      pointerState,
      mozilla::LayoutDeviceIntPoint::Round(aX, aY), aPressure,
      static_cast<uint32_t>(
          std::round(aRotationAngle * 180.0 / 3.14159265358979323846)));
  input.modifiers = GeckoModifiers(aModifiers);
  input.mInputSource = inputSource;

  auto updateGeometry = [=](mozilla::SingleTouchData &aTouch) {
    if (aTouch.mIdentifier != aId) {
      return;
    }
    aTouch.mRadius = mozilla::ScreenSize(aRadiusX, aRadiusY);
    aTouch.mRotationAngle =
        aRotationAngle * 180.0f / 3.14159265358979323846f;
    aTouch.mForce = aPressure;
  };
  for (auto &touch : state.mTouches) {
    updateGeometry(touch);
  }
  for (auto &touch : input.mTouches) {
    updateGeometry(touch);
  }

  mozilla::WidgetTouchEvent event = input.ToWidgetEvent(widget);
  widget->DispatchInputEvent(&event);
  if (state.mTouches.IsEmpty()) {
    sTouchStates.erase(stateEntry);
  }
}

mozilla::KeyNameIndex GeckoKeyName(int32_t aWindowsKeyCode,
                                   char16_t aCharacter) {
  if (aCharacter >= 0x20) {
    return mozilla::KEY_NAME_INDEX_USE_STRING;
  }
  switch (aWindowsKeyCode) {
  case 0x08:
    return mozilla::KEY_NAME_INDEX_Backspace;
  case 0x09:
    return mozilla::KEY_NAME_INDEX_Tab;
  case 0x0d:
    return mozilla::KEY_NAME_INDEX_Enter;
  case 0x10:
    return mozilla::KEY_NAME_INDEX_Shift;
  case 0x11:
    return mozilla::KEY_NAME_INDEX_Control;
  case 0x12:
    return mozilla::KEY_NAME_INDEX_Alt;
  case 0x13:
    return mozilla::KEY_NAME_INDEX_Pause;
  case 0x14:
    return mozilla::KEY_NAME_INDEX_CapsLock;
  case 0x1b:
    return mozilla::KEY_NAME_INDEX_Escape;
  case 0x21:
    return mozilla::KEY_NAME_INDEX_PageUp;
  case 0x22:
    return mozilla::KEY_NAME_INDEX_PageDown;
  case 0x23:
    return mozilla::KEY_NAME_INDEX_End;
  case 0x24:
    return mozilla::KEY_NAME_INDEX_Home;
  case 0x25:
    return mozilla::KEY_NAME_INDEX_ArrowLeft;
  case 0x26:
    return mozilla::KEY_NAME_INDEX_ArrowUp;
  case 0x27:
    return mozilla::KEY_NAME_INDEX_ArrowRight;
  case 0x28:
    return mozilla::KEY_NAME_INDEX_ArrowDown;
  case 0x2d:
    return mozilla::KEY_NAME_INDEX_Insert;
  case 0x2e:
    return mozilla::KEY_NAME_INDEX_Delete;
  case 0x5b:
  case 0x5c:
    return mozilla::KEY_NAME_INDEX_Meta;
  case 0x5d:
    return mozilla::KEY_NAME_INDEX_ContextMenu;
  case 0x90:
    return mozilla::KEY_NAME_INDEX_NumLock;
  case 0x91:
    return mozilla::KEY_NAME_INDEX_ScrollLock;
  default:
    return mozilla::KEY_NAME_INDEX_Unidentified;
  }
}

void DispatchKey(uint32_t aBrowserId, uint32_t aEventType, uint32_t aModifiers,
                 int32_t aWindowsKeyCode, char16_t aCharacter,
                 char16_t aUnmodifiedCharacter) {
  nsCOMPtr<nsIWidget> widget = BrowserWidget(aBrowserId);
  if (!widget) {
    return;
  }
  mozilla::EventMessage message;
  switch (aEventType) {
  case 0:
  case 1:
    message = mozilla::eKeyDown;
    break;
  case 2:
    message = mozilla::eKeyUp;
    break;
  case 3:
    message = mozilla::eKeyPress;
    break;
  default:
    return;
  }
  mozilla::WidgetKeyboardEvent event(true, message, widget);
  event.mModifiers = GeckoModifiers(aModifiers);
  event.mKeyCode = aEventType == 3 ? 0 : aWindowsKeyCode;
  event.mCharCode = aEventType == 3 ? aCharacter : 0;
  event.mPseudoCharCode = aEventType <= 1 ? aCharacter : 0;
  event.mIsRepeat = (aModifiers & (1 << 13)) != 0;
  event.mLocation = aModifiers & (1 << 9) ? mozilla::eKeyLocationNumpad
                                          : mozilla::eKeyLocationStandard;
  char16_t keyCharacter =
      aUnmodifiedCharacter != 0 ? aUnmodifiedCharacter : aCharacter;
  event.mKeyNameIndex = GeckoKeyName(aWindowsKeyCode, keyCharacter);
  if (event.mKeyNameIndex == mozilla::KEY_NAME_INDEX_USE_STRING) {
    event.mKeyValue.Assign(keyCharacter);
  }
  widget->DispatchInputEvent(&event);
}

template <typename Callback>
int DispatchBrowserInput(uint32_t aBrowserId, const char *aName,
                         Callback &&aCallback) {
  {
    mozilla::StaticMutexAutoLock lock(sMutex);
    if (sConfiguredBrowsers.find(aBrowserId) == sConfiguredBrowsers.end()) {
      return 0;
    }
  }
  if (NS_IsMainThread()) {
    aCallback();
    return 1;
  }
  nsresult result = NS_DispatchToMainThread(
      NS_NewRunnableFunction(aName, std::forward<Callback>(aCallback)));
  return NS_SUCCEEDED(result);
}

void MaybeFireAfterCreated(uint32_t aBrowserId) {
  FirefoxCefCallbacks callbacks;
  {
    mozilla::StaticMutexAutoLock lock(sMutex);
    if (sAfterCreatedBrowsers.find(aBrowserId) != sAfterCreatedBrowsers.end() ||
        !sRuntimeReady || sWidgets.find(aBrowserId) == sWidgets.end() ||
        sReadyBrowsers.find(aBrowserId) == sReadyBrowsers.end() ||
        sConfiguredBrowsers.find(aBrowserId) == sConfiguredBrowsers.end()) {
      return;
    }
    sAfterCreatedBrowsers.insert(aBrowserId);
    callbacks = sCallbacks;
  }
  if (callbacks.onAfterCreated) {
    callbacks.onAfterCreated(callbacks.context, aBrowserId, 0);
  }
}

nsCString BrowserCommand(const char *aName, uint32_t aBrowserId) {
  nsAutoCString command(aName);
  command.Append('\t');
  command.AppendInt(aBrowserId);
  return command;
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
  NS_DispatchToMainThread(NS_NewRunnableFunction(
      "FirefoxCefBridge::NotifyCommand", std::move(notify)));
}

bool IsFlag(const char *aArgument, const char *aFlag) {
  if (!aArgument || *aArgument != '-') {
    return false;
  }
  ++aArgument;
  if (*aArgument == '-') {
    ++aArgument;
  }
  return strcasecmp(aArgument, aFlag) == 0;
}

} // namespace

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

NS_IMETHODIMP FirefoxCefBridge::GetBrowserId(uint32_t *aBrowserId) {
  StaticMutexAutoLock lock(sMutex);
  *aBrowserId = sBrowserId;
  return NS_OK;
}

NS_IMETHODIMP FirefoxCefBridge::GetInitialUrl(nsACString &aInitialUrl) {
  StaticMutexAutoLock lock(sMutex);
  aInitialUrl = sInitialUrl;
  return NS_OK;
}

NS_IMETHODIMP FirefoxCefBridge::GetInitialWidth(uint32_t *aInitialWidth) {
  StaticMutexAutoLock lock(sMutex);
  *aInitialWidth = sInitialWidth;
  return NS_OK;
}

NS_IMETHODIMP FirefoxCefBridge::GetInitialHeight(uint32_t *aInitialHeight) {
  StaticMutexAutoLock lock(sMutex);
  *aInitialHeight = sInitialHeight;
  return NS_OK;
}

NS_IMETHODIMP FirefoxCefBridge::RuntimeReady() {
  std::vector<std::pair<uint32_t, nsCString>> pendingBrowsers;
  {
    StaticMutexAutoLock lock(sMutex);
    sRuntimeReady = true;
    for (const auto &[browserId, config] : sConfiguredBrowsers) {
      if (browserId != sBrowserId) {
        pendingBrowsers.emplace_back(browserId, config.url);
      }
    }
  }
  nsCOMPtr<nsIObserverService> observerService =
      mozilla::services::GetObserverService();
  if (observerService && !sObservingCookies) {
    nsresult rv = observerService->AddObserver(this, "cookie-changed", false);
    if (NS_SUCCEEDED(rv)) {
      sObservingCookies = true;
    }
  }
  for (const auto &[browserId, url] : pendingBrowsers) {
    nsCString command = BrowserCommand("create", browserId);
    command.Append('\t');
    command.Append(url);
    NotifyCommand(std::move(command));
  }
  return NS_OK;
}

NS_IMETHODIMP FirefoxCefBridge::AttachWindow(uint32_t aBrowserId,
                                             nsIBaseWindow *aBaseWindow) {
  if (!aBaseWindow) {
    return NS_ERROR_INVALID_ARG;
  }
  uint32_t width;
  uint32_t height;
  {
    StaticMutexAutoLock lock(sMutex);
    auto config = sConfiguredBrowsers.find(aBrowserId);
    if (config == sConfiguredBrowsers.end()) {
      return NS_ERROR_INVALID_ARG;
    }
    width = config->second.width;
    height = config->second.height;
  }
  nsCOMPtr<nsIWidget> widget;
  if (NS_FAILED(aBaseWindow->GetMainWidget(getter_AddRefs(widget))) ||
      !widget) {
    return NS_ERROR_FAILURE;
  }
  widget->Resize(mozilla::DesktopSize(width, height), true);
  {
    StaticMutexAutoLock lock(sMutex);
    auto previous = sWidgets.find(aBrowserId);
    if (previous != sWidgets.end()) {
      sWidgetBrowsers.erase(previous->second.get());
    }
    sWidgets[aBrowserId] = widget;
    sWidgetBrowsers[widget.get()] = aBrowserId;
  }
  MaybeFireAfterCreated(aBrowserId);
  return NS_OK;
}

NS_IMETHODIMP FirefoxCefBridge::BrowserReady(uint32_t aBrowserId) {
  {
    StaticMutexAutoLock lock(sMutex);
    if (sConfiguredBrowsers.find(aBrowserId) == sConfiguredBrowsers.end()) {
      return NS_ERROR_INVALID_ARG;
    }
    sReadyBrowsers.insert(aBrowserId);
  }
  MaybeFireAfterCreated(aBrowserId);
  return NS_OK;
}

NS_IMETHODIMP FirefoxCefBridge::AddressChanged(uint32_t aBrowserId,
                                               const nsACString &aUrl) {
  FirefoxCefCallbacks callbacks = Callbacks();
  if (callbacks.onAddressChange) {
    callbacks.onAddressChange(callbacks.context, aBrowserId,
                              PromiseFlatCString(aUrl).get());
  }
  return NS_OK;
}

NS_IMETHODIMP FirefoxCefBridge::TitleChanged(uint32_t aBrowserId,
                                             const nsACString &aTitle) {
  FirefoxCefCallbacks callbacks = Callbacks();
  if (callbacks.onTitleChange) {
    callbacks.onTitleChange(callbacks.context, aBrowserId,
                            PromiseFlatCString(aTitle).get());
  }
  return NS_OK;
}

NS_IMETHODIMP FirefoxCefBridge::Observe(nsISupports *aSubject,
                                        const char *aTopic,
                                        const char16_t * /* aData */) {
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

NS_IMETHODIMP FirefoxCefBridge::LoadingStateChanged(uint32_t aBrowserId,
                                                    bool aLoading) {
  FirefoxCefCallbacks callbacks = Callbacks();
  if (callbacks.onLoadingStateChange) {
    callbacks.onLoadingStateChange(callbacks.context, aBrowserId, aLoading);
  }
  return NS_OK;
}

NS_IMETHODIMP FirefoxCefBridge::FullscreenChanged(uint32_t aBrowserId,
                                                  bool aFullscreen) {
  FirefoxCefCallbacks callbacks = Callbacks();
  if (callbacks.onFullscreenChange) {
    callbacks.onFullscreenChange(callbacks.context, aBrowserId, aFullscreen);
  }
  return NS_OK;
}

NS_IMETHODIMP FirefoxCefBridge::LoadError(uint32_t aBrowserId,
                                          int32_t aErrorCode,
                                          const nsACString &aErrorText,
                                          const nsACString &aFailedUrl) {
  FirefoxCefCallbacks callbacks = Callbacks();
  if (callbacks.onLoadError) {
    callbacks.onLoadError(callbacks.context, aBrowserId, aErrorCode,
                          PromiseFlatCString(aErrorText).get(),
                          PromiseFlatCString(aFailedUrl).get());
  }
  return NS_OK;
}

NS_IMETHODIMP FirefoxCefBridge::BrowserCrashed(uint32_t aBrowserId,
                                               const nsACString &aReason) {
  FirefoxCefCallbacks callbacks = Callbacks();
  if (callbacks.onBrowserCrashed) {
    callbacks.onBrowserCrashed(callbacks.context, aBrowserId,
                               PromiseFlatCString(aReason).get());
  }
  return NS_OK;
}

NS_IMETHODIMP FirefoxCefBridge::BeforeClose(uint32_t aBrowserId) {
  FirefoxCefCallbacks callbacks;
  {
    StaticMutexAutoLock lock(sMutex);
    if (sBeforeCloseBrowsers.find(aBrowserId) != sBeforeCloseBrowsers.end()) {
      return NS_OK;
    }
    sBeforeCloseBrowsers.insert(aBrowserId);
    sConfiguredBrowsers.erase(aBrowserId);
    sReadyBrowsers.erase(aBrowserId);
    auto widget = sWidgets.find(aBrowserId);
    if (widget != sWidgets.end()) {
      sWidgetBrowsers.erase(widget->second.get());
    }
    sWidgets.erase(aBrowserId);
    for (auto touch = sTouchStates.begin(); touch != sTouchStates.end();) {
      if ((touch->first >> 8) == aBrowserId) {
        touch = sTouchStates.erase(touch);
      } else {
        ++touch;
      }
    }
    callbacks = sCallbacks;
  }
  if (callbacks.onBeforeClose) {
    callbacks.onBeforeClose(callbacks.context, aBrowserId);
  }
  return NS_OK;
}

} // namespace mozilla

extern "C" NS_EXPORT void
firefox_cef_gecko_set_callbacks(const FirefoxCefCallbacks *aCallbacks) {
  mozilla::StaticMutexAutoLock lock(sMutex);
  if (aCallbacks && aCallbacks->size >= sizeof(FirefoxCefCallbacks)) {
    sCallbacks = *aCallbacks;
  } else {
    sCallbacks = {};
  }
}

extern "C" NS_EXPORT int firefox_cef_gecko_configure(uint32_t aBrowserId,
                                                     uint64_t aParentWindow,
                                                     uint32_t aWidth,
                                                     uint32_t aHeight,
                                                     const char *aInitialUrl) {
  bool initialBrowser;
  bool runtimeReady;
  nsAutoCString url(aInitialUrl ? aInitialUrl : "about:blank");
  {
    mozilla::StaticMutexAutoLock lock(sMutex);
    if (!aBrowserId ||
        sConfiguredBrowsers.find(aBrowserId) != sConfiguredBrowsers.end()) {
      return 0;
    }
    initialBrowser = sConfiguredBrowsers.empty();
    sConfiguredBrowsers.emplace(
        aBrowserId, BrowserConfig{aParentWindow, std::max(aWidth, 2U),
                                  std::max(aHeight, 2U), url});
    runtimeReady = sRuntimeReady;
    if (initialBrowser) {
      sBrowserId = aBrowserId;
      sInitialWidth = std::max(aWidth, 2U);
      sInitialHeight = std::max(aHeight, 2U);
      sInitialUrl = url;
    }
  }
  if (runtimeReady) {
    nsCString command = BrowserCommand("create", aBrowserId);
    command.Append('\t');
    command.Append(url);
    NotifyCommand(std::move(command));
  }
  return 1;
}

extern "C" NS_EXPORT int firefox_cef_gecko_resize(uint32_t aBrowserId,
                                                  uint32_t aWidth,
                                                  uint32_t aHeight) {
  auto resize = [aBrowserId, aWidth, aHeight] {
    nsCOMPtr<nsIWidget> widget = BrowserWidget(aBrowserId);
    if (!widget) {
      return;
    }
    widget->Resize(
        mozilla::DesktopSize(std::max(aWidth, 2U), std::max(aHeight, 2U)),
        true);
  };
  if (NS_IsMainThread()) {
    resize();
    return 1;
  }
  return NS_SUCCEEDED(NS_DispatchToMainThread(
      NS_NewRunnableFunction("FirefoxCefBridge::Resize", std::move(resize))));
}

extern "C" NS_EXPORT int firefox_cef_gecko_invalidate(uint32_t aBrowserId) {
  NotifyCommand(BrowserCommand("invalidate", aBrowserId));
  return 1;
}

extern "C" NS_EXPORT uint32_t
firefox_cef_gecko_browser_for_widget(nsIWidget *aWidget) {
  mozilla::StaticMutexAutoLock lock(sMutex);
  auto browser = sWidgetBrowsers.find(aWidget);
  return browser == sWidgetBrowsers.end() ? 0 : browser->second;
}

extern "C" NS_EXPORT void
firefox_cef_gecko_cursor_changed(nsIWidget *aWidget, uint32_t aCursor) {
  const uint32_t browserId = firefox_cef_gecko_browser_for_widget(aWidget);
  FirefoxCefCallbacks callbacks = Callbacks();
  if (browserId && aCursor < eCursorCount && callbacks.onCursorChange) {
    callbacks.onCursorChange(callbacks.context, browserId,
                             ToCefCursorType(static_cast<nsCursor>(aCursor)));
  }
}

extern "C" NS_EXPORT void firefox_cef_gecko_emit_accelerated_frame(
    uint32_t aBrowserId, uint64_t aFrameId, uint32_t aWidth, uint32_t aHeight,
    uint64_t aModifier, const FirefoxCefPlane *aPlanes, size_t aPlaneCount,
    int aFenceFd) {
  FirefoxCefCallbacks callbacks = Callbacks();
  if (callbacks.onAcceleratedFrame) {
    callbacks.onAcceleratedFrame(callbacks.context, aBrowserId, aFrameId,
                                 aWidth, aHeight, aModifier, aPlanes,
                                 aPlaneCount, aFenceFd);
  }
}

extern "C" NS_EXPORT int
firefox_cef_gecko_visit_cookies(FirefoxCefCookieVisitor aVisitor,
                                FirefoxCefCookieCompletion aCompletion,
                                void *aContext) {
  if (!aVisitor || !aCompletion) {
    return 0;
  }
  nsresult rv = NS_DispatchToMainThread(NS_NewRunnableFunction(
      "FirefoxCefBridge::VisitCookies", [aVisitor, aCompletion, aContext] {
        nsCOMPtr<nsICookieManager> manager =
            do_GetService(NS_COOKIEMANAGER_CONTRACTID);
        nsTArray<RefPtr<nsICookie>> cookies;
        bool success = manager && NS_SUCCEEDED(manager->GetCookies(cookies));
        if (success) {
          for (const auto &cookie : cookies) {
            VisitCookie(cookie, aVisitor, aContext);
          }
        }
        aCompletion(aContext, static_cast<uint8_t>(success));
      }));
  return NS_SUCCEEDED(rv);
}

extern "C" NS_EXPORT int
firefox_cef_gecko_set_cookie(const FirefoxCefCookie *aCookie,
                             FirefoxCefCookieCompletion aCompletion,
                             void *aContext) {
  if (!NS_IsMainThread() || !aCookie || !aCompletion || !aCookie->name ||
      !aCookie->value || !aCookie->domain || !aCookie->path) {
    return 0;
  }
  nsCOMPtr<nsICookieManager> manager =
      do_GetService(NS_COOKIEMANAGER_CONTRACTID);
  nsCOMPtr<nsIURI> cookieUri;
  mozilla::OriginAttributes attributes;
  bool success = manager &&
                 NS_SUCCEEDED(CookieUri(aCookie, getter_AddRefs(cookieUri))) &&
                 CookieOriginAttributes(aCookie, attributes);
  if (success) {
    nsCOMPtr<nsICookieValidation> validation;
    nsICookie::schemeType scheme = nsICookie::SCHEME_HTTPS;
    success = NS_SUCCEEDED(manager->AddNative(
        cookieUri, nsDependentCString(aCookie->domain),
        nsDependentCString(aCookie->path), nsDependentCString(aCookie->name),
        nsDependentCString(aCookie->value), aCookie->secure != 0,
        aCookie->httpOnly != 0, aCookie->session != 0,
        aCookie->expiresMilliseconds, &attributes, aCookie->sameSite, scheme,
        aCookie->partitioned != 0, true, nullptr, getter_AddRefs(validation)));
  }
  DispatchCookieCompletion(aCompletion, aContext, success);
  return 1;
}

extern "C" NS_EXPORT int
firefox_cef_gecko_delete_cookie(const FirefoxCefCookie *aCookie,
                                FirefoxCefCookieCompletion aCompletion,
                                void *aContext) {
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

extern "C" NS_EXPORT int firefox_cef_gecko_navigate(uint32_t aBrowserId,
                                                    const char *aUrl) {
  if (!aUrl) {
    return 0;
  }
  nsCString command = BrowserCommand("navigate", aBrowserId);
  command.Append('\t');
  command.Append(aUrl);
  NotifyCommand(std::move(command));
  return 1;
}

extern "C" NS_EXPORT int firefox_cef_gecko_reload(uint32_t aBrowserId) {
  NotifyCommand(BrowserCommand("reload", aBrowserId));
  return 1;
}

extern "C" NS_EXPORT int firefox_cef_gecko_focus(uint32_t aBrowserId) {
  NotifyCommand(BrowserCommand("focus", aBrowserId));
  return 1;
}

extern "C" NS_EXPORT int firefox_cef_gecko_set_visibility(uint32_t aBrowserId,
                                                          uint8_t aVisible) {
  nsCString command = BrowserCommand("visibility", aBrowserId);
  command.Append('\t');
  command.Append(aVisible ? '1' : '0');
  NotifyCommand(std::move(command));
  return 1;
}

extern "C" NS_EXPORT int
firefox_cef_gecko_exit_fullscreen(uint32_t aBrowserId) {
  NotifyCommand(BrowserCommand("exit-fullscreen", aBrowserId));
  return 1;
}

extern "C" NS_EXPORT int
firefox_cef_gecko_send_mouse_move(uint32_t aBrowserId, int32_t aX, int32_t aY,
                                  uint32_t aModifiers, uint8_t aLeaving) {
  return DispatchBrowserInput(aBrowserId, "FirefoxCefBridge::MouseMove", [=] {
    DispatchMouseMove(aBrowserId, aX, aY, aModifiers, aLeaving);
  });
}

extern "C" NS_EXPORT int
firefox_cef_gecko_send_mouse_click(uint32_t aBrowserId, int32_t aX, int32_t aY,
                                   uint32_t aModifiers, uint32_t aButton,
                                   uint8_t aMouseUp, int32_t aClickCount) {
  if (aButton > 2 || aClickCount <= 0) {
    return 0;
  }
  return DispatchBrowserInput(aBrowserId, "FirefoxCefBridge::MouseClick", [=] {
    DispatchMouseClick(aBrowserId, aX, aY, aModifiers, aButton, aMouseUp,
                       aClickCount);
  });
}

extern "C" NS_EXPORT int
firefox_cef_gecko_send_mouse_wheel(uint32_t aBrowserId, int32_t aX, int32_t aY,
                                   uint32_t aModifiers, int32_t aDeltaX,
                                   int32_t aDeltaY) {
  return DispatchBrowserInput(aBrowserId, "FirefoxCefBridge::MouseWheel", [=] {
    DispatchMouseWheel(aBrowserId, aX, aY, aModifiers, aDeltaX, aDeltaY);
  });
}

extern "C" NS_EXPORT int firefox_cef_gecko_send_touch(
    uint32_t aBrowserId, int32_t aId, float aX, float aY, float aRadiusX,
    float aRadiusY, float aRotationAngle, float aPressure,
    uint32_t aEventType, uint32_t aModifiers, uint32_t aPointerType) {
  if (aId == -1 || !std::isfinite(aX) || !std::isfinite(aY) ||
      !std::isfinite(aRadiusX) || !std::isfinite(aRadiusY) ||
      !std::isfinite(aRotationAngle) || !std::isfinite(aPressure) ||
      aRadiusX < 0.0f || aRadiusY < 0.0f || aPressure < 0.0f ||
      aPressure > 1.0f || aEventType > 3 ||
      (aPointerType != 0 && aPointerType != 2 && aPointerType != 3)) {
    return 0;
  }
  return DispatchBrowserInput(aBrowserId, "FirefoxCefBridge::Touch", [=] {
    DispatchTouch(aBrowserId, aId, aX, aY, aRadiusX, aRadiusY,
                  aRotationAngle, aPressure, aEventType, aModifiers,
                  aPointerType);
  });
}

extern "C" NS_EXPORT int
firefox_cef_gecko_send_key(uint32_t aBrowserId, uint32_t aEventType,
                           uint32_t aModifiers, int32_t aWindowsKeyCode,
                           int32_t aNativeKeyCode, uint8_t aSystemKey,
                           char16_t aCharacter, char16_t aUnmodifiedCharacter) {
  if (aEventType > 3) {
    return 0;
  }
  (void)aNativeKeyCode;
  (void)aSystemKey;
  return DispatchBrowserInput(aBrowserId, "FirefoxCefBridge::Key", [=] {
    DispatchKey(aBrowserId, aEventType, aModifiers, aWindowsKeyCode, aCharacter,
                aUnmodifiedCharacter);
  });
}

extern "C" NS_EXPORT int firefox_cef_gecko_close(uint32_t aBrowserId,
                                                 uint8_t aForce) {
  nsCString command = BrowserCommand("close", aBrowserId);
  command.Append('\t');
  command.Append(aForce ? '1' : '0');
  NotifyCommand(std::move(command));
  return 1;
}

extern "C" NS_EXPORT int firefox_cef_gecko_shutdown() {
  NotifyCommand(nsCString("shutdown\t0"));
  return 1;
}

extern "C" NS_EXPORT int firefox_cef_gecko_post_task(void (*aTask)(void *),
                                                     void *aContext) {
  if (!aTask) {
    return 0;
  }
  nsresult result = NS_DispatchToMainThread(NS_NewRunnableFunction(
      "FirefoxCefBridge::PostTask", [aTask, aContext] { aTask(aContext); }));
  return NS_SUCCEEDED(result);
}

extern "C" NS_EXPORT int firefox_cef_gecko_run(int aArgc, char **aArgv,
                                               const char *aAppIni) {
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
    nsresult result = bootstrap->XRE_InitChildProcess(aArgc, aArgv, &childData);
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
