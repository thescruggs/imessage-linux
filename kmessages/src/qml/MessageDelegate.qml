// SPDX-License-Identifier: SSPL-1.0
import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.components as Components
import org.imessage.kmessages

// One transcript row: optional time separator, sender name (groups),
// reply quote, attachments, the text bubble with its tail on the last
// bubble of a run, tapback chips, and the delivery label.
Item {
    id: root

    required property int index
    required property string messageId
    required property string richText
    required property bool hasText
    required property bool isEmojiOnly
    required property string senderName
    required property bool isFromMe
    required property bool isSms
    required property string time
    required property bool showSeparator
    required property string separatorLabel
    required property bool firstInGroup
    required property bool lastInGroup
    required property bool showSenderName
    required property bool showAvatar
    required property color avatarColor
    required property string avatarSource
    required property string deliveryLabel
    required property string delivery
    required property bool edited
    required property bool unsent
    required property bool verified
    required property var reactions
    required property var attachments
    required property string replyTo
    required property string replyToText
    required property string replyToSender
    required property string subject
    required property var unsupported
    required property var link
    required property bool linkOnly

    signal menuRequested(Item anchor)
    signal tapbackRequested(Item anchor)

    // iMessage blue, text-message (SMS / RCS) green, as Messages.app.
    readonly property color sentColor: isSms ? "#34c759" : "#1982fc"
    // Over a chat background: a stronger tint, and no carved tail (its
    // carving block is painted in the plain background colour).
    readonly property bool overBackground: App.currentChatBackground !== ""
    readonly property color receivedColor: overBackground
        ? Qt.alpha(Kirigami.ColorUtils.tintWithAlpha(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, 0.16), 0.94)
        : Kirigami.ColorUtils.tintWithAlpha(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, 0.11)
    readonly property color bubbleColor: isFromMe ? sentColor : receivedColor
    readonly property color bubbleTextColor: isFromMe ? "white" : Kirigami.Theme.textColor
    readonly property real sideMargin: Kirigami.Units.largeSpacing * 1.5
    readonly property real avatarGutter: App.messages.isGroup && !isFromMe ? Kirigami.Units.gridUnit * 1.5 + Kirigami.Units.smallSpacing : 0
    readonly property real maxBubbleWidth: Math.max(Kirigami.Units.gridUnit * 8, Math.min((width - sideMargin * 2 - avatarGutter) * 0.75, Kirigami.Units.gridUnit * 30))
    readonly property bool hasLink: (link.url ?? "").length > 0
    // A message that is only a URL shows the preview card instead (as Messages).
    readonly property string bodyText: linkOnly ? "" : hasText ? richText : (unsupported.length > 0 ? i18n("Unsupported message: %1", unsupported.join(", ")) : "")

    width: ListView.view ? ListView.view.width : implicitWidth
    implicitHeight: column.implicitHeight

    ColumnLayout {
        id: column

        width: parent.width
        spacing: 0

        QQC2.Label {
            visible: root.showSeparator
            Layout.alignment: Qt.AlignHCenter
            topPadding: Kirigami.Units.largeSpacing
            bottomPadding: Kirigami.Units.smallSpacing
            text: root.separatorLabel
            font: Kirigami.Theme.smallFont
            opacity: root.overBackground ? 0.9 : 0.6
            leftPadding: root.overBackground ? Kirigami.Units.smallSpacing * 2 : 0
            rightPadding: leftPadding
            background: Rectangle {
                visible: root.overBackground
                anchors.fill: parent
                anchors.topMargin: parent.topPadding - 2
                anchors.bottomMargin: parent.bottomPadding - 2
                radius: height / 2
                color: Qt.alpha(Kirigami.Theme.backgroundColor, 0.75)
            }
        }

        // Sender name in groups, and the unverified-sender badge.
        RowLayout {
            visible: !root.unsent && !root.isFromMe && (root.showSenderName || (!root.verified && root.firstInGroup))
            Layout.leftMargin: root.sideMargin + root.avatarGutter + Kirigami.Units.largeSpacing
            Layout.topMargin: Kirigami.Units.smallSpacing
            spacing: Kirigami.Units.smallSpacing

            QQC2.Label {
                visible: root.showSenderName
                text: root.senderName
                font: Kirigami.Theme.smallFont
                opacity: 0.7
            }
            Kirigami.Icon {
                visible: !root.verified
                source: "security-low"
                implicitWidth: Kirigami.Units.iconSizes.small
                implicitHeight: Kirigami.Units.iconSizes.small
            }
            QQC2.Label {
                visible: !root.verified
                text: i18n("Unverified sender")
                font: Kirigami.Theme.smallFont
                color: Kirigami.Theme.negativeTextColor

                QQC2.ToolTip.visible: unverifiedHover.hovered
                QQC2.ToolTip.text: i18n("This message decrypted, but the sender's signing key could not be verified. It may not really be from %1.", root.senderName)
                HoverHandler {
                    id: unverifiedHover
                }
            }
        }

        QQC2.Label {
            visible: root.unsent
            Layout.alignment: Qt.AlignHCenter
            Layout.topMargin: Kirigami.Units.smallSpacing
            Layout.bottomMargin: Kirigami.Units.smallSpacing
            text: "<i>" + (root.isFromMe ? i18n("You unsent a message") : i18n("%1 unsent a message", root.senderName)) + "</i>"
            textFormat: Text.StyledText
            font: Kirigami.Theme.smallFont
            opacity: 0.6
        }

        // Inline reply quote.
        QQC2.Control {
            visible: !root.unsent && root.replyTo.length > 0
            Layout.alignment: root.isFromMe ? Qt.AlignRight : Qt.AlignLeft
            Layout.leftMargin: root.isFromMe ? 0 : root.sideMargin + root.avatarGutter
            Layout.rightMargin: root.isFromMe ? root.sideMargin : 0
            Layout.topMargin: Kirigami.Units.smallSpacing
            Layout.maximumWidth: root.maxBubbleWidth
            padding: Kirigami.Units.smallSpacing
            leftPadding: Kirigami.Units.largeSpacing
            rightPadding: Kirigami.Units.largeSpacing
            opacity: 0.75
            background: Rectangle {
                radius: height / 2
                color: "transparent"
                border.color: root.receivedColor
                border.width: 2
            }
            contentItem: QQC2.Label {
                text: root.replyToSender.length > 0 ? i18nc("reply quote: sender: text", "%1: %2", root.replyToSender, root.replyToText) : root.replyToText
                elide: Text.ElideRight
                font: Kirigami.Theme.smallFont
            }
        }

        RowLayout {
            id: mainRow

            visible: !root.unsent
            Layout.fillWidth: true
            Layout.leftMargin: root.sideMargin
            Layout.rightMargin: root.sideMargin
            Layout.topMargin: (root.firstInGroup ? Kirigami.Units.smallSpacing * 1.5 : 1) + (root.reactions.length > 0 ? Kirigami.Units.gridUnit * 0.9 : 0)
            layoutDirection: root.isFromMe ? Qt.RightToLeft : Qt.LeftToRight
            spacing: Kirigami.Units.smallSpacing

            Item {
                visible: App.messages.isGroup && !root.isFromMe
                z: 1 // above the neighbouring bubble's tail-carving block
                Layout.preferredWidth: Kirigami.Units.gridUnit * 1.5
                Layout.preferredHeight: Kirigami.Units.gridUnit * 1.5
                Layout.alignment: Qt.AlignBottom

                Components.Avatar {
                    anchors.fill: parent
                    visible: root.showAvatar
                    name: root.senderName
                    color: root.avatarColor
                    source: root.avatarSource
                    // Same rule as ChatListDelegate: no "+" initials for phone numbers.
                    initialsMode: /^[+(0-9]/.test(root.senderName) ? Components.Avatar.InitialsMode.UseIcon : Components.Avatar.InitialsMode.UseInitials
                    iconSource: "user"
                }
            }

            // Attachments + text bubble; tapbacks sit on the stack's top corner.
            Item {
                id: stack
                implicitWidth: stackColumn.implicitWidth
                implicitHeight: stackColumn.implicitHeight
                Layout.maximumWidth: root.maxBubbleWidth
                Layout.preferredWidth: stackColumn.implicitWidth

            ReactionChips {
                reactions: root.reactions
                receivedColor: root.receivedColor
                y: -height + Kirigami.Units.smallSpacing
                x: root.isFromMe ? -width / 2 : stack.width - width / 2
                z: 2
            }

            ColumnLayout {
                id: stackColumn
                width: stack.width
                spacing: 2

                Repeater {
                    model: root.attachments
                    delegate: AttachmentView {
                        required property var modelData
                        attachment: modelData
                        fromMe: root.isFromMe
                        bubbleColor: root.bubbleColor
                        textColor: root.bubbleTextColor
                        maxWidth: root.maxBubbleWidth
                        Layout.alignment: root.isFromMe ? Qt.AlignRight : Qt.AlignLeft
                        onMenuRequested: anchor => root.menuRequested(anchor)
                    }
                }

                Item {
                    id: bubble

                    readonly property real hPad: root.isEmojiOnly ? 0 : Kirigami.Units.largeSpacing
                    readonly property real vPad: root.isEmojiOnly ? 0 : Kirigami.Units.smallSpacing * 1.5

                    visible: root.bodyText.length > 0
                    Layout.alignment: root.isFromMe ? Qt.AlignRight : Qt.AlignLeft
                    implicitWidth: textLabel.width + hPad * 2
                    implicitHeight: textLabel.implicitHeight + vPad * 2

                    // Tail: a bubble-coloured block whose outer edge is carved
                    // by a background-coloured rounded block (classic CSS trick).
                    Rectangle {
                        visible: root.lastInGroup && !root.isEmojiOnly && !root.overBackground && !root.hasLink
                        width: 20
                        height: Math.min(25, bubble.height)
                        y: bubble.height - height
                        x: root.isFromMe ? bubble.width - 13 : -7
                        color: root.bubbleColor
                        bottomLeftRadius: root.isFromMe ? 16 : 0
                        bottomRightRadius: root.isFromMe ? 0 : 16
                    }
                    Rectangle {
                        visible: root.lastInGroup && !root.isEmojiOnly && !root.overBackground && !root.hasLink
                        width: 26
                        height: Math.min(25, bubble.height)
                        y: bubble.height - height
                        x: root.isFromMe ? bubble.width : -26
                        color: Kirigami.Theme.backgroundColor
                        bottomLeftRadius: root.isFromMe ? 10 : 0
                        bottomRightRadius: root.isFromMe ? 0 : 10
                    }

                    Rectangle {
                        anchors.fill: parent
                        visible: !root.isEmojiOnly
                        radius: Math.min(Kirigami.Units.gridUnit * 0.9, height / 2)
                        color: root.bubbleColor
                    }

                    QQC2.Label {
                        id: textLabel
                        x: bubble.hPad
                        y: bubble.vPad
                        width: Math.min(implicitWidth, root.maxBubbleWidth - bubble.hPad * 2)
                        text: root.bodyText
                        textFormat: root.hasText ? Text.StyledText : Text.PlainText
                        wrapMode: Text.Wrap
                        color: root.bubbleTextColor
                        linkColor: root.isFromMe ? "white" : Kirigami.Theme.linkColor
                        font.pointSize: root.isEmojiOnly ? Kirigami.Theme.defaultFont.pointSize * 3 : Kirigami.Theme.defaultFont.pointSize
                        font.italic: !root.hasText
                        onLinkActivated: link => Qt.openUrlExternally(link)

                        HoverHandler {
                            cursorShape: textLabel.hoveredLink.length > 0 ? Qt.PointingHandCursor : Qt.ArrowCursor
                        }
                    }

                    TapHandler {
                        acceptedButtons: Qt.RightButton
                        onTapped: root.menuRequested(bubble)
                    }
                    TapHandler {
                        acceptedButtons: Qt.LeftButton
                        onLongPressed: root.menuRequested(bubble)
                        onDoubleTapped: if (!root.isSms) root.tapbackRequested(bubble)
                    }

                    QQC2.ToolTip.visible: bubbleHover.hovered && !textLabel.hoveredLink
                    QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay * 2
                    QQC2.ToolTip.text: root.time
                    HoverHandler {
                        id: bubbleHover
                    }

                }


                LinkPreview {
                    objectName: "linkPreview"
                    visible: root.hasLink
                    link: root.link
                    cardColor: root.bubbleColor
                    textColor: root.bubbleTextColor
                    maxWidth: root.maxBubbleWidth
                    Layout.alignment: root.isFromMe ? Qt.AlignRight : Qt.AlignLeft
                    onMenuRequested: anchor => root.menuRequested(anchor)
                }

                QQC2.Label {
                    visible: root.edited
                    Layout.alignment: root.isFromMe ? Qt.AlignRight : Qt.AlignLeft
                    text: i18n("Edited")
                    font: Kirigami.Theme.smallFont
                    color: Kirigami.Theme.linkColor
                }
            }
            }

            Item {
                Layout.fillWidth: true
            }
        }

        QQC2.Label {
            visible: root.deliveryLabel.length > 0
            Layout.alignment: Qt.AlignRight
            Layout.rightMargin: root.sideMargin + Kirigami.Units.smallSpacing
            Layout.topMargin: 2
            text: root.deliveryLabel
            font: Kirigami.Theme.smallFont
            color: root.delivery === "failed" ? Kirigami.Theme.negativeTextColor : Kirigami.Theme.textColor
            opacity: root.delivery === "failed" || root.overBackground ? 1 : 0.6
            leftPadding: root.overBackground ? Kirigami.Units.smallSpacing * 2 : 0
            rightPadding: leftPadding
            background: Rectangle {
                visible: root.overBackground
                radius: height / 2
                color: Qt.alpha(Kirigami.Theme.backgroundColor, 0.75)
            }
        }
    }
}
