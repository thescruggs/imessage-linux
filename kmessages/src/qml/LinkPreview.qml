// SPDX-License-Identifier: SSPL-1.0
import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.imessage.kmessages

// A link preview card (Messages' URL balloon), as the sender fetched it:
// the page's image on top when there is one, else the site icon beside the
// title; the title in bold, the site name (or the host) under it. Click
// opens the URL; hover shows it; right-click / press and hold opens the
// message menu (with "Copy Link").
QQC2.Control {
    id: root

    // MessageListModel's `link` role: url, title, summary, siteName, host,
    // openable, image / icon ({id, localPath, downloading, error} or {}).
    property var link: ({})
    property color cardColor
    property color textColor
    property real maxWidth: Kirigami.Units.gridUnit * 16

    signal menuRequested(Item anchor)

    readonly property string url: link.url ?? ""
    readonly property string title: (link.title ?? "").length > 0 ? link.title : url
    readonly property string site: (link.siteName ?? "").length > 0 ? link.siteName : (link.host ?? "")
    readonly property var image: link.image ?? ({})
    readonly property var icon: link.icon ?? ({})
    readonly property bool hasImage: (image.id ?? "").length > 0 && (image.error ?? "").length === 0
    readonly property bool imageReady: hasImage && (image.localPath ?? "").length > 0 && probe.status === Image.Ready && probe.implicitWidth > 0
    readonly property bool hasIcon: (icon.localPath ?? "").length > 0
    readonly property real cardWidth: Math.min(maxWidth, Kirigami.Units.gridUnit * 15)
    readonly property real radius: Kirigami.Units.gridUnit * 0.9

    function fetch(a) {
        if ((a.id ?? "").length > 0 && (a.localPath ?? "").length === 0 && a.downloading !== true && (a.error ?? "").length === 0) {
            App.messages.downloadAttachment(a.id);
        }
    }

    Component.onCompleted: {
        fetch(image);
        fetch(icon);
    }

    implicitWidth: cardWidth
    padding: 0
    leftPadding: 0
    rightPadding: 0
    topPadding: 0
    bottomPadding: 0
    leftInset: 0
    rightInset: 0
    topInset: 0
    bottomInset: 0
    Accessible.role: Accessible.Link
    Accessible.name: title

    background: Rectangle {
        radius: root.radius
        color: root.cardColor
    }

    // Loads the file to learn its aspect ratio; the visible copy hits the pixmap cache.
    Image {
        id: probe
        visible: false
        source: heroImage.source
        sourceSize.width: root.cardWidth * 2
        asynchronous: true
    }

    contentItem: ColumnLayout {
        spacing: 0

        Item {
            visible: root.hasImage
            Layout.fillWidth: true
            Layout.preferredHeight: root.imageReady ? Math.min(root.cardWidth, root.cardWidth * probe.implicitHeight / probe.implicitWidth) : root.cardWidth * 0.52

            Rectangle {
                anchors.fill: parent
                visible: !root.imageReady
                radius: root.radius
                color: Kirigami.ColorUtils.tintWithAlpha(root.cardColor, root.textColor, 0.08)
                QQC2.BusyIndicator {
                    anchors.centerIn: parent
                    running: root.image.downloading === true
                    visible: running
                }
            }
            Image {
                id: heroImage
                objectName: "linkPreviewImage"
                anchors.fill: parent
                visible: root.imageReady
                source: root.hasImage && (root.image.localPath ?? "").length > 0
                    ? "image://attachment/" + encodeURIComponent(root.image.localPath) + "?rw=" + (root.radius / root.cardWidth).toFixed(4)
                    : ""
                fillMode: Image.PreserveAspectCrop
                asynchronous: true
                sourceSize.width: root.cardWidth * 2
            }
        }

        RowLayout {
            Layout.fillWidth: true
            Layout.margins: Kirigami.Units.largeSpacing
            Layout.topMargin: Kirigami.Units.smallSpacing * 1.5
            Layout.bottomMargin: Kirigami.Units.smallSpacing * 1.5
            spacing: Kirigami.Units.largeSpacing

            Kirigami.Icon {
                visible: !root.hasImage
                Layout.alignment: Qt.AlignVCenter
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
                source: root.hasIcon ? "file://" + root.icon.localPath : "globe"
                fallback: "globe"
                color: root.textColor
            }

            ColumnLayout {
                Layout.fillWidth: true
                spacing: 0

                QQC2.Label {
                    objectName: "linkPreviewTitle"
                    Layout.fillWidth: true
                    text: root.title
                    textFormat: Text.PlainText
                    font.bold: true
                    color: root.textColor
                    wrapMode: Text.Wrap
                    maximumLineCount: 2
                    elide: Text.ElideRight
                }
                QQC2.Label {
                    Layout.fillWidth: true
                    visible: text.length > 0
                    text: root.site
                    textFormat: Text.PlainText
                    font: Kirigami.Theme.smallFont
                    color: root.textColor
                    opacity: 0.7
                    elide: Text.ElideRight
                }
            }
        }
    }

    HoverHandler {
        id: hover
        cursorShape: root.link.openable === true ? Qt.PointingHandCursor : Qt.ArrowCursor
    }
    QQC2.ToolTip.visible: hover.hovered && root.url.length > 0
    QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
    QQC2.ToolTip.text: root.url

    TapHandler {
        acceptedButtons: Qt.LeftButton
        onTapped: if (root.link.openable === true) Qt.openUrlExternally(root.url)
        onLongPressed: root.menuRequested(root)
    }
    TapHandler {
        acceptedButtons: Qt.RightButton
        onTapped: root.menuRequested(root)
    }
}
