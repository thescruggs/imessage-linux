// SPDX-License-Identifier: SSPL-1.0
import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Dialogs
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.imessage.kmessages

// Message entry: attach (file dialog, drop, paste), emoji, Enter sends /
// Shift+Enter newline, typing indicator with debounce, reply and edit modes.
// Attachments wait as chips above the field until the message is sent.
QQC2.Control {
    id: root

    property string replyToId
    property string replyToLabel
    property string editingId
    property bool typingSent: false
    property double typingSentAt: 0
    /// The recipient's indicator expires after ~60 s: say it again this often while typing goes on.
    property int typingRefreshMs: 30000
    property alias text: input.text
    property alias placeholderText: input.placeholderText
    property bool allowAttachments: true
    property bool restoring: false
    /// File URLs (strings) waiting to be sent with the next message.
    property var attachments: []

    signal sendRequested(string text, string replyTo)
    signal editRequested(string messageId, string text)
    signal filesSelected(var urls)
    signal typingChanged(bool typing)

    function startReply(messageId: string, sender: string, text: string) {
        editingId = "";
        replyToId = messageId;
        replyToLabel = text.length > 0 ? i18nc("reply banner: sender: text", "%1: %2", sender, text) : sender;
        input.forceActiveFocus();
    }

    function startEdit(messageId: string, text: string) {
        replyToId = "";
        editingId = messageId;
        input.text = text;
        input.cursorPosition = input.length;
        input.forceActiveFocus();
    }

    // Put back a message whose send/edit failed. Anything typed since is kept
    // after it; reply/edit mode is restored when it was in one.
    function restore(text: string, replyTo: string, replySender: string, replyText: string, editId: string) {
        const typed = input.text;
        if (editId.length > 0) {
            replyToId = "";
            editingId = editId;
        } else if (replyTo.length > 0) {
            editingId = "";
            replyToId = replyTo;
            replyToLabel = replyText.length > 0 ? i18nc("reply banner: sender: text", "%1: %2", replySender, replyText) : replySender;
        }
        restoring = true; // not a keystroke: no typing indicator
        input.text = typed.length > 0 && typed !== text ? text + "\n" + typed : text;
        restoring = false;
        input.cursorPosition = input.length;
    }

    function addAttachments(urls) {
        if (!allowAttachments) {
            return;
        }
        const list = attachments.slice();
        for (const url of urls) {
            const s = url.toString();
            if (s.startsWith("file:") && !list.includes(s)) {
                list.push(s);
            }
        }
        attachments = list;
        input.forceActiveFocus();
    }

    function removeAttachment(index: int) {
        const list = attachments.slice();
        list.splice(index, 1);
        attachments = list;
    }

    function cancel() {
        if (editingId.length > 0) {
            input.clear();
        }
        replyToId = "";
        editingId = "";
    }

    function setTyping(on: bool) {
        const now = Date.now();
        if (on !== typingSent || (on && now - typingSentAt >= typingRefreshMs)) {
            typingSent = on;
            typingSentAt = on ? now : 0;
            root.typingChanged(on);
        }
    }

    function send() {
        const text = input.text.trim();
        const editing = editingId.length > 0;
        if (text.length === 0 && (editing || attachments.length === 0)) {
            return;
        }
        if (editing) {
            root.editRequested(editingId, text);
        } else {
            if (attachments.length > 0) {
                root.filesSelected(attachments);
                attachments = [];
            }
            if (text.length > 0) {
                root.sendRequested(text, replyToId);
            }
        }
        typingTimer.stop();
        setTyping(false);
        input.clear();
        replyToId = "";
        editingId = "";
    }

    padding: Kirigami.Units.smallSpacing * 1.5
    leftPadding: Kirigami.Units.largeSpacing
    rightPadding: Kirigami.Units.largeSpacing

    background: Rectangle {
        color: Kirigami.Theme.backgroundColor
        Kirigami.Separator {
            anchors.top: parent.top
            width: parent.width
        }
    }

    Timer {
        id: typingTimer
        interval: 5000
        onTriggered: root.setTyping(false)
    }

    contentItem: ColumnLayout {
        spacing: Kirigami.Units.smallSpacing

        RowLayout {
            visible: root.replyToId.length > 0 || root.editingId.length > 0
            Layout.fillWidth: true
            Kirigami.Icon {
                source: root.editingId.length > 0 ? "document-edit" : "mail-reply-sender"
                implicitWidth: Kirigami.Units.iconSizes.small
                implicitHeight: Kirigami.Units.iconSizes.small
            }
            QQC2.Label {
                Layout.fillWidth: true
                text: root.editingId.length > 0 ? i18n("Editing message") : i18n("Replying to %1", root.replyToLabel)
                elide: Text.ElideRight
                font: Kirigami.Theme.smallFont
            }
            QQC2.ToolButton {
                icon.name: "dialog-cancel"
                text: i18n("Cancel")
                display: QQC2.AbstractButton.IconOnly
                onClicked: root.cancel()
            }
        }

        Flow {
            Layout.fillWidth: true
            visible: root.attachments.length > 0
            spacing: Kirigami.Units.smallSpacing

            Repeater {
                model: root.attachments
                delegate: QQC2.Control {
                    id: chip
                    required property string modelData
                    required property int index
                    readonly property string fileName: decodeURIComponent(modelData.substring(modelData.lastIndexOf("/") + 1))
                    // Only decode what looks like an image (a failed decode logs a warning).
                    readonly property bool looksLikeImage: /\.(png|jpe?g|gif|webp|bmp)$/i.test(fileName)

                    padding: Kirigami.Units.smallSpacing
                    background: Rectangle {
                        radius: Kirigami.Units.smallSpacing * 2
                        color: Kirigami.ColorUtils.tintWithAlpha(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, 0.08)
                    }
                    contentItem: RowLayout {
                        spacing: Kirigami.Units.smallSpacing
                        Image {
                            id: thumb
                            visible: chip.looksLikeImage && status === Image.Ready
                            source: chip.looksLikeImage ? chip.modelData : ""
                            asynchronous: true
                            fillMode: Image.PreserveAspectCrop
                            sourceSize.width: Kirigami.Units.gridUnit * 4
                            sourceSize.height: Kirigami.Units.gridUnit * 4
                            Layout.preferredWidth: Kirigami.Units.gridUnit * 2
                            Layout.preferredHeight: Kirigami.Units.gridUnit * 2
                        }
                        Kirigami.Icon {
                            visible: !thumb.visible
                            source: "mail-attachment"
                            Layout.preferredWidth: Kirigami.Units.iconSizes.medium
                            Layout.preferredHeight: Kirigami.Units.iconSizes.medium
                        }
                        QQC2.Label {
                            text: chip.fileName
                            elide: Text.ElideMiddle
                            Layout.maximumWidth: Kirigami.Units.gridUnit * 10
                        }
                        QQC2.ToolButton {
                            icon.name: "edit-delete-remove"
                            text: i18n("Remove")
                            display: QQC2.AbstractButton.IconOnly
                            onClicked: root.removeAttachment(chip.index)
                            QQC2.ToolTip.visible: hovered
                            QQC2.ToolTip.text: text
                        }
                    }
                }
            }
        }

        RowLayout {
            Layout.fillWidth: true
            spacing: Kirigami.Units.smallSpacing

            QQC2.ToolButton {
                visible: root.allowAttachments
                enabled: root.editingId.length === 0
                icon.name: "mail-attachment"
                text: i18n("Attach File…")
                display: QQC2.AbstractButton.IconOnly
                Layout.alignment: Qt.AlignBottom
                onClicked: fileDialog.open()
                QQC2.ToolTip.visible: hovered
                QQC2.ToolTip.text: text
            }

            QQC2.ScrollView {
                Layout.fillWidth: true
                Layout.maximumHeight: Kirigami.Units.gridUnit * 8

                QQC2.TextArea {
                    id: input
                    objectName: "composerInput"
                    placeholderText: i18n("iMessage")
                    wrapMode: TextEdit.Wrap
                    Accessible.name: i18n("Message")
                    leftPadding: Kirigami.Units.largeSpacing
                    rightPadding: Kirigami.Units.largeSpacing
                    background: Rectangle {
                        radius: Kirigami.Units.gridUnit * 0.9
                        color: Kirigami.Theme.backgroundColor
                        border.width: 1
                        border.color: input.activeFocus ? Kirigami.Theme.focusColor : Kirigami.ColorUtils.linearInterpolation(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, 0.25)
                    }

                    Keys.onPressed: event => {
                        if ((event.key === Qt.Key_Return || event.key === Qt.Key_Enter) && !(event.modifiers & Qt.ShiftModifier)) {
                            event.accepted = true;
                            root.send();
                        } else if (event.key === Qt.Key_Escape && (root.replyToId.length > 0 || root.editingId.length > 0)) {
                            event.accepted = true;
                            root.cancel();
                        } else if (event.matches(StandardKey.Paste) && root.allowAttachments && root.editingId.length === 0) {
                            // An image or copied files become attachments; text pastes as usual.
                            const files = App.clipboardFiles();
                            if (files.length > 0) {
                                event.accepted = true;
                                root.addAttachments(files);
                            }
                        }
                    }
                    onTextChanged: {
                        if (root.editingId.length > 0 || root.restoring) {
                            return;
                        }
                        if (text.length > 0) {
                            root.setTyping(true);
                            typingTimer.restart();
                        } else {
                            typingTimer.stop();
                            root.setTyping(false);
                        }
                    }
                }
            }

            QQC2.ToolButton {
                id: emojiButton
                icon.name: "smiley-shape"
                text: i18n("Emoji")
                display: QQC2.AbstractButton.IconOnly
                Layout.alignment: Qt.AlignBottom
                onClicked: emojiPicker.open()
                QQC2.ToolTip.visible: hovered
                QQC2.ToolTip.text: text

                EmojiPicker {
                    id: emojiPicker
                    y: -height - Kirigami.Units.smallSpacing
                    x: emojiButton.width - width
                    onPicked: emoji => {
                        input.insert(input.cursorPosition, emoji);
                        input.forceActiveFocus();
                    }
                }
            }

            QQC2.ToolButton {
                icon.name: root.editingId.length > 0 ? "dialog-ok-apply" : "document-send"
                text: root.editingId.length > 0 ? i18n("Save") : i18n("Send")
                display: QQC2.AbstractButton.IconOnly
                enabled: input.text.trim().length > 0 || (root.attachments.length > 0 && root.editingId.length === 0)
                Layout.alignment: Qt.AlignBottom
                onClicked: root.send()
                QQC2.ToolTip.visible: hovered
                QQC2.ToolTip.text: text
            }
        }
    }

    FileDialog {
        id: fileDialog
        title: i18n("Send Files")
        fileMode: FileDialog.OpenFiles
        onAccepted: root.addAttachments(selectedFiles)
    }
}
