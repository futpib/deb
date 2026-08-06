#include "browser_surface.h"

#include <QAbstractNativeEventFilter>
#include <QCoreApplication>
#include <QGuiApplication>
#include <QHash>
#include <QHoverEvent>
#include <QInputMethodEvent>
#include <QKeyEvent>
#include <QMouseEvent>
#include <QOpenGLContext>
#include <QOpenGLFunctions>
#include <QQuickWindow>
#include <QSGSimpleTextureNode>
#include <QSGTexture>
#include <QTimer>
#include <QWheelEvent>
#include <QWindow>

#include <QtGui/qguiapplication_platform.h>
#include <QtGui/qopenglcontext_platform.h>
#include <QtQuick/qsgtexture_platform.h>

#include <GL/glx.h>
#include <GL/glxext.h>
#include <X11/Xutil.h>
#include <X11/extensions/Xcomposite.h>
#include <X11/extensions/Xdamage.h>
#include <X11/extensions/Xfixes.h>
#include <X11/extensions/shape.h>
#include <xcb/damage.h>
#include <xcb/xcb.h>

namespace {

Display *display() {
    auto *x11 = qGuiApp->nativeInterface<QNativeInterface::QX11Application>();
    return x11 == nullptr ? nullptr : x11->display();
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
    case Qt::Key_Backspace:
        return 0x08;
    case Qt::Key_Tab:
    case Qt::Key_Backtab:
        return 0x09;
    case Qt::Key_Clear:
        return 0x0c;
    case Qt::Key_Return:
    case Qt::Key_Enter:
        return 0x0d;
    case Qt::Key_Shift:
        return 0x10;
    case Qt::Key_Control:
        return 0x11;
    case Qt::Key_Alt:
        return 0x12;
    case Qt::Key_Pause:
        return 0x13;
    case Qt::Key_CapsLock:
        return 0x14;
    case Qt::Key_Escape:
        return 0x1b;
    case Qt::Key_Space:
        return 0x20;
    case Qt::Key_PageUp:
        return 0x21;
    case Qt::Key_PageDown:
        return 0x22;
    case Qt::Key_End:
        return 0x23;
    case Qt::Key_Home:
        return 0x24;
    case Qt::Key_Left:
        return 0x25;
    case Qt::Key_Up:
        return 0x26;
    case Qt::Key_Right:
        return 0x27;
    case Qt::Key_Down:
        return 0x28;
    case Qt::Key_Select:
        return 0x29;
    case Qt::Key_Print:
        return 0x2a;
    case Qt::Key_Execute:
        return 0x2b;
    case Qt::Key_Insert:
        return 0x2d;
    case Qt::Key_Delete:
        return 0x2e;
    case Qt::Key_Help:
        return 0x2f;
    case Qt::Key_Meta:
    case Qt::Key_Super_L:
        return 0x5b;
    case Qt::Key_Super_R:
        return 0x5c;
    case Qt::Key_Menu:
        return 0x5d;
    case Qt::Key_Asterisk:
        return keypad ? 0x6a : '8';
    case Qt::Key_Plus:
        return keypad ? 0x6b : 0xbb;
    case Qt::Key_Minus:
    case Qt::Key_Underscore:
        return keypad ? 0x6d : 0xbd;
    case Qt::Key_Period:
        return keypad ? 0x6e : 0xbe;
    case Qt::Key_Slash:
    case Qt::Key_Question:
        return keypad ? 0x6f : 0xbf;
    case Qt::Key_NumLock:
        return 0x90;
    case Qt::Key_ScrollLock:
        return 0x91;
    case Qt::Key_Semicolon:
    case Qt::Key_Colon:
        return 0xba;
    case Qt::Key_Equal:
        return 0xbb;
    case Qt::Key_Comma:
    case Qt::Key_Less:
        return 0xbc;
    case Qt::Key_Greater:
        return 0xbe;
    case Qt::Key_QuoteLeft:
    case Qt::Key_AsciiTilde:
        return 0xc0;
    case Qt::Key_BracketLeft:
    case Qt::Key_BraceLeft:
        return 0xdb;
    case Qt::Key_Backslash:
    case Qt::Key_Bar:
        return 0xdc;
    case Qt::Key_BracketRight:
    case Qt::Key_BraceRight:
        return 0xdd;
    case Qt::Key_Apostrophe:
    case Qt::Key_QuoteDbl:
        return 0xde;
    case Qt::Key_Exclam:
        return '1';
    case Qt::Key_At:
        return '2';
    case Qt::Key_NumberSign:
        return '3';
    case Qt::Key_Dollar:
        return '4';
    case Qt::Key_Percent:
        return '5';
    case Qt::Key_AsciiCircum:
        return '6';
    case Qt::Key_Ampersand:
        return '7';
    case Qt::Key_ParenLeft:
        return '9';
    case Qt::Key_ParenRight:
        return '0';
    default:
        return 0;
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
    case Qt::LeftButton:
        return 0;
    case Qt::MiddleButton:
        return 1;
    case Qt::RightButton:
        return 2;
    default:
        return -1;
    }
}

class DamageFilter final : public QAbstractNativeEventFilter {
public:
    static DamageFilter &instance() {
        static DamageFilter filter;
        return filter;
    }

    void add(unsigned long damage, BrowserSurface *surface) {
        if (!installed_) {
            Display *xDisplay = display();
            if (xDisplay == nullptr ||
                !XDamageQueryExtension(xDisplay, &eventBase_, &errorBase_)) {
                return;
            }
            qApp->installNativeEventFilter(this);
            installed_ = true;
        }
        surfaces_.insert(damage, surface);
    }

    void remove(unsigned long damage) { surfaces_.remove(damage); }

    bool nativeEventFilter(const QByteArray &eventType, void *message,
                           qintptr *) override {
        if (eventType != "xcb_generic_event_t") {
            return false;
        }
        auto *event = static_cast<xcb_generic_event_t *>(message);
        if ((event->response_type & 0x7f) != eventBase_ + XDamageNotify) {
            return false;
        }
        auto *damageEvent = reinterpret_cast<xcb_damage_notify_event_t *>(event);
        if (auto surface = surfaces_.value(damageEvent->damage)) {
            surface->damageReceived();
        }
        return false;
    }

private:
    QHash<unsigned long, QPointer<BrowserSurface>> surfaces_;
    int eventBase_ = 0;
    int errorBase_ = 0;
    bool installed_ = false;
};

class CompositedNode final : public QSGSimpleTextureNode {
public:
    CompositedNode(QQuickWindow *window, Window source, quint64 generation)
        : window_(window), source_(source), generation_(generation) {
        importPixmap();
    }

    ~CompositedNode() override { releasePixmap(); }

    Window source() const { return source_; }
    quint64 generation() const { return generation_; }
    bool valid() const { return texture_ != nullptr; }

    void refresh() {
        if (!bound_ || bindTexture_ == nullptr || releaseTexture_ == nullptr) {
            return;
        }
        releaseTexture_(display_, glxPixmap_, GLX_FRONT_LEFT_EXT);
        bindTexture_(display_, glxPixmap_, GLX_FRONT_LEFT_EXT, nullptr);
    }

private:
    void importPixmap() {
        display_ = display();
        QOpenGLContext *context = QOpenGLContext::currentContext();
        if (display_ == nullptr || context == nullptr || source_ == None ||
            window_ == nullptr ||
            context->nativeInterface<QNativeInterface::QGLXContext>() ==
                nullptr) {
            return;
        }

        XWindowAttributes windowAttributes;
        if (!XGetWindowAttributes(display_, source_, &windowAttributes) ||
            windowAttributes.map_state != IsViewable) {
            return;
        }

        int configCount = 0;
        GLXFBConfig *configs =
            glXGetFBConfigs(display_,
                            XScreenNumberOfScreen(windowAttributes.screen),
                            &configCount);
        GLXFBConfig config = nullptr;
        bool yInverted = false;
        const VisualID visualId = XVisualIDFromVisual(windowAttributes.visual);
        for (int index = 0; index < configCount; ++index) {
            int candidateVisual = 0;
            int drawableTypes = 0;
            int bindRgb = 0;
            int bindRgba = 0;
            int targets = 0;
            int candidateYInverted = 0;
            glXGetFBConfigAttrib(display_, configs[index], GLX_VISUAL_ID,
                                 &candidateVisual);
            glXGetFBConfigAttrib(display_, configs[index], GLX_DRAWABLE_TYPE,
                                 &drawableTypes);
            glXGetFBConfigAttrib(display_, configs[index],
                                 GLX_BIND_TO_TEXTURE_RGB_EXT, &bindRgb);
            glXGetFBConfigAttrib(display_, configs[index],
                                 GLX_BIND_TO_TEXTURE_RGBA_EXT, &bindRgba);
            glXGetFBConfigAttrib(display_, configs[index],
                                 GLX_BIND_TO_TEXTURE_TARGETS_EXT, &targets);
            glXGetFBConfigAttrib(display_, configs[index], GLX_Y_INVERTED_EXT,
                                 &candidateYInverted);
            const bool bindable = windowAttributes.depth == 32
                                      ? bindRgba != 0
                                      : bindRgb != 0;
            if (candidateVisual == static_cast<int>(visualId) && bindable &&
                (drawableTypes & GLX_PIXMAP_BIT) &&
                (targets & GLX_TEXTURE_2D_BIT_EXT)) {
                config = configs[index];
                yInverted = candidateYInverted != 0;
                break;
            }
        }
        if (configs != nullptr) {
            XFree(configs);
        }
        if (config == nullptr) {
            return;
        }

        pixmap_ = XCompositeNameWindowPixmap(display_, source_);
        if (pixmap_ == None) {
            return;
        }
        Window root = None;
        int x = 0;
        int y = 0;
        unsigned int width = 0;
        unsigned int height = 0;
        unsigned int border = 0;
        unsigned int depth = 0;
        if (!XGetGeometry(display_, pixmap_, &root, &x, &y, &width, &height,
                          &border, &depth) ||
            width == 0 || height == 0) {
            releasePixmap();
            return;
        }

        const int pixmapAttributes[] = {
            GLX_TEXTURE_TARGET_EXT,
            GLX_TEXTURE_2D_EXT,
            GLX_TEXTURE_FORMAT_EXT,
            depth == 32 ? GLX_TEXTURE_FORMAT_RGBA_EXT
                        : GLX_TEXTURE_FORMAT_RGB_EXT,
            None,
        };
        glxPixmap_ =
            glXCreatePixmap(display_, config, pixmap_, pixmapAttributes);
        if (glxPixmap_ == None) {
            releasePixmap();
            return;
        }

        bindTexture_ = reinterpret_cast<PFNGLXBINDTEXIMAGEEXTPROC>(
            glXGetProcAddressARB(
                reinterpret_cast<const GLubyte *>("glXBindTexImageEXT")));
        releaseTexture_ = reinterpret_cast<PFNGLXRELEASETEXIMAGEEXTPROC>(
            glXGetProcAddressARB(
                reinterpret_cast<const GLubyte *>("glXReleaseTexImageEXT")));
        if (bindTexture_ == nullptr || releaseTexture_ == nullptr) {
            releasePixmap();
            return;
        }

        QOpenGLFunctions *functions = context->functions();
        functions->glGenTextures(1, &textureId_);
        functions->glBindTexture(GL_TEXTURE_2D, textureId_);
        functions->glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER,
                                   GL_LINEAR);
        functions->glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER,
                                   GL_LINEAR);
        functions->glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S,
                                   GL_CLAMP_TO_EDGE);
        functions->glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T,
                                   GL_CLAMP_TO_EDGE);
        bindTexture_(display_, glxPixmap_, GLX_FRONT_LEFT_EXT, nullptr);
        bound_ = true;
        functions->glBindTexture(GL_TEXTURE_2D, 0);

        QQuickWindow::CreateTextureOptions options =
            QQuickWindow::TextureOwnsGLTexture;
        options.setFlag(QQuickWindow::TextureHasAlphaChannel, depth == 32);
        options.setFlag(QQuickWindow::TextureIsOpaque, depth != 32);
        texture_ = QNativeInterface::QSGOpenGLTexture::fromNative(
            textureId_, window_, QSize(static_cast<int>(width),
                                       static_cast<int>(height)),
            options);
        if (texture_ == nullptr) {
            releasePixmap();
            return;
        }
        setTexture(texture_);
        setTextureCoordinatesTransform(
            yInverted ? NoTransform : MirrorVertically);
    }

    void releasePixmap() {
        if (display_ != nullptr && glxPixmap_ != None && bound_ &&
            releaseTexture_ != nullptr) {
            releaseTexture_(display_, glxPixmap_, GLX_FRONT_LEFT_EXT);
        }
        bound_ = false;
        delete texture_;
        texture_ = nullptr;
        textureId_ = 0;
        if (display_ != nullptr && glxPixmap_ != None) {
            glXDestroyPixmap(display_, glxPixmap_);
        }
        glxPixmap_ = None;
        if (display_ != nullptr && pixmap_ != None) {
            XFreePixmap(display_, pixmap_);
        }
        pixmap_ = None;
    }

    QQuickWindow *window_ = nullptr;
    Display *display_ = nullptr;
    Window source_ = None;
    quint64 generation_ = 0;
    Pixmap pixmap_ = None;
    GLXPixmap glxPixmap_ = None;
    GLuint textureId_ = 0;
    QSGTexture *texture_ = nullptr;
    PFNGLXBINDTEXIMAGEEXTPROC bindTexture_ = nullptr;
    PFNGLXRELEASETEXIMAGEEXTPROC releaseTexture_ = nullptr;
    bool bound_ = false;
};

} // namespace

BrowserSurface::BrowserSurface(QQuickItem *parent) : QQuickItem(parent) {
    setFlag(ItemHasContents, true);
    setFlag(ItemAcceptsInputMethod, true);
    setAcceptedMouseButtons(Qt::AllButtons);
    setAcceptHoverEvents(true);
    setActiveFocusOnTab(true);
}

BrowserSurface::~BrowserSurface() { releaseSource(); }

QWindow *BrowserSurface::sourceWindow() const { return sourceWindow_; }

void BrowserSurface::setSourceWindow(QWindow *window) {
    if (sourceWindow_ == window) {
        return;
    }
    releaseSource();
    sourceWindow_ = window;
    if (sourceWindow_ != nullptr) {
        connect(sourceWindow_, &QWindow::visibleChanged, this,
                [this] { redirectSource(); });
        connect(sourceWindow_, &QWindow::widthChanged, this, [this] {
            ++generation_;
            update();
        });
        connect(sourceWindow_, &QWindow::heightChanged, this, [this] {
            ++generation_;
            update();
        });
        QTimer::singleShot(0, this, [this] { redirectSource(); });
    }
    emit sourceWindowChanged();
}

void BrowserSurface::redirectSource() {
    if (sourceWindow_ == nullptr || redirected_) {
        return;
    }
    Display *xDisplay = display();
    if (xDisplay == nullptr) {
        return;
    }
    sourceId_ = sourceWindow_->winId();
    int compositeEvent = 0;
    int compositeError = 0;
    int damageEvent = 0;
    int damageError = 0;
    if (sourceId_ == None ||
        !XCompositeQueryExtension(xDisplay, &compositeEvent, &compositeError) ||
        !XDamageQueryExtension(xDisplay, &damageEvent, &damageError)) {
        sourceId_ = None;
        return;
    }
    XCompositeRedirectWindow(xDisplay, sourceId_, CompositeRedirectManual);
    XserverRegion empty = XFixesCreateRegion(xDisplay, nullptr, 0);
    XFixesSetWindowShapeRegion(xDisplay, sourceId_, ShapeInput, 0, 0, empty);
    XFixesDestroyRegion(xDisplay, empty);
    damage_ = XDamageCreate(xDisplay, sourceId_, XDamageReportNonEmpty);
    DamageFilter::instance().add(damage_, this);
    XSync(xDisplay, False);
    redirected_ = true;
    ++generation_;
    update();
}

void BrowserSurface::releaseSource() {
    if (sourceWindow_ != nullptr) {
        disconnect(sourceWindow_, nullptr, this, nullptr);
    }
    Display *xDisplay = display();
    if (damage_ != 0) {
        DamageFilter::instance().remove(damage_);
        if (xDisplay != nullptr) {
            XDamageDestroy(xDisplay, damage_);
        }
    }
    damage_ = 0;
    if (redirected_ && xDisplay != nullptr && sourceId_ != None) {
        XFixesSetWindowShapeRegion(xDisplay, sourceId_, ShapeInput, 0, 0, None);
        XCompositeUnredirectWindow(xDisplay, sourceId_, CompositeRedirectManual);
        XSync(xDisplay, False);
    }
    redirected_ = false;
    sourceId_ = None;
    sourceWindow_.clear();
    ++generation_;
    update();
}

void BrowserSurface::damageReceived() {
    if (damage_ == 0) {
        return;
    }
    if (Display *xDisplay = display()) {
        XDamageSubtract(xDisplay, damage_, None, None);
        XFlush(xDisplay);
    }
    update();
}

QSGNode *BrowserSurface::updatePaintNode(QSGNode *oldNode,
                                         UpdatePaintNodeData *) {
    auto *node = static_cast<CompositedNode *>(oldNode);
    if (sourceId_ == None || window() == nullptr) {
        delete node;
        return nullptr;
    }
    if (node == nullptr || node->source() != sourceId_ ||
        node->generation() != generation_) {
        delete node;
        node = new CompositedNode(window(), sourceId_, generation_);
        if (!node->valid()) {
            delete node;
            QMetaObject::invokeMethod(
                this,
                [this] { QTimer::singleShot(100, this, [this] { update(); }); },
                Qt::QueuedConnection);
            return nullptr;
        }
    } else {
        node->refresh();
    }
    node->setRect(boundingRect());
    return node;
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
    if (mappedButton < 0) {
        return;
    }
    emit pointerButton(qRound(position.x()), qRound(position.y()),
                       cefModifiers(modifiers, buttons), mappedButton, mouseUp,
                       clickCount);
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

void BrowserSurface::wheelEvent(QWheelEvent *event) {
    QPoint delta = event->angleDelta();
    int modifiers = cefModifiers(event->modifiers(), Qt::NoButton);
    if (!event->pixelDelta().isNull()) {
        delta = event->pixelDelta();
        modifiers |= 1 << 14;
    }
    emit pointerWheel(qRound(event->position().x()),
                      qRound(event->position().y()),
                      modifiers, delta.x(), delta.y());
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
            if (character == 0) {
                event->accept();
                return;
            }
            emit browserKey(2, modifiers, windowsKeyCode, nativeKeyCode,
                            event->modifiers().testFlag(Qt::AltModifier),
                            character, unmodified);
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

void BrowserSurface::keyPressEvent(QKeyEvent *event) {
    sendKey(event, false);
}

void BrowserSurface::keyReleaseEvent(QKeyEvent *event) {
    sendKey(event, true);
}

void BrowserSurface::inputMethodEvent(QInputMethodEvent *event) {
    for (const QChar value : event->commitString()) {
        emit browserKey(2, 0, 0, 0, false, value.unicode(), value.unicode());
    }
    event->accept();
}
