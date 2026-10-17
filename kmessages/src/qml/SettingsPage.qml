// SPDX-License-Identifier: SSPL-1.0
import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.imessage.kmessages

FormCard.FormCardPage {
    id: page

    title: i18n("Settings")

    readonly property string state_: App.daemon.connectionState

    FormCard.FormHeader {
        title: i18n("Account")
    }
    FormCard.FormCard {
        FormCard.FormTextDelegate {
            text: i18n("Status")
            description: {
                switch (page.state_) {
                case "connected":
                    return i18n("Connected");
                case "needs_login":
                    return i18n("Not signed in");
                case "needs_two_factor":
                    return i18n("Waiting for a verification code");
                case "unavailable":
                    return i18n("imessaged is not running");
                case "error":
                    return i18n("Error: %1", App.daemon.connectionDetail);
                }
                return page.state_;
            }
        }
        FormCard.FormDelegateSeparator {
            visible: profilesStatus.visible
        }
        FormCard.FormTextDelegate {
            id: profilesStatus
            // Only worth a line when something needs doing.
            visible: App.daemon.profilesState === "needs_login" || App.daemon.profilesState === "unavailable"
            text: i18n("Shared names and photos")
            description: App.daemon.profilesState === "needs_login"
                ? i18n("Run `imsg login` once to see the names and photos your contacts share.")
                : i18n("Unavailable: %1", App.daemon.profilesDetail)
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextDelegate {
            text: i18n("Sending from")
            visible: App.daemon.ownSender.length > 0
            description: App.daemon.ownSender.replace(/^(mailto|tel):/, "")
        }
        FormCard.FormDelegateSeparator {
            visible: App.daemon.ownSender.length > 0
        }
        FormCard.FormTextDelegate {
            text: i18n("Addresses")
            description: App.daemon.ownHandles.length > 0 ? App.daemon.ownHandles.map(h => h.replace(/^(mailto|tel):/, "")).join(", ") : i18n("None registered")
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextFieldDelegate {
            objectName: "otherHandlesField"
            label: i18n("My other addresses")
            placeholderText: i18n("e.g. +1 555 123 4567, me@icloud.com")
            text: App.settings.otherHandles
            description: i18n("Your phone number and other addresses this computer does not send from, separated by commas. Messages you send from them on another device (your iPhone) arrive without a notification and do not count as unread.")
            onEditingFinished: App.settings.otherHandles = text
        }
        FormCard.FormDelegateSeparator {
            visible: daemonOtherHandles.visible
        }
        FormCard.FormTextDelegate {
            id: daemonOtherHandles
            objectName: "daemonOtherHandles"
            visible: App.daemon.daemonOtherHandles.length > 0
            text: i18n("Also yours (from the daemon)")
            description: App.daemon.daemonOtherHandles.map(h => h.replace(/^(mailto|tel):/, "")).join(", ")
        }
        FormCard.FormDelegateSeparator {
            visible: signIn.visible || signOut.visible
        }
        FormCard.FormButtonDelegate {
            id: signIn
            visible: page.state_ === "needs_login" || page.state_ === "needs_two_factor"
            icon.name: "im-user"
            text: page.state_ === "needs_two_factor" ? i18n("Enter Verification Code…") : i18n("Sign In…")
            onClicked: applicationWindow().showLogin(page.state_ === "needs_two_factor" ? 1 : 0)
        }
        FormCard.FormButtonDelegate {
            id: signOut
            visible: App.daemon.registered || page.state_ === "connected"
            icon.name: "system-log-out"
            text: i18n("Sign Out")
            description: i18n("Forgets the account on this computer. Message history is kept.")
            onClicked: App.daemon.logout()
        }
    }

    FormCard.FormHeader {
        title: i18n("General")
    }
    FormCard.FormCard {
        FormCard.FormSwitchDelegate {
            text: i18n("Notifications")
            description: i18n("Show a notification with quick reply when a message arrives.")
            checked: App.settings.notificationsEnabled
            onToggled: App.settings.notificationsEnabled = checked
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormSwitchDelegate {
            text: i18n("Start minimised")
            description: App.trayAvailable ? i18n("Start in the system tray without opening the window.") : i18n("Needs the system tray icon.")
            enabled: App.trayAvailable
            checked: App.settings.startMinimized
            onToggled: App.settings.startMinimized = checked
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormSwitchDelegate {
            text: i18n("Keep running in the system tray")
            description: i18n("Closing the window hides it so notifications keep arriving.")
            enabled: App.trayAvailable
            checked: App.settings.closeToTray
            onToggled: App.settings.closeToTray = checked
        }
    }

    FormCard.FormHeader {
        title: i18n("About")
    }
    FormCard.FormCard {
        FormCard.FormTextDelegate {
            text: i18n("imessaged version")
            description: App.daemon.version.length > 0 ? App.daemon.version : i18n("unknown")
        }
    }
}
