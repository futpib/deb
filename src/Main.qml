pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls.Fusion
import QtQuick.Layouts
import deb
import deb_native

ApplicationWindow {
    id: root
    objectName: "window.main"
    width: 1440
    height: 860
    minimumWidth: 900
    minimumHeight: 560
    visible: true
    title: "deb · Chromium + Gecko"
    property int nextViewId: 1
    property var detachedWindows: []

    ProfileManager {
        id: profileManager
    }

    ListModel {
        id: profilesModel
    }

    function allocateViewId() {
        return String(nextViewId++)
    }

    function openDetachedWindow(backendObject, profileId, profileName) {
        const browserWindow = detachedWindowComponent.createObject(root, {
            "backendObject": backendObject,
            "profileId": profileId,
            "profileName": profileName,
            "viewId": allocateViewId()
        })
        detachedWindows = detachedWindows.concat([browserWindow])
        browserWindow.show()
        return browserWindow
    }

    function releaseDetachedWindow(browserWindow) {
        detachedWindows = detachedWindows.filter(candidate => candidate !== browserWindow)
        if (detachedWindows.length === 0 && !root.visible) {
            Qt.callLater(root.close)
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

    header: ToolBar {
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
        anchors.fill: parent
        currentIndex: profileTabs.currentIndex

        Repeater {
            id: profileRepeater
            model: profilesModel

            ProfileWorkspace {
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
        Backend {
            id: backend
            profileId: workspace.profileId
        }

        BrowserView {
            anchors.fill: parent
            backendObject: backend
            profileId: workspace.profileId
            profileName: workspace.profileName
            viewId: workspace.viewId
            windowLabel: `${workspace.profileName} · main window`
            viewVisible: workspace.visible && root.visible
            viewFocused: root.active && workspace.visible
        }

        function stop() {
            backend.stop()
        }

        Component.onDestruction: stop()
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
        property bool rebuildingTabs: false
        property bool registered: false
        property string activeTabId: ""
        property string currentUrl: ""
        property string currentStatus: "Waiting for native host…"
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
            tabsModel.clear()
            moveTargetsModel.clear()
            for (const candidate of windows) {
                if (candidate.id !== viewId) {
                    moveTargetsModel.append({
                        "targetId": candidate.id,
                        "targetLabel": candidate.label
                    })
                }
            }
            if (ownWindow === undefined) {
                activeTabId = ""
                currentUrl = ""
                rebuildingTabs = false
                return
            }
            activeTabId = ownWindow.activeTabId
            let activeIndex = -1
            for (let tabIndex = 0; tabIndex < ownWindow.tabs.length; ++tabIndex) {
                const tab = ownWindow.tabs[tabIndex]
                tabsModel.append({
                    "tabId": tab.id,
                    "engine": tab.engine,
                    "tabUrl": tab.url,
                    "tabTitle": tab.title,
                    "tabStatus": tab.status,
                    "loading": tab.loading,
                    "crashed": tab.crashed
                })
                if (tab.id === activeTabId) {
                    activeIndex = tabIndex
                    currentUrl = tab.url
                    currentStatus = tab.status
                }
            }
            if (activeIndex >= 0) {
                browserTabs.currentIndex = activeIndex
                enginePicker.currentIndex = tabsModel.get(activeIndex).engine === "firefox" ? 1 : 0
            }
            rebuildingTabs = false
        }

        function syncNativeGeometry() {
            if (!registered && !viewVisible) {
                return
            }
            const globalOrigin = nativeSurface.mapToGlobal(1, 1)
            backendObject.sync_geometry(
                viewId,
                browserSurface.nativeParentWindow,
                Math.round(globalOrigin.x),
                Math.round(globalOrigin.y),
                Math.round(nativeSurface.width),
                Math.round(nativeSurface.height),
                windowLabel,
                viewVisible,
                viewFocused
            )
            registered = true
        }

        ColumnLayout {
            anchors.fill: parent
            spacing: 0

            ToolBar {
                Layout.fillWidth: true

                ColumnLayout {
                    anchors.fill: parent
                    anchors.leftMargin: 10
                    anchors.rightMargin: 10
                    spacing: 4

                    RowLayout {
                        Layout.fillWidth: true
                        spacing: 4

                        TabBar {
                            id: browserTabs
                            objectName: `browser.tabs.${browserView.viewId}`
                            Accessible.id: objectName
                            Layout.fillWidth: true

                            Repeater {
                                model: tabsModel

                                TabButton {
                                    required property string tabId
                                    required property string engine
                                    required property string tabUrl
                                    required property string tabTitle
                                    required property string tabStatus
                                    objectName: `browser.tab.${browserView.profileId}.${tabId}`
                                    Accessible.id: objectName
                                    Accessible.name: tabTitle || tabUrl
                                    Accessible.description: `${engine} tab at ${tabUrl}`
                                    Accessible.selected: checked
                                    text: `${engine === "firefox" ? "🦊" : "◉"} ${tabTitle || tabUrl}`
                                    width: Math.max(150, Math.min(260, implicitWidth))
                                    ToolTip.visible: hovered
                                    ToolTip.text: `${tabUrl}\n${tabStatus}`
                                    onClicked: browserView.requestTabSelection(tabId)
                                }
                            }
                        }

                        Button {
                            objectName: `browser.new.chromium.${browserView.viewId}`
                            Accessible.id: objectName
                            Accessible.name: "New Chromium tab"
                            text: "+ Chromium"
                            onClicked: browserView.backendObject.new_tab(browserView.viewId, "chromium")
                        }

                        Button {
                            objectName: `browser.new.firefox.${browserView.viewId}`
                            Accessible.id: objectName
                            Accessible.name: "New Firefox tab"
                            text: "+ Firefox"
                            onClicked: browserView.backendObject.new_tab(browserView.viewId, "firefox")
                        }
                    }

                    RowLayout {
                        Layout.fillWidth: true

                        Button {
                            objectName: `browser.reload.${browserView.viewId}`
                            Accessible.id: objectName
                            Accessible.name: "Reload this tab"
                            text: "↻"
                            ToolTip.visible: hovered
                            ToolTip.text: "Reload this tab"
                            onClicked: browserView.backendObject.reload(browserView.viewId)
                        }

                        ComboBox {
                            id: enginePicker
                            objectName: `browser.engine.${browserView.viewId}`
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
                            objectName: `browser.address.${browserView.viewId}`
                            Accessible.id: objectName
                            Accessible.name: "Address"
                            Layout.fillWidth: true
                            text: browserView.currentUrl
                            selectByMouse: true
                            onAccepted: browserView.backendObject.navigate(browserView.viewId, text)
                        }

                        Button {
                            objectName: `browser.go.${browserView.viewId}`
                            Accessible.id: objectName
                            Accessible.name: text
                            text: "Go"
                            onClicked: browserView.backendObject.navigate(browserView.viewId, address.text)
                        }

                        Button {
                            objectName: `browser.close.${browserView.viewId}`
                            Accessible.id: objectName
                            Accessible.name: text
                            text: "Close tab"
                            enabled: browserView.activeTabId.length > 0
                            onClicked: browserView.backendObject.close_tab(browserView.activeTabId)
                        }

                        Button {
                            objectName: `browser.new-window.${browserView.viewId}`
                            Accessible.id: objectName
                            Accessible.name: text
                            text: "New window"
                            onClicked: root.openDetachedWindow(
                                browserView.backendObject,
                                browserView.profileId,
                                browserView.profileName
                            )
                        }

                        Button {
                            id: moveButton
                            objectName: `browser.move.${browserView.viewId}`
                            Accessible.id: objectName
                            Accessible.name: text
                            text: "Move tab"
                            enabled: browserView.activeTabId.length > 0 && moveTargetsModel.count > 0
                            onClicked: moveMenu.open()

                            Menu {
                                id: moveMenu

                                Repeater {
                                    model: moveTargetsModel

                                    MenuItem {
                                        required property string targetId
                                        required property string targetLabel
                                        objectName: `browser.move-target.${targetId}`
                                        Accessible.id: objectName
                                        Accessible.name: `Move tab to ${targetLabel}`
                                        text: targetLabel
                                        onTriggered: browserView.backendObject.move_tab(
                                            browserView.activeTabId,
                                            targetId
                                        )
                                    }
                                }
                            }
                        }

                        Label {
                            objectName: `browser.status.${browserView.viewId}`
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
        Component.onCompleted: Qt.callLater(syncNativeGeometry)
    }

    onClosing: function(close) {
        if (detachedWindows.length > 0) {
            close.accepted = false
            root.hide()
            return
        }
        for (let index = 0; index < profileRepeater.count; ++index) {
            profileRepeater.itemAt(index).stop()
        }
        Qt.quit()
    }
}
