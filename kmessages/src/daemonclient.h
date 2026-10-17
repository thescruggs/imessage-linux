// SPDX-License-Identifier: SSPL-1.0
//
// Asynchronous client for the imessaged D-Bus API (org.imessage.Daemon1,
// docs/DBUS.md). Every call is non-blocking (QDBusPendingCallWatcher);
// results come back through callbacks (C++ models) or signals (QML).
// The service name is watched: when the daemon (re)appears the client
// re-reads its status and emits resyncRequested() so models reload.
#pragma once

#include "types.h"

#include <QDBusConnection>
#include <QDBusMessage>
#include <QObject>
#include <QSet>
#include <QUrl>

#include <functional>

class QDBusServiceWatcher;

namespace km
{

class DaemonClient : public QObject
{
    Q_OBJECT
    /// The daemon owns its bus name.
    Q_PROPERTY(bool available READ isAvailable NOTIFY availableChanged)
    /// `ConnectionState` property (`connected`, `needs_login`, …); `unavailable` while the daemon is not on the bus.
    Q_PROPERTY(QString connectionState READ connectionState NOTIFY connectionStateChanged)
    Q_PROPERTY(QString connectionDetail READ connectionDetail NOTIFY statusChanged)
    /// Shared names and photos: `ready`, `pending`, `needs_login`, `unavailable` ("" = older daemon).
    Q_PROPERTY(QString profilesState READ profilesState NOTIFY statusChanged)
    Q_PROPERTY(QString profilesDetail READ profilesDetail NOTIFY statusChanged)
    Q_PROPERTY(bool registered READ isRegistered NOTIFY statusChanged)
    Q_PROPERTY(QString version READ version NOTIFY statusChanged)
    Q_PROPERTY(QStringList ownHandles READ ownHandles NOTIFY statusChanged)
    /// The handle messages are sent from (`registration.sender`), else the first own handle, else "".
    Q_PROPERTY(QString ownSender READ ownSender NOTIFY statusChanged)
    /// The user's handles that are *not* registered here (e.g. their phone
    /// number, used by their iPhone): messages from them are the user's own
    /// (no notification, no unread). From the "My other addresses" setting.
    Q_PROPERTY(QStringList otherOwnHandles READ otherOwnHandles NOTIFY otherOwnHandlesChanged)
    /// The daemon's own list of the user's other handles (`registration.other_handles`, read-only here).
    /// Messages from them already arrive with `is_from_me`; they also count for isOtherOwnHandle().
    Q_PROPERTY(QStringList daemonOtherHandles READ daemonOtherHandles NOTIFY statusChanged)
    /// A GetStatus reply has arrived since the daemon (re)appeared, so ownHandles is meaningful.
    Q_PROPERTY(bool statusKnown READ isStatusKnown NOTIFY statusChanged)
    /// Last error not tied to a QML-visible call (activation failure, BackgroundError).
    Q_PROPERTY(QString lastError READ lastError NOTIFY lastErrorChanged)

public:
    static const QString Service;
    static const QString Path;
    static const QString Interface;

    using ErrorFn = std::function<void(const QString &name, const QString &message)>;

    explicit DaemonClient(const QDBusConnection &bus = QDBusConnection::sessionBus(), QObject *parent = nullptr);
    ~DaemonClient() override;

    bool isAvailable() const { return m_available; }
    QString connectionState() const { return m_available ? m_status.connectionState : QStringLiteral("unavailable"); }
    QString connectionDetail() const { return m_status.connectionDetail; }
    QString profilesState() const { return m_status.profilesState; }
    QString profilesDetail() const { return m_status.profilesDetail; }
    bool isRegistered() const { return m_status.registered; }
    QString version() const { return m_status.version; }
    QStringList ownHandles() const { return m_status.handles; }
    QString ownSender() const { return !m_status.sender.isEmpty() ? m_status.sender : m_status.handles.value(0); }
    QStringList otherOwnHandles() const { return m_otherOwnHandles; }
    QStringList daemonOtherHandles() const { return m_status.otherHandles; }
    /// IDS URIs (normalized by the caller).
    void setOtherOwnHandles(const QStringList &uris);
    /// `uri` is one of otherOwnHandles() or daemonOtherHandles(). Phone numbers
    /// compare by all digits when both carry a country code (`tel:+…`), by
    /// their last ten digits when one is a bare national number.
    bool isOtherOwnHandle(const QString &uri) const;
    bool hasOtherOwnHandles() const { return !m_mine.isEmpty(); }
    /// The phone-number / handle matching rule of isOtherOwnHandle().
    static bool sameHandle(const QString &a, const QString &b);
    /// Sent by the user: from here (`is_from_me`) or from one of their other handles.
    bool isFromMyself(const Message &m) const { return m.isFromMe || isOtherOwnHandle(m.sender.uri); }
    bool isStatusKnown() const { return m_statusKnown; }
    QString lastError() const { return m_lastError; }
    const Status &status() const { return m_status; }
    /// Whether `uri` is one of our own handles.
    bool isOwnHandle(const QString &uri) const;

    // ---- typed asynchronous API (C++) ----------------------------------
    void listChats(uint limit, uint offset, std::function<void(const QList<Chat> &)> ok, ErrorFn err = {});
    void getChat(const QString &chatId, std::function<void(const Chat &)> ok, ErrorFn err = {});
    void listMessages(const QString &chatId,
                      const QString &beforeCursor,
                      uint limit,
                      std::function<void(const MessagePage &)> ok,
                      ErrorFn err = {});
    void search(const QString &query, uint limit, std::function<void(const QList<Message> &)> ok, ErrorFn err = {});
    void lookupHandles(const QStringList &handles, std::function<void(const QList<HandleReachability> &)> ok, ErrorFn err = {});
    void downloadAttachment(const QString &attachmentId, std::function<void(const QString &path)> ok, ErrorFn err = {});
    void sendTextTo(const QString &chatId,
                    const QStringList &participants,
                    const QString &text,
                    const QString &replyTo,
                    std::function<void(const SentMessage &)> ok,
                    ErrorFn err = {});
    void sendFileTo(const QString &chatId,
                    const QStringList &participants,
                    const QString &path,
                    const QString &mime,
                    std::function<void(const SentMessage &)> ok,
                    ErrorFn err = {});
    void getStatus(std::function<void(const Status &)> ok, ErrorFn err = {});
    /// sendText() that also reports the outcome to `done` (ok, error message).
    void sendTextThen(const QString &chatId,
                      const QStringList &participants,
                      const QString &text,
                      const QString &replyTo,
                      std::function<void(bool ok, const QString &error)> done);
    /// sendFile() that also reports the outcome to `done` (ok, the sent message, error message).
    void sendFileThen(const QString &chatId,
                      const QStringList &participants,
                      const QUrl &file,
                      const QString &mime,
                      std::function<void(bool ok, const SentMessage &sent, const QString &error)> done);
    /// edit() that also reports the outcome to `done`.
    void editThen(const QString &chatId, const QString &messageId, const QString &text, std::function<void(bool ok, const QString &error)> done);
    /// refreshStatus(), then `done` once the new status is applied (not called on failure).
    void refreshStatusThen(std::function<void()> done);

    // ---- QML API: results come back as signals -----------------------------
    Q_INVOKABLE void login(const QString &appleId, const QString &password);
    Q_INVOKABLE void submitTwoFactor(const QString &code);
    Q_INVOKABLE void logout();
    /// Send to `chatId`, or (chatId empty) to `participants` (creates/uses a chat). Emits sendFinished.
    Q_INVOKABLE void sendText(const QString &chatId, const QStringList &participants, const QString &text, const QString &replyTo = QString());
    /// `file` may be a local path or file:// URL; mime is guessed when empty. Emits sendFinished.
    /// Files the sandboxed daemon cannot read (outside $HOME, /tmp, /run …)
    /// are sent from a copy under ~/.cache/kmessages/outgoing (outgoingfiles.h).
    Q_INVOKABLE void sendFile(const QString &chatId, const QStringList &participants, const QUrl &file, const QString &mime = QString());
    Q_INVOKABLE void react(const QString &chatId, const QString &messageId, const QString &kind, bool remove);
    Q_INVOKABLE void edit(const QString &chatId, const QString &messageId, const QString &text);
    Q_INVOKABLE void unsend(const QString &chatId, const QString &messageId);
    Q_INVOKABLE void setTyping(const QString &chatId, bool typing);
    Q_INVOKABLE void markRead(const QString &chatId, const QString &upToMessageId = QString());
    Q_INVOKABLE void refreshStatus();
    /// Ask the bus to start the daemon (D-Bus activation) if it is not running.
    Q_INVOKABLE void ensureStarted();

    /// Strip the `org.imessage.Daemon1.Error.` prefix.
    static QString shortErrorName(const QString &dbusName);
    /// The call got no reply in time (the daemon may still be working on it).
    static bool isTimeout(const QString &errorName);

Q_SIGNALS:
    void availableChanged();
    void connectionStateChanged();
    void statusChanged();
    void lastErrorChanged();
    void otherOwnHandlesChanged();

    void messageReceived(const QString &chatId, const km::Message &message);
    void messageUpdated(const QString &chatId, const km::Message &message);
    void chatUpdated(const km::Chat &chat);
    void typingChanged(const QString &chatId, const QString &handle, bool typing);
    void twoFactorRequired();
    void backgroundError(const QString &message);
    /// A contact's shared name / photo changed (already applied to the ContactResolver).
    void handleProfileUpdated(const QString &handle);
    /// The daemon (re)appeared or reported dropped events: reload everything.
    void resyncRequested();

    /// result: "ok" / "two_factor_required"; on failure result is empty and errorName/errorMessage are set.
    void loginFinished(const QString &result, const QString &errorName, const QString &errorMessage);
    void twoFactorFinished(const QString &result, const QString &errorName, const QString &errorMessage);
    /// chatId is the chat the message went to (new chats included).
    void sendFinished(bool ok, const QString &chatId, const QString &messageId, const QString &errorMessage);
    /// A fire-and-forget call (react, edit, unsend, markRead, …) failed.
    void operationFailed(const QString &operation, const QString &errorName, const QString &errorMessage);

private Q_SLOTS:
    void onMessageReceived(const QString &chatId, const QString &json);
    void onMessageUpdated(const QString &chatId, const QString &json);
    void onChatUpdated(const QString &json);
    void onTyping(const QString &chatId, const QString &handle, bool typing);
    void onConnectionStateChanged(const QString &state);
    void onPropertiesChanged(const QString &interface, const QVariantMap &changed, const QStringList &invalidated);
    void onTwoFactorRequired();
    void onBackgroundError(const QString &message);
    void onHandleProfileUpdated(const QString &handle, const QString &json);

private:
    void call(const QString &method,
              const QVariantList &args,
              std::function<void(const QDBusMessage &)> ok,
              ErrorFn err,
              int timeoutMs = -1);
    /// Fire-and-forget call; failures emit operationFailed.
    void callVoid(const QString &method, const QVariantList &args, int timeoutMs = -1);
    void setAvailable(bool available);
    void setLastError(const QString &error);
    void setStatus(const Status &status);
    static QString firstString(const QDBusMessage &reply);
    static QString participantsJson(const QStringList &participants);

    QDBusConnection m_bus;
    QDBusServiceWatcher *m_watcher = nullptr;
    bool m_available = false;
    bool m_statusKnown = false;
    Status m_status;
    QString m_lastError;
    QStringList m_otherOwnHandles;
    QStringList m_mine; ///< m_otherOwnHandles + m_status.otherHandles
    void rebuildMine();
};

} // namespace km
