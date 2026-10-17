// SPDX-License-Identifier: SSPL-1.0
import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.imessage.kmessages

// Apple ID + password, then (if Apple asks) the 2FA code.
Kirigami.Dialog {
    id: dialog

    property int step: 0
    property bool busy: false
    property string error

    title: step === 0 ? i18n("Sign in to iMessage") : i18n("Two-Factor Authentication")
    preferredWidth: Kirigami.Units.gridUnit * 24
    padding: Kirigami.Units.largeSpacing
    standardButtons: Kirigami.Dialog.NoButton

    function openStep(s: int) {
        step = s;
        error = "";
        busy = false;
        codeField.text = "";
        open();
        if (s === 0) {
            (appleIdField.text.length > 0 ? passwordField : appleIdField).forceActiveFocus();
        } else {
            codeField.forceActiveFocus();
        }
    }

    function describe(errorName: string, errorMessage: string): string {
        switch (errorName) {
        case "NoHardwareConfig":
            return i18n("No Mac hardware identity has been imported yet. Run “imsg hwconfig import” first.");
        case "BadTwoFactorCode":
            return i18n("That code is not correct. Try again.");
        case "Transient":
            return i18n("Apple's servers could not be reached. Try again later. (%1)", errorMessage);
        case "NoPendingTwoFactor":
            return i18n("The sign-in attempt expired. Start again.");
        case "org.freedesktop.DBus.Error.NoReply":
        case "org.freedesktop.DBus.Error.Timeout":
            return i18n("imessaged did not answer in time. It may still be starting; try again.");
        case "org.freedesktop.DBus.Error.ServiceUnknown":
            return i18n("imessaged is not running.");
        }
        return errorMessage.length > 0 ? errorMessage : errorName;
    }

    function canSubmit(): bool {
        if (busy) {
            return false;
        }
        return step === 0 ? appleIdField.text.trim().length > 0 && passwordField.text.length > 0 : codeField.text.trim().length >= 6;
    }

    function submit() {
        if (!canSubmit()) {
            return;
        }
        busy = true;
        error = "";
        if (step === 0) {
            App.daemon.login(appleIdField.text.trim(), passwordField.text);
        } else {
            App.daemon.submitTwoFactor(codeField.text.trim());
        }
    }

    customFooterActions: [
        Kirigami.Action {
            text: dialog.step === 0 ? i18n("Sign In") : i18n("Verify")
            icon.name: "go-next"
            enabled: dialog.canSubmit()
            onTriggered: dialog.submit()
        },
        Kirigami.Action {
            text: i18n("Cancel")
            icon.name: "dialog-cancel"
            onTriggered: dialog.close()
        }
    ]

    ColumnLayout {
        spacing: Kirigami.Units.largeSpacing

        Kirigami.InlineMessage {
            Layout.fillWidth: true
            visible: dialog.error.length > 0
            type: Kirigami.MessageType.Error
            text: dialog.error
        }

        QQC2.Label {
            Layout.fillWidth: true
            wrapMode: Text.Wrap
            text: dialog.step === 0 ? i18n("Use the Apple ID you use for iMessage. Your password is handed to the local imessaged service, which signs in to Apple.") : i18n("Enter the six-digit verification code shown on your other Apple devices or sent by SMS.")
        }

        Kirigami.FormLayout {
            Layout.fillWidth: true
            visible: dialog.step === 0

            QQC2.TextField {
                id: appleIdField
                Kirigami.FormData.label: i18n("Apple ID:")
                placeholderText: i18n("name@example.com")
                inputMethodHints: Qt.ImhEmailCharactersOnly | Qt.ImhNoAutoUppercase
                enabled: !dialog.busy
                onAccepted: passwordField.forceActiveFocus()
            }
            Kirigami.PasswordField {
                id: passwordField
                Kirigami.FormData.label: i18n("Password:")
                enabled: !dialog.busy
                onAccepted: dialog.submit()
            }
        }

        Kirigami.FormLayout {
            Layout.fillWidth: true
            visible: dialog.step === 1

            QQC2.TextField {
                id: codeField
                Kirigami.FormData.label: i18n("Code:")
                placeholderText: "000000"
                maximumLength: 8
                inputMethodHints: Qt.ImhDigitsOnly
                enabled: !dialog.busy
                onAccepted: dialog.submit()
            }
        }

        RowLayout {
            visible: dialog.busy
            Layout.alignment: Qt.AlignHCenter
            QQC2.BusyIndicator {
                running: dialog.busy
            }
            QQC2.Label {
                text: dialog.step === 0 ? i18n("Signing in…") : i18n("Verifying…")
            }
        }
    }

    Connections {
        target: App.daemon
        function onLoginFinished(result: string, errorName: string, errorMessage: string) {
            dialog.busy = false;
            if (result === "ok") {
                passwordField.text = "";
                dialog.close();
            } else if (result === "two_factor_required") {
                passwordField.text = "";
                dialog.step = 1;
                codeField.forceActiveFocus();
            } else {
                dialog.error = dialog.describe(errorName, errorMessage);
            }
        }
        function onTwoFactorFinished(result: string, errorName: string, errorMessage: string) {
            dialog.busy = false;
            if (result === "ok") {
                dialog.close();
            } else if (result === "two_factor_required") {
                codeField.text = "";
                dialog.error = i18n("Apple accepted the code but wants another one.");
            } else {
                dialog.error = dialog.describe(errorName, errorMessage);
                codeField.selectAll();
            }
        }
    }
}
