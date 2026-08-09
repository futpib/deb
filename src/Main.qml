pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import QtQuick.Window
import deb
import deb_native

Item {
    id: root
    objectName: "window.main"
    property int nextViewId: 1
    property var detachedWindows: []
    property bool shuttingDown: false
    readonly property int detachedWindowCount: detachedWindows.length
    readonly property var activeWorkspace: {
        if (profileTabs.currentIndex < 0) {
            return null
        }
        return profileRepeater.itemAt(profileTabs.currentIndex)
    }
    readonly property string activeUrl: activeWorkspace === null
        ? "" : activeWorkspace.activeUrl
    readonly property string activeStatus: activeWorkspace === null
        ? "Waiting for native host…" : activeWorkspace.activeStatus
    readonly property string activeEngine: activeWorkspace === null
        ? "chromium" : activeWorkspace.activeEngine()
    readonly property bool contentFullscreen: {
        return activeWorkspace !== null && activeWorkspace.contentFullscreen
    }

    ProfileManager {
        id: profileManager
    }

    ListModel {
        id: profilesModel
    }

    function allocateViewId() {
        return String(nextViewId++)
    }

    function openDetachedWindow(backendObject, profileId, profileName, tabId) {
        const browserWindow = detachedWindowComponent.createObject(root, {
            "backendObject": backendObject,
            "profileId": profileId,
            "profileName": profileName,
            "viewId": allocateViewId(),
            "adoptTabId": tabId ?? ""
        })
        detachedWindows = detachedWindows.concat([browserWindow])
        browserWindow.show()
        return browserWindow
    }

    function containsGlobalPoint(windowObject, globalPoint) {
        if (windowObject === root) {
            const local = root.mapFromGlobal(globalPoint.x, globalPoint.y)
            return root.Window.visibility !== Window.Hidden
                && local.x >= 0 && local.y >= 0
                && local.x < root.width && local.y < root.height
        }
        return windowObject.visible
            && globalPoint.x >= windowObject.x
            && globalPoint.y >= windowObject.y
            && globalPoint.x < windowObject.x + windowObject.width
            && globalPoint.y < windowObject.y + windowObject.height
    }

    function windowTargetAt(backendObject, globalPoint) {
        for (const browserWindow of detachedWindows) {
            if (browserWindow.backendObject === backendObject
                    && containsGlobalPoint(browserWindow, globalPoint)) {
                return browserWindow.viewId
            }
        }
        if (containsGlobalPoint(root, globalPoint)) {
            for (let index = 0; index < profileRepeater.count; ++index) {
                const workspace = profileRepeater.itemAt(index)
                if (workspace.visible && workspace.backendObject === backendObject) {
                    return workspace.viewId
                }
            }
        }
        return ""
    }

    function releaseDetachedWindow(browserWindow) {
        detachedWindows = detachedWindows.filter(candidate => candidate !== browserWindow)
        if (detachedWindows.length === 0
                && root.Window.visibility === Window.Hidden) {
            shutdown()
            Qt.callLater(Qt.quit)
        }
    }

    function newTab() {
        if (activeWorkspace !== null) {
            activeWorkspace.newTab(activeWorkspace.activeEngine())
        }
    }

    function newChromiumTab() {
        if (activeWorkspace !== null) {
            activeWorkspace.newTab("chromium")
        }
    }

    function newFirefoxTab() {
        if (activeWorkspace !== null) {
            activeWorkspace.newTab("firefox")
        }
    }

    function reloadActiveTab() {
        if (activeWorkspace !== null) {
            activeWorkspace.reload()
        }
    }

    function switchActiveEngine(engine) {
        if (activeWorkspace !== null) {
            activeWorkspace.switchActiveEngine(engine)
        }
    }

    function closeActiveTab() {
        if (activeWorkspace !== null) {
            activeWorkspace.closeActiveTab()
        }
    }

    function navigateActive(address) {
        if (activeWorkspace !== null) {
            activeWorkspace.navigate(address)
        }
    }

    function newWindow() {
        if (activeWorkspace !== null) {
            activeWorkspace.newWindow()
        }
    }

    function exitContentFullscreen() {
        if (activeWorkspace !== null) {
            activeWorkspace.exitContentFullscreen()
        }
    }

    function shutdown() {
        if (shuttingDown) {
            return
        }
        shuttingDown = true
        for (let index = 0; index < profileRepeater.count; ++index) {
            profileRepeater.itemAt(index).stop()
        }
    }

    function loadProfiles() {
        const profiles = JSON.parse(profileManager.profilesJson)
        for (const profile of profiles) {
            profilesModel.append({
                "profileId": profile.id,
                "profileName": profile.name,
                "profileViewId": root.allocateViewId()
            })
        }
        if (profilesModel.count > 0) {
            profileTabs.currentIndex = 0
        }
    }

    Component.onCompleted: loadProfiles()

    Connections {
        target: profileManager

        function onLastCreatedProfileJsonChanged() {
            if (profileManager.lastCreatedProfileJson.length === 0) {
                return
            }
            const profile = JSON.parse(profileManager.lastCreatedProfileJson)
            for (let index = 0; index < profilesModel.count; ++index) {
                if (profilesModel.get(index).profileId === profile.id) {
                    return
                }
            }
            profilesModel.append({
                "profileId": profile.id,
                "profileName": profile.name,
                "profileViewId": allocateViewId()
            })
            profileTabs.currentIndex = profilesModel.count - 1
            newProfileName.clear()
        }
    }

    ColumnLayout {
        anchors.fill: parent
        spacing: 0

        ToolBar {
            visible: !root.contentFullscreen
            Layout.fillWidth: true

            RowLayout {
                anchors.fill: parent
                anchors.leftMargin: 10
                anchors.rightMargin: 10
                spacing: 8

                TabBar {
                    id: profileTabs
                    objectName: "profiles.tabs"
                    Accessible.id: objectName
                    Layout.fillWidth: true

                    Repeater {
                        model: profilesModel

                        TabButton {
                            required property string profileId
                            required property string profileName
                            objectName: `profile.tab.${profileId}`
                            Accessible.id: objectName
                            Accessible.name: profileName
                            Accessible.description: `Browser profile ${profileName}`
                            text: profileName
                            ToolTip.visible: hovered
                            ToolTip.text: `Profile ${profileId}`
                        }
                    }
                }

                Label {
                    objectName: "profile.error"
                    Accessible.id: objectName
                    Accessible.role: Accessible.AlertMessage
                    Accessible.name: text
                    visible: profileManager.error.length > 0
                    text: profileManager.error
                    color: palette.brightText
                    elide: Text.ElideRight
                    Layout.maximumWidth: 260
                }

                TextField {
                    id: newProfileName
                    objectName: "profile.name-input"
                    Accessible.id: objectName
                    Accessible.name: "New profile name"
                    placeholderText: "New profile name"
                    Layout.preferredWidth: 170
                    onAccepted: profileManager.create_profile(text)
                }

                Button {
                    objectName: "profile.add"
                    Accessible.id: objectName
                    Accessible.name: text
                    text: "Add profile"
                    enabled: newProfileName.text.trim().length > 0
                    onClicked: profileManager.create_profile(newProfileName.text)
                }
            }
        }

        StackLayout {
            currentIndex: profileTabs.currentIndex
            Layout.fillWidth: true
            Layout.fillHeight: true

            Repeater {
                id: profileRepeater
                model: profilesModel

                ProfileWorkspace {
                }
            }
        }
    }

    Component {
        id: detachedWindowComponent

        ApplicationWindow {
            id: detachedWindow
            required property var backendObject
            required property string profileId
            required property string profileName
            required property string viewId
            required property string adoptTabId
            objectName: `window.${viewId}`
            width: 1280
            height: 760
            minimumWidth: 760
            minimumHeight: 480
            title: `deb · ${profileName}`

            BrowserView {
                id: detachedBrowserView
                anchors.fill: parent
                backendObject: detachedWindow.backendObject
                profileId: detachedWindow.profileId
                profileName: detachedWindow.profileName
                viewId: detachedWindow.viewId
                windowLabel: detachedWindow.title
                viewVisible: detachedWindow.visible
                viewFocused: detachedWindow.active
                adoptTabId: detachedWindow.adoptTabId
            }

            onClosing: function(close) {
                if (detachedBrowserView.registered) {
                    backendObject.unregister_window(viewId)
                    detachedBrowserView.registered = false
                }
                root.releaseDetachedWindow(detachedWindow)
                close.accepted = true
                Qt.callLater(detachedWindow.destroy)
            }

            Component.onDestruction: {
                if (detachedBrowserView.registered) {
                    backendObject.unregister_window(viewId)
                    detachedBrowserView.registered = false
                }
                root.releaseDetachedWindow(detachedWindow)
            }
        }
    }

    component ProfileWorkspace: Item {
        id: workspace
        required property int index
        readonly property string profileId: profilesModel.get(index).profileId
        readonly property string profileName: profilesModel.get(index).profileName
        readonly property string viewId: profilesModel.get(index).profileViewId
        readonly property var backendObject: backend
        readonly property bool contentFullscreen: mainBrowserView.activeFullscreen
        readonly property string activeUrl: mainBrowserView.currentUrl
        readonly property string activeStatus: mainBrowserView.currentStatus
        Backend {
            id: backend
            profileId: workspace.profileId
        }

        BrowserView {
            id: mainBrowserView
            anchors.fill: parent
            backendObject: backend
            profileId: workspace.profileId
            profileName: workspace.profileName
            viewId: workspace.viewId
            windowLabel: `${workspace.profileName} · main window`
            viewVisible: workspace.visible
                && root.Window.visibility !== Window.Hidden
            viewFocused: root.Window.active && workspace.visible
            adoptTabId: ""
            nativeToolbar: true
        }

        function activeEngine() {
            return mainBrowserView.activeEngine()
        }

        function newTab(engine) {
            backend.new_tab(viewId, engine)
        }

        function reload() {
            backend.reload(viewId)
        }

        function switchActiveEngine(engine) {
            mainBrowserView.switchActiveEngine(engine)
        }

        function closeActiveTab() {
            if (mainBrowserView.activeTabId.length > 0) {
                backend.close_tab(mainBrowserView.activeTabId)
            }
        }

        function navigate(address) {
            backend.navigate(viewId, address)
        }

        function newWindow() {
            root.openDetachedWindow(backend, profileId, profileName)
        }

        function exitContentFullscreen() {
            mainBrowserView.exitContentFullscreen()
        }

        function stop() {
            backend.stop()
        }

        Component.onDestruction: stop()
    }

    component BrowserTabStrip: Frame {
        id: tabStripControl
        required property var host
        required property var tabsModel
        required property var moveTargetsModel
        property string draggedTabId: ""
        property point dragTranslation: Qt.point(0, 0)
        padding: 0
        implicitHeight: 38

        function indexAtGlobal(globalPoint) {
            const local = tabList.mapFromGlobal(globalPoint.x, globalPoint.y)
            if (local.x < 0 || local.y < 0 || local.x >= tabList.width
                    || local.y >= tabList.height || tabsModel.count === 0) {
                return -1
            }
            const index = tabList.indexAt(
                local.x + tabList.contentX,
                local.y + tabList.contentY
            )
            return index >= 0 ? index : tabsModel.count - 1
        }

        background: Rectangle {
            color: tabStripControl.palette.window
            border.color: tabStripControl.palette.mid
        }

        contentItem: RowLayout {
            spacing: 0

            ToolButton {
                objectName: `browser.new.default.${tabStripControl.host.viewId}`
                Accessible.id: objectName
                Accessible.name: "Open a new tab"
                icon.name: "tab-new"
                text: "+"
                display: AbstractButton.IconOnly
                Layout.fillHeight: true
                ToolTip.visible: hovered
                ToolTip.text: `Open a new ${tabStripControl.host.activeEngine()} tab`
                onClicked: tabStripControl.host.backendObject.new_tab(
                    tabStripControl.host.viewId,
                    tabStripControl.host.activeEngine()
                )
            }

            ToolButton {
                objectName: `browser.new-menu.${tabStripControl.host.viewId}`
                Accessible.id: objectName
                Accessible.name: "Choose new tab engine"
                text: "▾"
                Layout.fillHeight: true
                Layout.preferredWidth: 22
                onClicked: newTabMenu.open()

                Menu {
                    id: newTabMenu

                    MenuItem {
                        objectName: `browser.new.chromium.${tabStripControl.host.viewId}`
                        Accessible.id: objectName
                        Accessible.name: text
                        text: "New Chromium Tab"
                        icon.name: "tab-new"
                        onTriggered: tabStripControl.host.backendObject.new_tab(
                            tabStripControl.host.viewId, "chromium"
                        )
                    }

                    MenuItem {
                        objectName: `browser.new.firefox.${tabStripControl.host.viewId}`
                        Accessible.id: objectName
                        Accessible.name: text
                        text: "New Firefox Tab"
                        icon.name: "tab-new"
                        onTriggered: tabStripControl.host.backendObject.new_tab(
                            tabStripControl.host.viewId, "firefox"
                        )
                    }
                }
            }

            ListView {
                id: tabList
                objectName: `browser.tabs.${tabStripControl.host.viewId}`
                Accessible.id: objectName
                Accessible.role: Accessible.PageTabList
                Layout.fillWidth: true
                Layout.fillHeight: true
                orientation: ListView.Horizontal
                boundsBehavior: Flickable.StopAtBounds
                clip: true
                model: tabStripControl.tabsModel

                TapHandler {
                    acceptedButtons: Qt.LeftButton
                    onDoubleTapped: function(eventPoint) {
                        const index = tabList.indexAt(
                            eventPoint.position.x + tabList.contentX,
                            eventPoint.position.y + tabList.contentY
                        )
                        if (index < 0) {
                            tabStripControl.host.backendObject.new_tab(
                                tabStripControl.host.viewId,
                                tabStripControl.host.activeEngine()
                            )
                        }
                    }
                }

                delegate: TabButton {
                    id: tabDelegate
                    required property int index
                    required property string tabId
                    required property string engine
                    required property string tabUrl
                    required property string tabTitle
                    required property string tabStatus
                    required property bool loading
                    required property bool crashed
                    objectName: `browser.tab.${tabStripControl.host.profileId}.${tabId}`
                    Accessible.id: objectName
                    Accessible.name: tabTitle || tabUrl
                    Accessible.description: `${engine} tab at ${tabUrl}`
                    Accessible.selected: checked
                    checked: tabId === tabStripControl.host.activeTabId
                    width: Math.max(
                        150,
                        Math.min(260, tabList.width / Math.max(1, tabStripControl.tabsModel.count))
                    )
                    height: tabList.height
                    z: tabDragArea.dragging ? 2 : 0
                    onClicked: tabStripControl.host.requestTabSelection(tabId)

                    contentItem: RowLayout {
                        spacing: 5

                        Label {
                            text: tabDelegate.crashed
                                ? "!"
                                : tabDelegate.loading
                                    ? "↻"
                                    : tabDelegate.engine === "firefox" ? "F" : "C"
                            color: tabDelegate.crashed
                                ? tabDelegate.palette.brightText
                                : tabDelegate.palette.buttonText
                            font.bold: true
                            horizontalAlignment: Text.AlignHCenter
                            Layout.preferredWidth: 14
                        }

                        Label {
                            text: tabDelegate.tabTitle || tabDelegate.tabUrl
                            color: tabDelegate.palette.buttonText
                            elide: Text.ElideLeft
                            verticalAlignment: Text.AlignVCenter
                            Layout.fillWidth: true
                        }

                        ToolButton {
                            objectName: `browser.tab-close.${tabStripControl.host.profileId}.${tabDelegate.tabId}`
                            Accessible.id: objectName
                            Accessible.name: `Close ${tabDelegate.tabTitle || tabDelegate.tabUrl}`
                            icon.name: "tab-close"
                            text: "×"
                            display: AbstractButton.IconOnly
                            flat: true
                            implicitWidth: 24
                            implicitHeight: 24
                            onClicked: tabStripControl.host.backendObject.close_tab(tabDelegate.tabId)
                        }
                    }

                    transform: Translate {
                        x: tabDragArea.dragging
                            ? tabStripControl.dragTranslation.x : 0
                        y: tabDragArea.dragging
                            ? tabStripControl.dragTranslation.y : 0
                    }

                    MouseArea {
                        id: tabDragArea
                        anchors.fill: parent
                        anchors.rightMargin: 30
                        acceptedButtons: Qt.LeftButton | Qt.MiddleButton | Qt.RightButton
                        hoverEnabled: true
                        property bool dragging: false
                        property point pressGlobal: Qt.point(0, 0)

                        onPressed: function(mouse) {
                            if (mouse.button === Qt.LeftButton) {
                                pressGlobal = mapToGlobal(mouse.x, mouse.y)
                                tabStripControl.draggedTabId = tabDelegate.tabId
                                tabStripControl.dragTranslation = Qt.point(0, 0)
                            }
                        }

                        onPositionChanged: function(mouse) {
                            if (!(mouse.buttons & Qt.LeftButton)) {
                                return
                            }
                            const globalPoint = mapToGlobal(mouse.x, mouse.y)
                            const delta = Qt.point(globalPoint.x - pressGlobal.x,
                                                   globalPoint.y - pressGlobal.y)
                            if (!dragging && Math.hypot(delta.x, delta.y)
                                    >= Application.styleHints.startDragDistance) {
                                dragging = true
                            }
                            if (dragging) {
                                tabStripControl.dragTranslation = delta
                            }
                        }

                        onReleased: function(mouse) {
                            if (mouse.button === Qt.LeftButton) {
                                if (dragging) {
                                    tabStripControl.host.finishTabDrag(
                                        tabDelegate.tabId,
                                        mapToGlobal(mouse.x, mouse.y)
                                    )
                                } else {
                                    tabStripControl.host.requestTabSelection(tabDelegate.tabId)
                                }
                            } else if (mouse.button === Qt.MiddleButton) {
                                tabStripControl.host.backendObject.close_tab(tabDelegate.tabId)
                            } else if (mouse.button === Qt.RightButton) {
                                tabMenu.popup()
                            }
                            dragging = false
                            tabStripControl.draggedTabId = ""
                            tabStripControl.dragTranslation = Qt.point(0, 0)
                        }

                        onCanceled: {
                            dragging = false
                            tabStripControl.draggedTabId = ""
                            tabStripControl.dragTranslation = Qt.point(0, 0)
                        }
                    }

                    ToolTip.visible: tabDelegate.hovered && !tabDragArea.dragging
                    ToolTip.text: `${tabDelegate.tabUrl}\n${tabDelegate.tabStatus}`

                    Menu {
                        id: tabMenu

                        MenuItem {
                            text: "Reload Tab"
                            icon.name: "view-refresh"
                            onTriggered: {
                                tabStripControl.host.requestTabSelection(tabDelegate.tabId)
                                Qt.callLater(function() {
                                    tabStripControl.host.backendObject.reload(tabStripControl.host.viewId)
                                })
                            }
                        }

                        Menu {
                            title: "Open With Engine"

                            MenuItem {
                                text: "Chromium"
                                checkable: true
                                checked: tabDelegate.engine === "chromium"
                                onTriggered: tabStripControl.host.backendObject.switch_engine(
                                    tabDelegate.tabId, "chromium"
                                )
                            }

                            MenuItem {
                                text: "Firefox"
                                checkable: true
                                checked: tabDelegate.engine === "firefox"
                                onTriggered: tabStripControl.host.backendObject.switch_engine(
                                    tabDelegate.tabId, "firefox"
                                )
                            }
                        }

                        Menu {
                            objectName: `browser.move-menu.${tabStripControl.host.profileId}.${tabDelegate.tabId}`
                            title: "Move Tab To Window"
                            enabled: tabStripControl.moveTargetsModel.count > 0

                            Repeater {
                                model: tabStripControl.moveTargetsModel

                                MenuItem {
                                    required property string targetId
                                    required property string targetLabel
                                    objectName: `browser.move-target.${targetId}`
                                    Accessible.id: objectName
                                    Accessible.name: `Move tab to ${targetLabel}`
                                    text: targetLabel
                                    onTriggered: tabStripControl.host.backendObject.move_tab(
                                        tabDelegate.tabId, targetId
                                    )
                                }
                            }
                        }

                        MenuItem {
                            objectName: `browser.detach.${tabStripControl.host.profileId}.${tabDelegate.tabId}`
                            Accessible.id: objectName
                            Accessible.name: text
                            text: "Detach Tab"
                            icon.name: "tab-detach"
                            onTriggered: tabStripControl.host.detachTab(tabDelegate.tabId)
                        }

                        MenuSeparator {
                        }

                        MenuItem {
                            text: "Close Tab"
                            icon.name: "tab-close"
                            onTriggered: tabStripControl.host.backendObject.close_tab(tabDelegate.tabId)
                        }
                    }
                }

                ScrollIndicator.horizontal: ScrollIndicator {
                }
            }

            ToolButton {
                objectName: `browser.search-tabs.${tabStripControl.host.viewId}`
                Accessible.id: objectName
                Accessible.name: "Search tabs"
                icon.name: "quickopen"
                text: "⌄"
                display: AbstractButton.IconOnly
                Layout.fillHeight: true
                enabled: tabStripControl.tabsModel.count > 0
                ToolTip.visible: hovered
                ToolTip.text: "List all tabs"
                onClicked: tabsMenu.open()

                Menu {
                    id: tabsMenu

                    Repeater {
                        model: tabStripControl.tabsModel

                        MenuItem {
                            required property string tabId
                            required property string engine
                            required property string tabUrl
                            required property string tabTitle
                            text: `${engine === "firefox" ? "F" : "C"}  ${tabTitle || tabUrl}`
                            checkable: true
                            checked: tabId === tabStripControl.host.activeTabId
                            onTriggered: tabStripControl.host.requestTabSelection(tabId)
                        }
                    }
                }
            }

            ToolButton {
                objectName: `browser.close.${tabStripControl.host.viewId}`
                Accessible.id: objectName
                Accessible.name: "Close this tab"
                icon.name: "tab-close"
                text: "×"
                display: AbstractButton.IconOnly
                Layout.fillHeight: true
                enabled: tabStripControl.host.activeTabId.length > 0
                ToolTip.visible: hovered
                ToolTip.text: "Close this tab"
                onClicked: tabStripControl.host.backendObject.close_tab(
                    tabStripControl.host.activeTabId
                )
            }
        }
    }

    component BrowserView: Item {
        id: browserView
        required property var backendObject
        required property string profileId
        required property string profileName
        required property string viewId
        required property string windowLabel
        required property bool viewVisible
        required property bool viewFocused
        required property string adoptTabId
        property bool nativeToolbar: false
        property bool rebuildingTabs: false
        property bool registered: false
        property string activeTabId: ""
        property string currentUrl: ""
        property string currentStatus: "Waiting for native host…"
        readonly property bool activeFullscreen: {
            const state = JSON.parse(backendObject.windowStateJson)
            const ownWindow = (state.windows ?? []).find(candidate => candidate.id === viewId)
            if (ownWindow === undefined) {
                return false
            }
            const activeTab = ownWindow.tabs.find(tab => tab.id === ownWindow.activeTabId)
            return activeTab !== undefined && activeTab.fullscreen === true
        }
        property bool fullscreenApplied: false
        property int fullscreenRestoreVisibility: Window.Windowed
        objectName: `browser.view.${viewId}`
        Accessible.id: objectName
        Accessible.role: Accessible.Pane
        Accessible.name: windowLabel

        function requestTabSelection(tabId) {
            if (rebuildingTabs || tabId.length === 0 || tabId === activeTabId) {
                return
            }
            backendObject.select_tab(viewId, tabId)
        }

        function selectRelativeTab(offset) {
            if (rebuildingTabs || tabsModel.count < 2) {
                return
            }
            let activeIndex = -1
            for (let index = 0; index < tabsModel.count; ++index) {
                if (tabsModel.get(index).tabId === activeTabId) {
                    activeIndex = index
                    break
                }
            }
            if (activeIndex < 0) {
                return
            }
            const targetIndex = (activeIndex + offset + tabsModel.count) % tabsModel.count
            requestTabSelection(tabsModel.get(targetIndex).tabId)
        }

        function activeEngine() {
            for (let index = 0; index < tabsModel.count; ++index) {
                const tab = tabsModel.get(index)
                if (tab.tabId === activeTabId) {
                    return tab.engine
                }
            }
            return "chromium"
        }

        function switchActiveEngine(engine) {
            if (activeTabId.length > 0 && activeEngine() !== engine) {
                backendObject.switch_engine(activeTabId, engine)
            }
        }

        function findTabIndex(tabId) {
            for (let index = 0; index < tabsModel.count; ++index) {
                if (tabsModel.get(index).tabId === tabId) {
                    return index
                }
            }
            return -1
        }

        function updateTab(index, tab) {
            const roles = {
                "engine": tab.engine,
                "tabUrl": tab.url,
                "tabTitle": tab.title,
                "tabStatus": tab.status,
                "loading": tab.loading,
                "crashed": tab.crashed,
                "fullscreen": tab.fullscreen
            }
            for (const role of Object.keys(roles)) {
                if (tabsModel.get(index)[role] !== roles[role]) {
                    tabsModel.setProperty(index, role, roles[role])
                }
            }
        }

        function reconcileTabs(tabs) {
            for (let targetIndex = 0; targetIndex < tabs.length; ++targetIndex) {
                const tab = tabs[targetIndex]
                let currentIndex = findTabIndex(tab.id)
                if (currentIndex < 0) {
                    tabsModel.insert(targetIndex, {
                        "tabId": tab.id,
                        "engine": tab.engine,
                        "tabUrl": tab.url,
                        "tabTitle": tab.title,
                        "tabStatus": tab.status,
                        "loading": tab.loading,
                        "crashed": tab.crashed,
                        "fullscreen": tab.fullscreen
                    })
                    currentIndex = targetIndex
                } else if (currentIndex !== targetIndex) {
                    tabsModel.move(currentIndex, targetIndex, 1)
                    currentIndex = targetIndex
                }
                updateTab(currentIndex, tab)
            }
            while (tabsModel.count > tabs.length) {
                tabsModel.remove(tabsModel.count - 1)
            }
        }

        function reconcileMoveTargets(windows) {
            const targets = windows.filter(candidate => candidate.id !== viewId)
            for (let targetIndex = 0; targetIndex < targets.length; ++targetIndex) {
                const target = targets[targetIndex]
                let currentIndex = -1
                for (let index = 0; index < moveTargetsModel.count; ++index) {
                    if (moveTargetsModel.get(index).targetId === target.id) {
                        currentIndex = index
                        break
                    }
                }
                if (currentIndex < 0) {
                    moveTargetsModel.insert(targetIndex, {
                        "targetId": target.id,
                        "targetLabel": target.label
                    })
                } else {
                    if (currentIndex !== targetIndex) {
                        moveTargetsModel.move(currentIndex, targetIndex, 1)
                    }
                    if (moveTargetsModel.get(targetIndex).targetLabel !== target.label) {
                        moveTargetsModel.setProperty(targetIndex, "targetLabel", target.label)
                    }
                }
            }
            while (moveTargetsModel.count > targets.length) {
                moveTargetsModel.remove(moveTargetsModel.count - 1)
            }
        }

        function reorderTab(tabId, targetIndex) {
            const currentIndex = findTabIndex(tabId)
            if (currentIndex < 0 || targetIndex < 0 || targetIndex >= tabsModel.count
                    || currentIndex === targetIndex) {
                return
            }
            tabsModel.move(currentIndex, targetIndex, 1)
            backendObject.reorder_tab(viewId, tabId, targetIndex)
        }

        function detachTab(tabId) {
            root.openDetachedWindow(backendObject, profileId, profileName, tabId)
        }

        function finishTabDrag(tabId, globalPoint) {
            const targetWindow = root.windowTargetAt(backendObject, globalPoint)
            if (targetWindow === viewId) {
                const targetIndex = tabStrip.indexAtGlobal(globalPoint)
                if (targetIndex >= 0) {
                    reorderTab(tabId, targetIndex)
                }
            } else if (targetWindow.length > 0) {
                backendObject.move_tab(tabId, targetWindow)
            } else {
                detachTab(tabId)
            }
        }

        ListModel {
            id: tabsModel
        }

        ListModel {
            id: moveTargetsModel
        }

        Shortcut {
            sequence: "Ctrl+Tab"
            context: Qt.WindowShortcut
            enabled: browserView.viewVisible && tabsModel.count > 1
            onActivated: browserView.selectRelativeTab(1)
        }

        Shortcut {
            sequence: "Ctrl+Shift+Tab"
            context: Qt.WindowShortcut
            enabled: browserView.viewVisible && tabsModel.count > 1
            onActivated: browserView.selectRelativeTab(-1)
        }

        Shortcut {
            sequence: "Ctrl+PgDown"
            context: Qt.WindowShortcut
            enabled: browserView.viewVisible && tabsModel.count > 1
            onActivated: browserView.selectRelativeTab(1)
        }

        Shortcut {
            sequence: "Ctrl+PgUp"
            context: Qt.WindowShortcut
            enabled: browserView.viewVisible && tabsModel.count > 1
            onActivated: browserView.selectRelativeTab(-1)
        }

        Shortcut {
            sequence: "Ctrl+Shift+T"
            context: Qt.WindowShortcut
            enabled: browserView.viewVisible && !browserView.nativeToolbar
            onActivated: browserView.backendObject.new_tab(
                browserView.viewId, browserView.activeEngine()
            )
        }

        Shortcut {
            sequence: "Ctrl+W"
            context: Qt.WindowShortcut
            enabled: browserView.viewVisible && !browserView.nativeToolbar
                && browserView.activeTabId.length > 0
            onActivated: browserView.backendObject.close_tab(browserView.activeTabId)
        }

        Connections {
            target: browserView.backendObject

            function onWindowStateJsonChanged() {
                browserView.rebuildState()
            }
        }

        function rebuildState() {
            const state = JSON.parse(backendObject.windowStateJson)
            const windows = state.windows ?? []
            const ownWindow = windows.find(candidate => candidate.id === viewId)
            rebuildingTabs = true
            reconcileMoveTargets(windows)
            if (ownWindow === undefined) {
                activeTabId = ""
                currentUrl = ""
                reconcileTabs([])
                rebuildingTabs = false
                return
            }
            activeTabId = ownWindow.activeTabId
            reconcileTabs(ownWindow.tabs)
            let activeIndex = -1
            for (let tabIndex = 0; tabIndex < ownWindow.tabs.length; ++tabIndex) {
                const tab = ownWindow.tabs[tabIndex]
                if (tab.id === activeTabId) {
                    activeIndex = tabIndex
                    currentUrl = tab.url
                    currentStatus = tab.status
                }
            }
            if (activeIndex >= 0) {
                enginePicker.currentIndex = tabsModel.get(activeIndex).engine === "firefox" ? 1 : 0
            } else {
                currentUrl = ""
                currentStatus = "Waiting for a tab…"
            }
            rebuildingTabs = false
        }

        function syncNativeGeometry() {
            if (!registered && !viewVisible) {
                return
            }
            const globalOrigin = nativeSurface.mapToGlobal(1, 1)
            const firstRegistration = !registered
            backendObject.sync_geometry(
                viewId,
                browserSurface.nativeParentWindow,
                Math.round(globalOrigin.x),
                Math.round(globalOrigin.y),
                Math.round(nativeSurface.width),
                Math.round(nativeSurface.height),
                windowLabel,
                viewVisible,
                viewFocused,
                adoptTabId.length === 0
            )
            registered = true
            if (firstRegistration && adoptTabId.length > 0) {
                backendObject.move_tab(adoptTabId, viewId)
                adoptTabId = ""
            }
        }

        function applyWindowFullscreen() {
            if (nativeToolbar) {
                return
            }
            const hostWindow = browserView.Window.window
            if (hostWindow === null) {
                return
            }
            if (activeFullscreen && !fullscreenApplied) {
                fullscreenRestoreVisibility = hostWindow.visibility
                fullscreenApplied = true
                hostWindow.showFullScreen()
            } else if (!activeFullscreen && fullscreenApplied) {
                fullscreenApplied = false
                if (fullscreenRestoreVisibility === Window.Maximized) {
                    hostWindow.showMaximized()
                } else if (fullscreenRestoreVisibility === Window.Minimized) {
                    hostWindow.showMinimized()
                } else if (fullscreenRestoreVisibility === Window.FullScreen) {
                    hostWindow.showFullScreen()
                } else {
                    hostWindow.showNormal()
                }
            }
        }

        function exitContentFullscreen() {
            if (!activeFullscreen) {
                return
            }
            backendObject.key_event(viewId, 1, 0, 0x1b, 0, false, 0, 0)
            backendObject.key_event(viewId, 3, 0, 0x1b, 0, false, 0, 0)
        }

        onActiveFullscreenChanged: {
            Qt.callLater(applyWindowFullscreen)
        }

        ColumnLayout {
            anchors.fill: parent
            spacing: 0

            ToolBar {
                visible: !browserView.activeFullscreen
                Layout.fillWidth: true

                ColumnLayout {
                    anchors.fill: parent
                    anchors.leftMargin: 10
                    anchors.rightMargin: 10
                    spacing: 4

                    BrowserTabStrip {
                        id: tabStrip
                        Layout.fillWidth: true
                        host: browserView
                        tabsModel: tabsModel
                        moveTargetsModel: moveTargetsModel
                    }

                    RowLayout {
                        visible: !browserView.nativeToolbar
                        Layout.fillWidth: true

                        Button {
                            objectName: browserView.nativeToolbar
                                ? "" : `browser.reload.${browserView.viewId}`
                            Accessible.id: objectName
                            Accessible.name: "Reload this tab"
                            text: "↻"
                            ToolTip.visible: hovered
                            ToolTip.text: "Reload this tab"
                            onClicked: browserView.backendObject.reload(browserView.viewId)
                        }

                        ComboBox {
                            id: enginePicker
                            objectName: browserView.nativeToolbar
                                ? "" : `browser.engine.${browserView.viewId}`
                            Accessible.id: objectName
                            Accessible.name: "Tab engine"
                            model: ["Chromium", "Firefox"]
                            Layout.preferredWidth: 125
                            onActivated: {
                                if (browserView.activeTabId.length > 0) {
                                    browserView.backendObject.switch_engine(
                                        browserView.activeTabId,
                                        currentIndex === 1 ? "firefox" : "chromium"
                                    )
                                }
                            }
                        }

                        TextField {
                            id: address
                            objectName: browserView.nativeToolbar
                                ? "" : `browser.address.${browserView.viewId}`
                            Accessible.id: objectName
                            Accessible.name: "Address"
                            Layout.fillWidth: true
                            text: browserView.currentUrl
                            selectByMouse: true
                            onAccepted: browserView.backendObject.navigate(browserView.viewId, text)
                        }

                        Button {
                            objectName: browserView.nativeToolbar
                                ? "" : `browser.go.${browserView.viewId}`
                            Accessible.id: objectName
                            Accessible.name: text
                            text: "Go"
                            onClicked: browserView.backendObject.navigate(browserView.viewId, address.text)
                        }

                        Button {
                            objectName: browserView.nativeToolbar
                                ? "" : `browser.new-window.${browserView.viewId}`
                            Accessible.id: objectName
                            Accessible.name: text
                            text: "New window"
                            onClicked: root.openDetachedWindow(
                                browserView.backendObject,
                                browserView.profileId,
                                browserView.profileName
                            )
                        }

                        Label {
                            objectName: browserView.nativeToolbar
                                ? "" : `browser.status.${browserView.viewId}`
                            Accessible.id: objectName
                            Accessible.role: Accessible.StaticText
                            Accessible.name: text
                            text: browserView.currentStatus
                            elide: Text.ElideRight
                            Layout.maximumWidth: 260
                        }
                    }
                }
            }

            Rectangle {
                id: nativeSurface
                Layout.fillWidth: true
                Layout.fillHeight: true
                color: "#202124"
                border.color: palette.mid

                Label {
                    Accessible.ignored: true
                    anchors.centerIn: parent
                    text: "Mounting active browser tab…"
                    color: "white"
                    opacity: 0.7
                }

                BrowserSurface {
                    id: browserSurface
                    objectName: `browser.surface.${browserView.viewId}`
                    Accessible.id: objectName
                    Accessible.role: Accessible.WebDocument
                    Accessible.name: `Browser content for ${browserView.profileName}`
                    Accessible.focusable: true
                    Accessible.focused: activeFocus
                    anchors.fill: parent
                    anchors.margins: 1
                    surfaceId: browserView.viewId
                    focus: true

                    onPointerMoved: function(x, y, modifiers, leaving) {
                        browserView.backendObject.pointer_move(
                            browserView.viewId, x, y, modifiers, leaving
                        )
                    }

                    onPointerButton: function(x, y, modifiers, button, mouseUp, clickCount) {
                        browserView.backendObject.pointer_button(
                            browserView.viewId,
                            x,
                            y,
                            modifiers,
                            button,
                            mouseUp,
                            clickCount
                        )
                    }

                    onPointerWheel: function(x, y, modifiers, deltaX, deltaY) {
                        browserView.backendObject.pointer_wheel(
                            browserView.viewId,
                            x,
                            y,
                            modifiers,
                            deltaX,
                            deltaY
                        )
                    }

                    onTouchContact: function(
                        contactId,
                        x,
                        y,
                        radiusX,
                        radiusY,
                        rotationAngle,
                        pressure,
                        eventType,
                        modifiers,
                        pointerType
                    ) {
                        browserView.backendObject.touch_event(
                            browserView.viewId,
                            contactId,
                            x,
                            y,
                            radiusX,
                            radiusY,
                            rotationAngle,
                            pressure,
                            eventType,
                            modifiers,
                            pointerType
                        )
                    }

                    onBrowserKey: function(
                        eventType,
                        modifiers,
                        windowsKeyCode,
                        nativeKeyCode,
                        systemKey,
                        character,
                        unmodifiedCharacter
                    ) {
                        browserView.backendObject.key_event(
                            browserView.viewId,
                            eventType,
                            modifiers,
                            windowsKeyCode,
                            nativeKeyCode,
                            systemKey,
                            character,
                            unmodifiedCharacter
                        )
                    }
                }

            }
        }

        Timer {
            interval: 100
            running: browserView.registered && browserView.viewVisible
            repeat: true
            onTriggered: browserView.syncNativeGeometry()
        }

        onViewVisibleChanged: Qt.callLater(syncNativeGeometry)
        onViewFocusedChanged: Qt.callLater(syncNativeGeometry)
        Component.onCompleted: Qt.callLater(function() {
            syncNativeGeometry()
            applyWindowFullscreen()
        })
    }

    Component.onDestruction: shutdown()
}
