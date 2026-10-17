// SPDX-License-Identifier: SSPL-1.0
import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import org.imessage.kmessages

Kirigami.ApplicationWindow {
    id: root

    title: App.currentChatTitle.length > 0 ? i18nc("window title", "%1 — Messages", App.currentChatTitle) : i18n("Messages")
    width: Kirigami.Units.gridUnit * 64
    height: Kirigami.Units.gridUnit * 42
    minimumWidth: Kirigami.Units.gridUnit * 18
    minimumHeight: Kirigami.Units.gridUnit * 20

    pageStack.defaultColumnWidth: Kirigami.Units.gridUnit * 21
    pageStack.globalToolBar.style: Kirigami.ApplicationHeaderStyle.ToolBar
    pageStack.globalToolBar.showNavigationButtons: Kirigami.ApplicationHeaderStyle.ShowBackButton
    pageStack.initialPage: ChatListPage {
        id: chatListPage
    }

    // The chat counts as "visible" only while its page is on screen.
    Binding {
        target: App
        property: "conversationVisible"
        value: root.pageStack.wideMode || (root.pageStack.currentItem !== null && root.pageStack.currentItem.isConversationPage === true)
    }

    Component {
        id: conversationComponent
        ConversationPage {}
    }
    Component {
        id: newMessageComponent
        NewMessagePage {}
    }
    Component {
        id: detailsComponent
        ChatDetailsPage {}
    }
    Component {
        id: settingsComponent
        SettingsPage {}
    }

    LoginDialog {
        id: loginDialog
    }

    // The ConversationPage that owns App.currentChatId. Removing exactly this
    // page from the row (Back, popToList) closes the chat; a page replaced by
    // openChat()/showNewMessage() is detached first so its (deferred)
    // removal cannot clear the chat that replaced it.
    property Item activeConversationPage: null

    function closeLayers() {
        while (pageStack.layers.depth > 1) {
            pageStack.layers.pop();
        }
    }

    function popToList() {
        closeLayers();
        pageStack.pop(chatListPage);
    }

    function conversationIndex(): int {
        for (let i = 0; i < pageStack.depth; ++i) {
            if (activeConversationPage !== null && pageStack.get(i) === activeConversationPage) {
                return i;
            }
        }
        return -1;
    }

    function openChat(chatId: string) {
        const index = conversationIndex();
        if (index >= 0 && App.currentChatId === chatId) {
            // Already open: just bring it to the front (and drop the details column).
            closeLayers();
            if (pageStack.depth > index + 1) {
                pageStack.pop(activeConversationPage);
            }
            pageStack.currentIndex = index;
            return;
        }
        activeConversationPage = null;
        popToList();
        App.currentChatId = chatId;
        activeConversationPage = pageStack.push(conversationComponent, { chatId: chatId });
    }

    function showNewMessage() {
        activeConversationPage = null;
        popToList();
        App.currentChatId = "";
        pageStack.push(newMessageComponent);
    }

    Connections {
        target: root.pageStack
        function onPageRemoved(page: Item) {
            if (page !== null && page === root.activeConversationPage) {
                root.activeConversationPage = null;
                App.currentChatId = "";
            }
        }
    }

    function toggleDetails() {
        if (pageStack.depth > 2) {
            pageStack.pop(pageStack.get(1));
        } else if (App.currentChatId.length > 0) {
            pageStack.push(detailsComponent, { chatId: App.currentChatId });
        }
    }

    function showSettings() {
        if (pageStack.layers.depth <= 1) {
            pageStack.layers.push(settingsComponent);
        }
    }

    function showLogin(step: int) {
        loginDialog.openStep(step);
    }

    function closeOverlays() {
        loginDialog.close();
        closeLayers();
    }

    onClosing: close => {
        if (App.trayAvailable && App.settings.closeToTray) {
            close.accepted = false;
            root.hide();
        }
    }

    Connections {
        target: App
        function onOpenChatRequested(chatId: string) {
            root.openChat(chatId);
        }
        function onNewMessageRequested() {
            root.showNewMessage();
        }
        function onShowPassiveNotification(message: string) {
            root.showPassiveNotification(message);
        }
        function onLoginRequested(step: int) {
            root.showLogin(step);
        }
    }

    Connections {
        target: App.daemon
        function onTwoFactorRequired() {
            if (!loginDialog.opened || loginDialog.step !== 1) {
                loginDialog.openStep(1);
            }
        }
        function onBackgroundError(message: string) {
            root.showPassiveNotification(message, "long");
        }
    }

    Shortcut {
        sequences: [StandardKey.New]
        onActivated: root.showNewMessage()
    }
    Shortcut {
        sequences: [StandardKey.Find]
        onActivated: chatListPage.focusSearch()
    }
    Shortcut {
        sequences: [StandardKey.Quit]
        onActivated: App.quit()
    }
}
