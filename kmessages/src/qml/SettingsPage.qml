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
        title: i18n("iCloud Contacts")
        visible: contactsCard.visible
    }
    FormCard.FormCard {
        id: contactsCard
        objectName: "contactsSyncCard"
        // Older daemons cannot sync contacts.
        visible: App.daemon.contactsSyncAvailable
        readonly property bool signedIn: App.daemon.contactsAccount.length > 0
        readonly property bool busy: App.daemon.contactsBusy || App.daemon.contactsSyncing
        // The registered email address: usually the Apple ID.
        readonly property string suggestedAppleId: {
            const mail = App.daemon.ownHandles.find(h => h.startsWith("mailto:"));
            return mail ? mail.substring(7) : "";
        }
        // A refused sign-in, shown under the password field.
        property string signInError: ""

        function signIn() {
            const appleId = appleIdField.text.trim().length > 0 ? appleIdField.text.trim() : suggestedAppleId;
            if (appleId.length === 0 || passwordField.text.length === 0 || busy) {
                return;
            }
            signInError = "";
            App.daemon.setContactsAccount(appleId, passwordField.text);
            passwordField.text = "";
        }

        // ---- signed in ----
        FormCard.FormTextDelegate {
            objectName: "contactsAccount"
            visible: contactsCard.signedIn
            text: i18n("Apple ID")
            description: App.daemon.contactsAccount
        }
        FormCard.FormDelegateSeparator {
            visible: contactsCard.signedIn
        }
        FormCard.FormTextDelegate {
            objectName: "contactsLastSync"
            visible: contactsCard.signedIn
            text: i18n("Last synced")
            description: {
                if (App.daemon.contactsSyncing) {
                    return i18n("Syncing…");
                }
                if (App.daemon.contactsLastSyncText.length === 0) {
                    return i18n("Not synced yet");
                }
                return App.daemon.contactsSummary.length > 0
                    ? i18nc("%1 = date, %2 = '312 contacts (3 updated)'", "%1: %2", App.daemon.contactsLastSyncText, App.daemon.contactsSummary)
                    : App.daemon.contactsLastSyncText;
            }
        }
        FormCard.FormDelegateSeparator {
            visible: contactsError.visible
        }
        FormCard.FormTextDelegate {
            id: contactsError
            objectName: "contactsLastError"
            visible: contactsCard.signedIn && (App.daemon.contactsAuthFailed || App.daemon.contactsLastError.length > 0)
            text: App.daemon.contactsAuthFailed ? i18n("Password refused") : i18n("Last sync failed")
            description: App.daemon.contactsAuthFailed
                ? i18n("Apple refused the app-specific password (it may have been revoked). Contacts are not synced until you sign in again with a new one.")
                : App.daemon.contactsLastError
            textItem.color: Kirigami.Theme.negativeTextColor
        }
        FormCard.FormDelegateSeparator {
            visible: contactsCard.signedIn
        }
        FormCard.FormButtonDelegate {
            objectName: "contactsSyncNowButton"
            visible: contactsCard.signedIn && !App.daemon.contactsAuthFailed
            enabled: !contactsCard.busy
            icon.name: "view-refresh"
            text: contactsCard.busy ? i18n("Syncing…") : i18n("Sync Now")
            description: App.daemon.contactsSyncInterval > 0
                ? i18np("Contacts also sync automatically every minute.", "Contacts also sync automatically every %1 minutes.", App.daemon.contactsSyncInterval)
                : ""
            onClicked: App.daemon.syncContacts()
        }
        // A new app-specific password: required after a refused one, offered
        // any time (the old one was revoked, or a new one is wanted).
        FormCard.FormButtonDelegate {
            objectName: "contactsSignInAgainButton"
            visible: contactsCard.signedIn
            enabled: !contactsCard.busy
            icon.name: App.daemon.contactsAuthFailed ? "im-user" : "dialog-password"
            text: App.daemon.contactsAuthFailed ? i18n("Sign In Again…") : i18n("Change App-Specific Password…")
            description: App.daemon.contactsAuthFailed ? "" : i18n("Enter a new app-specific password for %1.", App.daemon.contactsAccount)
            onClicked: {
                appleIdField.text = App.daemon.contactsAccount;
                contactsCard.signInError = "";
                reSignIn.open();
            }
        }
        FormCard.FormButtonDelegate {
            objectName: "contactsSignOutButton"
            visible: contactsCard.signedIn
            enabled: !App.daemon.contactsBusy
            icon.name: "system-log-out"
            text: i18n("Sign Out")
            description: i18n("Stops syncing contacts from this Apple ID.")
            onClicked: signOutPrompt.open()
        }

        // ---- signed out ----
        FormCard.FormTextFieldDelegate {
            id: appleIdField
            objectName: "contactsAppleIdField"
            visible: !contactsCard.signedIn
            label: i18n("Apple ID")
            placeholderText: contactsCard.suggestedAppleId.length > 0 ? contactsCard.suggestedAppleId : i18n("e.g. me@icloud.com")
            inputMethodHints: Qt.ImhEmailCharactersOnly | Qt.ImhNoAutoUppercase
            enabled: !contactsCard.busy
            onAccepted: passwordField.forceActiveFocus()
        }
        FormCard.FormDelegateSeparator {
            visible: !contactsCard.signedIn
        }
        FormCard.FormPasswordFieldDelegate {
            id: passwordField
            objectName: "contactsPasswordField"
            visible: !contactsCard.signedIn
            label: i18n("App-specific password")
            placeholderText: i18nc("the format of an app-specific password", "xxxx-xxxx-xxxx-xxxx")
            enabled: !contactsCard.busy
            statusMessage: contactsCard.signInError
            status: Kirigami.MessageType.Error
            onAccepted: contactsCard.signIn()
        }
        FormCard.FormDelegateSeparator {
            visible: !contactsCard.signedIn
        }
        FormCard.FormTextDelegate {
            objectName: "contactsHelp"
            visible: !contactsCard.signedIn
            text: i18n("Names and photos of your iCloud contacts")
            description: i18n("Create an app-specific password at appleid.apple.com → Sign-In and Security → App-Specific Passwords. Your normal Apple ID password will not work. Contacts are then kept up to date automatically.")
        }
        FormCard.FormDelegateSeparator {
            visible: !contactsCard.signedIn
        }
        FormCard.FormButtonDelegate {
            objectName: "contactsSignInButton"
            visible: !contactsCard.signedIn
            enabled: !contactsCard.busy && passwordField.text.length > 0
                && (appleIdField.text.trim().length > 0 || contactsCard.suggestedAppleId.length > 0)
            icon.name: "im-user"
            text: App.daemon.contactsBusy ? i18n("Signing In…") : i18n("Sign In")
            description: App.daemon.contactsBusy ? i18n("Checking the password and fetching your contacts. This can take a while.") : ""
            onClicked: contactsCard.signIn()
        }

        Connections {
            target: App.daemon
            function onContactsSyncFinished(operation: string, ok: bool, errorName: string, message: string) {
                if (operation === "signIn") {
                    contactsCard.signInError = ok ? "" : message;
                    if (ok) {
                        appleIdField.text = "";
                        reSignIn.close();
                    }
                }
            }
        }

        // A new app-specific password for the same account (the old one was refused, or is being replaced).
        Kirigami.PromptDialog {
            id: reSignIn
            objectName: "contactsReSignInDialog"
            parent: QQC2.Overlay.overlay
            title: App.daemon.contactsAuthFailed ? i18n("Sign In to iCloud Contacts") : i18n("Change the App-Specific Password")
            subtitle: i18n("Create a new app-specific password at appleid.apple.com → Sign-In and Security → App-Specific Passwords. Your normal Apple ID password will not work. The password is checked with iCloud before it replaces the stored one.")
            standardButtons: Kirigami.Dialog.Cancel
            customFooterActions: [
                Kirigami.Action {
                    objectName: "contactsReSignInAction"
                    icon.name: "im-user"
                    text: i18n("Sign In")
                    enabled: reSignInPassword.text.length > 0 && !App.daemon.contactsBusy
                    onTriggered: {
                        contactsCard.signInError = "";
                        App.daemon.setContactsAccount(App.daemon.contactsAccount, reSignInPassword.text);
                        reSignInPassword.text = "";
                    }
                }
            ]
            onOpened: reSignInPassword.forceActiveFocus()
            onClosed: reSignInPassword.text = ""

            Kirigami.PasswordField {
                id: reSignInPassword
                objectName: "contactsReSignInPassword"
                placeholderText: i18n("App-specific password for %1", App.daemon.contactsAccount)
                enabled: !App.daemon.contactsBusy
            }
        }

        Kirigami.PromptDialog {
            id: signOutPrompt
            objectName: "contactsSignOutDialog"
            parent: QQC2.Overlay.overlay
            title: i18n("Sign Out of iCloud Contacts?")
            subtitle: i18n("Contacts from %1 will no longer be synced. Keep the contacts synced so far on this computer, or remove them?", App.daemon.contactsAccount)
            standardButtons: Kirigami.Dialog.Cancel
            customFooterActions: [
                Kirigami.Action {
                    objectName: "contactsSignOutRemove"
                    icon.name: "edit-delete"
                    text: i18n("Remove Contacts")
                    onTriggered: {
                        signOutPrompt.close();
                        App.daemon.clearContactsAccount(true);
                    }
                },
                Kirigami.Action {
                    objectName: "contactsSignOutKeep"
                    icon.name: "system-log-out"
                    text: i18n("Keep Contacts")
                    onTriggered: {
                        signOutPrompt.close();
                        App.daemon.clearContactsAccount(false);
                    }
                }
            ]
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
                ? i18n("The ssh address of your Mac, to copy pinned conversations from. Leave empty to use the daemon's setting, %1.", App.daemon.macHost)
                : i18n("The ssh address of your Mac, to copy pinned conversations from. The Mac needs Remote Login and Full Disk Access for ssh. You can also set mac_host in the daemon's config.toml.")
            onEditingFinished: App.settings.macHost = text
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormButtonDelegate {
            objectName: "macSyncNowButton"
            icon.name: "view-refresh"
            text: App.macSync.running ? i18n("Syncing…") : i18n("Sync Pins Now")
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
        FormCard.FormSpinBoxDelegate {
            objectName: "awayAfterSpinBox"
            label: i18n("Treat me as away after")
            description: i18n("Without keyboard or mouse input for this long, the open conversation is not marked read: new messages in it are notified and stay unread until you are back. “Never” marks them read whenever the window is in front.")
            from: 0
            to: 3600
            stepSize: 15
            value: App.settings.awayAfterSeconds
            textFromValue: (value, locale) => value === 0 ? i18nc("away after: never", "Never") : i18np("%1 second", "%1 seconds", value)
            valueFromText: (text, locale) => {
                const n = parseInt(text);
                return isNaN(n) ? 0 : n;
            }
            onValueModified: App.settings.awayAfterSeconds = value
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormSwitchDelegate {
            objectName: "unreadBadgeSwitch"
            text: i18n("Unread count on the task manager")
            description: i18n("Show the number of unread messages on the Messages entry in the task manager.")
            checked: App.settings.unreadBadge
            onToggled: App.settings.unreadBadge = checked
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
            description: i18n("Typing a word such as “snake” offers 🐍; Tab puts it in place of the word (Enter still sends, unless you picked one with the arrow keys or typed “:snake:”).")
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
