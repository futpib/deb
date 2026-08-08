#include "browser_surface.h"

#include <QCoreApplication>
#include <QCursor>
#include <QGuiApplication>
#include <QHash>
#include <QHoverEvent>
#include <QImage>
#include <QInputMethodEvent>
#include <QKeyEvent>
#include <QMetaObject>
#include <QMouseEvent>
#include <QMutex>
#include <QMutexLocker>
#include <QOpenGLContext>
#include <QOpenGLExtraFunctions>
#include <QPointer>
#include <QPixmap>
#include <QQmlEngine>
#include <QQuickWindow>
#include <QSGSimpleTextureNode>
#include <QSGTexture>
#include <QTouchEvent>
#include <QWheelEvent>
#include <QtMath>

#include <QtGui/qopenglcontext_platform.h>
#include <QtQuick/qsgtexture_platform.h>

#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GL/gl.h>

#include <array>
#include <memory>
#include <mutex>
#include <unistd.h>
#include <utility>
#include <vector>

extern "C" void deb_release_dmabuf_lease(unsigned long long leaseId);
extern "C" void deb_present_dmabuf_lease(unsigned long long leaseId);
extern "C" void deb_rebind_dmabuf_lease(unsigned long long leaseId,
                                          unsigned long long surfaceGeneration);

class DmabufFrame {
public:
    ~DmabufFrame() {
        for (const int fd : fds) {
            if (fd >= 0) {
                close(fd);
            }
        }
        if (acquireFenceFd >= 0) {
            close(acquireFenceFd);
        }
        if (leaseId != 0) {
            deb_release_dmabuf_lease(leaseId);
        }
    }

    int takeAcquireFence() {
        return std::exchange(acquireFenceFd, -1);
    }

    unsigned long long leaseId = 0;
    unsigned long long browserId = 0;
    int layer = 0;
    int x = 0;
    int y = 0;
    unsigned int width = 0;
    unsigned int height = 0;
    unsigned int drmFormat = 0;
    unsigned long long modifier = 0;
    bool flipY = false;
    std::vector<int> fds;
    std::vector<unsigned int> strides;
    std::vector<unsigned long long> offsets;
    int acquireFenceFd = -1;
};

namespace {

constexpr unsigned long long DrmFormatModifierInvalid =
    0x00ffffffffffffffULL;

QHash<QString, QPointer<BrowserSurface>> &surfaceRegistry() {
    static QHash<QString, QPointer<BrowserSurface>> registry;
    return registry;
}

QHash<unsigned long long, std::shared_ptr<DmabufFrame>> &frameCache() {
    static QHash<unsigned long long, std::shared_ptr<DmabufFrame>> cache;
    return cache;
}

QHash<unsigned long long, QCursor> &cursorCache() {
    static QHash<unsigned long long, QCursor> cache;
    return cache;
}

QCursor cursorForCefType(int type, const QByteArray &customBgra,
                         unsigned int width, unsigned int height,
                         int hotspotX, int hotspotY, float imageScaleFactor) {
    switch (type) {
    case 1:
    case 31:
    case 39:
    case 40:
        return QCursor(Qt::CrossCursor);
    case 2:
        return QCursor(Qt::PointingHandCursor);
    case 3:
    case 30:
        return QCursor(Qt::IBeamCursor);
    case 4:
        return QCursor(Qt::WaitCursor);
    case 5:
        return QCursor(Qt::WhatsThisCursor);
    case 6:
    case 13:
    case 15:
    case 18:
    case 21:
    case 28:
    case 44:
        return QCursor(Qt::SizeHorCursor);
    case 7:
    case 10:
    case 14:
    case 19:
    case 22:
    case 25:
    case 43:
        return QCursor(Qt::SizeVerCursor);
    case 8:
    case 12:
    case 16:
    case 23:
    case 27:
        return QCursor(Qt::SizeBDiagCursor);
    case 9:
    case 11:
    case 17:
    case 24:
    case 26:
        return QCursor(Qt::SizeFDiagCursor);
    case 20:
    case 29:
        return QCursor(Qt::SizeAllCursor);
    case 33:
    case 49:
        return QCursor(Qt::DragLinkCursor);
    case 34:
        return QCursor(Qt::BusyCursor);
    case 35:
    case 38:
    case 46:
        return QCursor(Qt::ForbiddenCursor);
    case 36:
    case 48:
        return QCursor(Qt::DragCopyCursor);
    case 37:
        return QCursor(Qt::BlankCursor);
    case 41:
        return QCursor(Qt::OpenHandCursor);
    case 42:
        return QCursor(Qt::ClosedHandCursor);
    case 45: {
        if (width == 0 || height == 0 || customBgra.isEmpty()) {
            return QCursor(Qt::ArrowCursor);
        }
        const QImage borrowed(
            reinterpret_cast<const unsigned char *>(customBgra.constData()),
            static_cast<int>(width), static_cast<int>(height),
            QImage::Format_ARGB32);
        QPixmap pixmap = QPixmap::fromImage(borrowed.copy());
        pixmap.setDevicePixelRatio(imageScaleFactor);
        return QCursor(pixmap, hotspotX, hotspotY);
    }
    case 47:
        return QCursor(Qt::DragMoveCursor);
    default:
        return QCursor(Qt::ArrowCursor);
    }
}

int cefModifiers(Qt::KeyboardModifiers modifiers, Qt::MouseButtons buttons) {
    unsigned int state = 0;
    if (modifiers.testFlag(Qt::ShiftModifier)) {
        state |= 2;
    }
    if (modifiers.testFlag(Qt::ControlModifier)) {
        state |= 4;
    }
    if (modifiers.testFlag(Qt::AltModifier)) {
        state |= 8;
    }
    if (modifiers.testFlag(Qt::MetaModifier)) {
        state |= 128;
    }
    if (modifiers.testFlag(Qt::KeypadModifier)) {
        state |= 512;
    }
    if (modifiers.testFlag(Qt::GroupSwitchModifier)) {
        state |= 4096;
    }
    if (buttons.testFlag(Qt::LeftButton)) {
        state |= 16;
    }
    if (buttons.testFlag(Qt::MiddleButton)) {
        state |= 32;
    }
    if (buttons.testFlag(Qt::RightButton)) {
        state |= 64;
    }
    return static_cast<int>(state);
}

int cefWindowsKeyCode(int key, Qt::KeyboardModifiers modifiers) {
    if ((key >= Qt::Key_0 && key <= Qt::Key_9) ||
        (key >= Qt::Key_A && key <= Qt::Key_Z)) {
        return key;
    }
    if (key >= Qt::Key_F1 && key <= Qt::Key_F24) {
        return 0x70 + key - Qt::Key_F1;
    }
    const bool keypad = modifiers.testFlag(Qt::KeypadModifier);
    switch (key) {
    case Qt::Key_Backspace: return 0x08;
    case Qt::Key_Tab:
    case Qt::Key_Backtab: return 0x09;
    case Qt::Key_Clear: return 0x0c;
    case Qt::Key_Return:
    case Qt::Key_Enter: return 0x0d;
    case Qt::Key_Shift: return 0x10;
    case Qt::Key_Control: return 0x11;
    case Qt::Key_Alt: return 0x12;
    case Qt::Key_Pause: return 0x13;
    case Qt::Key_CapsLock: return 0x14;
    case Qt::Key_Escape: return 0x1b;
    case Qt::Key_Space: return 0x20;
    case Qt::Key_PageUp: return 0x21;
    case Qt::Key_PageDown: return 0x22;
    case Qt::Key_End: return 0x23;
    case Qt::Key_Home: return 0x24;
    case Qt::Key_Left: return 0x25;
    case Qt::Key_Up: return 0x26;
    case Qt::Key_Right: return 0x27;
    case Qt::Key_Down: return 0x28;
    case Qt::Key_Select: return 0x29;
    case Qt::Key_Print: return 0x2a;
    case Qt::Key_Execute: return 0x2b;
    case Qt::Key_Insert: return 0x2d;
    case Qt::Key_Delete: return 0x2e;
    case Qt::Key_Help: return 0x2f;
    case Qt::Key_Meta:
    case Qt::Key_Super_L: return 0x5b;
    case Qt::Key_Super_R: return 0x5c;
    case Qt::Key_Menu: return 0x5d;
    case Qt::Key_Asterisk: return keypad ? 0x6a : '8';
    case Qt::Key_Plus: return keypad ? 0x6b : 0xbb;
    case Qt::Key_Minus:
    case Qt::Key_Underscore: return keypad ? 0x6d : 0xbd;
    case Qt::Key_Period: return keypad ? 0x6e : 0xbe;
    case Qt::Key_Slash:
    case Qt::Key_Question: return keypad ? 0x6f : 0xbf;
    case Qt::Key_NumLock: return 0x90;
    case Qt::Key_ScrollLock: return 0x91;
    case Qt::Key_Semicolon:
    case Qt::Key_Colon: return 0xba;
    case Qt::Key_Equal: return 0xbb;
    case Qt::Key_Comma:
    case Qt::Key_Less: return 0xbc;
    case Qt::Key_Greater: return 0xbe;
    case Qt::Key_QuoteLeft:
    case Qt::Key_AsciiTilde: return 0xc0;
    case Qt::Key_BracketLeft:
    case Qt::Key_BraceLeft: return 0xdb;
    case Qt::Key_Backslash:
    case Qt::Key_Bar: return 0xdc;
    case Qt::Key_BracketRight:
    case Qt::Key_BraceRight: return 0xdd;
    case Qt::Key_Apostrophe:
    case Qt::Key_QuoteDbl: return 0xde;
    case Qt::Key_Exclam: return '1';
    case Qt::Key_At: return '2';
    case Qt::Key_NumberSign: return '3';
    case Qt::Key_Dollar: return '4';
    case Qt::Key_Percent: return '5';
    case Qt::Key_AsciiCircum: return '6';
    case Qt::Key_Ampersand: return '7';
    case Qt::Key_ParenLeft: return '9';
    case Qt::Key_ParenRight: return '0';
    default: return 0;
    }
}

int firstCharacter(const QString &text) {
    return text.isEmpty() ? 0 : text.front().unicode();
}

int unmodifiedCharacter(const QKeyEvent *event) {
    if (event->key() >= Qt::Key_A && event->key() <= Qt::Key_Z) {
        return event->modifiers().testFlag(Qt::ShiftModifier)
                   ? event->key()
                   : event->key() - Qt::Key_A + 'a';
    }
    if (event->key() == Qt::Key_Return || event->key() == Qt::Key_Enter) {
        return '\r';
    }
    return firstCharacter(event->text());
}

int keyCharacter(const QKeyEvent *event, int windowsKeyCode) {
    if (event->modifiers().testFlag(Qt::ControlModifier) &&
        windowsKeyCode >= 'A' && windowsKeyCode <= 'Z') {
        return windowsKeyCode - 'A' + 1;
    }
    return firstCharacter(event->text());
}

int cefButton(Qt::MouseButton button) {
    switch (button) {
    case Qt::LeftButton: return 0;
    case Qt::MiddleButton: return 1;
    case Qt::RightButton: return 2;
    default: return -1;
    }
}

int wirePointerType(QPointingDevice::PointerType type) {
    switch (type) {
    case QPointingDevice::PointerType::Pen:
        return 2;
    case QPointingDevice::PointerType::Eraser:
        return 3;
    default:
        return 1;
    }
}

int wireTouchEventType(QEventPoint::State state, bool cancelled) {
    if (cancelled) {
        return 4;
    }
    switch (state) {
    case QEventPoint::State::Pressed:
        return 2;
    case QEventPoint::State::Updated:
        return 3;
    case QEventPoint::State::Released:
        return 1;
    default:
        return 0;
    }
}

class ImportedFrame {
public:
    ~ImportedFrame() {
        delete texture;
        if (image != EGL_NO_IMAGE_KHR && destroyImage != nullptr) {
            destroyImage(display, image);
        }
    }

    std::shared_ptr<DmabufFrame> frame;
    EGLDisplay display = EGL_NO_DISPLAY;
    EGLImageKHR image = EGL_NO_IMAGE_KHR;
    PFNEGLDESTROYIMAGEKHRPROC destroyImage = nullptr;
    QSGTexture *texture = nullptr;
};

class RenderRetirement {
public:
    void retire(std::unique_ptr<ImportedFrame> frame) {
        if (frame != nullptr) {
            pending_.push_back(std::move(frame));
        }
    }

    void afterRendering() {
        for (const auto leaseId : presented_) {
            deb_present_dmabuf_lease(leaseId);
        }
        presented_.clear();
        if (pending_.empty()) {
            return;
        }
        auto *context = QOpenGLContext::currentContext();
        if (context == nullptr) {
            return;
        }
        auto *functions = context->extraFunctions();
        const GLsync fence = functions->glFenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0);
        functions->glFlush();
        if (fence == nullptr) {
            functions->glFinish();
            pending_.clear();
            return;
        }
        batches_.push_back({fence, std::move(pending_)});
        pending_.clear();
    }

    void beforeRendering() {
        auto *context = QOpenGLContext::currentContext();
        if (context == nullptr) {
            return;
        }
        auto *functions = context->extraFunctions();
        for (auto batch = batches_.begin(); batch != batches_.end();) {
            const GLenum result =
                functions->glClientWaitSync(batch->fence, 0, 0);
            if (result == GL_ALREADY_SIGNALED ||
                result == GL_CONDITION_SATISFIED) {
                functions->glDeleteSync(batch->fence);
                batch = batches_.erase(batch);
            } else {
                ++batch;
            }
        }
    }

    void invalidate() {
        auto *context = QOpenGLContext::currentContext();
        if (context != nullptr) {
            auto *functions = context->extraFunctions();
            functions->glFinish();
            for (const auto &batch : batches_) {
                functions->glDeleteSync(batch.fence);
            }
        }
        batches_.clear();
        pending_.clear();
        presented_.clear();
    }

    void presented(unsigned long long leaseId) {
        if (leaseId != 0) {
            presented_.push_back(leaseId);
        }
    }

private:
    struct Batch {
        GLsync fence;
        std::vector<std::unique_ptr<ImportedFrame>> frames;
    };
    std::vector<std::unique_ptr<ImportedFrame>> pending_;
    std::vector<Batch> batches_;
    std::vector<unsigned long long> presented_;
};

std::unique_ptr<ImportedFrame>
importFrame(QQuickWindow *window, const std::shared_ptr<DmabufFrame> &frame) {
    auto *context = QOpenGLContext::currentContext();
    if (window == nullptr || context == nullptr || frame == nullptr ||
        frame->fds.empty() || frame->fds.size() > 4) {
        return nullptr;
    }
    auto *nativeContext =
        context->nativeInterface<QNativeInterface::QEGLContext>();
    if (nativeContext == nullptr) {
        return nullptr;
    }
    const EGLDisplay display = nativeContext->display();
    auto createImage = reinterpret_cast<PFNEGLCREATEIMAGEKHRPROC>(
        eglGetProcAddress("eglCreateImageKHR"));
    auto destroyImage = reinterpret_cast<PFNEGLDESTROYIMAGEKHRPROC>(
        eglGetProcAddress("eglDestroyImageKHR"));
    auto imageTarget = reinterpret_cast<PFNGLEGLIMAGETARGETTEXTURE2DOESPROC>(
        eglGetProcAddress("glEGLImageTargetTexture2DOES"));
    if (display == EGL_NO_DISPLAY || createImage == nullptr ||
        destroyImage == nullptr || imageTarget == nullptr) {
        return nullptr;
    }

    static constexpr std::array<EGLint, 4> planeFd = {
        EGL_DMA_BUF_PLANE0_FD_EXT, EGL_DMA_BUF_PLANE1_FD_EXT,
        EGL_DMA_BUF_PLANE2_FD_EXT, EGL_DMA_BUF_PLANE3_FD_EXT};
    static constexpr std::array<EGLint, 4> planeOffset = {
        EGL_DMA_BUF_PLANE0_OFFSET_EXT, EGL_DMA_BUF_PLANE1_OFFSET_EXT,
        EGL_DMA_BUF_PLANE2_OFFSET_EXT, EGL_DMA_BUF_PLANE3_OFFSET_EXT};
    static constexpr std::array<EGLint, 4> planePitch = {
        EGL_DMA_BUF_PLANE0_PITCH_EXT, EGL_DMA_BUF_PLANE1_PITCH_EXT,
        EGL_DMA_BUF_PLANE2_PITCH_EXT, EGL_DMA_BUF_PLANE3_PITCH_EXT};
    static constexpr std::array<EGLint, 4> modifierLo = {
        EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
        EGL_DMA_BUF_PLANE1_MODIFIER_LO_EXT,
        EGL_DMA_BUF_PLANE2_MODIFIER_LO_EXT,
        EGL_DMA_BUF_PLANE3_MODIFIER_LO_EXT};
    static constexpr std::array<EGLint, 4> modifierHi = {
        EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
        EGL_DMA_BUF_PLANE1_MODIFIER_HI_EXT,
        EGL_DMA_BUF_PLANE2_MODIFIER_HI_EXT,
        EGL_DMA_BUF_PLANE3_MODIFIER_HI_EXT};

    std::vector<EGLint> attributes = {
        EGL_WIDTH, static_cast<EGLint>(frame->width),
        EGL_HEIGHT, static_cast<EGLint>(frame->height),
        EGL_LINUX_DRM_FOURCC_EXT, static_cast<EGLint>(frame->drmFormat)};
    for (std::size_t index = 0; index < frame->fds.size(); ++index) {
        attributes.insert(attributes.end(), {
            planeFd[index], frame->fds[index],
            planeOffset[index], static_cast<EGLint>(frame->offsets[index]),
            planePitch[index], static_cast<EGLint>(frame->strides[index])});
        if (frame->modifier != DrmFormatModifierInvalid) {
            attributes.insert(attributes.end(), {
                modifierLo[index], static_cast<EGLint>(frame->modifier),
                modifierHi[index], static_cast<EGLint>(frame->modifier >> 32)});
        }
    }
    attributes.push_back(EGL_NONE);
    const EGLImageKHR image = createImage(
        display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, nullptr,
        attributes.data());
    if (image == EGL_NO_IMAGE_KHR) {
        return nullptr;
    }

    const int acquireFence = frame->takeAcquireFence();
    if (acquireFence >= 0) {
        auto createSync = reinterpret_cast<PFNEGLCREATESYNCKHRPROC>(
            eglGetProcAddress("eglCreateSyncKHR"));
        auto waitSync = reinterpret_cast<PFNEGLWAITSYNCKHRPROC>(
            eglGetProcAddress("eglWaitSyncKHR"));
        auto destroySync = reinterpret_cast<PFNEGLDESTROYSYNCKHRPROC>(
            eglGetProcAddress("eglDestroySyncKHR"));
        const EGLint syncAttributes[] = {
            EGL_SYNC_NATIVE_FENCE_FD_ANDROID, acquireFence, EGL_NONE};
        const EGLSyncKHR sync =
            createSync != nullptr && waitSync != nullptr && destroySync != nullptr
                ? createSync(display, EGL_SYNC_NATIVE_FENCE_ANDROID,
                             syncAttributes)
                : EGL_NO_SYNC_KHR;
        if (sync != EGL_NO_SYNC_KHR) {
            waitSync(display, sync, 0);
            destroySync(display, sync);
        } else {
            close(acquireFence);
        }
    }

    auto *functions = context->functions();
    GLuint textureId = 0;
    functions->glGenTextures(1, &textureId);
    functions->glBindTexture(GL_TEXTURE_2D, textureId);
    functions->glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
    functions->glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
    functions->glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S,
                               GL_CLAMP_TO_EDGE);
    functions->glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T,
                               GL_CLAMP_TO_EDGE);
    imageTarget(GL_TEXTURE_2D, image);
    functions->glBindTexture(GL_TEXTURE_2D, 0);

    QQuickWindow::CreateTextureOptions options(
        QQuickWindow::TextureOwnsGLTexture);
    options.setFlag(QQuickWindow::TextureHasAlphaChannel);
    QSGTexture *texture = QNativeInterface::QSGOpenGLTexture::fromNative(
        textureId, window,
        QSize(static_cast<int>(frame->width), static_cast<int>(frame->height)),
        options);
    if (texture == nullptr) {
        functions->glDeleteTextures(1, &textureId);
        destroyImage(display, image);
        return nullptr;
    }
    auto imported = std::make_unique<ImportedFrame>();
    imported->frame = frame;
    imported->display = display;
    imported->image = image;
    imported->destroyImage = destroyImage;
    imported->texture = texture;
    return imported;
}

class DmabufNode final : public QSGSimpleTextureNode {
public:
    DmabufNode(std::unique_ptr<ImportedFrame> imported,
               std::shared_ptr<RenderRetirement> retirement)
        : imported_(std::move(imported)), retirement_(std::move(retirement)) {
        setTexture(imported_->texture);
        if (imported_->frame->flipY) {
            setTextureCoordinatesTransform(QSGSimpleTextureNode::MirrorVertically);
        }
    }

    ~DmabufNode() override {
        retirement_->retire(std::move(imported_));
    }

    const DmabufFrame &frame() const { return *imported_->frame; }

private:
    std::unique_ptr<ImportedFrame> imported_;
    std::shared_ptr<RenderRetirement> retirement_;
};

class SurfaceRootNode final : public QSGNode {
public:
    DmabufNode *view = nullptr;
    DmabufNode *popup = nullptr;
};

struct PendingLayer {
    bool changed = false;
    std::shared_ptr<DmabufFrame> frame;
};

} // namespace

class BrowserSurfacePrivate {
public:
    void connectWindow(QQuickWindow *window) {
        retirement = std::make_shared<RenderRetirement>();
        if (window == nullptr) {
            return;
        }
        QObject::connect(window, &QQuickWindow::beforeRendering, window,
                         [retirement = retirement] {
                             retirement->beforeRendering();
                         }, Qt::DirectConnection);
        QObject::connect(window, &QQuickWindow::afterRendering, window,
                         [retirement = retirement] {
                             retirement->afterRendering();
                         }, Qt::DirectConnection);
        QObject::connect(window, &QQuickWindow::sceneGraphInvalidated, window,
                         [retirement = retirement] {
                             retirement->invalidate();
                         }, Qt::DirectConnection);
    }

    QString surfaceId;
    unsigned long long browserId = 0;
    std::mutex pendingMutex;
    PendingLayer view;
    PendingLayer popup;
    std::shared_ptr<RenderRetirement> retirement =
        std::make_shared<RenderRetirement>();
};

BrowserSurface::BrowserSurface(QQuickItem *parent)
    : QQuickItem(parent), d_(std::make_unique<BrowserSurfacePrivate>()) {
    setFlag(ItemHasContents, true);
    setFlag(ItemAcceptsInputMethod, true);
    setAcceptedMouseButtons(Qt::AllButtons);
    setAcceptHoverEvents(true);
    setAcceptTouchEvents(true);
    setActiveFocusOnTab(true);
    connect(this, &QQuickItem::windowChanged, this,
            [this](QQuickWindow *window) {
                d_->connectWindow(window);
                emit nativeParentWindowChanged();
                update();
            });
}

BrowserSurface::~BrowserSurface() {
    if (!d_->surfaceId.isEmpty() &&
        surfaceRegistry().value(d_->surfaceId) == this) {
        surfaceRegistry().remove(d_->surfaceId);
    }
}

QString BrowserSurface::surfaceId() const { return d_->surfaceId; }

void BrowserSurface::setSurfaceId(const QString &surfaceId) {
    if (d_->surfaceId == surfaceId) {
        return;
    }
    if (!d_->surfaceId.isEmpty() &&
        surfaceRegistry().value(d_->surfaceId) == this) {
        surfaceRegistry().remove(d_->surfaceId);
    }
    {
        std::lock_guard lock(d_->pendingMutex);
        d_->view = {true, nullptr};
        d_->popup = {true, nullptr};
    }
    d_->surfaceId = surfaceId;
    if (!surfaceId.isEmpty()) {
        surfaceRegistry().insert(surfaceId, this);
    }
    emit surfaceIdChanged();
    update();
}

QString BrowserSurface::nativeParentWindow() const {
    return window() == nullptr ? QString() : QString::number(window()->winId());
}

void BrowserSurface::bindBrowser(
    unsigned long long browserId, unsigned long long surfaceGeneration,
    const std::shared_ptr<DmabufFrame> &frame) {
    {
        std::lock_guard lock(d_->pendingMutex);
        d_->browserId = browserId;
        d_->view = {true, frame};
        d_->popup = {true, nullptr};
    }
    if (frame != nullptr) {
        deb_rebind_dmabuf_lease(frame->leaseId, surfaceGeneration);
    }
    const auto cursor = cursorCache().constFind(browserId);
    if (cursor == cursorCache().constEnd()) {
        unsetCursor();
    } else {
        setCursor(*cursor);
    }
    update();
}

void BrowserSurface::setBrowserCursor(unsigned long long browserId,
                                      const QCursor &cursor) {
    if (d_->browserId == browserId) {
        setCursor(cursor);
    }
}

void BrowserSurface::submitFrame(const std::shared_ptr<DmabufFrame> &frame) {
    if (frame == nullptr || frame->browserId != d_->browserId ||
        (frame->layer != 1 && frame->layer != 2)) {
        return;
    }
    {
        std::lock_guard lock(d_->pendingMutex);
        PendingLayer &layer = frame->layer == 1 ? d_->view : d_->popup;
        layer.changed = true;
        layer.frame = frame;
    }
    update();
}

void BrowserSurface::clearLayer(int layer) {
    if (layer != 1 && layer != 2) {
        return;
    }
    {
        std::lock_guard lock(d_->pendingMutex);
        PendingLayer &pending = layer == 1 ? d_->view : d_->popup;
        pending.changed = true;
        pending.frame.reset();
    }
    update();
}

QSGNode *BrowserSurface::updatePaintNode(QSGNode *oldNode,
                                         UpdatePaintNodeData *) {
    auto *root = static_cast<SurfaceRootNode *>(oldNode);
    if (root == nullptr) {
        root = new SurfaceRootNode();
    }
    PendingLayer view;
    PendingLayer popup;
    {
        std::lock_guard lock(d_->pendingMutex);
        view = std::move(d_->view);
        popup = std::move(d_->popup);
        d_->view.changed = false;
        d_->popup.changed = false;
    }
    const auto replace = [this, root](DmabufNode *&node,
                                      const PendingLayer &pending) {
        if (!pending.changed) {
            return;
        }
        if (node != nullptr) {
            root->removeChildNode(node);
            delete node;
            node = nullptr;
        }
        auto imported = importFrame(window(), pending.frame);
        if (imported != nullptr) {
            node = new DmabufNode(std::move(imported), d_->retirement);
            root->appendChildNode(node);
            d_->retirement->presented(node->frame().leaseId);
        }
    };
    replace(root->view, view);
    replace(root->popup, popup);
    if (root->view != nullptr) {
        root->view->setRect(boundingRect());
    }
    if (root->popup != nullptr) {
        const auto &frame = root->popup->frame();
        root->popup->setRect(frame.x, frame.y, frame.width, frame.height);
    }
    return root;
}

void BrowserSurface::geometryChange(const QRectF &newGeometry,
                                    const QRectF &oldGeometry) {
    QQuickItem::geometryChange(newGeometry, oldGeometry);
    update();
}

void BrowserSurface::sendMotion(const QPointF &position,
                                Qt::MouseButtons buttons,
                                Qt::KeyboardModifiers modifiers,
                                bool leaving) {
    emit pointerMoved(qRound(position.x()), qRound(position.y()),
                      cefModifiers(modifiers, buttons), leaving);
}

void BrowserSurface::sendButton(const QPointF &position,
                                Qt::MouseButton button, bool mouseUp,
                                Qt::MouseButtons buttons,
                                Qt::KeyboardModifiers modifiers,
                                int clickCount) {
    const int mappedButton = cefButton(button);
    if (mappedButton >= 0) {
        emit pointerButton(qRound(position.x()), qRound(position.y()),
                           cefModifiers(modifiers, buttons), mappedButton,
                           mouseUp, clickCount);
    }
}

void BrowserSurface::hoverMoveEvent(QHoverEvent *event) {
    sendMotion(event->position(), Qt::NoButton, event->modifiers(), false);
    event->accept();
}

void BrowserSurface::hoverLeaveEvent(QHoverEvent *event) {
    sendMotion(event->position(), Qt::NoButton, event->modifiers(), true);
    event->accept();
}

void BrowserSurface::mouseMoveEvent(QMouseEvent *event) {
    sendMotion(event->position(), event->buttons(), event->modifiers(), false);
    event->accept();
}

void BrowserSurface::mousePressEvent(QMouseEvent *event) {
    forceActiveFocus(Qt::MouseFocusReason);
    clickCount_ = 1;
    sendButton(event->position(), event->button(), false, event->buttons(),
               event->modifiers(), clickCount_);
    event->accept();
}

void BrowserSurface::mouseDoubleClickEvent(QMouseEvent *event) {
    forceActiveFocus(Qt::MouseFocusReason);
    clickCount_ = 2;
    sendButton(event->position(), event->button(), false, event->buttons(),
               event->modifiers(), clickCount_);
    event->accept();
}

void BrowserSurface::mouseReleaseEvent(QMouseEvent *event) {
    sendButton(event->position(), event->button(), true, event->buttons(),
               event->modifiers(), clickCount_);
    event->accept();
}

void BrowserSurface::touchEvent(QTouchEvent *event) {
    if (event->isBeginEvent()) {
        forceActiveFocus(Qt::MouseFocusReason);
    }
    const bool cancelled = event->type() == QEvent::TouchCancel;
    const int modifiers = cefModifiers(event->modifiers(), Qt::NoButton);
    const int pointerType = wirePointerType(event->pointerType());
    for (const QEventPoint &point : event->points()) {
        const int eventType = wireTouchEventType(point.state(), cancelled);
        if (eventType == 0) {
            continue;
        }
        const QSizeF diameter = point.ellipseDiameters();
        emit touchContact(
            point.id(), point.position().x(), point.position().y(),
            diameter.width() / 2.0, diameter.height() / 2.0,
            qDegreesToRadians(point.rotation()),
            qBound(0.0, point.pressure(), 1.0), eventType, modifiers,
            pointerType);
    }
    event->accept();
}

void BrowserSurface::wheelEvent(QWheelEvent *event) {
    QPoint delta = event->angleDelta();
    int modifiers = cefModifiers(event->modifiers(), Qt::NoButton);
    if (!event->pixelDelta().isNull()) {
        delta = event->pixelDelta();
        modifiers |= 1 << 14;
    }
    emit pointerWheel(qRound(event->position().x()),
                      qRound(event->position().y()), modifiers, delta.x(),
                      delta.y());
    event->accept();
}

void BrowserSurface::sendKey(QKeyEvent *event, bool keyUp) {
    int modifiers = cefModifiers(event->modifiers(), Qt::NoButton);
    if (event->isAutoRepeat()) {
        modifiers |= 1 << 13;
    }
    const int windowsKeyCode =
        cefWindowsKeyCode(event->key(), event->modifiers());
    const int nativeKeyCode = static_cast<int>(event->nativeScanCode());
    const int character = keyCharacter(event, windowsKeyCode);
    const int unmodified = unmodifiedCharacter(event);
    emit browserKey(keyUp ? 3 : 1, modifiers, windowsKeyCode, nativeKeyCode,
                    event->modifiers().testFlag(Qt::AltModifier), character,
                    unmodified);
    if (!keyUp) {
        const bool controlCharacter =
            event->modifiers().testFlag(Qt::ControlModifier) &&
            windowsKeyCode >= 'A' && windowsKeyCode <= 'Z';
        if (controlCharacter || event->text().isEmpty()) {
            if (character != 0) {
                emit browserKey(2, modifiers, windowsKeyCode, nativeKeyCode,
                                event->modifiers().testFlag(Qt::AltModifier),
                                character, unmodified);
            }
        } else {
            for (const QChar value : event->text()) {
                emit browserKey(2, modifiers, windowsKeyCode, nativeKeyCode,
                                event->modifiers().testFlag(Qt::AltModifier),
                                value.unicode(), value.unicode());
            }
        }
    }
    event->accept();
}

void BrowserSurface::keyPressEvent(QKeyEvent *event) { sendKey(event, false); }

void BrowserSurface::keyReleaseEvent(QKeyEvent *event) { sendKey(event, true); }

void BrowserSurface::inputMethodEvent(QInputMethodEvent *event) {
    for (const QChar value : event->commitString()) {
        emit browserKey(2, 0, 0, 0, false, value.unicode(), value.unicode());
    }
    event->accept();
}

extern "C" void register_browser_surface() {
    qmlRegisterType<BrowserSurface>("deb_native", 1, 0, "BrowserSurface");
}

extern "C" void deb_browser_surface_submit(
    const char *surfaceId, unsigned long long browserId,
    unsigned long long leaseId, int layer, int x, int y, unsigned int width,
    unsigned int height, unsigned int drmFormat, unsigned long long modifier,
    int flipY, unsigned int planeCount, const int *fds,
    const unsigned int *strides, const unsigned long long *offsets,
    int acquireFenceFd) {
    auto frame = std::make_shared<DmabufFrame>();
    frame->leaseId = leaseId;
    frame->browserId = browserId;
    frame->layer = layer;
    frame->x = x;
    frame->y = y;
    frame->width = width;
    frame->height = height;
    frame->drmFormat = drmFormat;
    frame->modifier = modifier;
    frame->flipY = flipY != 0;
    frame->acquireFenceFd = acquireFenceFd;
    if (planeCount > 0 && planeCount <= 4 && fds != nullptr &&
        strides != nullptr && offsets != nullptr) {
        frame->fds.assign(fds, fds + planeCount);
        frame->strides.assign(strides, strides + planeCount);
        frame->offsets.assign(offsets, offsets + planeCount);
    }
    const QString id = QString::fromUtf8(surfaceId == nullptr ? "" : surfaceId);
    QMetaObject::invokeMethod(
        qApp,
        [id, frame] {
            if (frame->layer == 1) {
                frameCache().insert(frame->browserId, frame);
            }
            if (BrowserSurface *surface = surfaceRegistry().value(id)) {
                surface->submitFrame(frame);
            }
        },
        Qt::QueuedConnection);
}

extern "C" void deb_browser_surface_bind(
    const char *surfaceId, unsigned long long browserId,
    unsigned long long surfaceGeneration) {
    const QString id = QString::fromUtf8(surfaceId == nullptr ? "" : surfaceId);
    QMetaObject::invokeMethod(
        qApp,
        [id, browserId, surfaceGeneration] {
            if (BrowserSurface *surface = surfaceRegistry().value(id)) {
                surface->bindBrowser(browserId, surfaceGeneration,
                                     frameCache().value(browserId));
            }
        },
        Qt::QueuedConnection);
}

extern "C" void deb_browser_surface_forget(unsigned long long browserId) {
    QMetaObject::invokeMethod(
        qApp,
        [browserId] {
            frameCache().remove(browserId);
            cursorCache().remove(browserId);
        },
        Qt::QueuedConnection);
}

extern "C" void deb_browser_surface_set_cursor(
    unsigned long long browserId, int cefType, const unsigned char *customBgra,
    std::size_t customBgraLength, unsigned int width, unsigned int height,
    int hotspotX, int hotspotY, float imageScaleFactor) {
    const QByteArray pixels(
        reinterpret_cast<const char *>(customBgra),
        customBgra == nullptr ? 0 : static_cast<qsizetype>(customBgraLength));
    QMetaObject::invokeMethod(
        qApp,
        [browserId, cefType, pixels, width, height, hotspotX, hotspotY,
         imageScaleFactor] {
            const QCursor cursor = cursorForCefType(
                cefType, pixels, width, height, hotspotX, hotspotY,
                imageScaleFactor);
            cursorCache().insert(browserId, cursor);
            for (BrowserSurface *surface : surfaceRegistry()) {
                if (surface != nullptr) {
                    surface->setBrowserCursor(browserId, cursor);
                }
            }
        },
        Qt::QueuedConnection);
}

extern "C" void deb_browser_surface_clear(const char *surfaceId, int layer) {
    const QString id = QString::fromUtf8(surfaceId == nullptr ? "" : surfaceId);
    QMetaObject::invokeMethod(
        qApp,
        [id, layer] {
            if (BrowserSurface *surface = surfaceRegistry().value(id)) {
                surface->clearLayer(layer);
            }
        },
        Qt::QueuedConnection);
}
