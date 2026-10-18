// SPDX-License-Identifier: SSPL-1.0
#include "messagelistmodel.h"
#include "chatlistmodel.h"
#include "contactresolver.h"
#include "daemonclient.h"

#include <KLocalizedString>

#include <QClipboard>
#include <QDesktopServices>
#include <QFileInfo>
#include <QGuiApplication>
#include <QRegularExpression>
#include <QTimer>

#include <algorithm>

using namespace Qt::StringLiterals;

namespace km
{

namespace
{
constexpr qint64 GroupGapMs = 5 * 60 * 1000;
constexpr qint64 SeparatorGapMs = 60 * 60 * 1000;
}

MessageListModel::MessageListModel(DaemonClient *client, ChatListModel *chats, QObject *parent)
    : QAbstractListModel(parent)
    , m_client(client)
    , m_chats(chats)
{
    connect(client, &DaemonClient::messageReceived, this, &MessageListModel::onMessageReceived);
    connect(client, &DaemonClient::messageUpdated, this, &MessageListModel::onMessageUpdated);
    connect(client, &DaemonClient::typingChanged, this, &MessageListModel::onTyping);
    connect(client, &DaemonClient::resyncRequested, this, &MessageListModel::reload);
    m_ownHandles = client->ownHandles();
    m_lastSmsActive = client->isSmsActive();
    connect(this, &MessageListModel::countChanged, this, &MessageListModel::smsModeChanged);
    connect(client, &DaemonClient::statusChanged, this, [this] {
        if (m_client && m_client->isSmsActive() != m_lastSmsActive) {
            m_lastSmsActive = m_client->isSmsActive();
            checkReachability();
            Q_EMIT smsModeChanged();
        }
    });
    connect(client, &DaemonClient::statusChanged, this, [this] {
        // Own handles decide "my reaction"; refresh only when they change.
        if (!m_client || m_client->ownHandles() == m_ownHandles) {
            return;
        }
        m_ownHandles = m_client->ownHandles();
        if (!m_messages.isEmpty()) {
            Q_EMIT dataChanged(index(0), index(int(m_messages.size()) - 1), {ReactionsRole, MyReactionRole});
        }
    });
    if (chats) {
        connect(chats, &ChatListModel::modelReset, this, &MessageListModel::chatIdChanged);
    }
    // Sender names, reaction tooltips and avatars come from the contacts.
    if (auto *contacts = ContactResolver::instance()) {
        connect(contacts, &ContactResolver::contactsChanged, this, [this] {
            touchRows(0, int(m_messages.size()) - 1);
            Q_EMIT typingChanged();
        });
    }
}

int MessageListModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : int(m_messages.size());
}

QHash<int, QByteArray> MessageListModel::roleNames() const
{
    return {
        {MessageIdRole, "messageId"},
        {TextRole, "text"},
        {RichTextRole, "richText"},
        {HasTextRole, "hasText"},
        {IsEmojiOnlyRole, "isEmojiOnly"},
        {SenderRole, "sender"},
        {SenderNameRole, "senderName"},
        {IsFromMeRole, "isFromMe"},
        {TimestampRole, "timestamp"},
        {TimeRole, "time"},
        {ShowSeparatorRole, "showSeparator"},
        {SeparatorLabelRole, "separatorLabel"},
        {FirstInGroupRole, "firstInGroup"},
        {LastInGroupRole, "lastInGroup"},
        {ShowSenderNameRole, "showSenderName"},
        {ShowAvatarRole, "showAvatar"},
        {AvatarColorRole, "avatarColor"},
        {AvatarSourceRole, "avatarSource"},
        {InitialsRole, "initials"},
        {DeliveryRole, "delivery"},
        {DeliveryLabelRole, "deliveryLabel"},
        {EditedRole, "edited"},
        {UnsentRole, "unsent"},
        {VerifiedRole, "verified"},
        {ReactionsRole, "reactions"},
        {MyReactionRole, "myReaction"},
        {AttachmentsRole, "attachments"},
        {HasAttachmentsRole, "hasAttachments"},
        {ReplyToRole, "replyTo"},
        {ReplyToTextRole, "replyToText"},
        {ReplyToSenderRole, "replyToSender"},
        {SubjectRole, "subject"},
        {EffectRole, "effect"},
        {UnsupportedRole, "unsupported"},
        {IsSmsRole, "isSms"},
        {LinkRole, "link"},
        {LinkOnlyRole, "linkOnly"},
    };
}

bool MessageListModel::isGroup() const
{
    if (m_chatId.isEmpty()) {
        return false;
    }
    return m_chats ? m_chats->isGroupChat(m_chatId) : m_chatId.startsWith("group"_L1);
}

QString MessageListModel::typingText() const
{
    if (m_typing.isEmpty()) {
        return {};
    }
    QStringList names;
    for (const auto &h : m_typing) {
        names << (m_chats ? m_chats->nameForHandle(h) : Handle{h, {}}.displayName());
    }
    names.sort();
    return i18np("%2 is typing…", "%2 are typing…", names.size(), names.join(", "_L1));
}

QString MessageListModel::senderName(const Message &m) const
{
    if (m.isFromMe) {
        return i18n("You");
    }
    return m_chats ? m_chats->nameForHandle(m.sender.uri) : m.sender.displayName();
}

bool MessageListModel::sameGroup(int older, int newer) const
{
    const Message &a = m_messages.at(older);
    const Message &b = m_messages.at(newer);
    // A change of `verified` starts a new run, so every unverified run shows its badge.
    if (a.unsent || b.unsent || a.isFromMe != b.isFromMe || a.sender.uri != b.sender.uri || a.verified != b.verified) {
        return false;
    }
    return qAbs(b.sentAt - a.sentAt) <= GroupGapMs;
}

bool MessageListModel::showSeparator(int row) const
{
    const int older = row + 1;
    if (older >= m_messages.size()) {
        return true;
    }
    const auto a = QDateTime::fromMSecsSinceEpoch(m_messages.at(older).sentAt);
    const auto b = QDateTime::fromMSecsSinceEpoch(m_messages.at(row).sentAt);
    return a.date() != b.date() || b.toMSecsSinceEpoch() - a.toMSecsSinceEpoch() > SeparatorGapMs;
}

bool MessageListModel::firstInGroup(int row) const
{
    const int older = row + 1;
    return older >= m_messages.size() || showSeparator(row) || !sameGroup(older, row);
}

bool MessageListModel::lastInGroup(int row) const
{
    const int newer = row - 1;
    return newer < 0 || showSeparator(newer) || !sameGroup(row, newer);
}

QString MessageListModel::deliveryLabel(int row) const
{
    const Message &m = m_messages.at(row);
    if (!m.isFromMe || m.unsent) {
        return {};
    }
    if (m.delivery == Delivery::Failed) {
        return i18n("Not Delivered");
    }
    if (row != m_lastOwnRow) {
        return {};
    }
    switch (m.delivery) {
    case Delivery::Pending:
        return i18n("Sending…");
    case Delivery::Sent:
        return i18n("Sent");
    case Delivery::Delivered:
        return i18n("Delivered");
    case Delivery::Read:
        return i18n("Read");
    case Delivery::Failed:
        break;
    }
    return {};
}

QString MessageListModel::myReaction(const Message &m) const
{
    for (const auto &r : m.reactions) {
        if (r.added && m_client && m_client->isOwnHandle(r.sender.uri)) {
            return r.methodKind();
        }
    }
    return {};
}

QVariantList MessageListModel::reactions(const Message &m) const
{
    struct Agg {
        QString glyph;
        QString kind;
        int count = 0;
        bool fromMe = false;
        QStringList senders;
    };
    QList<Agg> aggs;
    for (const auto &r : m.reactions) {
        if (!r.added) {
            continue;
        }
        const QString glyph = r.glyph();
        auto it = std::find_if(aggs.begin(), aggs.end(), [&](const Agg &a) {
            return a.glyph == glyph;
        });
        if (it == aggs.end()) {
            aggs.append({glyph, r.methodKind(), 0, false, {}});
            it = aggs.end() - 1;
        }
        const bool mine = m_client && m_client->isOwnHandle(r.sender.uri);
        it->count++;
        it->fromMe = it->fromMe || mine;
        it->senders << (mine ? i18n("You") : (m_chats ? m_chats->nameForHandle(r.sender.uri) : r.sender.displayName()));
    }
    QVariantList out;
    for (const auto &a : aggs) {
        out << QVariantMap{{u"glyph"_s, a.glyph}, {u"kind"_s, a.kind}, {u"count"_s, a.count}, {u"fromMe"_s, a.fromMe}, {u"senders"_s, a.senders.join(", "_L1)}};
    }
    return out;
}

QVariantList MessageListModel::attachments(const Message &m) const
{
    QVariantList out;
    const auto atts = m.attachments();
    for (const auto &a : atts) {
        const QString path = m_paths.value(a.id);
        out << QVariantMap{
            {u"id"_s, a.id},
            {u"mime"_s, a.mime},
            {u"filename"_s, a.filename},
            {u"size"_s, a.size},
            {u"sizeText"_s, formatSize(a.size)},
            {u"isImage"_s, a.isImage()},
            {u"localPath"_s, path},
            {u"url"_s, path.isEmpty() ? QUrl() : QUrl::fromLocalFile(path)},
            {u"downloading"_s, m_downloading.contains(a.id)},
            {u"error"_s, m_downloadErrors.value(a.id)},
        };
    }
    return out;
}

QVariantMap MessageListModel::attachmentState(const QString &id) const
{
    if (id.isEmpty()) {
        return {};
    }
    const QString path = m_paths.value(id);
    return {
        {u"id"_s, id},
        {u"localPath"_s, path},
        {u"downloading"_s, m_downloading.contains(id)},
        {u"error"_s, m_downloadErrors.value(id)},
    };
}

QVariantMap MessageListModel::link(const Message &m) const
{
    const auto l = m.link();
    if (!l) {
        return {};
    }
    const QUrl url(l->url.trimmed());
    QString host = url.host();
    if (host.startsWith("www."_L1)) {
        host = host.mid(4);
    }
    return {
        {u"url"_s, l->url.trimmed()},
        {u"title"_s, l->title.trimmed()},
        {u"summary"_s, l->summary.trimmed()},
        {u"siteName"_s, l->siteName.trimmed()},
        {u"host"_s, host},
        {u"openable"_s, url.isValid() && (url.scheme() == "http"_L1 || url.scheme() == "https"_L1)},
        {u"image"_s, attachmentState(l->imageAttachmentId)},
        {u"icon"_s, attachmentState(l->iconAttachmentId)},
    };
}

const Message *MessageListModel::findMessage(const QString &id) const
{
    for (const auto &m : m_messages) {
        if (m.id == id) {
            return &m;
        }
    }
    return nullptr;
}

QString MessageListModel::linkify(const QString &plain)
{
    // Match on the plain text (escaping first would turn a following `"` into
    // `&quot;` and pull it into the URL), then escape each piece.
    static const QRegularExpression url(u"https?://[^\\s<>\"]+"_s);
    QString html;
    qsizetype pos = 0;
    auto it = url.globalMatch(plain);
    while (it.hasNext()) {
        const auto match = it.next();
        QString link = match.captured();
        // Sentence punctuation after a URL is not part of it; a `)` only
        // when it does not close a `(` inside the URL (Wikipedia links).
        while (!link.isEmpty()) {
            const QChar c = link.back();
            if (c == u'.' || c == u',' || c == u';' || c == u':' || c == u'!' || c == u'?' || c == u'\'') {
                link.chop(1);
            } else if (c == u')' && link.count(u'(') < link.count(u')')) {
                link.chop(1);
            } else {
                break;
            }
        }
        if (link.endsWith("://"_L1)) {
            continue; // nothing left but the scheme
        }
        html += plain.mid(pos, match.capturedStart() - pos).toHtmlEscaped();
        const QString escaped = link.toHtmlEscaped();
        html += "<a href=\""_L1 + escaped + "\">"_L1 + escaped + "</a>"_L1;
        pos = match.capturedStart() + link.size();
    }
    html += plain.mid(pos).toHtmlEscaped();
    html.replace(u'\n', "<br>"_L1);
    return html;
}

QString MessageListModel::richText(const Message &m)
{
    QString out;
    bool any = false;
    for (const auto &p : m.parts) {
        if (p.type == MessagePart::Type::Text) {
            out += linkify(p.text);
            any = true;
        } else if (p.type == MessagePart::Type::Mention) {
            out += "<b>"_L1 + p.text.toHtmlEscaped() + "</b>"_L1;
            any = true;
        }
    }
    if (!any && !m.text.isEmpty()) {
        out = linkify(m.text);
    }
    return out;
}

bool MessageListModel::isEmojiOnly(const QString &text)
{
    const QString t = text.trimmed();
    if (t.isEmpty() || t.size() > 16) {
        return false;
    }
    int glyphs = 0;
    const auto ucs = t.toUcs4();
    for (const char32_t c : ucs) {
        if (c == 0x200D || c == 0xFE0F || (c >= 0x1F3FB && c <= 0x1F3FF)) {
            continue; // joiner, variation selector, skin tone
        }
        const auto cat = QChar::category(c);
        if (cat != QChar::Symbol_Other && !(c >= 0x1F000)) {
            return false;
        }
        ++glyphs;
    }
    return glyphs > 0 && glyphs <= 3;
}

QVariant MessageListModel::data(const QModelIndex &index, int role) const
{
    if (!checkIndex(index, CheckIndexOption::IndexIsValid | CheckIndexOption::ParentIsInvalid)) {
        return {};
    }
    const int row = index.row();
    const Message &m = m_messages.at(row);
    switch (role) {
    case Qt::DisplayRole:
    case TextRole:
        return m.text;
    case MessageIdRole:
        return m.id;
    case RichTextRole:
        return richText(m);
    case HasTextRole:
        return !m.unsent && !m.text.trimmed().isEmpty();
    case IsEmojiOnlyRole:
        return !m.unsent && m.attachments().isEmpty() && isEmojiOnly(m.text);
    case SenderRole:
        return m.sender.uri;
    case SenderNameRole:
        return senderName(m);
    case IsFromMeRole:
        return m.isFromMe;
    case TimestampRole:
        return m.sentAt;
    case TimeRole:
        return QLocale().toString(QDateTime::fromMSecsSinceEpoch(m.sentAt).time(), QLocale::ShortFormat);
    case ShowSeparatorRole:
        return showSeparator(row);
    case SeparatorLabelRole:
        return formatSeparatorTime(m.sentAt);
    case FirstInGroupRole:
        return firstInGroup(row);
    case LastInGroupRole:
        return lastInGroup(row);
    case ShowSenderNameRole:
        return isGroup() && !m.isFromMe && !m.unsent && firstInGroup(row);
    case ShowAvatarRole:
        return isGroup() && !m.isFromMe && !m.unsent && lastInGroup(row);
    case AvatarColorRole:
        return avatarColorFor(m.sender.uri);
    case AvatarSourceRole: {
        const auto *contacts = ContactResolver::instance();
        return contacts && !m.isFromMe ? contacts->avatarSource(m.sender.uri) : QString();
    }
    case InitialsRole:
        return initialsFor(senderName(m));
    case DeliveryRole:
        return deliveryToString(m.delivery);
    case DeliveryLabelRole:
        return deliveryLabel(row);
    case EditedRole:
        return m.edited;
    case UnsentRole:
        return m.unsent;
    case VerifiedRole:
        return m.verified;
    case ReactionsRole:
        return reactions(m);
    case MyReactionRole:
        return myReaction(m);
    case AttachmentsRole:
        return attachments(m);
    case HasAttachmentsRole:
        return !m.attachments().isEmpty();
    case ReplyToRole:
        return m.replyTo;
    case ReplyToTextRole: {
        if (m.replyTo.isEmpty()) {
            return QString();
        }
        const Message *orig = findMessage(m.replyTo);
        return orig ? orig->snippet() : i18n("Original message not loaded");
    }
    case ReplyToSenderRole: {
        const Message *orig = m.replyTo.isEmpty() ? nullptr : findMessage(m.replyTo);
        return orig ? senderName(*orig) : QString();
    }
    case SubjectRole:
        return m.subject;
    case EffectRole:
        return m.effect;
    case UnsupportedRole:
        return m.unsupported();
    case IsSmsRole:
        return m.isSms();
    case LinkRole:
        return link(m);
    case LinkOnlyRole:
        return m.isOnlyLink();
    }
    return {};
}

QStringList MessageListModel::phoneRecipients() const
{
    QStringList raw;
    if (m_chatId.startsWith("tel:"_L1)) {
        raw << m_chatId;
    } else if (m_chatId.startsWith("group-p:"_L1)) {
        raw = m_chatId.mid(8).split(u',', Qt::SkipEmptyParts);
    } else {
        return {};
    }
    QStringList out;
    for (const auto &h : std::as_const(raw)) {
        if (m_client && (m_client->isOwnHandle(h) || m_client->isOtherOwnHandle(h) || DaemonClient::sameHandle(h, m_client->smsNumber()))) {
            continue;
        }
        if (!h.startsWith("tel:"_L1)) {
            return {};
        }
        out << h;
    }
    return out;
}

bool MessageListModel::smsMode() const
{
    if (!m_client || !m_client->isSmsActive() || phoneRecipients().isEmpty()) {
        return false;
    }
    if (m_reachable >= 0) {
        return m_reachable == 0;
    }
    return !m_messages.isEmpty() && m_messages.first().isSms();
}

void MessageListModel::checkReachability()
{
    m_reachable = -1;
    const QStringList numbers = phoneRecipients();
    if (!m_client || !m_client->isSmsActive() || numbers.isEmpty()) {
        return;
    }
    QPointer<MessageListModel> self(this);
    const QString chat = m_chatId;
    m_client->lookupHandles(numbers, [self, chat](const QList<HandleReachability> &result) {
        if (!self || self->m_chatId != chat) {
            return;
        }
        const bool all = !result.isEmpty() && std::all_of(result.cbegin(), result.cend(), [](const HandleReachability &r) {
            return r.reachable;
        });
        self->m_reachable = all ? 1 : 0;
        Q_EMIT self->smsModeChanged();
    });
}

void MessageListModel::setChatId(const QString &chatId)
{
    if (chatId == m_chatId) {
        return;
    }
    m_chatId = chatId;
    if (!m_typing.isEmpty()) {
        m_typing.clear();
        Q_EMIT typingChanged();
    }
    Q_EMIT chatIdChanged();
    checkReachability();
    Q_EMIT smsModeChanged();
    reload();
}

void MessageListModel::setPageSize(int size)
{
    size = qBound(1, size, 500);
    if (size != m_pageSize) {
        m_pageSize = size;
        Q_EMIT pageSizeChanged();
    }
}

void MessageListModel::setLoading(bool loading)
{
    if (loading != m_loading) {
        m_loading = loading;
        Q_EMIT loadingChanged();
    }
}

void MessageListModel::setLastError(const QString &error)
{
    if (error != m_lastError) {
        m_lastError = error;
        Q_EMIT lastErrorChanged();
    }
}

void MessageListModel::reload()
{
    const quint64 gen = ++m_generation;
    beginResetModel();
    m_messages.clear();
    m_cursor.clear();
    m_lastOwnRow = -1;
    m_loaded = false;
    endResetModel();
    Q_EMIT countChanged();
    Q_EMIT canLoadMoreChanged();
    setLastError(QString());
    if (m_chatId.isEmpty() || !m_client || !m_client->isAvailable()) {
        setLoading(false);
        Q_EMIT loadingChanged();
        return;
    }
    setLoading(true);
    QPointer<MessageListModel> self(this);
    m_client->listMessages(m_chatId, QString(), uint(m_pageSize), [self, gen](const MessagePage &page) {
        if (!self || gen != self->m_generation) {
            return;
        }
        // Live messages may have been inserted meanwhile; the page includes them.
        self->beginResetModel();
        self->m_messages.clear();
        for (auto it = page.messages.crbegin(); it != page.messages.crend(); ++it) {
            self->m_messages.append(*it);
        }
        self->m_cursor = page.nextCursor;
        self->m_loaded = true;
        self->recomputeLastOwn();
        self->endResetModel();
        self->setLoading(false);
        Q_EMIT self->loadingChanged();
        Q_EMIT self->countChanged();
        Q_EMIT self->canLoadMoreChanged();
    }, [self, gen](const QString &, const QString &message) {
        if (!self || gen != self->m_generation) {
            return;
        }
        self->setLastError(message);
        self->setLoading(false);
    });
}

bool MessageListModel::canFetchMore(const QModelIndex &parent) const
{
    return !parent.isValid() && canLoadMore() && !m_loading;
}

void MessageListModel::fetchMore(const QModelIndex &parent)
{
    if (!parent.isValid()) {
        loadOlder();
    }
}

void MessageListModel::loadOlder()
{
    if (m_loading || !canLoadMore() || !m_client) {
        return;
    }
    const quint64 gen = m_generation;
    setLoading(true);
    QPointer<MessageListModel> self(this);
    m_client->listMessages(m_chatId, m_cursor, uint(m_pageSize), [self, gen](const MessagePage &page) {
        if (!self || gen != self->m_generation) {
            return;
        }
        QList<Message> older;
        for (auto it = page.messages.crbegin(); it != page.messages.crend(); ++it) {
            if (self->rowOf(it->id) < 0) {
                older.append(*it);
            }
        }
        const int first = int(self->m_messages.size());
        if (!older.isEmpty()) {
            self->beginInsertRows(QModelIndex(), first, first + int(older.size()) - 1);
            self->m_messages.append(older);
            self->recomputeLastOwn();
            self->endInsertRows();
            // The previously oldest row's separator/grouping depend on the new neighbours.
            self->touchRows(first - 1, first - 1);
        }
        self->m_cursor = page.nextCursor;
        self->setLoading(false);
        Q_EMIT self->countChanged();
        Q_EMIT self->canLoadMoreChanged();
    }, [self, gen](const QString &, const QString &message) {
        if (!self || gen != self->m_generation) {
            return;
        }
        self->setLastError(message);
        self->setLoading(false);
    });
}

void MessageListModel::recomputeLastOwn()
{
    m_lastOwnRow = -1;
    for (int i = 0; i < m_messages.size(); ++i) {
        if (m_messages.at(i).isFromMe && !m_messages.at(i).unsent) {
            m_lastOwnRow = i;
            break;
        }
    }
}

void MessageListModel::touchRows(int from, int to)
{
    from = qMax(0, from);
    to = qMin(int(m_messages.size()) - 1, to);
    if (from <= to) {
        Q_EMIT dataChanged(index(from), index(to));
    }
}

int MessageListModel::rowOf(const QString &messageId) const
{
    for (int i = 0; i < m_messages.size(); ++i) {
        if (m_messages.at(i).id == messageId) {
            return i;
        }
    }
    return -1;
}

QVariantMap MessageListModel::get(int row) const
{
    QVariantMap out;
    if (row < 0 || row >= m_messages.size()) {
        return out;
    }
    const auto roles = roleNames();
    for (auto it = roles.cbegin(); it != roles.cend(); ++it) {
        out.insert(QString::fromLatin1(it.value()), data(index(row), it.key()));
    }
    return out;
}

void MessageListModel::onMessageReceived(const QString &chatId, const Message &msg)
{
    if (chatId != m_chatId || m_chatId.isEmpty()) {
        return;
    }
    if (!msg.isFromMe && m_typing.remove(msg.sender.uri)) {
        Q_EMIT typingChanged();
    }
    if (rowOf(msg.id) >= 0) {
        onMessageUpdated(chatId, msg);
        return;
    }
    adoptOutgoingPaths(chatId, msg);
    int pos = 0;
    while (pos < m_messages.size() && m_messages.at(pos).sentAt > msg.sentAt) {
        ++pos;
    }
    const int oldLastOwn = m_lastOwnRow;
    beginInsertRows(QModelIndex(), pos, pos);
    m_messages.insert(pos, msg);
    recomputeLastOwn();
    endInsertRows();
    Q_EMIT countChanged();
    touchRows(pos - 1, pos + 1);
    if (oldLastOwn >= 0) {
        const int shifted = oldLastOwn >= pos ? oldLastOwn + 1 : oldLastOwn;
        touchRows(shifted, shifted);
    }
    if (pos == 0) {
        Q_EMIT newestMessageAdded(msg.isFromMe);
    }
}

void MessageListModel::onMessageUpdated(const QString &chatId, const Message &msg)
{
    if (chatId != m_chatId) {
        return;
    }
    const int row = rowOf(msg.id);
    if (row < 0) {
        return;
    }
    const int oldLastOwn = m_lastOwnRow;
    m_messages[row] = msg;
    recomputeLastOwn();
    touchRows(row - 1, row + 1);
    if (oldLastOwn != m_lastOwnRow) {
        touchRows(oldLastOwn, oldLastOwn);
        touchRows(m_lastOwnRow, m_lastOwnRow);
    }
    // Replies quoting this message show its new text.
    for (int i = 0; i < m_messages.size(); ++i) {
        if (m_messages.at(i).replyTo == msg.id) {
            const auto idx = index(i);
            Q_EMIT dataChanged(idx, idx, {ReplyToTextRole});
        }
    }
}

void MessageListModel::onTyping(const QString &chatId, const QString &handle, bool typing)
{
    if (chatId != m_chatId) {
        return;
    }
    const bool changed = typing ? !m_typing.contains(handle) : m_typing.contains(handle);
    if (typing) {
        m_typing.insert(handle);
    } else {
        m_typing.remove(handle);
    }
    if (changed) {
        Q_EMIT typingChanged();
    }
}

void MessageListModel::sendText(const QString &text, const QString &replyTo)
{
    if (!m_client || m_chatId.isEmpty() || text.trimmed().isEmpty()) {
        return;
    }
    QPointer<MessageListModel> self(this);
    const QString chatId = m_chatId;
    m_client->sendTextThen(chatId, {}, text, replyTo, [self, chatId, text, replyTo](bool ok, const QString &error) {
        if (self && !ok) {
            Q_EMIT self->sendTextFailed(chatId, text, replyTo, error);
        }
    });
}

void MessageListModel::sendFile(const QUrl &file)
{
    if (!m_client || m_chatId.isEmpty() || file.isEmpty()) {
        return;
    }
    const QString local = QFileInfo(file.isLocalFile() ? file.toLocalFile() : file.toString(QUrl::PreferLocalFile)).absoluteFilePath();
    const quint64 token = ++m_outgoingSeq;
    m_outgoing.append({token, m_chatId, QFileInfo(local).fileName(), local, QFileInfo(local).size(), false});
    QPointer<MessageListModel> self(this);
    m_client->sendFileThen(m_chatId, {}, file, QString(), [self, token, local](bool ok, const SentMessage &sent, const QString &) {
        if (!self) {
            return;
        }
        self->m_outgoing.removeIf([token](const OutgoingFile &o) {
            return o.token == token;
        });
        // Our own image shows at once from the file we picked (SendFile's
        // attachment is `<message id>:0`); no DownloadAttachment round trip.
        const QString attachmentId = sent.id + ":0"_L1;
        if (ok && !self->m_paths.contains(attachmentId)) {
            self->m_paths.insert(attachmentId, local);
            self->m_downloadErrors.remove(attachmentId);
            self->touchAttachment(attachmentId);
        }
    });
}

void MessageListModel::adoptOutgoingPaths(const QString &chatId, const Message &msg)
{
    if (!msg.isFromMe || m_outgoing.isEmpty()) {
        return;
    }
    const auto atts = msg.attachments();
    for (const auto &a : atts) {
        if (m_paths.contains(a.id)) {
            continue;
        }
        // Two in-flight files of the same name: the size tells them apart.
        for (auto &o : m_outgoing) {
            if (!o.adopted && o.chatId == chatId && o.fileName == a.filename && (a.size <= 0 || o.size == a.size)) {
                o.adopted = true;
                m_paths.insert(a.id, o.localPath);
                break;
            }
        }
    }
}

void MessageListModel::react(const QString &messageId, const QString &kind)
{
    const int row = rowOf(messageId);
    if (!m_client || row < 0 || m_messages.at(row).isSms()) {
        return; // no tapbacks on text messages
    }
    if (!m_client->isStatusKnown()) {
        // Our handles are not known yet, so "is this tapback already mine"
        // cannot be answered: read the status first, then decide.
        QPointer<MessageListModel> self(this);
        const QString chatId = m_chatId;
        m_client->refreshStatusThen([self, chatId, messageId, kind] {
            if (self && self->m_chatId == chatId && self->m_client && self->m_client->isStatusKnown()) {
                self->react(messageId, kind);
            }
        });
        return;
    }
    const bool remove = myReaction(m_messages.at(row)) == kind;
    m_client->react(m_chatId, messageId, kind, remove);
}

void MessageListModel::edit(const QString &messageId, const QString &text)
{
    if (!m_client || text.trimmed().isEmpty()) {
        return;
    }
    QPointer<MessageListModel> self(this);
    const QString chatId = m_chatId;
    m_client->editThen(chatId, messageId, text, [self, chatId, messageId, text](bool ok, const QString &error) {
        if (self && !ok) {
            Q_EMIT self->editFailed(chatId, messageId, text, error);
        }
    });
}

void MessageListModel::unsend(const QString &messageId)
{
    if (m_client) {
        m_client->unsend(m_chatId, messageId);
    }
}

void MessageListModel::setTyping(bool typing)
{
    // Text messages have no typing indicators.
    if (m_client && !m_chatId.isEmpty() && !smsMode()) {
        m_client->setTyping(m_chatId, typing);
    }
}

void MessageListModel::markRead()
{
    if (m_client && !m_chatId.isEmpty()) {
        m_client->markRead(m_chatId, QString());
    }
}

void MessageListModel::downloadAttachment(const QString &attachmentId)
{
    if (!m_client || m_paths.contains(attachmentId) || m_downloading.contains(attachmentId)) {
        return;
    }
    m_downloading.insert(attachmentId);
    m_downloadErrors.remove(attachmentId);
    touchAttachment(attachmentId);
    QPointer<MessageListModel> self(this);
    m_client->downloadAttachment(attachmentId, [self, attachmentId](const QString &path) {
        if (!self) {
            return;
        }
        self->m_downloading.remove(attachmentId);
        self->m_downloadRetried.remove(attachmentId);
        self->m_paths.insert(attachmentId, path);
        self->touchAttachment(attachmentId);
        Q_EMIT self->attachmentReady(attachmentId, path);
        if (self->m_openWhenReady.remove(attachmentId)) {
            QDesktopServices::openUrl(QUrl::fromLocalFile(path));
        }
    }, [self, attachmentId](const QString &name, const QString &message) {
        if (!self) {
            return;
        }
        if (DaemonClient::isTimeout(name) && !self->m_downloadRetried.contains(attachmentId)) {
            // A big file can outlast the call: the daemon keeps downloading,
            // so stay "downloading" and ask again (once) a little later.
            self->m_downloadRetried.insert(attachmentId);
            QTimer::singleShot(self->m_downloadRetryMs, self, [self, attachmentId] {
                self->m_downloading.remove(attachmentId);
                self->downloadAttachment(attachmentId);
            });
            return;
        }
        self->m_downloadRetried.remove(attachmentId);
        self->m_downloading.remove(attachmentId);
        self->m_downloadErrors.insert(attachmentId, message);
        self->m_openWhenReady.remove(attachmentId);
        self->touchAttachment(attachmentId);
    });
}

void MessageListModel::reloadAttachment(const QString &attachmentId)
{
    if (m_reloaded.contains(attachmentId) || !m_paths.contains(attachmentId)) {
        return;
    }
    m_reloaded.insert(attachmentId);
    m_paths.remove(attachmentId);
    downloadAttachment(attachmentId);
}

void MessageListModel::touchAttachment(const QString &attachmentId)
{
    for (int i = 0; i < m_messages.size(); ++i) {
        const auto &m = m_messages.at(i);
        const auto atts = m.attachments();
        for (const auto &a : atts) {
            if (a.id == attachmentId) {
                const auto idx = index(i);
                Q_EMIT dataChanged(idx, idx, {AttachmentsRole});
            }
        }
        if (const auto l = m.link(); l && (l->imageAttachmentId == attachmentId || l->iconAttachmentId == attachmentId)) {
            const auto idx = index(i);
            Q_EMIT dataChanged(idx, idx, {LinkRole});
        }
    }
}

void MessageListModel::openAttachment(const QString &attachmentId)
{
    const QString path = m_paths.value(attachmentId);
    if (!path.isEmpty()) {
        QDesktopServices::openUrl(QUrl::fromLocalFile(path));
        return;
    }
    m_openWhenReady.insert(attachmentId);
    downloadAttachment(attachmentId);
}

void MessageListModel::copyText(const QString &messageId)
{
    const Message *m = findMessage(messageId);
    if (m && QGuiApplication::clipboard()) {
        QGuiApplication::clipboard()->setText(m->text);
    }
}

void MessageListModel::copyLink(const QString &messageId)
{
    const Message *m = findMessage(messageId);
    const auto l = m ? m->link() : std::nullopt;
    if (l && QGuiApplication::clipboard()) {
        QGuiApplication::clipboard()->setText(l->url.trimmed());
    }
}

} // namespace km
