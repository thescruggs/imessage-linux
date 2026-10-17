// SPDX-License-Identifier: SSPL-1.0
import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.imessage.kmessages

// An image (downloaded on demand, placeholder meanwhile) or a file chip.
Item {
    id: root

    property var attachment: ({})
    property bool fromMe: false
    property color bubbleColor
    property color textColor
    property real maxWidth: Kirigami.Units.gridUnit * 16

    signal menuRequested(Item anchor)

    readonly property bool isImage: attachment.isImage === true
    readonly property bool ready: (attachment.localPath ?? "").length > 0
    readonly property real imageWidth: Math.min(maxWidth, Kirigami.Units.gridUnit * 15)

    readonly property bool imageReady: isImage && probe.status === Image.Ready && probe.implicitWidth > 0
    readonly property string error: attachment.error ?? ""

    implicitWidth: isImage ? imageWidth : chip.implicitWidth
    implicitHeight: isImage ? (imageReady ? imageWidth * probe.implicitHeight / probe.implicitWidth : imageWidth * 0.66) : chip.implicitHeight

    Component.onCompleted: {
        if (isImage && !ready && !attachment.downloading && !attachment.error) {
            App.messages.downloadAttachment(attachment.id);
        }
    }

    // ---- image -------------------------------------------------------------
    // Loads the file to learn its aspect ratio; the visible copy below hits the pixmap cache.
    Image {
        id: probe
        visible: false
        source: image.source
        sourceSize.width: root.imageWidth * 2
        asynchronous: true
        // The local file went away (a sent original that was moved or
        // deleted): fall back to the daemon's copy.
        onStatusChanged: {
            if (status === Image.Error && root.ready) {
                App.messages.reloadAttachment(root.attachment.id);
            }
        }
    }

    Rectangle {
        id: placeholder
        visible: root.isImage && !root.imageReady
        anchors.fill: parent
        radius: Kirigami.Units.gridUnit * 0.9
        color: Kirigami.ColorUtils.tintWithAlpha(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, 0.08)

        QQC2.BusyIndicator {
            anchors.centerIn: parent
            running: root.attachment.downloading === true
            visible: running
        }
        ColumnLayout {
            anchors.centerIn: parent
            visible: root.attachment.downloading !== true
            Kirigami.Icon {
                Layout.alignment: Qt.AlignHCenter
                source: root.error.length > 0 ? "dialog-warning" : "image-x-generic"
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
            QQC2.Label {
                Layout.alignment: Qt.AlignHCenter
                text: root.error.length > 0 ? i18n("Could not download") : root.attachment.filename
                font: Kirigami.Theme.smallFont
                opacity: 0.7
            }
            QQC2.Label {
                Layout.alignment: Qt.AlignHCenter
                visible: root.error.length > 0
                text: i18n("Click to try again")
                font: Kirigami.Theme.smallFont
                opacity: 0.5
            }
        }
        HoverHandler {
            id: placeholderHover
        }
        QQC2.ToolTip.visible: placeholderHover.hovered && root.error.length > 0
        QQC2.ToolTip.text: root.error
    }

    // Served by AttachmentImageProvider: EXIF orientation applied and the
    // corners rounded in the decoder (Kirigami.ShadowedImage has no
    // autoTransform and draws upside down under software rendering).
    Image {
        id: image
        visible: root.imageReady
        anchors.fill: parent
        source: root.isImage && root.ready
            ? "image://attachment/" + encodeURIComponent(root.attachment.localPath) + "?rw=" + (Kirigami.Units.gridUnit * 0.9 / root.imageWidth).toFixed(4)
            : ""
        fillMode: Image.PreserveAspectFit
        asynchronous: true
        sourceSize.width: root.imageWidth * 2
    }

    // ---- other files ---------------------------------------------------------
    QQC2.Control {
        id: chip
        visible: !root.isImage
        padding: Kirigami.Units.smallSpacing * 1.5
        leftPadding: Kirigami.Units.largeSpacing
        rightPadding: Kirigami.Units.smallSpacing
        background: Rectangle {
            radius: Kirigami.Units.gridUnit * 0.9
            color: root.bubbleColor
        }
        contentItem: RowLayout {
            spacing: Kirigami.Units.smallSpacing
            Kirigami.Icon {
                source: (root.attachment.mime ?? "application/octet-stream").replace("/", "-")
                fallback: "unknown"
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
            ColumnLayout {
                spacing: 0
                Layout.maximumWidth: root.maxWidth - Kirigami.Units.gridUnit * 5
                QQC2.Label {
                    Layout.fillWidth: true
                    text: root.attachment.filename ?? ""
                    elide: Text.ElideMiddle
                    color: root.textColor
                }
                QQC2.Label {
                    text: (root.attachment.error ?? "").length > 0 ? root.attachment.error : root.attachment.sizeText ?? ""
                    font: Kirigami.Theme.smallFont
                    color: root.textColor
                    opacity: 0.8
                    elide: Text.ElideRight
                    Layout.fillWidth: true
                }
            }
            QQC2.BusyIndicator {
                visible: root.attachment.downloading === true
                running: visible
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
            QQC2.ToolButton {
                visible: root.attachment.downloading !== true
                icon.name: root.ready ? "document-open" : "download"
                icon.color: root.textColor
                text: root.ready ? i18n("Open") : i18n("Download")
                display: QQC2.AbstractButton.IconOnly
                onClicked: root.ready ? App.messages.openAttachment(root.attachment.id) : App.messages.downloadAttachment(root.attachment.id)
                QQC2.ToolTip.visible: hovered
                QQC2.ToolTip.text: text
            }
        }
    }

    TapHandler {
        acceptedButtons: Qt.LeftButton
        enabled: root.isImage
        onTapped: {
            if (root.error.length > 0 && !root.ready) {
                App.messages.downloadAttachment(root.attachment.id); // retry
            } else {
                App.messages.openAttachment(root.attachment.id);
            }
        }
        onLongPressed: root.menuRequested(root)
    }
    TapHandler {
        acceptedButtons: Qt.RightButton
        onTapped: root.menuRequested(root)
    }
}
