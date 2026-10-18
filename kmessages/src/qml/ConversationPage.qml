// SPDX-License-Identifier: SSPL-1.0
import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.imessage.kmessages

Kirigami.Page {
    id: page

    property string chatId
    readonly property bool isConversationPage: true

    title: App.currentChatTitle
    padding: 0
    Kirigami.Theme.colorSet: Kirigami.Theme.View
    Kirigami.Theme.inherit: false

    actions: [
        Kirigami.Action {
            icon.name: "documentinfo"
            text: i18n("Details")
            displayHint: Kirigami.DisplayHint.IconOnly
            onTriggered: applicationWindow().toggleDetails()
        }
    ]

    // The chat's transcript background (Messages "Backgrounds"), if any,
    // dimmed in dark mode so bubbles and labels stay readable.
    background: Rectangle {
        color: Kirigami.Theme.backgroundColor

        Image {
            id: backgroundImage
            anchors.fill: parent
            source: App.currentChatBackground
            visible: source != "" && status === Image.Ready
            fillMode: Image.PreserveAspectCrop
            // Imported photos (HEIC/JPEG) carry an EXIF orientation.
            autoTransform: true
            asynchronous: true
            cache: false
            sourceSize.width: Math.max(width, 1) * Screen.devicePixelRatio
        }
        Rectangle {
            anchors.fill: parent
            visible: backgroundImage.visible
            color: "black"
            opacity: Kirigami.ColorUtils.brightnessForColor(Kirigami.Theme.backgroundColor) === Kirigami.ColorUtils.Dark ? 0.35 : 0.08
        }
    }

    // Closing the chat when this page goes away is Main.qml's job
    // (pageStack.onPageRemoved, compared by page identity): a destruction
    // handler here would run after a replacement page for the same chat was
    // pushed and blank it.

    function openMenu(row: int, anchor: Item) {
        messageMenu.message = App.messages.get(row);
        messageMenu.anchorItem = anchor;
        messageMenu.popup(anchor);
    }

    function openTapbacks(row: int, anchor: Item) {
        const m = App.messages.get(row);
        if (m.unsent) {
            return;
        }
        tapbackPicker.messageId = m.messageId;
        tapbackPicker.myReaction = m.myReaction;
        tapbackPicker.parent = anchor;
        tapbackPicker.x = m.isFromMe ? anchor.width - tapbackPicker.width : 0;
        tapbackPicker.y = -tapbackPicker.height - Kirigami.Units.smallSpacing;
        tapbackPicker.open();
    }

    ColumnLayout {
        anchors.fill: parent
        spacing: 0

        QQC2.ScrollView {
            Layout.fillWidth: true
            Layout.fillHeight: true

            ListView {
                id: transcript
                objectName: "transcript"

                model: App.messages
                verticalLayoutDirection: ListView.BottomToTop
                spacing: 0
                clip: true // bubble tails reach outside the delegates
                topMargin: Kirigami.Units.largeSpacing
                bottomMargin: Kirigami.Units.largeSpacing
                boundsBehavior: Flickable.StopAtBounds

                header: Item {
                    width: ListView.view.width
                    height: typing.visible ? typing.height + Kirigami.Units.smallSpacing * 2 : 0
                    TypingBubble {
                        id: typing
                        visible: App.messages.someoneTyping
                        anchors.left: parent.left
                        anchors.leftMargin: Kirigami.Units.largeSpacing + (App.messages.isGroup ? Kirigami.Units.gridUnit * 1.5 + Kirigami.Units.smallSpacing : 0)
                        anchors.verticalCenter: parent.verticalCenter
                    }
                }

                delegate: MessageDelegate {
                    onMenuRequested: anchor => page.openMenu(index, anchor)
                    onTapbackRequested: anchor => page.openTapbacks(index, anchor)
                }

                // Stay on the newest message while the user has not scrolled away
                // (images loading and the typing bubble change heights). The
                // user scrolls by flicking, with the wheel (the desktop style's
                // Kirigami.WheelHandler sets contentY directly, so there is no
                // movementEnded), by dragging the scroll bar or with the
                // keyboard: all of them change contentY, so the pin follows
                // contentY and ignores only our own snap. Reaching the end by
                // any means pins again (a model reset moves contentY before
                // the rows are laid out, so atYEnd lags behind it).
                property bool pinnedToNewest: true
                property bool snapping: false
                function snapToNewest() {
                    snapping = true;
                    positionViewAtBeginning();
                    if (!atYEnd) {
                        // positionViewAtBeginning() ignores bottomMargin; land on
                        // the Flickable's end so atYEnd (and the pin) hold.
                        contentY = originY + contentHeight + bottomMargin - height;
                    }
                    snapping = false;
                }
                onContentYChanged: {
                    if (!snapping) {
                        pinnedToNewest = atYEnd;
                    }
                }
                onAtYEndChanged: {
                    if (atYEnd) {
                        pinnedToNewest = true;
                    }
                }
                onContentHeightChanged: {
                    if (pinnedToNewest) {
                        Qt.callLater(snapToNewest);
                    }
                    Qt.callLater(maybeLoadOlder);
                }
                onHeightChanged: {
                    if (pinnedToNewest) {
                        Qt.callLater(snapToNewest);
                    }
                }

                // Older pages: Qt Quick's ListView never calls canFetchMore()/
                // fetchMore(), so load the next page whenever the oldest end is
                // in view — scrolled to the top, or the content does not fill
                // the viewport yet (then there is no scrolling to trigger it).
                // Each step waits for the previous page (loading) and stops
                // once the view overflows or no cursor is left.
                function maybeLoadOlder() {
                    if (!App.messages.canLoadMore || App.messages.loading) {
                        return;
                    }
                    if (atYBeginning || contentHeight < height) {
                        App.messages.loadOlder();
                    }
                }
                Component.onCompleted: Qt.callLater(maybeLoadOlder)
                onAtYBeginningChanged: Qt.callLater(maybeLoadOlder)
                onCountChanged: Qt.callLater(maybeLoadOlder)

                Connections {
                    target: App.messages
                    function onLoadingChanged() {
                        Qt.callLater(transcript.maybeLoadOlder);
                    }
                    function onCanLoadMoreChanged() {
                        Qt.callLater(transcript.maybeLoadOlder);
                    }
                    function onNewestMessageAdded(fromMe: bool) {
                        if (fromMe || transcript.pinnedToNewest) {
                            transcript.pinnedToNewest = true;
                            Qt.callLater(transcript.snapToNewest);
                        }
                    }
                    function onChatIdChanged() {
                        transcript.pinnedToNewest = true;
                    }
                }

                Kirigami.LoadingPlaceholder {
                    anchors.centerIn: parent
                    visible: App.messages.loading && transcript.count === 0
                }

                Kirigami.PlaceholderMessage {
                    anchors.centerIn: parent
                    width: parent.width - Kirigami.Units.gridUnit * 4
                    visible: App.messages.loaded && transcript.count === 0
                    icon.name: "dialog-messages"
                    text: App.messages.lastError.length > 0 ? i18n("Could not load messages") : i18n("No messages yet")
                    explanation: App.messages.lastError
                }
            }
        }

        Composer {
            id: composer
            objectName: "conversationComposer"
            Layout.fillWidth: true
            sms: App.messages.smsMode
            enabled: App.daemon.connectionState === "connected" || App.daemon.connectionState === "registering"
            onSendRequested: (text, replyTo) => App.messages.sendText(text, replyTo)
            onEditRequested: (messageId, text) => App.messages.edit(messageId, text)
            onFilesSelected: urls => {
                for (const url of urls) {
                    App.messages.sendFile(url);
                }
            }
            onTypingChanged: typing => App.messages.setTyping(typing)
        }
    }

    // A failed send or edit puts the text back instead of losing it.
    Connections {
        target: App.messages
        function onSendTextFailed(chatId: string, text: string, replyTo: string, error: string) {
            if (chatId !== page.chatId) {
                return;
            }
            const row = replyTo.length > 0 ? App.messages.rowOf(replyTo) : -1;
            const quoted = row >= 0 ? App.messages.get(row) : null;
            composer.restore(text, quoted ? replyTo : "", quoted ? quoted.senderName : "", quoted ? (quoted.text ?? "") : "", "");
        }
        function onEditFailed(chatId: string, messageId: string, text: string, error: string) {
            if (chatId === page.chatId) {
                composer.restore(text, "", "", "", messageId);
            }
        }
    }

    // Files dropped anywhere on the conversation wait in the composer.
    DropArea {
        id: dropArea
        anchors.fill: parent
        enabled: composer.enabled && composer.allowAttachments
        keys: ["text/uri-list"]
        onEntered: drag => {
            drag.accepted = drag.hasUrls && drag.urls.some(u => u.toString().startsWith("file:"));
        }
        onDropped: drop => {
            composer.addAttachments(drop.urls);
            drop.accept(Qt.CopyAction);
        }

        Rectangle {
            anchors.fill: parent
            anchors.margins: Kirigami.Units.smallSpacing
            visible: dropArea.containsDrag
            radius: Kirigami.Units.largeSpacing
            color: Kirigami.ColorUtils.tintWithAlpha(Kirigami.Theme.backgroundColor, Kirigami.Theme.highlightColor, 0.15)
            border.color: Kirigami.Theme.highlightColor
            border.width: 2

            Kirigami.PlaceholderMessage {
                anchors.centerIn: parent
                icon.name: "mail-attachment"
                text: i18n("Drop to attach")
            }
        }
    }

    MessageMenu {
        id: messageMenu
        onReactRequested: anchor => page.openTapbacks(App.messages.rowOf(message.messageId), anchor)
        onReplyRequested: composer.startReply(message.messageId, message.senderName, message.text ?? "")
        onEditRequested: composer.startEdit(message.messageId, message.text ?? "")
    }

    TapbackPicker {
        id: tapbackPicker
        onPicked: kind => App.messages.react(messageId, kind)
    }
}
