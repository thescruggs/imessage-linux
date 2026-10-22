// SPDX-License-Identifier: SSPL-1.0
//
// Owns the daemon client and the models, and does the app-level glue that
// QML should not: notifications (with inline reply), the tray icon, the
// task manager's unread badge, and marking the visible chat read while the
// window is active and the user is not away.
#pragma once

#include "appsettings.h"
#include "chatlistmodel.h"
#include "contactresolver.h"
#include "contactsuggestionmodel.h"
#include "daemonclient.h"
#include "handlelookupmodel.h"
#include "launcherentry.h"
#include "macsync.h"
#include "messagelistmodel.h"
#include "messagesearchmodel.h"
#include "recipientpickermodel.h"

#include <QDBusConnection>
#include <QHash>
#include <QObject>
#include <QPointer>
#include <QTimer>
#include <QUrl>
#include <QWindow>

class KNotification;
class KStatusNotifierItem;

namespace km
{

struct AppOptions {
    bool tray = true; ///< create the KStatusNotifierItem
    bool notifications = true; ///< actually send KNotifications (the logic runs regardless)
    bool kpeople = true; ///< load contacts through KPeople (if built with it)
    bool idleDetection = true; ///< watch for keyboard/mouse idle time (KIdleTime) to notice the user is away
    bool launcherBadge = true; ///< the unread count on the task manager entry (Unity LauncherEntry)
};

class AppController : public QObject
{
    Q_OBJECT
    Q_PROPERTY(km::DaemonClient *daemon READ daemon CONSTANT)
    Q_PROPERTY(km::ChatListModel *chatList READ chatList CONSTANT)
    Q_PROPERTY(km::ChatFilterModel *chats READ chats CONSTANT)
    Q_PROPERTY(km::ChatFilterModel *recipientSuggestions READ recipientSuggestions CONSTANT)
    Q_PROPERTY(km::MessageListModel *messages READ messages CONSTANT)
    Q_PROPERTY(km::HandleLookupModel *recipients READ recipients CONSTANT)
    Q_PROPERTY(km::MessageSearchModel *search READ search CONSTANT)
    Q_PROPERTY(km::AppSettings *settings READ settings CONSTANT)
    Q_PROPERTY(km::ContactResolver *contacts READ contacts CONSTANT)
    Q_PROPERTY(km::ContactSuggestionModel *contactSuggestions READ contactSuggestions CONSTANT)
    /// contactSuggestions, then recipientSuggestions, as one list (the new-message page).
    Q_PROPERTY(km::RecipientPickerModel *recipientPicker READ recipientPicker CONSTANT)
    /// "Sync from Mac" (`imsg sync-mac`).
    Q_PROPERTY(km::MacSync *macSync READ macSync CONSTANT)
    Q_PROPERTY(QString currentChatId READ currentChatId WRITE setCurrentChatId NOTIFY currentChatIdChanged)
    Q_PROPERTY(QString currentChatTitle READ currentChatTitle NOTIFY currentChatTitleChanged)
    /// File URL of the current chat's background image, "" = none.
    Q_PROPERTY(QString currentChatBackground READ currentChatBackground NOTIFY currentChatBackgroundChanged)
    Q_PROPERTY(bool trayAvailable READ trayAvailable CONSTANT)
    /// The tray asks for attention: unread messages, or the daemon needs the user.
    Q_PROPERTY(bool needsAttention READ needsAttention NOTIFY needsAttentionChanged)
    /// Set by the UI: the current chat's page is on screen (not just in the page stack).
    Q_PROPERTY(bool conversationVisible READ conversationVisible WRITE setConversationVisible NOTIFY conversationVisibleChanged)

public:
    AppController(const QDBusConnection &bus, const AppOptions &options, KSharedConfigPtr config = KSharedConfig::openConfig(), QObject *parent = nullptr);
    ~AppController() override;

    DaemonClient *daemon() const { return m_client; }
    ChatListModel *chatList() const { return m_chatList; }
    ChatFilterModel *chats() const { return m_chats; }
    ChatFilterModel *recipientSuggestions() const { return m_suggestions; }
    MessageListModel *messages() const { return m_messages; }
    HandleLookupModel *recipients() const { return m_recipients; }
    MessageSearchModel *search() const { return m_search; }
    AppSettings *settings() const { return m_settings; }
    ContactResolver *contacts() const { return m_contacts; }
    ContactSuggestionModel *contactSuggestions() const { return m_contactSuggestions; }
    RecipientPickerModel *recipientPicker() const { return m_recipientPicker; }
    MacSync *macSync() const { return m_macSync; }
    QString currentChatId() const { return m_currentChatId; }
    void setCurrentChatId(const QString &chatId);
    QString currentChatTitle() const;
    QString currentChatBackground() const;
    bool trayAvailable() const { return m_tray != nullptr; }
    bool conversationVisible() const { return m_conversationVisible; }
    void setConversationVisible(bool visible);
    /// No keyboard or mouse input for AppSettings::awayAfterSeconds: the open
    /// chat counts as unseen (notified, left unread) until input resumes.
    bool userAway() const { return m_userAway; }
    /// Set by the idle watch (and tests). Ignored while the away time is 0;
    /// coming back marks the visible chat read.
    void setUserAway(bool away);
    /// Null with AppOptions::launcherBadge off.
    LauncherEntry *launcherEntry() const { return m_launcher; }
    bool needsAttention() const;

    void setWindow(QWindow *window);
    QWindow *window() const { return m_window; }

    Q_INVOKABLE void showWindow();
    Q_INVOKABLE void toggleWindow();
    /// Show the window and navigate to `chatId` (QML handles openChatRequested).
    Q_INVOKABLE void openChat(const QString &chatId);
    Q_INVOKABLE void quit();
    /// Inline reply from a notification: SendText, then MarkRead on success
    /// (like iOS); a failure gets its own notification.
    void sendReply(const QString &chatId, const QString &text);
    /// Files to attach for a paste: the clipboard's local file URLs, or its
    /// image (without text) saved as a PNG under ~/.cache/kmessages/outgoing.
    /// Empty: paste as text.
    Q_INVOKABLE QList<QUrl> clipboardFiles() const;

Q_SIGNALS:
    void currentChatIdChanged();
    void currentChatTitleChanged();
    void currentChatBackgroundChanged();
    void conversationVisibleChanged();
    void userAwayChanged();
    void openChatRequested(const QString &chatId);
    void newMessageRequested();
    /// A notification is due for an incoming message (emitted even when sending is disabled).
    void notificationRequested(const QString &chatId, const QString &title, const QString &text);
    /// Passive message for the UI (failed send, background error …).
    void showPassiveNotification(const QString &message);
    void needsAttentionChanged();
    /// The daemon needs the user (`key`: the connection state, or "background" for a BackgroundError);
    /// emitted when the notification is due, even when sending notifications is disabled.
    void connectionProblemNotified(const QString &key, const QString &text);
    /// The UI should open the login dialog at `step` (0 = Apple ID, 1 = code).
    void loginRequested(int step);
    /// sendReply() failed.
    void replyFailed(const QString &chatId, const QString &error);

private:
    void onMessageReceived(const QString &chatId, const Message &msg);
    void scheduleMarkRead();
    void markReadNow();
    /// MarkRead for `chatId`, but never a read receipt for the user's own
    /// message from another device: with other own handles known, marks up to
    /// the newest unread message from someone else (none: no MarkRead) and
    /// hides the own messages the daemon still counts as unread.
    void markChatRead(const QString &chatId);
    void updateTray();
    void updateLauncherBadge();
    /// (Re)arms the KIdleTime timeout for AppSettings::awayAfterSeconds.
    void updateIdleWatch();
    void checkConnectionProblem();
    void onBackgroundError(const QString &message);
    /// `loginStep` ≥ 0: the Open action also opens the login dialog at that step.
    void notifyProblem(const QString &key, const QString &title, const QString &text, int loginStep);
    /// The window is shown and focused: the UI itself shows what is going on.
    bool windowInFront() const;
    bool chatVisible(const QString &chatId) const;

    AppOptions m_options;
    ContactResolver *m_contacts = nullptr; ///< created first: the models consult it
    ContactSuggestionModel *m_contactSuggestions = nullptr;
    RecipientPickerModel *m_recipientPicker = nullptr;
    MacSync *m_macSync = nullptr;
    DaemonClient *m_client = nullptr;
    ChatListModel *m_chatList = nullptr;
    ChatFilterModel *m_chats = nullptr;
    ChatFilterModel *m_suggestions = nullptr;
    MessageListModel *m_messages = nullptr;
    HandleLookupModel *m_recipients = nullptr;
    MessageSearchModel *m_search = nullptr;
    AppSettings *m_settings = nullptr;
    KStatusNotifierItem *m_tray = nullptr;
    QPointer<QWindow> m_window;
    QString m_currentChatId;
    bool m_conversationVisible = true;
    bool m_userAway = false;
    int m_idleTimeoutId = 0; ///< KIdleTime timeout, 0 = none
    int m_idleTimeoutMs = -1; ///< what m_idleTimeoutId was added for
    LauncherEntry *m_launcher = nullptr;
    QTimer m_markReadTimer;
    QHash<QString, QPointer<KNotification>> m_notifications;
    QTimer m_problemTimer; ///< lets a state settle (and its error detail arrive) before notifying
    QString m_problemState; ///< needs_login / needs_two_factor / error, kept until connected again
    QString m_backgroundProblem; ///< last BackgroundError notified, until the window is used or the state turns connected
    QPointer<KNotification> m_problemNotification;
    bool m_lastAttention = false;
};

} // namespace km
