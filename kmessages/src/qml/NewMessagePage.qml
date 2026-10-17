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

    function addRecipient(text: string) {
        if (App.recipients.add(text)) {
            toField.clear();
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
                    onTextChanged: {
                        if (text.endsWith(",") || text.endsWith(";")) {
                            page.addRecipient(text.slice(0, -1));
                        } else {
                            App.recipientSuggestions.filterText = text;
                            App.contactSuggestions.filterText = text;
                        }
                    }
                    onAccepted: page.addRecipient(text)
                    Keys.onPressed: event => {
                        if (event.key === Qt.Key_Backspace && text.length === 0) {
                            App.recipients.removeLast();
                            event.accepted = true;
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
            Layout.fillWidth: true
            Layout.fillHeight: true
            visible: toField.text.length > 0 && (count > 0 || App.contactSuggestions.count > 0)
            clip: true
            model: App.recipientSuggestions

            // Address-book matches first (one row per email / phone number),
            // then existing conversations.
            header: ColumnLayout {
                width: ListView.view.width
                spacing: 0

                Kirigami.ListSectionHeader {
                    Layout.fillWidth: true
                    visible: App.contactSuggestions.count > 0
                    text: i18n("Contacts")
                }
                Repeater {
                    model: App.contactSuggestions
                    delegate: QQC2.ItemDelegate {
                        id: contact
                        required property string name
                        required property string uri
                        required property string address
                        required property string avatarSource
                        required property color avatarColor
                        required property string reachability
                        Layout.fillWidth: true
                        Accessible.name: i18nc("contact suggestion: name, address", "%1, %2", name, address)
                        contentItem: RowLayout {
                            spacing: Kirigami.Units.largeSpacing
                            Components.Avatar {
                                Layout.preferredWidth: Kirigami.Units.gridUnit * 2
                                Layout.preferredHeight: Kirigami.Units.gridUnit * 2
                                name: contact.name
                                color: contact.avatarColor
                                source: contact.avatarSource
                            }
                            ColumnLayout {
                                spacing: 0
                                Layout.fillWidth: true
                                QQC2.Label {
                                    Layout.fillWidth: true
                                    text: contact.name
                                    elide: Text.ElideRight
                                }
                                QQC2.Label {
                                    Layout.fillWidth: true
                                    text: contact.address
                                    font: Kirigami.Theme.smallFont
                                    // iMessage addresses in blue, like Messages.app
                                    color: contact.reachability === "reachable" ? "#1982fc" : Kirigami.Theme.textColor
                                    opacity: contact.reachability === "reachable" ? 1 : 0.7
                                    elide: Text.ElideRight
                                }
                            }
                            QQC2.Label {
                                visible: contact.reachability === "reachable" || contact.reachability === "unreachable"
                                text: contact.reachability === "reachable" ? i18n("iMessage") : i18n("Not on iMessage")
                                font: Kirigami.Theme.smallFont
                                opacity: 0.7
                            }
                            QQC2.BusyIndicator {
                                visible: contact.reachability === "checking"
                                running: visible
                                implicitWidth: Kirigami.Units.iconSizes.small
                                implicitHeight: Kirigami.Units.iconSizes.small
                            }
                        }
                        onClicked: page.addRecipient(uri)
                    }
                }
                Kirigami.ListSectionHeader {
                    Layout.fillWidth: true
                    visible: App.contactSuggestions.count > 0 && suggestions.count > 0
                    text: i18n("Conversations")
                }
            }
            delegate: QQC2.ItemDelegate {
                id: suggestion
                required property string chatId
                required property string name
                required property var participantUris
                required property var participantAddresses
                required property color avatarColor
                required property string avatarSource
                required property bool isGroup
                width: ListView.view.width
                contentItem: RowLayout {
                    spacing: Kirigami.Units.largeSpacing
                    Components.Avatar {
                        Layout.preferredWidth: Kirigami.Units.gridUnit * 2
                        Layout.preferredHeight: Kirigami.Units.gridUnit * 2
                        name: suggestion.name
                        color: suggestion.avatarColor
                        source: suggestion.avatarSource
                    }
                    ColumnLayout {
                        spacing: 0
                        Layout.fillWidth: true
                        QQC2.Label {
                            Layout.fillWidth: true
                            text: suggestion.name
                            elide: Text.ElideRight
                        }
                        QQC2.Label {
                            Layout.fillWidth: true
                            text: suggestion.participantAddresses.join(", ")
                            font: Kirigami.Theme.smallFont
                            opacity: 0.7
                            elide: Text.ElideRight
                        }
                    }
                }
                onClicked: {
                    if (isGroup) {
                        applicationWindow().openChat(chatId);
                    } else {
                        page.addRecipient(participantUris[0]);
                    }
                }
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
                text: App.recipients.count === 0 ? i18n("Who do you want to message?") : (App.recipients.canSend ? i18n("Ready to send") : (App.recipients.checking ? i18n("Checking iMessage availability…") : ""))
                explanation: App.recipients.count === 0 ? i18n("Type a name from your contacts, an email address or a phone number and press Enter. Add several people to start a group.") : ""
            }
        }

        Composer {
            id: composer
            Layout.fillWidth: true
            enabled: App.recipients.canSend && !page.sending && (App.daemon.connectionState === "connected" || App.daemon.connectionState === "registering")
            allowAttachments: false
            onSendRequested: (text, replyTo) => {
                page.error = "";
                page.sending = true;
                page.pendingText = text;
                App.daemon.sendText("", App.recipients.handles, text, "");
            }
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
