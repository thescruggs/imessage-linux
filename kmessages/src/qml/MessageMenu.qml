// SPDX-License-Identifier: SSPL-1.0
import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import org.imessage.kmessages

QQC2.Menu {
    id: root

    property var message: ({})
    property Item anchorItem: null

    signal reactRequested(Item anchor)
    signal replyRequested
    signal editRequested

    readonly property bool canModify: message.isFromMe === true && message.unsent !== true

    QQC2.MenuItem {
        text: i18n("Tapback…")
        icon.name: "love"
        enabled: root.message.unsent !== true
        onTriggered: root.reactRequested(root.anchorItem)
    }
    QQC2.MenuItem {
        text: i18n("Reply")
        icon.name: "mail-reply-sender"
        enabled: root.message.unsent !== true
        onTriggered: root.replyRequested()
    }
    QQC2.MenuItem {
        text: i18n("Copy")
        icon.name: "edit-copy"
        enabled: root.message.hasText === true
        onTriggered: App.messages.copyText(root.message.messageId)
    }
    QQC2.MenuItem {
        text: i18n("Open Attachment")
        icon.name: "document-open"
        visible: root.message.hasAttachments === true
        height: visible ? implicitHeight : 0
        onTriggered: App.messages.openAttachment(root.message.attachments[0].id)
    }
    QQC2.MenuSeparator {
        visible: root.canModify
    }
    QQC2.MenuItem {
        text: i18n("Edit")
        icon.name: "document-edit"
        visible: root.canModify && root.message.hasText === true
        height: visible ? implicitHeight : 0
        onTriggered: root.editRequested()
    }
    QQC2.MenuItem {
        text: i18n("Undo Send")
        icon.name: "edit-undo"
        visible: root.canModify
        height: visible ? implicitHeight : 0
        onTriggered: App.messages.unsend(root.message.messageId)
    }
}
