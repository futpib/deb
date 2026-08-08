#pragma once

#include <QQuickItem>
#include <QString>

#include <cstddef>
#include <memory>

class DmabufFrame;
class BrowserSurfacePrivate;
class QCursor;
class QHoverEvent;
class QInputMethodEvent;
class QKeyEvent;
class QMouseEvent;
class QTouchEvent;
class QWheelEvent;

class BrowserSurface : public QQuickItem {
    Q_OBJECT
    Q_PROPERTY(QString surfaceId READ surfaceId WRITE setSurfaceId NOTIFY surfaceIdChanged)
    Q_PROPERTY(QString nativeParentWindow READ nativeParentWindow NOTIFY nativeParentWindowChanged)

public:
    explicit BrowserSurface(QQuickItem *parent = nullptr);
    ~BrowserSurface() override;

    QString surfaceId() const;
    void setSurfaceId(const QString &surfaceId);
    QString nativeParentWindow() const;
    void bindBrowser(unsigned long long browserId,
                     unsigned long long surfaceGeneration,
                     const std::shared_ptr<DmabufFrame> &frame);
    void submitFrame(const std::shared_ptr<DmabufFrame> &frame);
    void clearLayer(int layer);
    void setBrowserCursor(unsigned long long browserId, const QCursor &cursor);

signals:
    void surfaceIdChanged();
    void nativeParentWindowChanged();
    void pointerMoved(int x, int y, int modifiers, bool leaving);
    void pointerButton(int x, int y, int modifiers, int button, bool mouseUp,
                       int clickCount);
    void pointerWheel(int x, int y, int modifiers, int deltaX, int deltaY);
    void touchContact(int id, double x, double y, double radiusX,
                      double radiusY, double rotationAngle, double pressure,
                      int eventType, int modifiers, int pointerType);
    void browserKey(int eventType, int modifiers, int windowsKeyCode,
                    int nativeKeyCode, bool systemKey, int character,
                    int unmodifiedCharacter);

protected:
    QSGNode *updatePaintNode(QSGNode *oldNode,
                             UpdatePaintNodeData *data) override;
    void geometryChange(const QRectF &newGeometry,
                        const QRectF &oldGeometry) override;
    void hoverMoveEvent(QHoverEvent *event) override;
    void hoverLeaveEvent(QHoverEvent *event) override;
    void mouseMoveEvent(QMouseEvent *event) override;
    void mousePressEvent(QMouseEvent *event) override;
    void mouseDoubleClickEvent(QMouseEvent *event) override;
    void mouseReleaseEvent(QMouseEvent *event) override;
    void touchEvent(QTouchEvent *event) override;
    void wheelEvent(QWheelEvent *event) override;
    void keyPressEvent(QKeyEvent *event) override;
    void keyReleaseEvent(QKeyEvent *event) override;
    void inputMethodEvent(QInputMethodEvent *event) override;

private:
    void sendMotion(const QPointF &position, Qt::MouseButtons buttons,
                    Qt::KeyboardModifiers modifiers, bool leaving);
    void sendButton(const QPointF &position, Qt::MouseButton button,
                    bool mouseUp, Qt::MouseButtons buttons,
                    Qt::KeyboardModifiers modifiers, int clickCount);
    void sendKey(QKeyEvent *event, bool keyUp);

    std::unique_ptr<BrowserSurfacePrivate> d_;
    int clickCount_ = 1;
};

extern "C" void register_browser_surface();
extern "C" void deb_browser_surface_submit(
    const char *surfaceId, unsigned long long browserId,
    unsigned long long leaseId, int layer, int x, int y, unsigned int width,
    unsigned int height, unsigned int drmFormat, unsigned long long modifier,
    int flipY, unsigned int planeCount, const int *fds,
    const unsigned int *strides, const unsigned long long *offsets,
    int acquireFenceFd);
extern "C" void deb_browser_surface_clear(const char *surfaceId, int layer);
extern "C" void deb_browser_surface_bind(
    const char *surfaceId, unsigned long long browserId,
    unsigned long long surfaceGeneration);
extern "C" void deb_browser_surface_forget(unsigned long long browserId);
extern "C" void deb_browser_surface_set_cursor(
    unsigned long long browserId, int cefType, const unsigned char *customBgra,
    std::size_t customBgraLength, unsigned int width, unsigned int height,
    int hotspotX, int hotspotY, float imageScaleFactor);
