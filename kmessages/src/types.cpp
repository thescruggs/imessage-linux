// SPDX-License-Identifier: SSPL-1.0
#include "types.h"
#include "contactresolver.h"

#include <KLocalizedString>

#include <QJsonArray>
#include <QJsonDocument>
#include <QLocale>
#include <QRegularExpression>
#include <QTimeZone>

using namespace Qt::StringLiterals;

namespace km
{

QString SharedProfile::displayName() const
{
    if (!name.trimmed().isEmpty()) {
        return name.trimmed();
    }
    return QStringList{first.trimmed(), last.trimmed()}.join(u' ').trimmed();
}

QString Handle::address() const
{
    if (uri.startsWith("mailto:"_L1)) {
        return uri.mid(7);
    }
    if (uri.startsWith("tel:"_L1)) {
        return uri.mid(4);
    }
    return uri;
}

QString Handle::displayName() const
{
    // The user's own contacts win over whatever name the daemon knows.
    if (const auto *contacts = ContactResolver::instance()) {
        if (const auto match = contacts->lookup(uri)) {
            return match->name;
        }
    }
    if (!display.isEmpty() && display != address() && display != uri) {
        return display;
    }
    return formattedAddress();
}

QString Handle::formattedAddress() const
{
    const QString a = address();
    // +15551234567 → +1 (555) 123-4567 for NANP numbers; others as-is.
    if (isPhone() && a.size() == 12 && a.startsWith("+1"_L1)) {
        return u"+1 (%1) %2-%3"_s.arg(a.mid(2, 3), a.mid(5, 3), a.mid(8, 4));
    }
    return a;
}

QString Handle::shortName() const
{
    if (const auto *contacts = ContactResolver::instance()) {
        if (const auto match = contacts->lookup(uri)) {
            return match->givenName;
        }
    }
    const QString n = displayName();
    return display.isEmpty() ? n : n.section(u' ', 0, 0);
}

QString Chat::title() const
{
    if (!displayName.isEmpty()) {
        return displayName;
    }
    if (participants.isEmpty()) {
        return id;
    }
    if (participants.size() == 1) {
        return participants.first().displayName();
    }
    QStringList names;
    for (const auto &p : participants) {
        names << p.shortName(); // first names only for groups, like Messages.app
    }
    if (names.size() > 3) {
        const auto rest = names.size() - 2;
        names = names.mid(0, 2);
        return i18nc("group chat title: first names, then a count", "%1 & %2 more", names.join(", "_L1), rest);
    }
    const QString last = names.takeLast();
    return i18nc("group chat title: list of names", "%1 & %2", names.join(", "_L1), last);
}

bool Attachment::isImage() const
{
    return mime.startsWith("image/"_L1);
}

QString Reaction::glyph() const
{
    if (kind == "love"_L1)
        return u"❤️"_s;
    if (kind == "like"_L1)
        return u"👍"_s;
    if (kind == "dislike"_L1)
        return u"👎"_s;
    if (kind == "laugh"_L1)
        return u"😂"_s;
    if (kind == "emphasize"_L1)
        return u"‼️"_s;
    if (kind == "question"_L1)
        return u"❓"_s;
    return emoji;
}

QString Reaction::methodKind() const
{
    return isEmoji() ? "emoji:"_L1 + emoji : kind;
}

QList<Attachment> Message::attachments() const
{
    QList<Attachment> out;
    for (const auto &p : parts) {
        if (p.type == MessagePart::Type::Attachment) {
            out.append({p.attachmentId, p.mime, p.filename, p.size});
        }
    }
    return out;
}

QStringList Message::unsupported() const
{
    QStringList out;
    for (const auto &p : parts) {
        if (p.type == MessagePart::Type::Unsupported) {
            out << p.description;
        }
    }
    return out;
}

QString Message::snippet() const
{
    if (unsent) {
        return i18n("This message was unsent");
    }
    if (!text.trimmed().isEmpty()) {
        return text.simplified();
    }
    const auto atts = attachments();
    if (!atts.isEmpty()) {
        if (atts.size() == 1) {
            return atts.first().isImage() ? i18n("Image") : i18n("Attachment: %1", atts.first().filename);
        }
        return i18np("%1 attachment", "%1 attachments", atts.size());
    }
    const auto u = unsupported();
    if (!u.isEmpty()) {
        return i18n("Unsupported: %1", u.first());
    }
    return {};
}

QString deliveryToString(Delivery d)
{
    switch (d) {
    case Delivery::Pending:
        return u"pending"_s;
    case Delivery::Sent:
        return u"sent"_s;
    case Delivery::Delivered:
        return u"delivered"_s;
    case Delivery::Read:
        return u"read"_s;
    case Delivery::Failed:
        return u"failed"_s;
    }
    return u"pending"_s;
}

Delivery deliveryFromString(const QString &s)
{
    if (s == "sent"_L1)
        return Delivery::Sent;
    if (s == "delivered"_L1)
        return Delivery::Delivered;
    if (s == "read"_L1)
        return Delivery::Read;
    if (s == "failed"_L1)
        return Delivery::Failed;
    return Delivery::Pending;
}

QString normalizeHandle(const QString &rawIn)
{
    const QString raw = rawIn.trimmed();
    const QString lower = raw.toLower();
    QString body;
    bool forcedEmail = false;
    if (lower.startsWith("mailto:"_L1)) {
        body = lower.mid(7).trimmed();
        forcedEmail = true;
    } else if (lower.startsWith("tel:"_L1)) {
        body = raw.mid(4).trimmed();
    } else {
        body = raw;
    }
    if (forcedEmail || body.contains(u'@')) {
        const QString email = body.toLower();
        const auto at = email.indexOf(u'@');
        if (at < 0) {
            return {};
        }
        const QString local = email.left(at);
        const QString domain = email.mid(at + 1);
        bool hasSpace = false;
        for (const QChar c : email) {
            hasSpace = hasSpace || c.isSpace();
        }
        if (local.isEmpty() || domain.isEmpty() || domain.contains(u'@') || hasSpace) {
            return {};
        }
        return "mailto:"_L1 + email;
    }
    QString digits;
    bool anyDigit = false;
    for (const QChar c : body) {
        const bool digit = c >= u'0' && c <= u'9';
        if (!digit && !QStringView(u"+ -.()").contains(c)) {
            return {};
        }
        if (digit || c == u'+') {
            digits += c;
        }
        anyDigit = anyDigit || digit;
    }
    if (!anyDigit) {
        return {};
    }
    return "tel:"_L1 + digits;
}

QString initialsFor(const QString &nameIn)
{
    QString name = nameIn.trimmed();
    if (name.contains(u'@')) {
        name = name.section(u'@', 0, 0);
    }
    if (name.startsWith(u'+') || (!name.isEmpty() && name.at(0).isDigit())) {
        return {}; // phone number: the UI shows a person icon
    }
    QString out;
    const auto words = name.split(QRegularExpression(u"[\\s._-]+"_s), Qt::SkipEmptyParts);
    for (const auto &w : words) {
        const QChar c = w.at(0);
        if (c.isLetterOrNumber()) {
            out += c.toUpper();
        }
        if (out.size() == 2) {
            break;
        }
    }
    return out;
}

QColor avatarColorFor(const QString &key)
{
    // Muted palette that reads on both light and dark backgrounds.
    static const QColor palette[] = {
        QColor(0xe9, 0x3d, 0x58),
        QColor(0xe9, 0x64, 0x3a),
        QColor(0xef, 0x97, 0x3c),
        QColor(0x3d, 0xae, 0xe9),
        QColor(0x2e, 0xcc, 0x71),
        QColor(0x1a, 0xbc, 0x9c),
        QColor(0x9b, 0x59, 0xb6),
        QColor(0x88, 0x8e, 0x9a),
    };
    uint h = 5381;
    for (const QChar c : key) {
        h = ((h << 5) + h) + c.unicode();
    }
    return palette[h % (sizeof(palette) / sizeof(palette[0]))];
}

QString formatListTime(qint64 ms, const QDateTime &now)
{
    if (ms <= 0) {
        return {};
    }
    const QDateTime t = QDateTime::fromMSecsSinceEpoch(ms, now.timeZone());
    const auto days = t.date().daysTo(now.date());
    const QLocale l;
    if (days <= 0) {
        return l.toString(t.time(), QLocale::ShortFormat);
    }
    if (days == 1) {
        return i18n("Yesterday");
    }
    if (days < 7) {
        return l.dayName(t.date().dayOfWeek(), QLocale::LongFormat);
    }
    return l.toString(t.date(), QLocale::ShortFormat);
}

QString formatSeparatorTime(qint64 ms, const QDateTime &now)
{
    const QDateTime t = QDateTime::fromMSecsSinceEpoch(ms, now.timeZone());
    const auto days = t.date().daysTo(now.date());
    const QLocale l;
    const QString time = l.toString(t.time(), QLocale::ShortFormat);
    if (days <= 0) {
        return i18nc("transcript separator", "Today %1", time);
    }
    if (days == 1) {
        return i18nc("transcript separator", "Yesterday %1", time);
    }
    if (days < 7) {
        return i18nc("transcript separator: weekday time", "%1 %2", l.dayName(t.date().dayOfWeek()), time);
    }
    return i18nc("transcript separator: date at time", "%1 at %2", l.toString(t.date(), u"MMM d, yyyy"_s), time);
}

QString formatSize(qint64 bytes)
{
    return QLocale().formattedDataSize(bytes, 1, QLocale::DataSizeTraditionalFormat);
}

namespace Json
{
namespace
{
void setError(QString *error, const QString &msg)
{
    if (error) {
        *error = msg;
    }
}

QString optString(const QJsonValue &v)
{
    return v.isString() ? v.toString() : QString();
}

qint64 optInt(const QJsonValue &v)
{
    return v.isDouble() ? v.toInteger() : 0;
}

std::optional<QJsonDocument> parseDoc(const QString &json, QString *error)
{
    QJsonParseError pe;
    auto doc = QJsonDocument::fromJson(json.toUtf8(), &pe);
    if (pe.error != QJsonParseError::NoError) {
        setError(error, pe.errorString());
        return std::nullopt;
    }
    return doc;
}

std::optional<QJsonObject> parseObject(const QString &json, QString *error)
{
    auto doc = parseDoc(json, error);
    if (!doc) {
        return std::nullopt;
    }
    if (!doc->isObject()) {
        setError(error, u"expected a JSON object"_s);
        return std::nullopt;
    }
    return doc->object();
}

std::optional<QJsonArray> parseArray(const QString &json, QString *error)
{
    auto doc = parseDoc(json, error);
    if (!doc) {
        return std::nullopt;
    }
    if (!doc->isArray()) {
        setError(error, u"expected a JSON array"_s);
        return std::nullopt;
    }
    return doc->array();
}

std::optional<MessagePart> part(const QJsonObject &o, QString *error)
{
    MessagePart p;
    const QString type = o.value("type"_L1).toString();
    if (type == "text"_L1) {
        p.type = MessagePart::Type::Text;
        p.text = o.value("text"_L1).toString();
    } else if (type == "attachment"_L1) {
        p.type = MessagePart::Type::Attachment;
        p.attachmentId = o.value("attachment_id"_L1).toString();
        p.mime = o.value("mime"_L1).toString();
        p.filename = o.value("filename"_L1).toString();
        p.size = optInt(o.value("size"_L1));
        if (p.attachmentId.isEmpty()) {
            setError(error, u"attachment part without attachment_id"_s);
            return std::nullopt;
        }
    } else if (type == "mention"_L1) {
        p.type = MessagePart::Type::Mention;
        p.handle = o.value("handle"_L1).toString();
        p.text = o.value("text"_L1).toString();
    } else if (type == "unsupported"_L1) {
        p.type = MessagePart::Type::Unsupported;
        p.description = o.value("description"_L1).toString();
    } else {
        // Forward compatibility: an unknown part type is shown as unsupported.
        p.type = MessagePart::Type::Unsupported;
        p.description = type;
    }
    return p;
}

std::optional<Reaction> reaction(const QJsonObject &o, QString *error)
{
    Reaction r;
    const QJsonValue k = o.value("kind"_L1);
    if (k.isString()) {
        r.kind = k.toString();
    } else if (k.isObject() && k.toObject().value("emoji"_L1).isString()) {
        r.kind = u"emoji"_s;
        r.emoji = k.toObject().value("emoji"_L1).toString();
    } else {
        setError(error, u"bad reaction kind"_s);
        return std::nullopt;
    }
    auto s = handle(o.value("sender"_L1), error);
    if (!s) {
        return std::nullopt;
    }
    r.sender = *s;
    r.added = o.value("added"_L1).toBool(true);
    return r;
}
} // namespace

std::optional<Handle> handle(const QJsonValue &v, QString *error)
{
    if (!v.isObject() || !v.toObject().value("uri"_L1).isString()) {
        setError(error, u"expected a handle object {\"uri\": …}"_s);
        return std::nullopt;
    }
    const auto o = v.toObject();
    return Handle{o.value("uri"_L1).toString(), optString(o.value("display"_L1)), sharedProfile(o.value("profile"_L1))};
}

std::optional<SharedProfile> sharedProfile(const QJsonValue &v)
{
    if (!v.isObject()) {
        return std::nullopt;
    }
    const auto o = v.toObject();
    SharedProfile p;
    p.name = optString(o.value("name"_L1));
    p.first = optString(o.value("first"_L1));
    p.last = optString(o.value("last"_L1));
    p.photo = optString(o.value("photo"_L1));
    p.updatedAt = optInt(o.value("updated_at"_L1));
    if (p.isEmpty()) {
        return std::nullopt;
    }
    return p;
}

bool parseHandleProfile(const QString &json, std::optional<SharedProfile> *out, QString *error)
{
    QJsonParseError pe;
    // "null" is a valid document only when wrapped; parse it by hand.
    const auto doc = QJsonDocument::fromJson("["_ba + json.toUtf8() + "]"_ba, &pe);
    if (pe.error != QJsonParseError::NoError || !doc.isArray() || doc.array().size() != 1) {
        setError(error, u"bad profile JSON"_s);
        return false;
    }
    const auto v = doc.array().at(0);
    if (!v.isNull() && !v.isObject()) {
        setError(error, u"expected a profile object or null"_s);
        return false;
    }
    *out = sharedProfile(v);
    return true;
}

std::optional<Chat> chat(const QJsonObject &o, QString *error)
{
    Chat c;
    c.id = o.value("id"_L1).toString();
    if (c.id.isEmpty()) {
        setError(error, u"chat without id"_s);
        return std::nullopt;
    }
    const auto parts = o.value("participants"_L1).toArray();
    for (const auto &p : parts) {
        auto h = handle(p, error);
        if (!h) {
            return std::nullopt;
        }
        c.participants.append(*h);
    }
    c.displayName = optString(o.value("display_name"_L1));
    c.isGroup = o.value("is_group"_L1).toBool();
    c.lastMessageAt = optInt(o.value("last_message_at"_L1));
    c.unreadCount = uint(qMax<qint64>(0, optInt(o.value("unread_count"_L1))));
    const auto bg = o.value("background"_L1).toObject();
    c.backgroundId = optString(bg.value("id"_L1));
    if (!c.backgroundId.isEmpty()) {
        c.backgroundPath = optString(bg.value("path"_L1));
    }
    c.iconPath = optString(o.value("icon_path"_L1));
    c.pinned = o.value("pinned"_L1).toBool();
    if (c.pinned) {
        const auto order = o.value("pin_order"_L1);
        c.pinOrder = order.isDouble() ? qMax(0, order.toInt()) : 0;
    }
    return c;
}

std::optional<Message> message(const QJsonObject &o, QString *error)
{
    Message m;
    m.id = o.value("id"_L1).toString();
    m.chatId = o.value("chat_id"_L1).toString();
    if (m.id.isEmpty() || m.chatId.isEmpty()) {
        setError(error, u"message without id/chat_id"_s);
        return std::nullopt;
    }
    auto s = handle(o.value("sender"_L1), error);
    if (!s) {
        return std::nullopt;
    }
    m.sender = *s;
    m.sentAt = optInt(o.value("sent_at"_L1));
    m.text = optString(o.value("text"_L1));
    const auto parts = o.value("parts"_L1).toArray();
    for (const auto &p : parts) {
        auto mp = part(p.toObject(), error);
        if (!mp) {
            return std::nullopt;
        }
        m.parts.append(*mp);
    }
    m.subject = optString(o.value("subject"_L1));
    m.replyTo = optString(o.value("reply_to"_L1));
    m.isFromMe = o.value("is_from_me"_L1).toBool();
    m.delivery = deliveryFromString(o.value("delivery"_L1).toString());
    m.edited = o.value("edited"_L1).toBool();
    m.unsent = o.value("unsent"_L1).toBool();
    const auto reactions = o.value("reactions"_L1).toArray();
    for (const auto &r : reactions) {
        auto rr = reaction(r.toObject(), error);
        if (!rr) {
            return std::nullopt;
        }
        m.reactions.append(*rr);
    }
    m.effect = optString(o.value("effect"_L1));
    m.verified = o.value("verified"_L1).toBool(true); // serde default = true
    const QString service = o.value("service"_L1).toString();
    m.service = service.isEmpty() ? u"imessage"_s : service; // serde default = imessage
    return m;
}

std::optional<Chat> parseChat(const QString &json, QString *error)
{
    auto o = parseObject(json, error);
    return o ? chat(*o, error) : std::nullopt;
}

std::optional<QList<Chat>> parseChatList(const QString &json, QString *error)
{
    auto a = parseArray(json, error);
    if (!a) {
        return std::nullopt;
    }
    QList<Chat> out;
    for (const auto &v : *a) {
        auto c = chat(v.toObject(), error);
        if (!c) {
            return std::nullopt;
        }
        out.append(*c);
    }
    return out;
}

std::optional<Message> parseMessage(const QString &json, QString *error)
{
    auto o = parseObject(json, error);
    return o ? message(*o, error) : std::nullopt;
}

std::optional<QList<Message>> parseMessageList(const QString &json, QString *error)
{
    auto a = parseArray(json, error);
    if (!a) {
        return std::nullopt;
    }
    QList<Message> out;
    for (const auto &v : *a) {
        auto m = message(v.toObject(), error);
        if (!m) {
            return std::nullopt;
        }
        out.append(*m);
    }
    return out;
}

std::optional<MessagePage> parseMessagePage(const QString &json, QString *error)
{
    auto o = parseObject(json, error);
    if (!o) {
        return std::nullopt;
    }
    MessagePage page;
    const auto msgs = o->value("messages"_L1).toArray();
    for (const auto &v : msgs) {
        auto m = message(v.toObject(), error);
        if (!m) {
            return std::nullopt;
        }
        page.messages.append(*m);
    }
    page.nextCursor = optString(o->value("next_cursor"_L1));
    return page;
}

std::optional<Status> parseStatus(const QString &json, QString *error)
{
    auto o = parseObject(json, error);
    if (!o) {
        return std::nullopt;
    }
    Status s;
    s.version = o->value("version"_L1).toString();
    const auto conn = o->value("connection"_L1).toObject();
    s.connectionState = conn.value("state"_L1).toString();
    s.connectionDetail = optString(conn.value("detail"_L1));
    if (s.connectionState.isEmpty()) {
        setError(error, u"status without connection.state"_s);
        return std::nullopt;
    }
    s.registered = o->value("registered"_L1).toBool();
    const auto reg = o->value("registration"_L1).toObject();
    const auto handles = reg.value("handles"_L1).toArray();
    for (const auto &h : handles) {
        auto hh = handle(h, error);
        if (!hh) {
            return std::nullopt;
        }
        s.handles << hh->uri;
    }
    // Normalized URIs (older daemons: missing = none).
    for (const auto &h : reg.value("other_handles"_L1).toArray()) {
        const QString uri = h.isObject() ? h.toObject().value("uri"_L1).toString() : h.toString();
        if (!uri.isEmpty() && !s.otherHandles.contains(uri)) {
            s.otherHandles << uri;
        }
    }
    if (const auto sender = reg.value("sender"_L1); sender.isObject()) {
        auto hh = handle(sender, error);
        if (!hh) {
            return std::nullopt;
        }
        s.sender = hh->uri;
    }
    const auto profiles = o->value("profiles"_L1).toObject();
    s.profilesState = optString(profiles.value("state"_L1));
    s.profilesDetail = optString(profiles.value("detail"_L1));
    applySms(o->value("sms"_L1).toObject(), &s);
    s.macHost = optString(o->value("mac_host"_L1));
    s.registeredAt = optInt(reg.value("registered_at"_L1));
    s.expiresAt = optInt(reg.value("expires_at"_L1));
    const auto unread = o->value("unread"_L1);
    s.unread = unread.isDouble() ? unread.toInt() : -1;
    return s;
}

void applySms(const QJsonObject &sms, Status *s)
{
    s->smsState = optString(sms.value("state"_L1));
    s->smsNumber = optString(sms.value("number"_L1));
    s->smsDetail = optString(sms.value("detail"_L1));
    s->smsSince = optInt(sms.value("since"_L1));
}

bool parseSmsForwarding(const QString &json, Status *s, QString *error)
{
    auto o = parseObject(json, error);
    if (!o) {
        return false;
    }
    applySms(*o, s);
    if (s->smsState.isEmpty()) {
        s->smsState = u"off"_s; // serde default
    }
    return true;
}

std::optional<SentMessage> parseSentMessage(const QString &json, QString *error)
{
    auto o = parseObject(json, error);
    if (!o) {
        return std::nullopt;
    }
    SentMessage s;
    s.id = o->value("id"_L1).toString();
    s.chatId = o->value("chat_id"_L1).toString();
    s.sentAt = optInt(o->value("sent_at"_L1));
    if (const QString service = o->value("service"_L1).toString(); !service.isEmpty()) {
        s.service = service;
    }
    if (s.id.isEmpty() || s.chatId.isEmpty()) {
        setError(error, u"sent message without id/chat_id"_s);
        return std::nullopt;
    }
    if (o->contains("sender"_L1)) {
        auto h = handle(o->value("sender"_L1), error);
        if (!h) {
            return std::nullopt;
        }
        s.sender = *h;
    }
    return s;
}

std::optional<QList<HandleReachability>> parseLookup(const QString &json, QString *error)
{
    auto a = parseArray(json, error);
    if (!a) {
        return std::nullopt;
    }
    QList<HandleReachability> out;
    for (const auto &v : *a) {
        const auto o = v.toObject();
        if (!o.value("handle"_L1).isString()) {
            setError(error, u"lookup entry without handle"_s);
            return std::nullopt;
        }
        out.append({o.value("handle"_L1).toString(), o.value("reachable"_L1).toBool()});
    }
    return out;
}

} // namespace Json
} // namespace km
