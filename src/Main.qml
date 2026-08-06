import QtQuick
import QtQuick.Controls.Fusion
import QtQuick.Layouts
import deb
import deb_native

ApplicationWindow {
    id: root
    width: 1440
    height: 860
    minimumWidth: 900
    minimumHeight: 560
    visible: true
    title: "deb · Chromium + Gecko"

    ProfileManager {
        id: profileManager
    }

    NativeWindowFactory {
        id: nativeWindows
    }

    ListModel {
        id: profilesModel
    }

    function loadProfiles() {
        const profiles = JSON.parse(profileManager.profilesJson)
        for (const profile of profiles) {
            profilesModel.append({
                "profileId": profile.id,
                "profileName": profile.name
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
                "profileName": profile.name
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
                Layout.fillWidth: true

                Repeater {
                    model: profilesModel

                    TabButton {
                        text: profilesModel.get(index).profileName
                        ToolTip.visible: hovered
                        ToolTip.text: `Profile ${profilesModel.get(index).profileId}`
                    }
                }
            }

            Label {
                visible: profileManager.error.length > 0
                text: profileManager.error
                color: palette.brightText
                elide: Text.ElideRight
                Layout.maximumWidth: 260
            }

            TextField {
                id: newProfileName
                placeholderText: "New profile name"
                Layout.preferredWidth: 170
                onAccepted: profileManager.create_profile(text)
            }

            Button {
                text: "Add profile"
                enabled: newProfileName.text.trim().length > 0
                onClicked: profileManager.create_profile(newProfileName.text)
            }
        }
    }

    StackLayout {
        id: profileStack
        anchors.fill: parent
        currentIndex: profileTabs.currentIndex

        Repeater {
            id: profileRepeater
            model: profilesModel

            ProfileWorkspace {
            }
        }
    }

    component ProfileWorkspace: Item {
        id: workspace
        required property int index
        readonly property string profileId: profilesModel.get(index).profileId
        readonly property string profileName: profilesModel.get(index).profileName
        property bool rebuildingTabs: false

        property var browserHost: nativeWindows.createHost()

        ListModel {
            id: tabsModel
        }

        Backend {
            id: backend
            profileId: workspace.profileId
        }

        Connections {
            target: backend

            function onTabsJsonChanged() {
                workspace.rebuildTabs()
            }
        }

        function rebuildTabs() {
            rebuildingTabs = true
            tabsModel.clear()
            const tabs = JSON.parse(backend.tabsJson)
            let activeIndex = -1
            for (let tabIndex = 0; tabIndex < tabs.length; ++tabIndex) {
                const tab = tabs[tabIndex]
                tabsModel.append({
                    "tabId": tab.id,
                    "engine": tab.engine,
                    "tabUrl": tab.url,
                    "tabTitle": tab.title,
                    "tabStatus": tab.status,
                    "loading": tab.loading,
                    "crashed": tab.crashed
                })
                if (tab.id === backend.activeTabId) {
                    activeIndex = tabIndex
                }
            }
            if (activeIndex >= 0) {
                browserTabs.currentIndex = activeIndex
                enginePicker.currentIndex = tabsModel.get(activeIndex).engine === "firefox" ? 1 : 0
            }
            rebuildingTabs = false
        }

        function syncNativeGeometry() {
            backend.sync_geometry(
                nativeWindows.windowId(browserHost),
                Math.round(nativeSurface.width),
                Math.round(nativeSurface.height)
            )
        }

        function stop() {
            backend.stop()
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
                            Layout.fillWidth: true

                            Repeater {
                                model: tabsModel

                                TabButton {
                                    text: `${tabsModel.get(index).engine === "firefox" ? "🦊" : "◉"} ${tabsModel.get(index).tabTitle || tabsModel.get(index).tabUrl}`
                                    width: Math.max(150, Math.min(260, implicitWidth))
                                    ToolTip.visible: hovered
                                    ToolTip.text: `${tabsModel.get(index).tabUrl}\n${tabsModel.get(index).tabStatus}`
                                }
                            }

                            onCurrentIndexChanged: {
                                if (!workspace.rebuildingTabs && currentIndex >= 0 && currentIndex < tabsModel.count) {
                                    backend.select_tab(tabsModel.get(currentIndex).tabId)
                                }
                            }
                        }

                        Button {
                            text: "+ Chromium"
                            onClicked: backend.new_tab("chromium")
                        }

                        Button {
                            text: "+ Firefox"
                            onClicked: backend.new_tab("firefox")
                        }
                    }

                    RowLayout {
                        Layout.fillWidth: true

                        Button {
                            text: "↻"
                            ToolTip.visible: hovered
                            ToolTip.text: "Reload this tab"
                            onClicked: backend.reload()
                        }

                        ComboBox {
                            id: enginePicker
                            model: ["Chromium", "Firefox"]
                            Layout.preferredWidth: 125
                            onActivated: {
                                if (backend.activeTabId.length > 0) {
                                    backend.switch_engine(
                                        backend.activeTabId,
                                        currentIndex === 1 ? "firefox" : "chromium"
                                    )
                                }
                            }
                        }

                        TextField {
                            id: address
                            Layout.fillWidth: true
                            text: backend.url
                            selectByMouse: true
                            onAccepted: {
                                backend.url = text
                                backend.navigate()
                            }
                        }

                        Button {
                            text: "Go"
                            onClicked: {
                                backend.url = address.text
                                backend.navigate()
                            }
                        }

                        Button {
                            text: "Close tab"
                            enabled: backend.activeTabId.length > 0
                            onClicked: backend.close_tab(backend.activeTabId)
                        }

                        Label {
                            text: backend.status
                            elide: Text.ElideRight
                            Layout.maximumWidth: 320
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
                    anchors.centerIn: parent
                    text: "Mounting active browser tab…"
                    color: "white"
                    opacity: 0.7
                }

                WindowContainer {
                    anchors.fill: parent
                    anchors.margins: 1
                    window: browserHost
                    visible: window !== null
                    focus: true
                }
            }
        }

        Timer {
            interval: 100
            running: workspace.visible && root.visible
            repeat: true
            onTriggered: workspace.syncNativeGeometry()
        }

        onVisibleChanged: {
            if (visible) {
                Qt.callLater(syncNativeGeometry)
            }
        }

        Component.onDestruction: stop()
    }

    onClosing: {
        for (let index = 0; index < profileRepeater.count; ++index) {
            profileRepeater.itemAt(index).stop()
        }
        Qt.quit()
    }
}
