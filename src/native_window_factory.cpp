#include "native_window_factory.h"
#include "browser_surface.h"

#include <QQmlEngine>
#include <QString>
#include <QWindow>

NativeWindowFactory::NativeWindowFactory(QObject *parent) : QObject(parent) {}

QWindow *NativeWindowFactory::createHost() {
    auto *window = new QWindow();
    window->QObject::setParent(this);
    windows_.append(window);
    return window;
}

QString NativeWindowFactory::windowId(QWindow *window) const {
    return window == nullptr ? QString() : QString::number(window->winId());
}

extern "C" void register_native_window_factory() {
    qmlRegisterType<NativeWindowFactory>(
        "deb_native", 1, 0, "NativeWindowFactory");
    qmlRegisterType<BrowserSurface>("deb_native", 1, 0, "BrowserSurface");
}
