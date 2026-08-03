import QtQuick
import QtQuick.Controls.Fusion
import QtQuick.Layouts
import dual_engine_browser
import dual_engine_native

ApplicationWindow {
    id: root
    width: 1440
    height: 860
    minimumWidth: 900
    minimumHeight: 560
    visible: true
    title: "Dual-engine browser · native on-screen surfaces"

    Backend {
        id: backend
    }

    NativeWindowFactory {
        id: nativeWindows
    }

    property var chromiumHost: nativeWindows.createHost()

    function syncNativeGeometry() {
        const firefoxPosition = firefoxPane.surface.mapToGlobal(Qt.point(0, 0))
        backend.sync_geometry(
            nativeWindows.windowId(chromiumHost),
            Math.round(chromiumPane.surface.width),
            Math.round(chromiumPane.surface.height),
            Math.round(firefoxPosition.x),
            Math.round(firefoxPosition.y),
            Math.round(firefoxPane.surface.width),
            Math.round(firefoxPane.surface.height)
        )
    }

    header: ToolBar {
        RowLayout {
            anchors.fill: parent
            anchors.leftMargin: 10
            anchors.rightMargin: 10
            spacing: 8

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
        anchors.fill: parent
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
            title: "Firefox / Gecko · managed stock process"
            status: backend.firefoxStatus
            hostedWindow: null
        }
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

    Timer {
        interval: 100
        running: root.visible
        repeat: true
        onTriggered: root.syncNativeGeometry()
    }

    onClosing: {
        backend.stop()
        Qt.quit()
    }
}
