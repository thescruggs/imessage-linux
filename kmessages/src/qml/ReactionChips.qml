// SPDX-License-Identifier: SSPL-1.0
import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

// Tapback chips overlapping a bubble's top corner.
Row {
    id: root

    property var reactions: []
    property color receivedColor

    spacing: -Kirigami.Units.smallSpacing
    visible: reactions.length > 0

    Repeater {
        model: root.reactions
        delegate: Rectangle {
            id: chip
            required property var modelData
            height: Kirigami.Units.gridUnit * 1.4
            width: Math.max(height, label.implicitWidth + Kirigami.Units.smallSpacing * 2)
            radius: height / 2
            color: modelData.fromMe ? "#1982fc" : root.receivedColor
            border.color: Kirigami.Theme.backgroundColor
            border.width: 2

            QQC2.Label {
                id: label
                anchors.centerIn: parent
                text: chip.modelData.count > 1 ? chip.modelData.glyph + " " + chip.modelData.count : chip.modelData.glyph
                font.pointSize: Kirigami.Theme.smallFont.pointSize
                color: chip.modelData.fromMe ? "white" : Kirigami.Theme.textColor
            }

            QQC2.ToolTip.visible: hover.hovered
            QQC2.ToolTip.text: chip.modelData.senders
            HoverHandler {
                id: hover
            }
        }
    }
}
