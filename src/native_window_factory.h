#pragma once

#include <QList>
#include <QObject>
#include <QPointer>

class QString;
class QWindow;

class NativeWindowFactory : public QObject {
    Q_OBJECT

public:
    explicit NativeWindowFactory(QObject *parent = nullptr);
    Q_INVOKABLE QWindow *createHost();
    Q_INVOKABLE QString windowId(QWindow *window) const;

private:
    QList<QPointer<QWindow>> windows_;
};

extern "C" void register_native_window_factory();
