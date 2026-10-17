// SPDX-License-Identifier: SSPL-1.0
import QtQuick
import org.kde.kirigami as Kirigami

// Three pulsing dots in a received-style bubble.
Rectangle {
    id: root

    property bool small: false
    readonly property real dot: small ? Kirigami.Units.smallSpacing * 1.5 : Kirigami.Units.smallSpacing * 2

    implicitWidth: dots.implicitWidth + dot * 4
    implicitHeight: dot * (small ? 3 : 4.5)
    width: implicitWidth
    height: implicitHeight
    radius: height / 2
    color: Kirigami.ColorUtils.tintWithAlpha(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, 0.11)

    Row {
        id: dots
        anchors.centerIn: parent
        spacing: root.dot * 0.6
        Repeater {
            model: 3
            delegate: Rectangle {
                required property int index
                width: root.dot
                height: root.dot
                radius: root.dot / 2
                color: Kirigami.Theme.textColor
                opacity: 0.35
                SequentialAnimation on opacity {
                    running: root.visible
                    loops: Animation.Infinite
                    PauseAnimation {
                        duration: index * 180
                    }
                    NumberAnimation {
                        to: 0.9
                        duration: 300
                    }
                    NumberAnimation {
                        to: 0.35
                        duration: 300
                    }
                    PauseAnimation {
                        duration: (2 - index) * 180
                    }
                }
            }
        }
    }
}
