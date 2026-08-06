#pragma once

#include <QPointer>
#include <QQuickItem>
#include <QWindow>

class QHoverEvent;
class QInputMethodEvent;
class QKeyEvent;
class QMouseEvent;
class QWheelEvent;

class BrowserSurface : public QQuickItem {
    Q_OBJECT
    Q_PROPERTY(QWindow *sourceWindow READ sourceWindow WRITE setSourceWindow NOTIFY sourceWindowChanged)

public:
    explicit BrowserSurface(QQuickItem *parent = nullptr);
    ~BrowserSurface() override;

    QWindow *sourceWindow() const;
    void setSourceWindow(QWindow *window);
    void damageReceived();

signals:
    void sourceWindowChanged();
    void pointerMoved(int x, int y, int modifiers, bool leaving);
    void pointerButton(int x, int y, int modifiers, int button, bool mouseUp,
                       int clickCount);
    void pointerWheel(int x, int y, int modifiers, int deltaX, int deltaY);
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
    void wheelEvent(QWheelEvent *event) override;
    void keyPressEvent(QKeyEvent *event) override;
    void keyReleaseEvent(QKeyEvent *event) override;
    void inputMethodEvent(QInputMethodEvent *event) override;

private:
    void redirectSource();
    void releaseSource();
    void sendMotion(const QPointF &position, Qt::MouseButtons buttons,
                    Qt::KeyboardModifiers modifiers, bool leaving);
    void sendButton(const QPointF &position, Qt::MouseButton button,
                    bool mouseUp, Qt::MouseButtons buttons,
                    Qt::KeyboardModifiers modifiers, int clickCount);
    void sendKey(QKeyEvent *event, bool keyUp);

    QPointer<QWindow> sourceWindow_;
    unsigned long sourceId_ = 0;
    unsigned long damage_ = 0;
    quint64 generation_ = 0;
    bool redirected_ = false;
    int clickCount_ = 1;
};
