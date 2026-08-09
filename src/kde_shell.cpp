#include "kde_shell.h"

#include "browser_surface.h"

#include <KAboutData>
#include <KActionCollection>
#include <KConfig>
#include <KConfigGroup>
#include <KEditToolBar>
#include <KLocalizedString>
#include <KSharedConfig>
#include <KShortcutsDialog>
#include <KToolBar>

#include <QAction>
#include <QApplication>
#include <QCloseEvent>
#include <QComboBox>
#include <QIcon>
#include <QLabel>
#include <QLineEdit>
#include <QMenu>
#include <QMenuBar>
#include <QMetaObject>
#include <QQmlComponent>
#include <QQmlEngine>
#include <QQuickView>
#include <QSignalBlocker>
#include <QStatusBar>
#include <QTemporaryDir>
#include <QVariant>
#include <QWidgetAction>

#include <cstring>
#include <cstdio>
#include <memory>

int qInitResources_deb_resources();

namespace {
QAction *makeAction(KActionCollection *collection, const QString &name,
                    const QString &text, const QString &icon,
                    const QKeySequence &shortcut = QKeySequence()) {
    auto *action = collection->addAction(name);
    action->setText(text);
    if (!icon.isEmpty()) {
        action->setIcon(QIcon::fromTheme(icon));
    }
    if (!shortcut.isEmpty()) {
        collection->setDefaultShortcut(action, shortcut);
    }
    return action;
}

QStringList toolbarActionNames(const KToolBar *toolbar) {
    QStringList names;
    for (QAction *action : toolbar->actions()) {
        if (!action->isSeparator()) {
            names.append(action->objectName());
        }
    }
    return names;
}

bool triggerToolbarToggle(DebMainWindow *window, const KToolBar *toolbar) {
    window->setupToolbarMenuActions();
    QAction *menuAction = window->toolBarMenuAction();
    QMenu *menu = menuAction == nullptr ? nullptr : menuAction->menu();
    if (menu == nullptr) {
        QAction *toggle = toolbar->toggleViewAction();
        if (toggle == nullptr) {
            return false;
        }
        toggle->trigger();
        return true;
    }

    Q_EMIT menu->aboutToShow();
    for (QAction *action : menu->actions()) {
        if (action->text() == toolbar->windowTitle()) {
            action->trigger();
            return true;
        }
    }
    return false;
}

int selfTestFailure(const char *message) {
    std::fprintf(stderr, "deb: failure: KXMLGUI self-test: %s\n", message);
    return 1;
}

int runKxmlGuiSelfTest() {
    const QStringList expected = {
        QStringLiteral("new-tab"),
        QStringLiteral("reload"),
        QStringLiteral("engine"),
        QStringLiteral("location"),
        QStringLiteral("new-window"),
    };

    auto *first = new DebMainWindow;
    auto *firstToolbar = first->findChild<KToolBar *>(QStringLiteral("mainToolBar"));
    const QStringList actual =
        firstToolbar == nullptr ? QStringList() : toolbarActionNames(firstToolbar);
    if (actual != expected) {
        const QByteArray actualText = actual.join(QLatin1Char(',')).toUtf8();
        std::fprintf(stderr, "deb: KXMLGUI self-test actual toolbar actions: %s\n",
                     actualText.constData());
        return selfTestFailure(
            "default toolbar actions are not new-tab, reload, engine, location, new-window");
    }
    if (first->actionCollection()->action(QStringLiteral("configure-toolbars")) == nullptr ||
        first->actionCollection()->action(QStringLiteral("configure-shortcuts")) == nullptr ||
        first->actionCollection()->action(QStringLiteral("developer-tools")) == nullptr) {
        return selfTestFailure("configuration actions are missing");
    }
    first->show();
    QApplication::processEvents();
    if (firstToolbar->isHidden()) {
        std::fprintf(stderr,
                     "deb: KXMLGUI self-test toolbar state: windowVisible=%d toolbarVisible=%d hidden=%d toggleChecked=%d\n",
                     first->isVisible(), firstToolbar->isVisible(),
                     firstToolbar->isHidden(),
                     firstToolbar->toggleViewAction()->isChecked());
        return selfTestFailure("default toolbar is hidden");
    }
    if (!triggerToolbarToggle(first, firstToolbar)) {
        return selfTestFailure("standard toolbar visibility action is missing");
    }
    QApplication::processEvents();
    if (!firstToolbar->isHidden()) {
        return selfTestFailure("standard toolbar visibility action did not hide toolbar");
    }
    QPointer<DebMainWindow> firstGuard(first);
    first->close();
    QApplication::processEvents();
    if (firstGuard != nullptr) {
        delete first;
    }

    auto *second = new DebMainWindow;
    auto *secondToolbar = second->findChild<KToolBar *>(QStringLiteral("mainToolBar"));
    second->show();
    QApplication::processEvents();
    if (secondToolbar == nullptr || !secondToolbar->isHidden()) {
        const KConfigGroup settings = second->autoSaveConfigGroup();
        std::fprintf(stderr,
                     "deb: restored toolbar state: file=%s group=%s hasState=%d stateSize=%lld hidden=%d\n",
                     settings.config()->name().toUtf8().constData(),
                     settings.name().toUtf8().constData(), settings.hasKey("State"),
                     static_cast<long long>(
                         settings.readEntry("State", QByteArray()).size()),
                     secondToolbar == nullptr ? -1 : secondToolbar->isHidden());
        return selfTestFailure("toolbar visibility did not persist");
    }
    {
        KEditToolBar editor(second->guiFactory(), second);
    }
    QPointer<DebMainWindow> secondGuard(second);
    second->close();
    QApplication::processEvents();
    if (secondGuard != nullptr) {
        delete second;
    }

    std::fputs(
        "deb: KXMLGUI self-test passed: native actions, toolbar editor, order, and persisted visibility\n",
        stderr);
    return 0;
}
}

DebMainWindow::DebMainWindow(const QByteArray &qml) {
    setComponentName(QStringLiteral("deb"), i18nc("@title", "deb"));
    setWindowTitle(i18nc("@title", "deb · Chromium + Gecko"));
    setAttribute(Qt::WA_NativeWindow);
    setupActions();

    if (qml.isEmpty()) {
        setCentralWidget(new QWidget(this));
        qmlLoaded_ = true;
    } else {
        qmlLoaded_ = loadQml(qml);
        if (!qmlLoaded_) {
            setCentralWidget(new QWidget(this));
        }
    }

    constexpr StandardWindowOptions options =
        ToolBar | Keys | StatusBar | Create;
    setupGUI(QSize(1440, 860), options, QStringLiteral("debui.rc"));

    statusLabel_ = new QLabel(statusBar());
    statusLabel_->setObjectName(QStringLiteral("browser.status.1"));
    statusLabel_->setAccessibleIdentifier(QStringLiteral("browser.status.1"));
    statusLabel_->setAccessibleName(i18nc("@info:status", "Waiting for native host…"));
    statusBar()->addPermanentWidget(statusLabel_);
    setAutoSaveSettings(
        KSharedConfig::openStateConfig()->group(QStringLiteral("MainWindow")));
    identifyToolbarWidgets();
    syncQmlState();
    syncContentFullscreen();
}

DebMainWindow::~DebMainWindow() = default;

bool DebMainWindow::qmlLoaded() const { return qmlLoaded_; }

void DebMainWindow::setupActions() {
    KActionCollection *collection = actionCollection();

    QAction *action = makeAction(collection, QStringLiteral("new-tab"),
                                 i18nc("@action", "New Tab"),
                                 QStringLiteral("tab-new"),
                                 QKeySequence(QStringLiteral("Ctrl+Shift+T")));
    connect(action, &QAction::triggered, this,
            [this] { invokeRoot("newTab"); });

    action = makeAction(collection, QStringLiteral("new-chromium-tab"),
                       i18nc("@action", "New Chromium Tab"),
                       QStringLiteral("internet-web-browser"));
    connect(action, &QAction::triggered, this,
            [this] { invokeRoot("newChromiumTab"); });

    action = makeAction(collection, QStringLiteral("new-firefox-tab"),
                       i18nc("@action", "New Firefox Tab"),
                       QStringLiteral("firefox"));
    connect(action, &QAction::triggered, this,
            [this] { invokeRoot("newFirefoxTab"); });

    action = makeAction(collection, QStringLiteral("new-window"),
                       i18nc("@action", "New Window"),
                       QStringLiteral("window-new"),
                       QKeySequence(QStringLiteral("Ctrl+N")));
    connect(action, &QAction::triggered, this,
            [this] { invokeRoot("newWindow"); });

    action = makeAction(collection, QStringLiteral("close-tab"),
                       i18nc("@action", "Close Tab"),
                       QStringLiteral("tab-close"),
                       QKeySequence(QStringLiteral("Ctrl+W")));
    connect(action, &QAction::triggered, this,
            [this] { invokeRoot("closeActiveTab"); });

    action = makeAction(collection, QStringLiteral("file_quit"),
                       i18nc("@action", "Quit"),
                       QStringLiteral("application-exit"),
                       QKeySequence(QStringLiteral("Ctrl+Q")));
    connect(action, &QAction::triggered, this, &QWidget::close);

    action = makeAction(collection, QStringLiteral("reload"),
                       i18nc("@action", "Reload"),
                       QStringLiteral("view-refresh"),
                       QKeySequence(QStringLiteral("Ctrl+R")));
    connect(action, &QAction::triggered, this,
            [this] { invokeRoot("reloadActiveTab"); });

    action = makeAction(collection, QStringLiteral("developer-tools"),
                       i18nc("@action", "Developer Tools"),
                       QStringLiteral("applications-development"),
                       QKeySequence(QStringLiteral("Ctrl+Shift+I")));
    connect(action, &QAction::triggered, this,
            [this] { invokeRoot("openDeveloperTools"); });

    action = makeAction(collection, QStringLiteral("focus-location"),
                       i18nc("@action", "Focus Location"),
                       QStringLiteral("edit-find"),
                       QKeySequence(QStringLiteral("Ctrl+L")));
    connect(action, &QAction::triggered, this, [this] {
        if (locationEdit_ != nullptr) {
            locationEdit_->setFocus(Qt::ShortcutFocusReason);
            locationEdit_->selectAll();
        }
    });

    enginePicker_ = new QComboBox(this);
    enginePicker_->setObjectName(QStringLiteral("browser.engine.1"));
    enginePicker_->setAccessibleIdentifier(QStringLiteral("browser.engine.1"));
    enginePicker_->setAccessibleName(i18nc("@label", "Tab engine"));
    enginePicker_->addItems(
        {i18nc("@item:inlistbox", "Chromium"), i18nc("@item:inlistbox", "Firefox")});
    connect(enginePicker_, &QComboBox::currentTextChanged, this,
            [this](const QString &engine) {
                invokeRoot("switchActiveEngine", engine.toLower());
            });
    auto *engineAction = new QWidgetAction(this);
    engineAction->setObjectName(QStringLiteral("engine"));
    engineAction->setText(i18nc("@action", "Tab Engine"));
    engineAction->setDefaultWidget(enginePicker_);
    collection->addAction(QStringLiteral("engine"), engineAction);

    locationEdit_ = new QLineEdit(this);
    locationEdit_->setObjectName(QStringLiteral("browser.address.1"));
    locationEdit_->setAccessibleIdentifier(QStringLiteral("browser.address.1"));
    locationEdit_->setAccessibleName(i18nc("@label", "Address"));
    locationEdit_->setClearButtonEnabled(true);
    locationEdit_->setMinimumWidth(360);
    locationEdit_->setPlaceholderText(i18nc("@label", "Search or enter address"));
    connect(locationEdit_, &QLineEdit::returnPressed, this, [this] {
        invokeRoot("navigateActive", locationEdit_->text());
    });
    auto *locationAction = new QWidgetAction(this);
    locationAction->setObjectName(QStringLiteral("location"));
    locationAction->setText(i18nc("@action", "Location"));
    locationAction->setDefaultWidget(locationEdit_);
    collection->addAction(QStringLiteral("location"), locationAction);

    manualFullscreenAction_ =
        makeAction(collection, QStringLiteral("view-full-screen"),
                   i18nc("@action", "Full Screen"),
                   QStringLiteral("view-fullscreen"), QKeySequence(Qt::Key_F11));
    manualFullscreenAction_->setCheckable(true);
    connect(manualFullscreenAction_, &QAction::toggled, this,
            [this] { applyFullscreen(); });

    exitFullscreenAction_ =
        makeAction(collection, QStringLiteral("exit-full-screen"),
                   i18nc("@action", "Exit Full Screen"), QString(),
                   QKeySequence(Qt::Key_Escape));
    exitFullscreenAction_->setShortcutContext(Qt::ApplicationShortcut);
    exitFullscreenAction_->setEnabled(false);
    connect(exitFullscreenAction_, &QAction::triggered, this,
            [this] { invokeRoot("exitContentFullscreen"); });

    action = makeAction(collection, QStringLiteral("configure-shortcuts"),
                       i18nc("@action", "Configure Keyboard Shortcuts…"),
                       QStringLiteral("configure-shortcuts"));
    connect(action, &QAction::triggered, this, [this] {
        KShortcutsDialog::showDialog(
            actionCollection(), KShortcutsEditor::LetterShortcutsAllowed, this);
    });

    action = makeAction(collection, QStringLiteral("configure-toolbars"),
                       i18nc("@action", "Configure Toolbars…"),
                       QStringLiteral("configure-toolbars"));
    connect(action, &QAction::triggered, this,
            [this] { configureToolbars(); });
}

bool DebMainWindow::loadQml(const QByteArray &qml) {
    quickView_ = new QQuickView;
    quickView_->setResizeMode(QQuickView::SizeRootObjectToView);
    quickView_->setTitle(windowTitle());

    const QUrl sourceUrl(QStringLiteral("qrc:/deb/Main.qml"));
    QQmlComponent component(quickView_->engine());
    component.setData(qml, sourceUrl);
    QObject *root = component.create();
    if (root == nullptr || component.isError()) {
        for (const QQmlError &error : component.errors()) {
            qCritical().noquote()
                << "deb: failure: KDE shell QML:" << error.toString();
        }
        delete root;
        delete quickView_;
        quickView_ = nullptr;
        return false;
    }
    quickView_->setContent(sourceUrl, &component, root);
    rootObject_ = root;

    auto *container = QWidget::createWindowContainer(quickView_, this);
    container->setObjectName(QStringLiteral("kde.quick-container"));
    container->setMinimumSize(QSize(900, 560));
    container->setFocusPolicy(Qt::StrongFocus);
    setCentralWidget(container);

    connect(root, SIGNAL(activeUrlChanged()), this, SLOT(syncQmlState()));
    connect(root, SIGNAL(activeStatusChanged()), this, SLOT(syncQmlState()));
    connect(root, SIGNAL(activeEngineChanged()), this, SLOT(syncQmlState()));
    connect(root, SIGNAL(contentFullscreenChanged()), this,
            SLOT(syncContentFullscreen()));
    return true;
}

void DebMainWindow::invokeRoot(const char *method) {
    if (rootObject_ != nullptr &&
        !QMetaObject::invokeMethod(rootObject_, method, Qt::DirectConnection)) {
        qWarning().noquote() << "deb: KDE shell could not invoke QML method" << method;
    }
}

void DebMainWindow::invokeRoot(const char *method, const QString &value) {
    if (rootObject_ != nullptr &&
        !QMetaObject::invokeMethod(rootObject_, method, Qt::DirectConnection,
                                   Q_ARG(QVariant, QVariant(value)))) {
        qWarning().noquote() << "deb: KDE shell could not invoke QML method" << method;
    }
}

void DebMainWindow::syncQmlState() {
    if (rootObject_ == nullptr) {
        return;
    }
    const QString url = rootObject_->property("activeUrl").toString();
    if (locationEdit_ != nullptr && !locationEdit_->hasFocus() &&
        locationEdit_->text() != url) {
        locationEdit_->setText(url);
    }
    const QString engine = rootObject_->property("activeEngine").toString();
    if (enginePicker_ != nullptr) {
        const QSignalBlocker blocker(enginePicker_);
        enginePicker_->setCurrentIndex(engine == QStringLiteral("firefox") ? 1 : 0);
    }
    const QString status = rootObject_->property("activeStatus").toString();
    if (statusLabel_ != nullptr) {
        statusLabel_->setText(status);
        statusLabel_->setAccessibleName(status);
    }
}

void DebMainWindow::syncContentFullscreen() {
    contentFullscreen_ = rootObject_ != nullptr &&
                         rootObject_->property("contentFullscreen").toBool();
    if (exitFullscreenAction_ != nullptr) {
        exitFullscreenAction_->setEnabled(contentFullscreen_);
    }
    applyFullscreen();
}

void DebMainWindow::applyFullscreen() {
    const bool wanted = contentFullscreen_ ||
                        (manualFullscreenAction_ != nullptr &&
                         manualFullscreenAction_->isChecked());
    if (wanted == fullscreenApplied_) {
        return;
    }

    fullscreenApplied_ = wanted;
    if (wanted) {
        restoreWindowState_ = windowState();
        chromeVisibility_.clear();
        auto rememberAndHide = [this](QWidget *widget) {
            if (widget != nullptr) {
                chromeVisibility_.append({widget, widget->isVisible()});
                widget->hide();
            }
        };
        rememberAndHide(menuBar());
        rememberAndHide(statusBar());
        for (KToolBar *toolbar : findChildren<KToolBar *>()) {
            rememberAndHide(toolbar);
        }
        showFullScreen();
        return;
    }

    showNormal();
    if (restoreWindowState_.testFlag(Qt::WindowMaximized)) {
        showMaximized();
    } else if (restoreWindowState_.testFlag(Qt::WindowMinimized)) {
        showMinimized();
    }
    for (const auto &[widget, visible] : chromeVisibility_) {
        if (widget != nullptr) {
            widget->setVisible(visible);
        }
    }
    chromeVisibility_.clear();
}

void DebMainWindow::identifyToolbarWidgets() {
    auto *toolbar = findChild<KToolBar *>(QStringLiteral("mainToolBar"));
    if (toolbar == nullptr) {
        return;
    }
    const KConfigGroup settings = autoSaveConfigGroup();
    if (!settings.hasKey(QStringLiteral("State"))) {
        toolbar->show();
    }
    toolbar->setAccessibleName(i18nc("@title:toolbar", "Main Toolbar"));
    toolbar->setAccessibleIdentifier(QStringLiteral("mainToolBar"));
    const QList<QPair<QString, QString>> ids = {
        {QStringLiteral("new-tab"), QStringLiteral("kde.action.new-tab")},
        {QStringLiteral("reload"), QStringLiteral("browser.reload.1")},
        {QStringLiteral("new-window"), QStringLiteral("browser.new-window.1")},
    };
    for (const auto &[actionName, objectName] : ids) {
        QAction *action = actionCollection()->action(actionName);
        if (action != nullptr) {
            if (QWidget *widget = toolbar->widgetForAction(action)) {
                widget->setObjectName(objectName);
                widget->setAccessibleIdentifier(objectName);
                widget->setAccessibleName(action->text());
            }
        }
    }
}

void DebMainWindow::closeEvent(QCloseEvent *event) {
    if (rootObject_ != nullptr &&
        rootObject_->property("detachedWindowCount").toInt() > 0) {
        hide();
        event->ignore();
        return;
    }
    invokeRoot("shutdown");
    if (autoSaveSettings()) {
        saveAutoSaveSettings();
    }
    KXmlGuiWindow::closeEvent(event);
}

void DebMainWindow::saveNewToolbarConfig() {
    KXmlGuiWindow::saveNewToolbarConfig();
    identifyToolbarWidgets();
}

extern "C" int deb_run_kde_shell(int argc, char **argv,
                                   const unsigned char *qml, std::size_t qmlSize,
                                   void (*registerRustTypes)()) {
    qInitResources_deb_resources();
    bool selfTest = false;
    for (int index = 1; index < argc; ++index) {
        if (std::strcmp(argv[index], "--self-test-kxmlgui") == 0) {
            selfTest = true;
            for (int move = index; move + 1 < argc; ++move) {
                argv[move] = argv[move + 1];
            }
            --argc;
            break;
        }
    }

    std::unique_ptr<QTemporaryDir> selfTestConfig;
    if (selfTest) {
        selfTestConfig = std::make_unique<QTemporaryDir>();
        if (!selfTestConfig->isValid()) {
            return selfTestFailure("could not create temporary config");
        }
        qputenv("XDG_CONFIG_HOME", selfTestConfig->path().toUtf8());
        qputenv("XDG_STATE_HOME", selfTestConfig->path().toUtf8());
    }

    QApplication app(argc, argv);
    QApplication::setApplicationName(selfTest ? QStringLiteral("deb-kxmlgui-self-test")
                                              : QStringLiteral("deb"));
    QApplication::setApplicationDisplayName(QStringLiteral("deb"));
    QApplication::setOrganizationDomain(QStringLiteral("kde.org"));
    QApplication::setDesktopFileName(QStringLiteral("org.kde.deb"));
    KLocalizedString::setApplicationDomain("deb");

    KAboutData about(QStringLiteral("deb"), i18nc("@title", "deb"),
                     QStringLiteral("0.1.0"));
    about.setShortDescription(
        i18nc("@info", "A tightly integrated Chromium and Firefox browser shell"));
    KAboutData::setApplicationData(about);

    if (selfTest) {
        QApplication::setQuitOnLastWindowClosed(false);
        return runKxmlGuiSelfTest();
    }

    register_browser_surface();
    registerRustTypes();
    DebMainWindow window(
        QByteArray(reinterpret_cast<const char *>(qml), qsizetype(qmlSize)));
    if (!window.qmlLoaded()) {
        return 2;
    }
    window.show();
    return QApplication::exec();
}
