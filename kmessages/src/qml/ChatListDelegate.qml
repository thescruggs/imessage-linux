// SPDX-License-Identifier: SSPL-1.0
import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.components as Components
import org.imessage.kmessages

QQC2.ItemDelegate {
    id: delegate
    objectName: "chatListDelegate"

    required property int index
    required property string chatId
    required property string name
    required property string snippet
    required property string time
    required property int unreadCount
    required property bool hasUnread
    required property color avatarColor
    required property string avatarSource
    required property bool isGroup
    required property bool typing
    required property bool pinned

    /// Right-click, or press and hold on touch: the row's context menu.
    signal menuRequested

    width: ListView.view ? ListView.view.width : implicitWidth
    highlighted: chatId === App.currentChatId && applicationWindow().pageStack.wideMode
    leftPadding: Kirigami.Units.smallSpacing
    rightPadding: Kirigami.Units.largeSpacing
    topPadding: Kirigami.Units.smallSpacing * 1.5
    bottomPadding: Kirigami.Units.smallSpacing * 1.5

    onPressAndHold: menuRequested()
    TapHandler {
        acceptedButtons: Qt.RightButton
        onTapped: delegate.menuRequested()
    }

    Accessible.name: hasUnread ? i18n("%1, %2 unread: %3", name, unreadCount, snippet) : i18n("%1: %2", name, snippet)

    contentItem: RowLayout {
        spacing: Kirigami.Units.smallSpacing

        Rectangle {
            id: unreadDot
            Layout.preferredWidth: Kirigami.Units.smallSpacing * 2.5
            Layout.preferredHeight: Layout.preferredWidth
            Layout.alignment: Qt.AlignVCenter
            radius: width / 2
            color: "#1982fc"
            opacity: delegate.hasUnread ? 1 : 0
        }

        Components.Avatar {
            Layout.preferredWidth: Kirigami.Units.gridUnit * 2.5
            Layout.preferredHeight: Kirigami.Units.gridUnit * 2.5
            name: delegate.name
            color: delegate.avatarColor
            source: delegate.avatarSource // contact photo; initials/icon otherwise
            initialsMode: delegate.isGroup || /^[+(0-9]/.test(delegate.name) ? Components.Avatar.InitialsMode.UseIcon : Components.Avatar.InitialsMode.UseInitials
            iconSource: delegate.isGroup ? "system-users" : "user"
        }

        ColumnLayout {
            Layout.fillWidth: true
            spacing: 0

            RowLayout {
                Layout.fillWidth: true
                QQC2.Label {
                    Layout.fillWidth: true
                    text: delegate.name
                    elide: Text.ElideRight
                    font.weight: delegate.hasUnread ? Font.Bold : Font.DemiBold
                    color: delegate.highlighted ? Kirigami.Theme.highlightedTextColor : Kirigami.Theme.textColor
                }
                Kirigami.Icon {
                    objectName: "pinnedIcon"
                    visible: delegate.pinned
                    source: "pin"
                    Layout.preferredWidth: Kirigami.Units.iconSizes.small
                    Layout.preferredHeight: Kirigami.Units.iconSizes.small
                    opacity: 0.6
                    color: delegate.highlighted ? Kirigami.Theme.highlightedTextColor : Kirigami.Theme.textColor
                    isMask: true
                    Accessible.name: i18n("Pinned")
                }
                QQC2.Label {
                    text: delegate.time
                    font: Kirigami.Theme.smallFont
                    opacity: 0.7
                    color: delegate.highlighted ? Kirigami.Theme.highlightedTextColor : Kirigami.Theme.textColor
                }
            }
            QQC2.Label {
                Layout.fillWidth: true
                visible: !delegate.typing
                text: delegate.snippet
                maximumLineCount: 2
                wrapMode: Text.Wrap
                elide: Text.ElideRight
                opacity: 0.7
                color: delegate.highlighted ? Kirigami.Theme.highlightedTextColor : Kirigami.Theme.textColor
            }
            TypingBubble {
                visible: delegate.typing
                small: true
            }
        }
    }
}
