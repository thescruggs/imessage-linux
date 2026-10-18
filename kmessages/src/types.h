// SPDX-License-Identifier: SSPL-1.0
//
// Plain data types mirroring crates/imessage-common/src/model.rs as it
// crosses the D-Bus boundary (JSON strings, docs/DBUS.md), plus the JSON
// parsers and small presentation helpers shared by the models.
#pragma once

#include <QColor>
#include <QDateTime>
#include <QJsonObject>
#include <QList>
#include <QMetaType>
#include <QString>
#include <QStringList>

#include <optional>

namespace km
{

/// The name and photo a person shared with us (Messages "Share Name and
/// Photo"), as the daemon fetched it (`profile` in docs/DBUS.md).
struct SharedProfile {
    QString name; ///< the display name they chose
    QString first;
    QString last;
    QString photo; ///< absolute path of the photo, empty = none
    qint64 updatedAt = 0; ///< ms since epoch

    /// `name`, else "first last".
    QString displayName() const;
    bool isEmpty() const { return displayName().isEmpty() && photo.isEmpty(); }
    bool operator==(const SharedProfile &o) const = default;
};

/// An IDS URI (`mailto:…` / `tel:…`) with an optional human name.
struct Handle {
    QString uri;
    QString display; ///< empty if the daemon did not send one
    std::optional<SharedProfile> profile = std::nullopt; ///< what they shared with us (chat participants only)

    /// The address without its scheme.
    QString address() const;
    /// The contact's name (ContactResolver: the address book, else the name
    /// the person shared), else `display` if set, else a readable address
    /// (phone numbers formatted lightly).
    QString displayName() const;
    /// First name for group titles ("Alice, Bob & Carol"); the full
    /// displayName() when there is no name.
    QString shortName() const;
    /// The address, phone numbers formatted lightly (never the contact name).
    QString formattedAddress() const;
    bool isEmail() const { return uri.startsWith(QLatin1String("mailto:")); }
    bool isPhone() const { return uri.startsWith(QLatin1String("tel:")); }
    bool operator==(const Handle &o) const { return uri == o.uri; }
};

struct Chat {
    QString id;
    QList<Handle> participants; ///< everyone except us
    QString displayName; ///< empty = unnamed
    bool isGroup = false;
    qint64 lastMessageAt = 0; ///< ms since epoch, 0 = no messages
    uint unreadCount = 0;
    QString backgroundId; ///< transcript background, empty = none
    QString backgroundPath; ///< its image (absolute path), empty = none / not downloaded
    QString iconPath; ///< the group's photo (absolute path), empty = none / not downloaded
    bool pinned = false; ///< pinned to the top of the chat list (local, `SetChatPinned`)
    int pinOrder = -1; ///< position among the pins (0 = first), -1 = not pinned

    /// Name to show: display name, else the participants' names.
    QString title() const;
};

struct MessagePart {
    enum class Type { Text, Attachment, Mention, Unsupported };
    Type type = Type::Text;
    QString text; ///< Text, Mention
    QString attachmentId; ///< Attachment
    QString mime; ///< Attachment
    QString filename; ///< Attachment
    qint64 size = 0; ///< Attachment
    QString handle; ///< Mention
    QString description; ///< Unsupported
};

/// Attachment as seen by the UI (derived from an attachment part).
struct Attachment {
    QString id;
    QString mime;
    QString filename;
    qint64 size = 0;
    bool isImage() const;
};

struct Reaction {
    /// `love`, `like`, `dislike`, `laugh`, `emphasize`, `question`, or `emoji`.
    QString kind;
    QString emoji; ///< only for kind == "emoji"
    Handle sender;
    bool added = true;

    bool isEmoji() const { return kind == QLatin1String("emoji"); }
    /// The glyph shown in the tapback chip.
    QString glyph() const;
    /// Value accepted by the `React` method (`love` / `emoji:🔥`).
    QString methodKind() const;
};

enum class Delivery { Pending, Sent, Delivered, Read, Failed };

struct Message {
    QString id;
    QString chatId;
    Handle sender;
    qint64 sentAt = 0;
    QString text; ///< null QString if the message has no text
    QList<MessagePart> parts;
    QString subject;
    QString replyTo;
    bool isFromMe = false;
    Delivery delivery = Delivery::Pending;
    bool edited = false;
    bool unsent = false;
    QList<Reaction> reactions;
    QString effect;
    bool verified = true;
    /// `imessage`, or `sms` for a text message relayed by the user's iPhone.
    QString service = QStringLiteral("imessage");

    bool isSms() const { return service == QLatin1String("sms"); }
    QList<Attachment> attachments() const;
    /// Descriptions of parts we cannot render.
    QStringList unsupported() const;
    /// One-line preview for the chat list / notifications.
    QString snippet() const;
};

struct MessagePage {
    QList<Message> messages; ///< oldest first
    QString nextCursor; ///< empty = this was the oldest page
};

struct Status {
    QString version;
    QString connectionState;
    QString connectionDetail;
    bool registered = false;
    QStringList handles; ///< our registered URIs
    QStringList otherHandles; ///< `registration.other_handles`: the user's handles not registered here (missing = none)
    QString sender; ///< `registration.sender`: the handle new messages go out from ("" if not registered / older daemon)
    qint64 registeredAt = 0; ///< seconds
    qint64 expiresAt = 0; ///< seconds
    int unread = -1; ///< -1 = store unavailable (null)
    QString profilesState; ///< `profiles.state` (ready / pending / needs_login / unavailable), empty = older daemon
    QString profilesDetail;
    QString smsState; ///< `sms.state` (off / requested / active / error), empty = older daemon
    QString smsNumber; ///< `sms.number`: the iPhone number that relays texts
    QString smsDetail;
    qint64 smsSince = 0; ///< ms
    QString macHost; ///< `mac_host`: the Mac `imsg sync-mac` copies from ("" = unset / older daemon)
};

struct SentMessage {
    QString id;
    QString chatId;
    qint64 sentAt = 0;
    Handle sender;
    QString service = QStringLiteral("imessage");
};

struct HandleReachability {
    QString handle;
    bool reachable = false;
};

QString deliveryToString(Delivery d);
Delivery deliveryFromString(const QString &s);

/// Port of `Handle::try_normalize`: IDS URI, or a null string if `raw` is
/// neither an email address nor a phone number.
QString normalizeHandle(const QString &raw);

/// Up to two initials for an avatar ("Bob Smith" → "BS", "bob@x" → "B").
QString initialsFor(const QString &name);
/// Stable avatar colour derived from a string.
QColor avatarColorFor(const QString &key);

/// Chat-list style timestamp: time today, "Yesterday", weekday within a
/// week, else a short date. `now` is injectable for tests.
QString formatListTime(qint64 ms, const QDateTime &now = QDateTime::currentDateTime());
/// Transcript separator: "Today 9:41 AM", "Yesterday 9:41 AM", "Monday 9:41 AM", "Mar 3, 2026 at 9:41 AM".
QString formatSeparatorTime(qint64 ms, const QDateTime &now = QDateTime::currentDateTime());
/// Human-readable byte size.
QString formatSize(qint64 bytes);

namespace Json
{
// Each parser returns std::nullopt and fills `error` on malformed input.
std::optional<Handle> handle(const QJsonValue &v, QString *error = nullptr);
std::optional<Chat> chat(const QJsonObject &o, QString *error = nullptr);
std::optional<Message> message(const QJsonObject &o, QString *error = nullptr);

std::optional<Chat> parseChat(const QString &json, QString *error = nullptr);
std::optional<QList<Chat>> parseChatList(const QString &json, QString *error = nullptr);
std::optional<Message> parseMessage(const QString &json, QString *error = nullptr);
std::optional<QList<Message>> parseMessageList(const QString &json, QString *error = nullptr);
std::optional<MessagePage> parseMessagePage(const QString &json, QString *error = nullptr);
std::optional<Status> parseStatus(const QString &json, QString *error = nullptr);
/// `sms` object of a status (also `SmsForwardingChanged` / `EnableSmsForwarding`): fills the sms* fields of `s`.
void applySms(const QJsonObject &sms, Status *s);
bool parseSmsForwarding(const QString &json, Status *s, QString *error = nullptr);
std::optional<SentMessage> parseSentMessage(const QString &json, QString *error = nullptr);
std::optional<QList<HandleReachability>> parseLookup(const QString &json, QString *error = nullptr);
/// A HandleProfile object, or std::nullopt for `null` / nothing to show.
std::optional<SharedProfile> sharedProfile(const QJsonValue &v);
/// `GetHandleProfile` / `HandleProfileUpdated` payload: false on malformed
/// JSON; `*out` = std::nullopt for `null`.
bool parseHandleProfile(const QString &json, std::optional<SharedProfile> *out, QString *error = nullptr);
}

} // namespace km

Q_DECLARE_METATYPE(km::Chat)
Q_DECLARE_METATYPE(km::Message)
