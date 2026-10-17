// SPDX-License-Identifier: SSPL-1.0
#include "daemonclient.h"
#include "contactresolver.h"
#include "outgoingfiles.h"

#include <KLocalizedString>

#include <QDBusConnectionInterface>
#include <QDBusPendingCallWatcher>
#include <QDBusPendingReply>
#include <QDBusServiceWatcher>
#include <QFileInfo>
#include <QJsonArray>
#include <QJsonDocument>
#include <QLoggingCategory>
#include <QMimeDatabase>
#include <QTimer>

using namespace Qt::StringLiterals;

Q_LOGGING_CATEGORY(KM_DBUS, "kmessages.dbus", QtInfoMsg)

namespace km
{

const QString DaemonClient::Service = u"org.imessage.Daemon1"_s;
const QString DaemonClient::Path = u"/org/imessage/Daemon1"_s;
const QString DaemonClient::Interface = u"org.imessage.Daemon1"_s;

namespace
{
/// Chat payloads carry the participants' shared names and photos: hand
/// them to the contact resolver, which names and pictures everyone.
void noteSharedProfiles(const Chat &c)
{
    if (auto *contacts = ContactResolver::instance()) {
        contacts->setSharedProfiles(c.participants);
    }
}

constexpr int LongTimeoutMs = 120 * 1000; // login, 2FA, file transfer, activation
constexpr int MediumTimeoutMs = 60 * 1000; // React / Edit / Unsend / MarkRead: an IDS round trip
const QString ErrorPrefix = u"org.imessage.Daemon1.Error."_s;
}

DaemonClient::DaemonClient(const QDBusConnection &bus, QObject *parent)
    : QObject(parent)
    , m_bus(bus)
{
    qRegisterMetaType<km::Chat>();
    qRegisterMetaType<km::Message>();
    m_status.connectionState = u"disconnected"_s;

    const struct {
        const char *name;
        const char *slot;
    } signals_[] = {
        {"MessageReceived", SLOT(onMessageReceived(QString, QString))},
        {"MessageUpdated", SLOT(onMessageUpdated(QString, QString))},
        {"ChatUpdated", SLOT(onChatUpdated(QString))},
        {"Typing", SLOT(onTyping(QString, QString, bool))},
        {"ConnectionStateChanged", SLOT(onConnectionStateChanged(QString))},
        {"TwoFactorRequired", SLOT(onTwoFactorRequired())},
        {"BackgroundError", SLOT(onBackgroundError(QString))},
        {"HandleProfileUpdated", SLOT(onHandleProfileUpdated(QString, QString))},
        {"SmsForwardingChanged", SLOT(onSmsForwardingChanged(QString))},
    };
    for (const auto &s : signals_) {
        if (!m_bus.connect(Service, Path, Interface, QString::fromLatin1(s.name), this, s.slot)) {
            qCWarning(KM_DBUS) << "cannot subscribe to" << s.name << m_bus.lastError().message();
        }
    }

    // Registered (and ConnectionState) also arrive as PropertiesChanged.
    m_bus.connect(Service,
                  Path,
                  u"org.freedesktop.DBus.Properties"_s,
                  u"PropertiesChanged"_s,
                  this,
                  SLOT(onPropertiesChanged(QString, QVariantMap, QStringList)));

    m_watcher = new QDBusServiceWatcher(Service, m_bus, QDBusServiceWatcher::WatchForOwnerChange, this);
    connect(m_watcher, &QDBusServiceWatcher::serviceOwnerChanged, this, [this](const QString &, const QString &, const QString &newOwner) {
        setAvailable(!newOwner.isEmpty());
    });

    QTimer::singleShot(0, this, &DaemonClient::ensureStarted);
}

DaemonClient::~DaemonClient() = default;

bool DaemonClient::isOwnHandle(const QString &uri) const
{
    return m_status.handles.contains(uri);
}

namespace
{
QString digitsOf(const QString &s)
{
    QString digits;
    for (const QChar c : s) {
        if (c.isDigit()) {
            digits += c;
        }
    }
    return digits;
}
}

bool DaemonClient::sameHandle(const QString &a, const QString &b)
{
    const bool telA = a.startsWith("tel:"_L1);
    const bool telB = b.startsWith("tel:"_L1);
    if (!telA || !telB) {
        return !telA && !telB && a.compare(b, Qt::CaseInsensitive) == 0;
    }
    const QString da = digitsOf(a);
    const QString db = digitsOf(b);
    if (da.isEmpty() || db.isEmpty()) {
        return false;
    }
    if (da == db) {
        return true;
    }
    // +44 7911 123456 is not +1 791-112-3456: with both country codes known,
    // all digits must agree. A bare national number matches by its last ten.
    if (a.startsWith("tel:+"_L1) && b.startsWith("tel:+"_L1)) {
        return false;
    }
    return da.size() >= 10 && db.size() >= 10 && da.right(10) == db.right(10);
}

void DaemonClient::setOtherOwnHandles(const QStringList &uris)
{
    if (uris == m_otherOwnHandles) {
        return;
    }
    m_otherOwnHandles = uris;
    rebuildMine();
    Q_EMIT otherOwnHandlesChanged();
}

void DaemonClient::rebuildMine()
{
    m_mine = m_otherOwnHandles;
    for (const auto &u : std::as_const(m_status.otherHandles)) {
        if (!m_mine.contains(u)) {
            m_mine << u;
        }
    }
}

bool DaemonClient::isOtherOwnHandle(const QString &uri) const
{
    for (const auto &m : m_mine) {
        if (sameHandle(m, uri)) {
            return true;
        }
    }
    return false;
}

bool DaemonClient::isTimeout(const QString &errorName)
{
    return errorName == "org.freedesktop.DBus.Error.NoReply"_L1 || errorName == "org.freedesktop.DBus.Error.Timeout"_L1
        || errorName == "org.freedesktop.DBus.Error.TimedOut"_L1;
}

QString DaemonClient::shortErrorName(const QString &dbusName)
{
    return dbusName.startsWith(ErrorPrefix) ? dbusName.mid(ErrorPrefix.size()) : dbusName;
}

void DaemonClient::call(const QString &method,
                        const QVariantList &args,
                        std::function<void(const QDBusMessage &)> ok,
                        ErrorFn err,
                        int timeoutMs)
{
    auto msg = QDBusMessage::createMethodCall(Service, Path, Interface, method);
    msg.setArguments(args);
    auto *watcher = new QDBusPendingCallWatcher(m_bus.asyncCall(msg, timeoutMs), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [method, ok = std::move(ok), err = std::move(err)](QDBusPendingCallWatcher *w) {
        w->deleteLater();
        const QDBusMessage reply = w->reply();
        if (reply.type() == QDBusMessage::ErrorMessage) {
            const QString name = shortErrorName(reply.errorName());
            qCDebug(KM_DBUS) << method << "failed:" << name << reply.errorMessage();
            if (err) {
                err(name, reply.errorMessage());
            }
            return;
        }
        if (ok) {
            ok(reply);
        }
    });
}

void DaemonClient::callVoid(const QString &method, const QVariantList &args, int timeoutMs)
{
    call(method, args, {}, [this, method](const QString &name, const QString &message) {
        Q_EMIT operationFailed(method, name, message);
    }, timeoutMs);
}

QString DaemonClient::firstString(const QDBusMessage &reply)
{
    const auto args = reply.arguments();
    return args.isEmpty() ? QString() : args.first().toString();
}

QString DaemonClient::participantsJson(const QStringList &participants)
{
    return QString::fromUtf8(QJsonDocument(QJsonArray::fromStringList(participants)).toJson(QJsonDocument::Compact));
}

// A reply whose JSON does not parse is reported through `err` as "Parse".
#define KM_PARSE_OR_FAIL(parser, var, reply, err)                                                                                                              \
    QString parseError_;                                                                                                                                       \
    auto var = parser(firstString(reply), &parseError_);                                                                                                       \
    if (!var) {                                                                                                                                                \
        qCWarning(KM_DBUS) << #parser << "failed:" << parseError_;                                                                                            \
        if (err) {                                                                                                                                             \
            err(u"Parse"_s, parseError_);                                                                                                                      \
        }                                                                                                                                                      \
        return;                                                                                                                                                \
    }

void DaemonClient::listChats(uint limit, uint offset, std::function<void(const QList<Chat> &)> ok, ErrorFn err)
{
    call(u"ListChats"_s, {limit, offset}, [ok, err](const QDBusMessage &r) {
        KM_PARSE_OR_FAIL(Json::parseChatList, v, r, err)
        for (const auto &c : std::as_const(*v)) {
            noteSharedProfiles(c);
        }
        ok(*v);
    }, err);
}

void DaemonClient::getChat(const QString &chatId, std::function<void(const Chat &)> ok, ErrorFn err)
{
    call(u"GetChat"_s, {chatId}, [ok, err](const QDBusMessage &r) {
        KM_PARSE_OR_FAIL(Json::parseChat, v, r, err)
        noteSharedProfiles(*v);
        ok(*v);
    }, err);
}

void DaemonClient::listMessages(const QString &chatId, const QString &beforeCursor, uint limit, std::function<void(const MessagePage &)> ok, ErrorFn err)
{
    call(u"ListMessages"_s, {chatId, beforeCursor, limit}, [ok, err](const QDBusMessage &r) {
        KM_PARSE_OR_FAIL(Json::parseMessagePage, v, r, err)
        ok(*v);
    }, err);
}

void DaemonClient::search(const QString &query, uint limit, std::function<void(const QList<Message> &)> ok, ErrorFn err)
{
    call(u"Search"_s, {query, limit}, [ok, err](const QDBusMessage &r) {
        KM_PARSE_OR_FAIL(Json::parseMessageList, v, r, err)
        ok(*v);
    }, err);
}

void DaemonClient::lookupHandles(const QStringList &handles, std::function<void(const QList<HandleReachability> &)> ok, ErrorFn err)
{
    call(u"LookupHandles"_s, {participantsJson(handles)}, [ok, err](const QDBusMessage &r) {
        KM_PARSE_OR_FAIL(Json::parseLookup, v, r, err)
        ok(*v);
    }, err, LongTimeoutMs);
}

void DaemonClient::downloadAttachment(const QString &attachmentId, std::function<void(const QString &)> ok, ErrorFn err)
{
    call(u"DownloadAttachment"_s, {attachmentId}, [ok](const QDBusMessage &r) {
        ok(firstString(r));
    }, err, LongTimeoutMs);
}

void DaemonClient::sendTextTo(const QString &chatId,
                              const QStringList &participants,
                              const QString &text,
                              const QString &replyTo,
                              std::function<void(const SentMessage &)> ok,
                              ErrorFn err)
{
    const QString pj = chatId.isEmpty() ? participantsJson(participants) : QString();
    call(u"SendText"_s, {chatId, pj, text, replyTo}, [ok, err](const QDBusMessage &r) {
        KM_PARSE_OR_FAIL(Json::parseSentMessage, v, r, err)
        ok(*v);
    }, err, LongTimeoutMs);
}

void DaemonClient::sendFileTo(const QString &chatId,
                              const QStringList &participants,
                              const QString &path,
                              const QString &mime,
                              std::function<void(const SentMessage &)> ok,
                              ErrorFn err)
{
    const QString pj = chatId.isEmpty() ? participantsJson(participants) : QString();
    call(u"SendFile"_s, {chatId, pj, path, mime}, [ok, err](const QDBusMessage &r) {
        KM_PARSE_OR_FAIL(Json::parseSentMessage, v, r, err)
        ok(*v);
    }, err, LongTimeoutMs);
}

void DaemonClient::getStatus(std::function<void(const Status &)> ok, ErrorFn err)
{
    call(u"GetStatus"_s, {}, [ok, err](const QDBusMessage &r) {
        KM_PARSE_OR_FAIL(Json::parseStatus, v, r, err)
        ok(*v);
    }, err);
}

#undef KM_PARSE_OR_FAIL

void DaemonClient::login(const QString &appleId, const QString &password)
{
    call(u"Login"_s, {appleId, password}, [this](const QDBusMessage &r) {
        Q_EMIT loginFinished(firstString(r), QString(), QString());
        refreshStatus();
    }, [this](const QString &name, const QString &message) {
        Q_EMIT loginFinished(QString(), name, message);
        refreshStatus();
    }, LongTimeoutMs);
}

void DaemonClient::submitTwoFactor(const QString &code)
{
    call(u"SubmitTwoFactor"_s, {code}, [this](const QDBusMessage &r) {
        Q_EMIT twoFactorFinished(firstString(r), QString(), QString());
        refreshStatus();
    }, [this](const QString &name, const QString &message) {
        Q_EMIT twoFactorFinished(QString(), name, message);
    }, LongTimeoutMs);
}

void DaemonClient::logout()
{
    call(u"Logout"_s, {}, [this](const QDBusMessage &) {
        refreshStatus();
    }, [this](const QString &name, const QString &message) {
        Q_EMIT operationFailed(u"Logout"_s, name, message);
    });
}

void DaemonClient::sendText(const QString &chatId, const QStringList &participants, const QString &text, const QString &replyTo)
{
    sendTextThen(chatId, participants, text, replyTo, {});
}

void DaemonClient::sendTextThen(const QString &chatId,
                                const QStringList &participants,
                                const QString &text,
                                const QString &replyTo,
                                std::function<void(bool, const QString &)> done)
{
    sendTextTo(chatId, participants, text, replyTo, [this, done](const SentMessage &s) {
        Q_EMIT sendFinished(true, s.chatId, s.id, QString());
        if (done) {
            done(true, QString());
        }
    }, [this, chatId, done](const QString &, const QString &message) {
        Q_EMIT sendFinished(false, chatId, QString(), message);
        if (done) {
            done(false, message);
        }
    });
}

void DaemonClient::sendFile(const QString &chatId, const QStringList &participants, const QUrl &file, const QString &mime)
{
    sendFileThen(chatId, participants, file, mime, {});
}

void DaemonClient::sendFileThen(const QString &chatId,
                                const QStringList &participants,
                                const QUrl &file,
                                const QString &mime,
                                std::function<void(bool, const SentMessage &, const QString &)> done)
{
    const QString path = file.isLocalFile() ? file.toLocalFile() : file.toString(QUrl::PreferLocalFile);
    const QString absPath = QFileInfo(path).absoluteFilePath();
    QString type = mime;
    if (type.isEmpty()) {
        type = QMimeDatabase().mimeTypeForFile(absPath).name();
    }
    auto fail = [this, chatId, done](const QString &message) {
        Q_EMIT sendFinished(false, chatId, QString(), message);
        if (done) {
            done(false, SentMessage(), message);
        }
    };
    // The daemon is sandboxed (PrivateTmp, ProtectSystem=strict): hand it a
    // copy it can read. The copy goes once the call is answered (the daemon
    // keeps its own copy of what it sent).
    QString sendPath = absPath;
    QString stagedDir;
    if (Outgoing::needsStaging(absPath)) {
        const auto staged = Outgoing::stage(absPath);
        if (!staged.error.isEmpty()) {
            fail(staged.error);
            return;
        }
        sendPath = staged.path;
        stagedDir = staged.dir;
    }
    sendFileTo(chatId, participants, sendPath, type, [this, done, stagedDir](const SentMessage &s) {
        Outgoing::removeStaged(stagedDir);
        Q_EMIT sendFinished(true, s.chatId, s.id, QString());
        if (done) {
            done(true, s, QString());
        }
    }, [fail, stagedDir, absPath](const QString &name, const QString &message) {
        // Without a reply the daemon may still be reading the copy: leave it
        // to Outgoing::purge().
        if (!isTimeout(name)) {
            Outgoing::removeStaged(stagedDir);
        }
        if (name == "InvalidInput"_L1 && message.contains("os error 2"_L1)) {
            fail(i18n("The messaging service cannot read that folder (%1).", QFileInfo(absPath).absolutePath()));
        } else {
            fail(message);
        }
    });
}

void DaemonClient::react(const QString &chatId, const QString &messageId, const QString &kind, bool remove)
{
    callVoid(u"React"_s, {chatId, messageId, kind, remove}, MediumTimeoutMs);
}

void DaemonClient::edit(const QString &chatId, const QString &messageId, const QString &text)
{
    editThen(chatId, messageId, text, {});
}

void DaemonClient::editThen(const QString &chatId, const QString &messageId, const QString &text, std::function<void(bool, const QString &)> done)
{
    call(u"Edit"_s, {chatId, messageId, text}, [done](const QDBusMessage &) {
        if (done) {
            done(true, QString());
        }
    }, [this, done](const QString &name, const QString &message) {
        Q_EMIT operationFailed(u"Edit"_s, name, message);
        if (done) {
            done(false, message);
        }
    }, MediumTimeoutMs);
}

void DaemonClient::unsend(const QString &chatId, const QString &messageId)
{
    callVoid(u"Unsend"_s, {chatId, messageId}, MediumTimeoutMs);
}

void DaemonClient::setTyping(const QString &chatId, bool typing)
{
    // Best effort: a failed typing indicator is not worth bothering the user.
    call(u"SetTyping"_s, {chatId, typing}, {}, {});
}

void DaemonClient::markRead(const QString &chatId, const QString &upToMessageId)
{
    callVoid(u"MarkRead"_s, {chatId, upToMessageId}, MediumTimeoutMs);
}

void DaemonClient::enableSmsForwarding(bool enable)
{
    call(u"EnableSmsForwarding"_s, {enable}, [this](const QDBusMessage &r) {
        if (Json::parseSmsForwarding(firstString(r), &m_status)) {
            Q_EMIT statusChanged();
        }
        Q_EMIT smsForwardingFinished(true, QString());
    }, [this](const QString &, const QString &message) {
        Q_EMIT smsForwardingFinished(false, message);
        refreshStatus();
    }, LongTimeoutMs);
}

void DaemonClient::onSmsForwardingChanged(const QString &json)
{
    QString error;
    if (!Json::parseSmsForwarding(json, &m_status, &error)) {
        qCWarning(KM_DBUS) << "bad SmsForwardingChanged payload:" << error;
        return;
    }
    Q_EMIT statusChanged();
}

void DaemonClient::refreshStatus()
{
    refreshStatusThen({});
}

void DaemonClient::refreshStatusThen(std::function<void()> done)
{
    getStatus([this, done](const Status &s) {
        setStatus(s);
        if (done) {
            done();
        }
    }, [this](const QString &name, const QString &message) {
        if (name != "org.freedesktop.DBus.Error.ServiceUnknown"_L1) {
            setLastError(message);
        }
    });
}

void DaemonClient::ensureStarted()
{
    auto check = QDBusMessage::createMethodCall(u"org.freedesktop.DBus"_s, u"/org/freedesktop/DBus"_s, u"org.freedesktop.DBus"_s, u"NameHasOwner"_s);
    check.setArguments({Service});
    auto *w = new QDBusPendingCallWatcher(m_bus.asyncCall(check), this);
    connect(w, &QDBusPendingCallWatcher::finished, this, [this](QDBusPendingCallWatcher *w) {
        w->deleteLater();
        QDBusPendingReply<bool> owned = *w;
        if (owned.isError()) {
            setLastError(i18n("Cannot reach the session bus: %1", owned.error().message()));
            return;
        }
        if (owned.value()) {
            setAvailable(true);
            return;
        }
        // Not running: try D-Bus activation (systemd user unit, docs/DBUS.md).
        auto start = QDBusMessage::createMethodCall(u"org.freedesktop.DBus"_s,
                                                    u"/org/freedesktop/DBus"_s,
                                                    u"org.freedesktop.DBus"_s,
                                                    u"StartServiceByName"_s);
        start.setArguments({Service, 0u});
        auto *sw = new QDBusPendingCallWatcher(m_bus.asyncCall(start, LongTimeoutMs), this);
        connect(sw, &QDBusPendingCallWatcher::finished, this, [this](QDBusPendingCallWatcher *sw) {
            sw->deleteLater();
            QDBusPendingReply<uint> started = *sw;
            if (started.isError()) {
                qCInfo(KM_DBUS) << "daemon activation failed:" << started.error().message();
                setLastError(i18n("The imessaged service is not running and could not be started."));
                return;
            }
            // The watcher reports the new owner; cover the race where it was already up.
            setAvailable(true);
        });
    });
}

void DaemonClient::setAvailable(bool available)
{
    if (available == m_available) {
        return;
    }
    m_available = available;
    qCInfo(KM_DBUS) << "daemon" << (available ? "available" : "gone");
    if (!available) {
        m_status = Status();
        m_status.connectionState = u"disconnected"_s;
        m_statusKnown = false;
        rebuildMine();
        Q_EMIT statusChanged();
    } else {
        setLastError(QString());
    }
    Q_EMIT availableChanged();
    Q_EMIT connectionStateChanged();
    if (available) {
        refreshStatus();
        Q_EMIT resyncRequested();
    }
}

void DaemonClient::setLastError(const QString &error)
{
    if (error != m_lastError) {
        m_lastError = error;
        Q_EMIT lastErrorChanged();
    }
}

void DaemonClient::setStatus(const Status &status)
{
    const bool stateChanged = status.connectionState != m_status.connectionState;
    const bool otherChanged = status.otherHandles != m_status.otherHandles;
    m_status = status;
    m_statusKnown = true;
    if (otherChanged) {
        rebuildMine();
    }
    Q_EMIT statusChanged();
    if (stateChanged) {
        Q_EMIT connectionStateChanged();
    }
}

void DaemonClient::onMessageReceived(const QString &chatId, const QString &json)
{
    QString error;
    if (auto m = Json::parseMessage(json, &error)) {
        Q_EMIT messageReceived(chatId, *m);
    } else {
        qCWarning(KM_DBUS) << "bad MessageReceived payload:" << error;
    }
}

void DaemonClient::onMessageUpdated(const QString &chatId, const QString &json)
{
    QString error;
    if (auto m = Json::parseMessage(json, &error)) {
        Q_EMIT messageUpdated(chatId, *m);
    } else {
        qCWarning(KM_DBUS) << "bad MessageUpdated payload:" << error;
    }
}

void DaemonClient::onChatUpdated(const QString &json)
{
    QString error;
    if (auto c = Json::parseChat(json, &error)) {
        noteSharedProfiles(*c);
        Q_EMIT chatUpdated(*c);
    } else {
        qCWarning(KM_DBUS) << "bad ChatUpdated payload:" << error;
    }
}

void DaemonClient::onTyping(const QString &chatId, const QString &handle, bool typing)
{
    Q_EMIT typingChanged(chatId, handle, typing);
}

void DaemonClient::onConnectionStateChanged(const QString &state)
{
    if (!m_available) {
        // A signal can beat the owner-change notification.
        setAvailable(true);
    }
    if (state != m_status.connectionState) {
        m_status.connectionState = state;
        if (state != "error"_L1) {
            m_status.connectionDetail.clear();
        }
        Q_EMIT connectionStateChanged();
        Q_EMIT statusChanged();
    }
    // Pick up handles / error detail that go with the new state.
    refreshStatus();
}

void DaemonClient::onPropertiesChanged(const QString &interface, const QVariantMap &changed, const QStringList &invalidated)
{
    if (interface != Interface) {
        return;
    }
    const QString stateKey = u"ConnectionState"_s;
    const QString registeredKey = u"Registered"_s;
    if (changed.contains(stateKey) && changed.value(stateKey).toString() != m_status.connectionState) {
        // Same handling as the ConnectionStateChanged signal (which also re-reads the status).
        onConnectionStateChanged(changed.value(stateKey).toString());
        return;
    }
    // Invalidated properties carry no value: re-read the whole status.
    if (invalidated.contains(stateKey) || invalidated.contains(registeredKey)
        || (changed.contains(registeredKey) && changed.value(registeredKey).toBool() != m_status.registered)) {
        refreshStatus();
    }
}

void DaemonClient::onTwoFactorRequired()
{
    Q_EMIT twoFactorRequired();
}

void DaemonClient::onHandleProfileUpdated(const QString &handle, const QString &json)
{
    std::optional<SharedProfile> profile;
    QString error;
    if (!Json::parseHandleProfile(json, &profile, &error)) {
        qCWarning(KM_DBUS) << "bad HandleProfileUpdated payload:" << error;
        return;
    }
    if (auto *contacts = ContactResolver::instance()) {
        contacts->setSharedProfile(handle, profile);
    }
    Q_EMIT handleProfileUpdated(handle);
}

void DaemonClient::onBackgroundError(const QString &message)
{
    setLastError(message);
    Q_EMIT backgroundError(message);
    if (message.contains("resync"_L1)) {
        Q_EMIT resyncRequested();
    }
}

} // namespace km
