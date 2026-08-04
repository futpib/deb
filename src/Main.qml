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

        property var chromiumHost: nativeWindows.createHost()
        property var firefoxHost: nativeWindows.createHost()

        Backend {
            id: backend
            profileId: workspace.profileId
        }

        function syncNativeGeometry() {
            backend.sync_geometry(
                nativeWindows.windowId(chromiumHost),
                Math.round(chromiumPane.surface.width),
                Math.round(chromiumPane.surface.height),
                nativeWindows.windowId(firefoxHost),
                Math.round(firefoxPane.surface.width),
                Math.round(firefoxPane.surface.height)
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

                RowLayout {
                    anchors.fill: parent
                    anchors.leftMargin: 10
                    anchors.rightMargin: 10
                    spacing: 8

                    Label {
                        text: workspace.profileName
                        font.bold: true
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
                        text: "Navigate both"
                        onClicked: {
                            backend.url = address.text
                            backend.navigate()
                        }
                    }
                }
            }

            SplitView {
                Layout.fillWidth: true
                Layout.fillHeight: true
                orientation: Qt.Horizontal

                EnginePane {
                    id: chromiumPane
                    SplitView.fillWidth: true
                    title: "CEF / Chromium"
                    status: backend.chromiumStatus
                    hostedWindow: chromiumHost
                }

                EnginePane {
                    id: firefoxPane
                    SplitView.fillWidth: true
                    title: "Firefox / Gecko · CEF ABI adapter"
                    status: backend.firefoxStatus
                    hostedWindow: firefoxHost
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

    component EnginePane: Pane {
        id: enginePane
        required property string title
        required property string status
        required property var hostedWindow
        property alias surface: nativeSurface

        padding: 8

        ColumnLayout {
            anchors.fill: parent
            spacing: 6

            Label {
                Layout.fillWidth: true
                text: `${enginePane.title}  —  ${enginePane.status}`
                font.bold: true
                font.pixelSize: 15
                elide: Text.ElideRight
            }

            Rectangle {
                id: nativeSurface
                Layout.fillWidth: true
                Layout.fillHeight: true
                color: "#202124"
                border.color: palette.mid

                Label {
                    anchors.centerIn: parent
                    text: "Mounting native browser window…"
                    color: "white"
                    opacity: 0.7
                }

                WindowContainer {
                    anchors.fill: parent
                    anchors.margins: 1
                    window: enginePane.hostedWindow
                    visible: window !== null
                    focus: true
                }
            }
        }
    }

    onClosing: {
        for (let index = 0; index < profileRepeater.count; ++index) {
            profileRepeater.itemAt(index).stop()
        }
        Qt.quit()
    }
}
