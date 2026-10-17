// SPDX-License-Identifier: SSPL-1.0
import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

// A small fixed emoji grid.
QQC2.Popup {
    id: root

    signal picked(string emoji)

    readonly property var emojis: ["😀", "😂", "🥹", "😊", "😍", "😘", "😎", "🤔", "😅", "😭", "😡", "😴", "🥳", "🤯", "😬", "🙄", "👍", "👎", "👏", "🙏", "💪", "👋", "🤝", "✌️", "❤️", "🧡", "💛", "💚", "💙", "💜", "🔥", "✨", "🎉", "🎂", "🎁", "☕", "🍕", "🍻", "⚽", "🚗", "✈️", "🏖️", "🌙", "☀️", "🌧️", "❄️", "✅", "❌", "‼️", "❓", "💯", "👀", "🤷", "🙈", "🐶", "🐱"]

    padding: Kirigami.Units.smallSpacing
    width: Kirigami.Units.gridUnit * 2 * 8 + padding * 2
    height: Kirigami.Units.gridUnit * 2 * 5 + padding * 2

    contentItem: GridView {
        clip: true
        cellWidth: Kirigami.Units.gridUnit * 2
        cellHeight: Kirigami.Units.gridUnit * 2
        model: root.emojis
        delegate: QQC2.ToolButton {
            required property string modelData
            width: GridView.view.cellWidth
            height: GridView.view.cellHeight
            text: modelData
            font.pointSize: Kirigami.Theme.defaultFont.pointSize * 1.4
            onClicked: {
                root.picked(modelData);
                root.close();
            }
        }
    }
}
