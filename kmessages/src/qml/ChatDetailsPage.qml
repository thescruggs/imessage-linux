// SPDX-License-Identifier: SSPL-1.0
import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.components as Components
import org.imessage.kmessages

// Third column: who is in the conversation.
Kirigami.ScrollablePage {
    id: page

    property string chatId
    readonly property var chat: {
        App.chatList.revision; // re-evaluate on any change (ChatUpdated renames, participants, …)
        return App.chatList.get(chatId);
    }
    readonly property bool isConversationPage: false

    title: i18n("Details")
    Kirigami.ColumnView.fillWidth: false
    Kirigami.ColumnView.preferredWidth: Kirigami.Units.gridUnit * 18

    actions: [
        Kirigami.Action {
            icon.name: "window-close"
            text: i18n("Close")
            displayHint: Kirigami.DisplayHint.IconOnly
            onTriggered: applicationWindow().toggleDetails()
        }
    ]

    ListView {
        model: page.chat.participants ?? []
        header: ColumnLayout {
            width: ListView.view.width
            spacing: Kirigami.Units.smallSpacing
            Components.Avatar {
                Layout.alignment: Qt.AlignHCenter
                Layout.topMargin: Kirigami.Units.largeSpacing
                Layout.preferredWidth: Kirigami.Units.gridUnit * 4
                Layout.preferredHeight: Kirigami.Units.gridUnit * 4
                name: page.chat.name ?? ""
                color: page.chat.avatarColor ?? Kirigami.Theme.highlightColor
                source: page.chat.avatarSource ?? ""
                initialsMode: page.chat.isGroup ? Components.Avatar.InitialsMode.UseIcon : Components.Avatar.InitialsMode.UseInitials
                iconSource: "system-users"
            }
            Kirigami.Heading {
                Layout.alignment: Qt.AlignHCenter
                Layout.maximumWidth: parent.width - Kirigami.Units.gridUnit
                level: 3
                text: page.chat.name ?? ""
                elide: Text.ElideRight
            }
            Kirigami.ListSectionHeader {
                Layout.fillWidth: true
                text: i18np("%1 participant", "%1 participants", (page.chat.participants ?? []).length)
            }
        }
        delegate: QQC2.ItemDelegate {
            id: person
            required property int index
            required property string modelData
            readonly property string uri: page.chat.participantUris[index]
            readonly property string address: page.chat.participantAddresses[index] ?? ""
            // What they shared with us ("Share Name and Photo"), if anything.
            readonly property string sharedName: (page.chat.participantSharedNames ?? [])[index] ?? ""
            readonly property string sharedPhoto: (page.chat.participantSharedPhotos ?? [])[index] ?? ""
            readonly property string avatar: (page.chat.participantAvatars ?? [])[index] ?? ""
            width: ListView.view.width
            contentItem: RowLayout {
                spacing: Kirigami.Units.largeSpacing
                Components.Avatar {
                    Layout.preferredWidth: Kirigami.Units.gridUnit * 2
                    Layout.preferredHeight: Kirigami.Units.gridUnit * 2
                    name: person.modelData
                    source: person.avatar
                    initialsMode: /^[+(0-9]/.test(person.modelData) ? Components.Avatar.InitialsMode.UseIcon : Components.Avatar.InitialsMode.UseInitials
                    iconSource: "user"
                }
                ColumnLayout {
                    spacing: 0
                    Layout.fillWidth: true
                    QQC2.Label {
                        Layout.fillWidth: true
                        text: person.modelData
                        elide: Text.ElideRight
                    }
                    QQC2.Label {
                        Layout.fillWidth: true
                        // The name is already the address when there is no contact.
                        visible: person.modelData !== person.address
                        text: person.address
                        font: Kirigami.Theme.smallFont
                        opacity: 0.7
                        elide: Text.ElideRight
                    }
                    QQC2.Label {
                        objectName: "sharedNameLabel"
                        Layout.fillWidth: true
                        visible: person.sharedName !== ""
                        text: i18nc("the name a person shared with us via Share Name and Photo", "Shared name: %1", person.sharedName)
                        font: Kirigami.Theme.smallFont
                        opacity: 0.7
                        elide: Text.ElideRight
                    }
                }
                // Their shared photo, when the address book's photo is shown instead.
                Components.Avatar {
                    objectName: "sharedPhoto"
                    visible: person.sharedPhoto !== "" && person.avatar !== person.sharedPhoto
                    Layout.preferredWidth: Kirigami.Units.gridUnit * 1.5
                    Layout.preferredHeight: Kirigami.Units.gridUnit * 1.5
                    name: person.sharedName
                    source: person.sharedPhoto
                    QQC2.ToolTip.visible: sharedPhotoHover.hovered
                    QQC2.ToolTip.text: i18n("Shared photo")
                    HoverHandler {
                        id: sharedPhotoHover
                    }
                }
            }
            onClicked: {
                if (page.chat.isGroup && App.chatList.indexOf(uri) >= 0) {
                    applicationWindow().openChat(uri);
                }
            }
        }
    }
}
