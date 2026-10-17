// SPDX-License-Identifier: SSPL-1.0
import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.imessage.kmessages

// Tapback bar: the six classic reactions plus any emoji.
QQC2.Popup {
    id: root

    property string messageId
    property string myReaction

    signal picked(string kind)

    readonly property var tapbacks: [
        { kind: "love", glyph: "❤️", label: i18n("Love") },
        { kind: "like", glyph: "👍", label: i18n("Like") },
        { kind: "dislike", glyph: "👎", label: i18n("Dislike") },
        { kind: "laugh", glyph: "😂", label: i18n("Laugh") },
        { kind: "emphasize", glyph: "‼️", label: i18n("Emphasize") },
        { kind: "question", glyph: "❓", label: i18n("Question") }
    ]

    padding: Kirigami.Units.smallSpacing
    background: Rectangle {
        radius: height / 2
        color: Kirigami.Theme.backgroundColor
        border.color: Kirigami.ColorUtils.linearInterpolation(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, 0.2)
    }

    contentItem: RowLayout {
        spacing: 0
        Repeater {
            model: root.tapbacks
            delegate: QQC2.ToolButton {
                required property var modelData
                text: modelData.glyph
                // Until our handles are known, "already mine" cannot be told apart.
                enabled: App.daemon.statusKnown
                checkable: true
                checked: root.myReaction === modelData.kind
                font.pointSize: Kirigami.Theme.defaultFont.pointSize * 1.3
                Accessible.name: modelData.label
                QQC2.ToolTip.visible: hovered
                QQC2.ToolTip.text: modelData.label
                onClicked: {
                    root.picked(modelData.kind);
                    root.close();
                }
            }
        }
        QQC2.ToolButton {
            id: more
            icon.name: "smiley-shape"
            text: i18n("Other Emoji…")
            display: QQC2.AbstractButton.IconOnly
            enabled: App.daemon.statusKnown
            onClicked: emoji.open()
            EmojiPicker {
                id: emoji
                y: more.height + Kirigami.Units.smallSpacing
                onPicked: e => {
                    root.picked("emoji:" + e);
                    root.close();
                }
            }
        }
    }
}
