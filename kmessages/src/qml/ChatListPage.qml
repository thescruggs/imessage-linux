// SPDX-License-Identifier: SSPL-1.0
import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.components as Components
import org.imessage.kmessages

Kirigami.ScrollablePage {
    id: page

    title: i18n("Messages")
    Kirigami.ColumnView.fillWidth: false
    Kirigami.ColumnView.minimumWidth: Kirigami.Units.gridUnit * 15
    Kirigami.ColumnView.preferredWidth: Kirigami.Units.gridUnit * 21
    Kirigami.ColumnView.maximumWidth: Kirigami.Units.gridUnit * 30
    leftPadding: 0
    rightPadding: 0
    topPadding: 0
    bottomPadding: 0

    readonly property string state_: App.daemon.connectionState
    readonly property bool searching: searchField.text.length > 0

    function focusSearch() {
        searchField.forceActiveFocus();
    }

    function openChatMenu(chatId: string, pinned: bool, anchor: Item) {
        chatMenu.chatId = chatId;
        chatMenu.pinned = pinned;
        chatMenu.popup(anchor);
    }

    actions: [
        Kirigami.Action {
            icon.name: "document-edit"
            text: i18n("New Message")
            displayHint: Kirigami.DisplayHint.IconOnly
            onTriggered: applicationWindow().showNewMessage()
        },
        Kirigami.Action {
            id: syncAction
            objectName: "syncFromMacAction"
            icon.name: "view-refresh"
            text: i18n("Sync from Mac")
            // No IconOnly hint: the toolbar would use its own icon button
            // instead of displayComponent (which is icon-only anyway).
            enabled: !App.macSync.running
            tooltip: {
                const what = App.macSync.host.length > 0
                    ? i18n("Copy contacts and pinned conversations from %1", App.macSync.host)
                    : i18n("Copy contacts and pinned conversations from your Mac");
                const when = App.macSync.running
                    ? i18n("Syncing…")
                    : (App.macSync.lastSyncText.length > 0 ? i18n("Last synced %1", App.macSync.lastSyncText) : i18n("Not synced yet"));
                return what + "\n" + when;
            }
            // A tool button that turns into a busy indicator while syncing.
            displayComponent: QQC2.ToolButton {
                objectName: "syncFromMacButton"
                display: QQC2.AbstractButton.IconOnly
                text: syncAction.text
                icon.name: App.macSync.running ? "" : syncAction.icon.name
                // Same size with or without the icon.
                implicitWidth: Kirigami.Units.iconSizes.smallMedium + leftPadding + rightPadding
                implicitHeight: Kirigami.Units.iconSizes.smallMedium + topPadding + bottomPadding
                enabled: syncAction.enabled
                onClicked: syncAction.trigger()
                QQC2.ToolTip.text: syncAction.tooltip
                QQC2.ToolTip.visible: hovered
                QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                Accessible.description: syncAction.tooltip

                QQC2.BusyIndicator {
                    objectName: "macSyncBusy"
                    anchors.centerIn: parent
                    width: Kirigami.Units.iconSizes.smallMedium
                    height: width
                    padding: 0
                    running: App.macSync.running
                    visible: running
                }
            }
            onTriggered: App.macSync.start()
        },
        Kirigami.Action {
            icon.name: "settings-configure"
            text: i18n("Settings")
            displayHint: Kirigami.DisplayHint.IconOnly
            onTriggered: applicationWindow().showSettings()
        }
    ]

    header: QQC2.Control {
        padding: Kirigami.Units.smallSpacing
        contentItem: Kirigami.SearchField {
            id: searchField
            placeholderText: i18n("Search")
            onTextChanged: {
                App.chats.filterText = text;
                App.search.query = text;
            }
        }
    }

    footer: StatusFooter {}

    ListView {
        id: list

        model: App.chats
        currentIndex: -1
        reuseItems: true

        delegate: ChatListDelegate {
            onClicked: applicationWindow().openChat(chatId)
            onMenuRequested: page.openChatMenu(chatId, pinned, this)
        }

        // "Pinned" above the pinned chats, "Conversations" above the rest;
        // no headers at all while nothing is pinned.
        section.property: "section"
        section.delegate: Kirigami.ListSectionHeader {
            required property string section
            width: list.width
            text: section
        }

        footer: ColumnLayout {
            width: ListView.view.width
            spacing: 0
            visible: page.searching && App.search.count > 0
            // An invisible footer must not keep its height (extra scroll space).
            height: visible ? implicitHeight : 0

            Kirigami.ListSectionHeader {
                Layout.fillWidth: true
                text: i18n("Messages")
            }
            Repeater {
                model: App.search
                delegate: QQC2.ItemDelegate {
                    id: hit
                    required property string chatId
                    required property string chatName
                    required property string snippet
                    required property string time
                    required property string senderName
                    required property string avatarSource
                    Layout.fillWidth: true
                    contentItem: RowLayout {
                        spacing: Kirigami.Units.largeSpacing
                        Components.Avatar {
                            Layout.preferredWidth: Kirigami.Units.gridUnit * 2
                            Layout.preferredHeight: Kirigami.Units.gridUnit * 2
                            name: hit.chatName
                            source: hit.avatarSource
                            initialsMode: /^[+(0-9]/.test(hit.chatName) ? Components.Avatar.InitialsMode.UseIcon : Components.Avatar.InitialsMode.UseInitials
                            iconSource: "user"
                        }
                        ColumnLayout {
                            Layout.fillWidth: true
                            spacing: 0
                            RowLayout {
                                QQC2.Label {
                                    Layout.fillWidth: true
                                    text: hit.chatName
                                    font.weight: Font.DemiBold
                                    elide: Text.ElideRight
                                }
                                QQC2.Label {
                                    text: hit.time
                                    opacity: 0.7
                                    font: Kirigami.Theme.smallFont
                                }
                            }
                            QQC2.Label {
                                Layout.fillWidth: true
                                text: i18nc("search hit: sender: text", "%1: %2", hit.senderName, hit.snippet)
                                elide: Text.ElideRight
                                opacity: 0.7
                            }
                        }
                    }
                    onClicked: applicationWindow().openChat(chatId)
                }
            }
        }

        Kirigami.PlaceholderMessage {
            anchors.centerIn: parent
            width: parent.width - Kirigami.Units.gridUnit * 2
            visible: list.count === 0 && !(page.searching && App.search.count > 0)
            icon.name: page.state_ === "needs_login" ? "im-user" : (page.state_ === "unavailable" ? "network-disconnect" : "dialog-messages")
            text: {
                if (page.state_ === "unavailable")
                    return i18n("imessaged is not running");
                if (page.state_ === "needs_login")
                    return i18n("Sign in to iMessage");
                if (page.searching)
                    return i18n("No matches");
                if (App.chatList.loading)
                    return i18n("Loading…");
                return i18n("No conversations yet");
            }
            explanation: {
                if (page.state_ === "unavailable")
                    return App.daemon.lastError.length > 0 ? App.daemon.lastError : i18n("Start it with “systemctl --user start imessaged”.");
                if (page.state_ === "needs_login")
                    return i18n("Use your Apple ID to send and receive messages on this computer.");
                return "";
            }
            helpfulAction: Kirigami.Action {
                icon.name: page.state_ === "unavailable" ? "view-refresh" : (page.state_ === "needs_login" ? "go-next" : "document-edit")
                text: page.state_ === "unavailable" ? i18n("Try Again") : (page.state_ === "needs_login" ? i18n("Sign In…") : i18n("New Message"))
                onTriggered: {
                    if (page.state_ === "unavailable")
                        App.daemon.ensureStarted();
                    else if (page.state_ === "needs_login")
                        applicationWindow().showLogin(0);
                    else
                        applicationWindow().showNewMessage();
                }
            }
        }
    }

    QQC2.Menu {
        id: chatMenu
        objectName: "chatContextMenu"
        property string chatId
        property bool pinned

        QQC2.MenuItem {
            objectName: "pinChatAction"
            text: chatMenu.pinned ? i18n("Unpin") : i18n("Pin")
            icon.name: chatMenu.pinned ? "window-unpin" : "pin"
            onTriggered: App.daemon.setChatPinned(chatMenu.chatId, !chatMenu.pinned, -1)
        }
    }
}
