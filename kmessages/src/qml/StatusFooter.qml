// SPDX-License-Identifier: SSPL-1.0
import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.imessage.kmessages

QQC2.ToolBar {
    id: footer

    position: QQC2.ToolBar.Footer
    readonly property string state_: App.daemon.connectionState

    function label(): string {
        switch (state_) {
        case "unavailable":
            return i18n("Service not running");
        case "disconnected":
            return i18n("Disconnected");
        case "connecting":
            return i18n("Connecting…");
        case "registering":
            return i18n("Registering…");
        case "connected": {
            const sender = App.daemon.ownSender;
            return sender.length > 0 ? i18n("Connected as %1", sender.replace(/^(mailto|tel):/, "")) : i18n("Connected");
        }
        case "needs_login":
            return i18n("Not signed in");
        case "needs_two_factor":
            return i18n("Verification code needed");
        case "error":
            return App.daemon.connectionDetail.length > 0 ? i18n("Error: %1", App.daemon.connectionDetail) : i18n("Error");
        }
        return state_;
    }

    contentItem: RowLayout {
        spacing: Kirigami.Units.smallSpacing

        Rectangle {
            Layout.preferredWidth: Kirigami.Units.smallSpacing * 2
            Layout.preferredHeight: Layout.preferredWidth
            Layout.leftMargin: Kirigami.Units.smallSpacing
            radius: width / 2
            color: {
                switch (footer.state_) {
                case "connected":
                    return Kirigami.Theme.positiveTextColor;
                case "connecting":
                case "registering":
                    return Kirigami.Theme.neutralTextColor;
                default:
                    return Kirigami.Theme.negativeTextColor;
                }
            }
        }
        QQC2.Label {
            id: statusLabel
            Layout.fillWidth: true
            text: footer.label()
            elide: Text.ElideRight
            font: Kirigami.Theme.smallFont

            QQC2.ToolTip.visible: hover.hovered && (App.daemon.version.length > 0 || App.daemon.lastError.length > 0)
            QQC2.ToolTip.text: App.daemon.lastError.length > 0 ? App.daemon.lastError : i18n("imessaged %1", App.daemon.version)
            HoverHandler {
                id: hover
            }
        }
        QQC2.ToolButton {
            visible: footer.state_ === "needs_login" || footer.state_ === "needs_two_factor"
            text: footer.state_ === "needs_two_factor" ? i18n("Enter Code…") : i18n("Sign In…")
            onClicked: applicationWindow().showLogin(footer.state_ === "needs_two_factor" ? 1 : 0)
        }
    }
}
