/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include <atomic>
#include <map>
#include <unordered_set>
#include <vector>

#include "GLContext.h"
#include "SharedSurfaceDMABUF.h"
#include "mozilla/StaticMutex.h"
#include "mozilla/gfx/Logging.h"
#include "mozilla/layers/LayersSurfaces.h"
#include "mozilla/webrender/RenderCompositor.h"
#include "mozilla/webrender/RenderThread.h"
#include "mozilla/widget/CompositorWidget.h"
#include "nsIWidget.h"
#include "prenv.h"

namespace {

struct FirefoxCefPlane {
  uint32_t stride;
  uint64_t offset;
  uint64_t size;
  int fd;
};

extern "C" uint32_t firefox_cef_gecko_browser_for_widget(nsIWidget *aWidget);
extern "C" void firefox_cef_gecko_emit_accelerated_frame(
    uint32_t aBrowserId, uint64_t aFrameId, uint32_t aWidth, uint32_t aHeight,
    uint64_t aModifier, const FirefoxCefPlane *aPlanes, size_t aPlaneCount,
    int aFenceFd);

mozilla::StaticMutex sReleaseMutex;
std::unordered_set<uint64_t> sReleasedFrames;
std::atomic<uint64_t> sNextFrameId{1};

bool TakeRelease(uint64_t aFrameId) {
  mozilla::StaticMutexAutoLock lock(sReleaseMutex);
  return sReleasedFrames.erase(aFrameId) != 0;
}

class FirefoxCefRenderCompositor final : public mozilla::wr::RenderCompositor {
public:
  FirefoxCefRenderCompositor(
      const RefPtr<mozilla::widget::CompositorWidget> &aWidget,
      RefPtr<mozilla::gl::GLContext> &&aGL)
      : RenderCompositor(aWidget), mGL(std::move(aGL)) {}

  bool BeginFrame() override {
    ReclaimReleasedFrames();
    if (!MakeCurrent()) {
      return false;
    }
    const auto size = GetBufferSize();
    if (size.IsEmpty()) {
      return false;
    }
    for (auto surface = mAvailable.begin(); surface != mAvailable.end();
         ++surface) {
      if ((*surface)->mDesc.size == size.ToUnknownSize()) {
        mCurrent = std::move(*surface);
        mAvailable.erase(surface);
        break;
      }
    }
    if (!mCurrent) {
      mozilla::gl::SharedSurfaceDesc desc{
          {mGL.get(), mozilla::gl::SharedSurfaceType::EGLSurfaceDMABUF,
           mozilla::layers::TextureType::DMABUF, true}};
      desc.size = size.ToUnknownSize();
      desc.colorSpace = mozilla::gfx::ColorSpace2::SRGB;
      mCurrent = mozilla::gl::SharedSurface_DMABUF::Create(desc);
      if (!mCurrent) {
        gfxCriticalNote << "Firefox CEF failed to allocate a DMA-BUF frame";
        return false;
      }
    }
    mCurrent->BeginWrite();
    gl()->fBindFramebuffer(LOCAL_GL_FRAMEBUFFER, mCurrent->mFb->mFB);
    return true;
  }

  void CancelFrame() override {
    if (!mCurrent) {
      return;
    }
    mCurrent->EndWrite();
    mAvailable.push_back(std::move(mCurrent));
  }

  mozilla::wr::RenderedFrameId
  EndFrame(const nsTArray<mozilla::wr::DeviceIntRect> &) override {
    const auto renderedFrameId = GetNextRenderFrameId();
    if (!mCurrent) {
      return renderedFrameId;
    }
    gl()->fFlush();
    mCurrent->EndWrite();
    gl()->fBindFramebuffer(LOCAL_GL_FRAMEBUFFER, 0);

    auto descriptor = mCurrent->ToSurfaceDescriptor();
    const uint32_t browserId =
        firefox_cef_gecko_browser_for_widget(mWidget->RealWidget());
    if (!descriptor || !browserId ||
        descriptor->type() !=
            mozilla::layers::SurfaceDescriptor::TSurfaceDescriptorDMABuf) {
      mAvailable.push_back(std::move(mCurrent));
      return renderedFrameId;
    }

    auto &dmabuf = descriptor->get_SurfaceDescriptorDMABuf();
    const size_t planeCount = dmabuf.fds().Length();
    if (!planeCount || planeCount > 4 ||
        dmabuf.strides().Length() < planeCount ||
        dmabuf.offsets().Length() < planeCount) {
      mAvailable.push_back(std::move(mCurrent));
      return renderedFrameId;
    }
    std::vector<FirefoxCefPlane> planes;
    planes.reserve(planeCount);
    for (size_t index = 0; index < planeCount; ++index) {
      const uint64_t planeHeight =
          index < dmabuf.heightAligned().Length()
              ? dmabuf.heightAligned()[index]
              : static_cast<uint32_t>(mCurrent->mDesc.size.height);
      planes.push_back(FirefoxCefPlane{
          dmabuf.strides()[index], dmabuf.offsets()[index],
          static_cast<uint64_t>(dmabuf.strides()[index]) * planeHeight,
          dmabuf.fds()[index]->GetHandle()});
    }
    const uint64_t modifier =
        dmabuf.modifier().IsEmpty() ? 0 : dmabuf.modifier()[0];
    const int fenceFd =
        dmabuf.fence().IsEmpty() ? -1 : dmabuf.fence()[0]->GetHandle();
    const uint64_t frameId =
        sNextFrameId.fetch_add(1, std::memory_order_relaxed);
    firefox_cef_gecko_emit_accelerated_frame(
        browserId, frameId, static_cast<uint32_t>(mCurrent->mDesc.size.width),
        static_cast<uint32_t>(mCurrent->mDesc.size.height), modifier,
        planes.data(), planes.size(), fenceFd);
    mLeased.emplace(frameId, std::move(mCurrent));
    return renderedFrameId;
  }

  void Pause() override {}
  bool Resume() override { return true; }
  bool IsPaused() override { return false; }
  mozilla::gl::GLContext *gl() const override { return mGL; }
  bool MakeCurrent() override { return mGL->MakeCurrent(); }
  mozilla::LayoutDeviceIntSize GetBufferSize() override {
    return mWidget->GetClientSize();
  }

private:
  void ReclaimReleasedFrames() {
    for (auto frame = mLeased.begin(); frame != mLeased.end();) {
      if (TakeRelease(frame->first)) {
        mAvailable.push_back(std::move(frame->second));
        frame = mLeased.erase(frame);
      } else {
        ++frame;
      }
    }
  }

  RefPtr<mozilla::gl::GLContext> mGL;
  mozilla::UniquePtr<mozilla::gl::SharedSurface_DMABUF> mCurrent;
  std::vector<mozilla::UniquePtr<mozilla::gl::SharedSurface_DMABUF>> mAvailable;
  std::map<uint64_t, mozilla::UniquePtr<mozilla::gl::SharedSurface_DMABUF>>
      mLeased;
};

} // namespace

namespace mozilla::wr {

UniquePtr<RenderCompositor> CreateFirefoxCefRenderCompositor(
    const RefPtr<widget::CompositorWidget> &aWidget, nsACString &aError) {
  if (!PR_GetEnv("FIREFOX_CEF_DIRECT")) {
    return nullptr;
  }
  RefPtr<gl::GLContext> gl = RenderThread::Get()->SingletonGL(aError);
  if (!gl) {
    aError.Append("(Firefox CEF DMA-BUF compositor)"_ns);
    return nullptr;
  }
  return MakeUnique<FirefoxCefRenderCompositor>(aWidget, std::move(gl));
}

} // namespace mozilla::wr

extern "C" NS_EXPORT int firefox_cef_gecko_release_frame(uint64_t aFrameId) {
  if (!aFrameId) {
    return 0;
  }
  mozilla::StaticMutexAutoLock lock(sReleaseMutex);
  sReleasedFrames.insert(aFrameId);
  return 1;
}
