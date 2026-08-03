import QtQuick
import QtQuick.Controls.Fusion
import QtQuick.Layouts
import dual_engine_browser

ApplicationWindow {
    id: root
    width: 1440
    height: 860
    minimumWidth: 900
    minimumHeight: 560
    visible: true
    title: "CEF / Gecko engine smoke test"

    Backend {
        id: backend
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
                    backend.render()
                }
            }

            Button {
                text: "Render both"
                onClicked: {
                    backend.url = address.text
                    backend.render()
                }
            }
        }
    }

    SplitView {
        anchors.fill: parent
        orientation: Qt.Horizontal

        EnginePane {
            SplitView.fillWidth: true
            title: "CEF / Chromium"
            status: backend.chromiumStatus
            imageSource: backend.chromiumImage === ""
                ? ""
                : `${backend.chromiumImage}?generation=${backend.renderGeneration}`
        }

        EnginePane {
            SplitView.fillWidth: true
            title: "Firefox / Gecko adapter bootstrap"
            status: backend.firefoxStatus
            imageSource: backend.firefoxImage === ""
                ? ""
                : `${backend.firefoxImage}?generation=${backend.renderGeneration}`
        }
    }

    component EnginePane: Pane {
        id: enginePane
        required property string title
        required property string status
        required property url imageSource

        padding: 8

        ColumnLayout {
            anchors.fill: parent
            spacing: 6

            RowLayout {
                Layout.fillWidth: true

                Label {
                    text: enginePane.title
                    font.bold: true
                    font.pixelSize: 16
                }

                Item {
                    Layout.fillWidth: true
                }

                Label {
                    text: enginePane.status
                    opacity: 0.75
                }
            }

            Rectangle {
                Layout.fillWidth: true
                Layout.fillHeight: true
                color: "white"
                border.color: palette.mid

                BusyIndicator {
                    anchors.centerIn: parent
                    running: enginePane.status.startsWith("Rendering")
                    visible: running
                }

                Image {
                    anchors.fill: parent
                    anchors.margins: 1
                    source: enginePane.imageSource
                    fillMode: Image.PreserveAspectFit
                    asynchronous: true
                    cache: false
                }
            }
        }
    }

    Component.onCompleted: backend.render()
}
