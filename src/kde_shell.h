#pragma once

#include <KXmlGuiWindow>

#include <QByteArray>
#include <QPointer>
#include <QString>
#include <Qt>

#include <cstddef>

class QAction;
class QCloseEvent;
class QComboBox;
class QLabel;
class QLineEdit;
class QQuickView;
class QWidget;

class DebMainWindow final : public KXmlGuiWindow {
    Q_OBJECT

public:
    explicit DebMainWindow(const QByteArray &qml = QByteArray());
    ~DebMainWindow() override;

    bool qmlLoaded() const;

protected:
    void closeEvent(QCloseEvent *event) override;
    void saveNewToolbarConfig() override;

private slots:
    void syncQmlState();
    void syncContentFullscreen();

private:
    void setupActions();
    bool loadQml(const QByteArray &qml);
    void invokeRoot(const char *method);
    void invokeRoot(const char *method, const QString &value);
    void applyFullscreen();
    void identifyToolbarWidgets();

    QQuickView *quickView_ = nullptr;
    QPointer<QObject> rootObject_;
    QComboBox *enginePicker_ = nullptr;
    QLineEdit *locationEdit_ = nullptr;
    QLabel *statusLabel_ = nullptr;
    QAction *manualFullscreenAction_ = nullptr;
    QAction *exitFullscreenAction_ = nullptr;
    bool qmlLoaded_ = false;
    bool contentFullscreen_ = false;
    bool fullscreenApplied_ = false;
    Qt::WindowStates restoreWindowState_;
    QList<QPair<QPointer<QWidget>, bool>> chromeVisibility_;
};

extern "C" int deb_run_kde_shell(int argc, char **argv,
                                   const unsigned char *qml, std::size_t qmlSize,
                                   void (*registerRustTypes)());
