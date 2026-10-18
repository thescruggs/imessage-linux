// SPDX-License-Identifier: SSPL-1.0
import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.imessage.kmessages

// A small fixed emoji grid with a search field: while searching, the grid
// shows the matches from the bundled keyword table (the model behind the
// composer's suggestions). Enter picks the first match.
QQC2.Popup {
    id: root

    signal picked(string emoji)

    readonly property var emojis: ["😀", "😂", "🥹", "😊", "😍", "😘", "😎", "🤔", "😅", "😭", "😡", "😴", "🥳", "🤯", "😬", "🙄", "👍", "👎", "👏", "🙏", "💪", "👋", "🤝", "✌️", "❤️", "🧡", "💛", "💚", "💙", "💜", "🔥", "✨", "🎉", "🎂", "🎁", "☕", "🍕", "🍻", "⚽", "🚗", "✈️", "🏖️", "🌙", "☀️", "🌧️", "❄️", "✅", "❌", "‼️", "❓", "💯", "👀", "🤷", "🙈", "🐶", "🐱"]
    readonly property bool searching: search.text.trim().length > 0

    function pick(emoji: string) {
        if (emoji.length > 0) {
            root.picked(emoji);
            root.close();
        }
    }

    padding: Kirigami.Units.smallSpacing
    width: Kirigami.Units.gridUnit * 2 * 8 + padding * 2
    height: Kirigami.Units.gridUnit * 2 * 5 + search.implicitHeight + Kirigami.Units.smallSpacing + padding * 2
    focus: true

    onOpened: search.forceActiveFocus()
    onClosed: search.clear()

    EmojiSuggestionModel {
        id: matches
        settings: App.settings
        limit: 64
        minimumLength: 1
    }

    contentItem: ColumnLayout {
        spacing: Kirigami.Units.smallSpacing

        Kirigami.SearchField {
            id: search
            objectName: "emojiSearchField"
            Layout.fillWidth: true
            placeholderText: i18n("Search emoji…")
            onTextChanged: matches.setWord(text.trim().replace(/ +/g, "_"))
            onAccepted: root.pick(root.searching ? matches.emojiAt(0) : "")
            Keys.onEscapePressed: event => {
                if (text.length > 0) {
                    clear();
                    event.accepted = true;
                } else {
                    event.accepted = false;
                }
            }
        }

        GridView {
            id: grid
            objectName: "emojiGrid"
            Layout.fillWidth: true
            Layout.fillHeight: true
            clip: true
            cellWidth: Kirigami.Units.gridUnit * 2
            cellHeight: Kirigami.Units.gridUnit * 2
            model: root.searching ? matches : root.emojis
            delegate: QQC2.ToolButton {
                required property var model
                readonly property string glyph: root.searching ? (model.emoji ?? "") : (model.modelData ?? "")
                width: GridView.view.cellWidth
                height: GridView.view.cellHeight
                text: glyph
                font.pointSize: Kirigami.Theme.defaultFont.pointSize * 1.4
                Accessible.name: root.searching ? (model.name ?? glyph) : glyph
                onClicked: root.pick(glyph)
            }

            QQC2.Label {
                anchors.centerIn: parent
                visible: root.searching && matches.count === 0
                text: i18n("No emoji found")
                opacity: 0.7
            }
        }
    }
}
