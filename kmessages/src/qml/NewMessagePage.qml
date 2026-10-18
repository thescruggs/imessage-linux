// SPDX-License-Identifier: SSPL-1.0
import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.components as Components
import org.imessage.kmessages

Kirigami.Page {
    id: page

    property bool sending: false
    property string error
    property string pendingText
    readonly property bool isConversationPage: false

    title: i18n("New Message")
    padding: 0
    Kirigami.Theme.colorSet: Kirigami.Theme.View
    Kirigami.Theme.inherit: false
    background: Rectangle {
        color: Kirigami.Theme.backgroundColor
    }

    Component.onCompleted: {
        App.recipients.clear();
        App.recipientSuggestions.filterText = "";
        App.contactSuggestions.filterText = "";
        toField.forceActiveFocus();
    }

    /// The highlighted suggestion row (contacts first, then conversations), -1 = none.
    property alias highlightedIndex: suggestions.currentIndex

    function addRecipient(text: string) {
        if (App.recipients.add(text)) {
            toField.clear();
        }
    }

    // A contact row adds its address; a conversation opens a group, or adds
    // the other person of a 1:1 chat.
    function activate(row: int) {
        const r = App.recipientPicker.rowData(row);
        if (r.kind === "contact") {
            page.addRecipient(r.uri);
        } else if (r.kind === "conversation") {
            if (r.isGroup) {
                applicationWindow().openChat(r.chatId);
            } else if (r.participantUris.length > 0) {
                page.addRecipient(r.participantUris[0]);
            }
        }
    }

    // Down / Up: wraps around, keeps the row in view.
    function moveHighlight(step: int) {
        const n = suggestions.count;
        if (!suggestions.visible || n === 0) {
            return;
        }
        const from = suggestions.currentIndex < 0 ? (step > 0 ? -1 : n) : suggestions.currentIndex;
        suggestions.currentIndex = ((from + step) % n + n) % n;
        suggestions.positionViewAtIndex(suggestions.currentIndex, ListView.Contain);
    }

    function resetHighlight() {
        suggestions.currentIndex = suggestions.count > 0 ? 0 : -1;
        if (suggestions.currentIndex === 0) {
            suggestions.positionViewAtBeginning();
        }
    }

    function stateColor(state: string): color {
        switch (state) {
        case "reachable":
            return "#1982fc";
        case "checking":
            return Kirigami.ColorUtils.tintWithAlpha(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, 0.3);
        default:
            return Kirigami.Theme.negativeTextColor;
        }
    }

    ColumnLayout {
        anchors.fill: parent
        spacing: 0

        QQC2.Control {
            Layout.fillWidth: true
            padding: Kirigami.Units.smallSpacing * 1.5
            leftPadding: Kirigami.Units.largeSpacing
            rightPadding: Kirigami.Units.largeSpacing

            contentItem: Flow {
                spacing: Kirigami.Units.smallSpacing

                QQC2.Label {
                    height: toField.height
                    verticalAlignment: Text.AlignVCenter
                    text: i18nc("recipient field label", "To:")
                    opacity: 0.7
                }

                Repeater {
                    model: App.recipients
                    delegate: Rectangle {
                        id: chip
                        required property int index
                        required property string label
                        required property string stateName
                        readonly property color foreground: stateName === "checking" ? Kirigami.Theme.textColor : "white"
                        height: toField.height
                        width: chipRow.implicitWidth + Kirigami.Units.largeSpacing
                        radius: height / 2
                        color: page.stateColor(stateName)

                        RowLayout {
                            id: chipRow
                            anchors.centerIn: parent
                            spacing: 0
                            QQC2.Label {
                                Layout.leftMargin: Kirigami.Units.smallSpacing
                                text: chip.label
                                // "checking" sits on a light tint of the background: use the text colour there.
                                color: chip.foreground
                            }
                            QQC2.BusyIndicator {
                                visible: chip.stateName === "checking"
                                running: visible
                                implicitWidth: Kirigami.Units.iconSizes.small
                                implicitHeight: Kirigami.Units.iconSizes.small
                            }
                            QQC2.ToolButton {
                                icon.name: "window-close-symbolic"
                                icon.color: chip.foreground
                                icon.width: Kirigami.Units.iconSizes.small
                                icon.height: Kirigami.Units.iconSizes.small
                                text: i18n("Remove %1", chip.label)
                                display: QQC2.AbstractButton.IconOnly
                                implicitHeight: chip.height
                                implicitWidth: chip.height
                                onClicked: App.recipients.remove(chip.index)
                            }
                        }
                    }
                }

                QQC2.TextField {
                    id: toField
                    objectName: "recipientField"
                    width: Math.max(Kirigami.Units.gridUnit * 12, implicitWidth)
                    placeholderText: i18n("Name, email or phone number")
                    inputMethodHints: Qt.ImhEmailCharactersOnly | Qt.ImhNoAutoUppercase
                    background: null
                    Accessible.name: i18n("To")
                    Accessible.description: i18n("Up and Down choose a suggestion, Enter adds it")
                    onTextChanged: {
                        if (text.endsWith(",") || text.endsWith(";")) {
                            page.addRecipient(text.slice(0, -1));
                        } else {
                            App.recipientSuggestions.filterText = text;
                            App.contactSuggestions.filterText = text;
                            page.resetHighlight();
                        }
                    }
                    // Enter: the highlighted suggestion, else the typed text.
                    onAccepted: {
                        if (suggestions.visible && suggestions.currentIndex >= 0) {
                            page.activate(suggestions.currentIndex);
                        } else {
                            page.addRecipient(text);
                        }
                    }
                    Keys.onPressed: event => {
                        if (event.key === Qt.Key_Backspace && text.length === 0) {
                            App.recipients.removeLast();
                            event.accepted = true;
                        } else if (event.key === Qt.Key_Down || event.key === Qt.Key_Up) {
                            event.accepted = suggestions.visible;
                            page.moveHighlight(event.key === Qt.Key_Down ? 1 : -1);
                        } else if (event.key === Qt.Key_Escape) {
                            event.accepted = true;
                            if (text.length > 0) {
                                clear();
                            } else {
                                applicationWindow().popToList();
                            }
                        }
                    }
                }
            }
        }

        Kirigami.Separator {
            Layout.fillWidth: true
        }

        Kirigami.InlineMessage {
            Layout.fillWidth: true
            Layout.margins: Kirigami.Units.smallSpacing
            visible: text.length > 0
            type: Kirigami.MessageType.Error
            text: page.error.length > 0 ? page.error : App.recipients.problem
        }

        ListView {
            id: suggestions
            objectName: "recipientSuggestionList"
            Layout.fillWidth: true
            Layout.fillHeight: true
            visible: toField.text.length > 0 && count > 0
            clip: true
            // Address-book matches first (one row per email / phone number),
            // then existing conversations; one list, so one highlight.
            model: App.recipientPicker
            currentIndex: -1
            // The highlight follows the keyboard (and the mouse): the To:
            // field keeps the focus, the list never takes it.
            keyNavigationEnabled: false
            highlightMoveDuration: 0
            section.property: "section"
            section.delegate: Kirigami.ListSectionHeader {
                required property string section
                width: ListView.view.width
                text: section
            }

            delegate: QQC2.ItemDelegate {
                id: row
                // Not `required property int index`: the desktop style's
                // ItemDelegate would then bind its bottomPadding to it (binding loop).
                required property var model
                readonly property int rowIndex: model.index
                required property string kind
                required property string name
                required property string uri
                required property string address
                required property string avatarSource
                required property color avatarColor
                required property string reachability
                readonly property bool isContact: kind === "contact"
                readonly property bool smsCapable: App.daemon.smsActive && uri.startsWith("tel:")
                width: ListView.view.width
                highlighted: ListView.isCurrentItem
                Accessible.name: i18nc("recipient suggestion: name, address", "%1, %2", name, address)
                Accessible.role: Accessible.ListItem
                Accessible.selected: highlighted
                onHoveredChanged: {
                    if (hovered) {
                        suggestions.currentIndex = row.rowIndex;
                    }
                }
                contentItem: RowLayout {
                    spacing: Kirigami.Units.largeSpacing
                    Components.Avatar {
                        Layout.preferredWidth: Kirigami.Units.gridUnit * 2
                        Layout.preferredHeight: Kirigami.Units.gridUnit * 2
                        name: row.name
                        color: row.avatarColor
                        source: row.avatarSource
                    }
                    ColumnLayout {
                        spacing: 0
                        Layout.fillWidth: true
                        QQC2.Label {
                            Layout.fillWidth: true
                            text: row.name
                            color: row.highlighted ? Kirigami.Theme.highlightedTextColor : Kirigami.Theme.textColor
                            elide: Text.ElideRight
                        }
                        QQC2.Label {
                            Layout.fillWidth: true
                            text: row.address
                            font: Kirigami.Theme.smallFont
                            // iMessage addresses in blue, like Messages.app
                            readonly property bool blue: row.reachability === "reachable" && !row.highlighted
                            color: blue ? "#1982fc" : (row.highlighted ? Kirigami.Theme.highlightedTextColor : Kirigami.Theme.textColor)
                            opacity: blue ? 1 : 0.7
                            elide: Text.ElideRight
                        }
                    }
                    QQC2.Label {
                        visible: row.isContact && (row.reachability === "reachable" || row.reachability === "unreachable")
                        text: row.reachability === "reachable" ? i18n("iMessage") : (row.smsCapable ? i18n("Text Message") : i18n("Not on iMessage"))
                        color: row.highlighted ? Kirigami.Theme.highlightedTextColor : (row.reachability !== "reachable" && row.smsCapable ? "#34c759" : Kirigami.Theme.textColor)
                        font: Kirigami.Theme.smallFont
                        opacity: 0.7
                    }
                    QQC2.BusyIndicator {
                        visible: row.isContact && row.reachability === "checking"
                        running: visible
                        implicitWidth: Kirigami.Units.iconSizes.small
                        implicitHeight: Kirigami.Units.iconSizes.small
                    }
                }
                onClicked: page.activate(row.rowIndex)
            }
        }

        Item {
            Layout.fillWidth: true
            Layout.fillHeight: true
            visible: !suggestions.visible

            Kirigami.PlaceholderMessage {
                anchors.centerIn: parent
                width: parent.width - Kirigami.Units.gridUnit * 4
                visible: text.length > 0
                icon.name: "document-edit"
                text: App.recipients.count === 0 ? i18n("Who do you want to message?") : (App.recipients.canSend ? (App.recipients.smsMode ? i18n("Ready to send as a text message through your iPhone") : i18n("Ready to send")) : (App.recipients.checking ? i18n("Checking iMessage availability…") : ""))
                explanation: App.recipients.count === 0 ? i18n("Type a name from your contacts, an email address or a phone number and press Enter. Add several people to start a group.") : ""
            }
        }

        Composer {
            id: composer
            Layout.fillWidth: true
            enabled: App.recipients.canSend && !page.sending && (App.daemon.connectionState === "connected" || App.daemon.connectionState === "registering")
            allowAttachments: false
            sms: App.recipients.smsMode
            onSendRequested: (text, replyTo) => {
                page.error = "";
                page.sending = true;
                page.pendingText = text;
                App.daemon.sendText("", App.recipients.handles, text, "");
            }
        }
    }

    // Rows coming and going later (lookups, chat updates) keep a valid highlight.
    Connections {
        target: App.recipientPicker
        function onCountChanged() {
            Qt.callLater(page.keepHighlight);
        }
    }

    function keepHighlight() {
        if (suggestions.currentIndex < 0 || suggestions.currentIndex >= suggestions.count) {
            page.resetHighlight();
        }
    }

    Connections {
        target: App.daemon
        function onSendFinished(ok: bool, chatId: string, messageId: string, errorMessage: string) {
            if (!page.sending) {
                return;
            }
            page.sending = false;
            if (ok) {
                page.pendingText = "";
                App.chatList.refresh();
                applicationWindow().openChat(chatId);
            } else {
                page.error = errorMessage;
                composer.restore(page.pendingText, "", "", "", "");
                page.pendingText = "";
            }
        }
    }
}
