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
        title: i18n("Text Message Forwarding")
        visible: smsCard.visible
    }
    FormCard.FormCard {
        id: smsCard
        objectName: "smsForwardingCard"
        // Older daemons do not report it.
        visible: App.daemon.smsState.length > 0
        property bool busy: false
        property string error: ""

        FormCard.FormTextDelegate {
            objectName: "smsForwardingStatus"
            text: i18n("SMS and RCS through your iPhone")
            description: {
                const via = App.daemon.smsNumber.replace(/^tel:/, "");
                switch (App.daemon.smsState) {
                case "active":
                    return i18n("On: text messages to and from %1 come through your iPhone.", via);
                case "requested":
                    return i18n("Requested. Waiting for your iPhone to turn this computer on.");
                case "error":
                    return i18n("Error: %1", App.daemon.smsDetail);
                }
                return via.length > 0 ? i18n("Off") : i18n("Off. Put your iPhone's number in the daemon's config.toml (other_handles or sms_relay_number) first.");
            }
        }
        FormCard.FormDelegateSeparator {
            visible: smsHelp.visible
        }
        FormCard.FormTextDelegate {
            id: smsHelp
            objectName: "smsForwardingHelp"
            visible: App.daemon.smsState === "requested" || App.daemon.smsState === "off" || App.daemon.smsState === "error"
            text: i18n("On your iPhone")
            description: i18n("Open Settings > Messages > Text Message Forwarding and turn this computer on. Texts to and from people without iMessage then show here in green.")
        }
        FormCard.FormDelegateSeparator {
            visible: smsCard.error.length > 0
        }
        FormCard.FormTextDelegate {
            visible: smsCard.error.length > 0
            text: i18n("Could not reach your iPhone")
            description: smsCard.error
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormButtonDelegate {
            objectName: "smsEnableButton"
            visible: App.daemon.smsState !== "active"
            enabled: !smsCard.busy && App.daemon.registered && App.daemon.smsNumber.length > 0
            icon.name: "phone"
            text: App.daemon.smsState === "requested" ? i18n("Ask Again") : i18n("Enable")
            description: i18n("Asks your iPhone to relay text messages to this computer.")
            onClicked: {
                smsCard.busy = true;
                smsCard.error = "";
                App.daemon.enableSmsForwarding(true);
            }
        }
        FormCard.FormButtonDelegate {
            objectName: "smsDisableButton"
            visible: App.daemon.smsState === "active" || App.daemon.smsState === "requested"
            enabled: !smsCard.busy
            icon.name: "process-stop"
            text: i18n("Disable")
            onClicked: {
                smsCard.busy = true;
                smsCard.error = "";
                App.daemon.enableSmsForwarding(false);
            }
        }
        Connections {
            target: App.daemon
            function onSmsForwardingFinished(ok: bool, errorMessage: string) {
                smsCard.busy = false;
                smsCard.error = ok ? "" : errorMessage;
            }
        }
    }

    FormCard.FormHeader {
        title: i18n("Sync from Mac")
    }
    FormCard.FormCard {
        objectName: "macSyncCard"
        FormCard.FormTextFieldDelegate {
            objectName: "macHostField"
            label: i18n("Mac")
            placeholderText: App.daemon.macHost.length > 0 ? App.daemon.macHost : i18n("e.g. me@my-mac.local")
            text: App.settings.macHost
            description: App.daemon.macHost.length > 0
                ? i18n("The ssh address of your Mac, to copy contacts (with photos) and pinned conversations from. Leave empty to use the daemon's setting, %1.", App.daemon.macHost)
                : i18n("The ssh address of your Mac, to copy contacts (with photos) and pinned conversations from. The Mac needs Remote Login and Full Disk Access for ssh. You can also set mac_host in the daemon's config.toml.")
            onEditingFinished: App.settings.macHost = text
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormButtonDelegate {
            objectName: "macSyncNowButton"
            icon.name: "view-refresh"
            text: App.macSync.running ? i18n("Syncing…") : i18n("Sync Now")
            enabled: !App.macSync.running
            description: App.macSync.lastSyncText.length > 0 ? i18n("Last synced %1", App.macSync.lastSyncText) : i18n("Not synced yet")
            onClicked: App.macSync.start()
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextFieldDelegate {
            objectName: "imsgPathField"
            label: i18n("imsg program (advanced)")
            placeholderText: i18n("imsg (found on PATH)")
            text: App.settings.imsgPath
            description: i18n("The command-line tool that does the sync. Only needed when imsg is not installed on your PATH, e.g. a development build. The KMESSAGES_IMSG environment variable overrides it.")
            onEditingFinished: App.settings.imsgPath = text
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
        FormCard.FormDelegateSeparator {}
        FormCard.FormSwitchDelegate {
            objectName: "emojiSuggestionsSwitch"
            text: i18n("Suggest emoji while typing")
            description: i18n("Typing a word such as “snake” offers 🐍; Tab or Enter puts it in place of the word.")
            checked: App.settings.emojiSuggestions
            onToggled: App.settings.emojiSuggestions = checked
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
