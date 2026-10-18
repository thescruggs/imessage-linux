// SPDX-License-Identifier: SSPL-1.0
#include "chatlistmodel.h"
#include "contactresolver.h"
#include "daemonclient.h"

#include <KLocalizedString>

#include <QLoggingCategory>
#include <QUrl>

#include <algorithm>

using namespace Qt::StringLiterals;

namespace km
{

ChatListModel::ChatListModel(DaemonClient *client, QObject *parent)
    : QAbstractListModel(parent)
    , m_client(client)
{
    connect(client, &DaemonClient::chatUpdated, this, &ChatListModel::onChatUpdated);
    connect(client, &DaemonClient::messageReceived, this, &ChatListModel::onMessageReceived);
    connect(client, &DaemonClient::messageUpdated, this, &ChatListModel::onMessageUpdated);
    connect(client, &DaemonClient::typingChanged, this, &ChatListModel::onTyping);
    m_refreshTimer.setSingleShot(true);
    m_refreshTimer.setInterval(250);
    connect(&m_refreshTimer, &QTimer::timeout, this, &ChatListModel::refresh);
    connect(client, &DaemonClient::resyncRequested, this, &ChatListModel::scheduleRefresh);
    // Login finishing (needs_login → connected) may bring history along.
    connect(client, &DaemonClient::connectionStateChanged, this, [this] {
        if (m_client && m_client->connectionState() == "connected"_L1 && m_loaded) {
            scheduleRefresh();
        }
    });
    auto bump = [this] {
        ++m_revision;
        Q_EMIT revisionChanged();
    };
    connect(this, &QAbstractItemModel::dataChanged, this, bump);
    connect(this, &QAbstractItemModel::modelReset, this, bump);
    connect(this, &QAbstractItemModel::rowsInserted, this, bump);
    connect(this, &QAbstractItemModel::rowsRemoved, this, bump);
    connect(this, &QAbstractItemModel::rowsMoved, this, bump);
    // Names, initials and photos all come from the contacts.
    if (auto *contacts = ContactResolver::instance()) {
        connect(contacts, &ContactResolver::contactsChanged, this, [this] {
            if (!m_entries.isEmpty()) {
                Q_EMIT dataChanged(index(0), index(int(m_entries.size()) - 1));
            }
        });
    }
    if (client->isAvailable()) {
        refresh();
    }
}

int ChatListModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : int(m_entries.size());
}

QHash<int, QByteArray> ChatListModel::roleNames() const
{
    return {
        {ChatIdRole, "chatId"},
        {NameRole, "name"},
        {SnippetRole, "snippet"},
        {TimeRole, "time"},
        {TimestampRole, "timestamp"},
        {UnreadCountRole, "unreadCount"},
        {HasUnreadRole, "hasUnread"},
        {InitialsRole, "initials"},
        {AvatarColorRole, "avatarColor"},
        {IsGroupRole, "isGroup"},
        {TypingRole, "typing"},
        {ParticipantsRole, "participants"},
        {ParticipantUrisRole, "participantUris"},
        {SearchTextRole, "searchText"},
        {LastMessageFromMeRole, "lastMessageFromMe"},
        {AvatarSourceRole, "avatarSource"},
        {ParticipantAvatarsRole, "participantAvatars"},
        {ParticipantAddressesRole, "participantAddresses"},
        {ParticipantSharedNamesRole, "participantSharedNames"},
        {ParticipantSharedPhotosRole, "participantSharedPhotos"},
        {IconSourceRole, "iconSource"},
        {PinnedRole, "pinned"},
        {PinOrderRole, "pinOrder"},
        {SectionRole, "section"},
    };
}

namespace
{
/// `image://attachment/…` for a group's photo, "" without one.
QString groupIconSource(const Chat &c)
{
    if (!c.isGroup || c.iconPath.isEmpty()) {
        return {};
    }
    return u"image://attachment/%1"_s.arg(QString::fromLatin1(QUrl::toPercentEncoding(c.iconPath)));
}
}

QVariant ChatListModel::data(const QModelIndex &index, int role) const
{
    if (!checkIndex(index, CheckIndexOption::IndexIsValid | CheckIndexOption::ParentIsInvalid)) {
        return {};
    }
    const Entry &e = m_entries.at(index.row());
    const Chat &c = e.chat;
    switch (role) {
    case Qt::DisplayRole:
    case NameRole:
        return c.title();
    case ChatIdRole:
        return c.id;
    case SnippetRole:
        return e.snippet;
    case TimeRole:
        return formatListTime(c.lastMessageAt);
    case TimestampRole:
        return c.lastMessageAt;
    case UnreadCountRole:
        return shownUnread(c);
    case HasUnreadRole:
        return shownUnread(c) > 0;
    case InitialsRole:
        return c.isGroup ? QString() : initialsFor(c.title());
    case AvatarColorRole:
        return avatarColorFor(c.id);
    case IsGroupRole:
        return c.isGroup;
    case TypingRole:
        return !e.typing.isEmpty();
    case ParticipantsRole: {
        QStringList names;
        for (const auto &p : c.participants) {
            names << p.displayName();
        }
        return names;
    }
    case ParticipantUrisRole: {
        QStringList uris;
        for (const auto &p : c.participants) {
            uris << p.uri;
        }
        return uris;
    }
    case SearchTextRole: {
        QStringList parts{c.title(), e.snippet};
        for (const auto &p : c.participants) {
            parts << p.address() << p.display << p.displayName(); // incl. the contact name
        }
        return parts.join(u'\n');
    }
    case LastMessageFromMeRole:
        return e.lastFromMe;
    case AvatarSourceRole: {
        if (c.isGroup) {
            return groupIconSource(c);
        }
        const auto *contacts = ContactResolver::instance();
        return contacts && c.participants.size() == 1 ? contacts->avatarSource(c.participants.first().uri) : QString();
    }
    case IconSourceRole:
        return groupIconSource(c);
    case PinnedRole:
        return c.pinned;
    case PinOrderRole:
        return c.pinned ? c.pinOrder : -1;
    case SectionRole:
        // No sections at all while nothing is pinned (ListView shows a header
        // whenever the value changes from the previous row's).
        if (!m_hasPinned) {
            return QString();
        }
        return c.pinned ? i18nc("chat list section", "Pinned") : i18nc("chat list section: the chats that are not pinned", "Conversations");
    case ParticipantSharedNamesRole: {
        const auto *contacts = ContactResolver::instance();
        QStringList names;
        for (const auto &p : c.participants) {
            names << (contacts ? contacts->sharedNameFor(p.uri) : QString());
        }
        return names;
    }
    case ParticipantSharedPhotosRole: {
        const auto *contacts = ContactResolver::instance();
        QStringList sources;
        for (const auto &p : c.participants) {
            sources << (contacts ? contacts->sharedPhotoSource(p.uri) : QString());
        }
        return sources;
    }
    case ParticipantAddressesRole: {
        QStringList addresses;
        for (const auto &p : c.participants) {
            addresses << p.formattedAddress();
        }
        return addresses;
    }
    case ParticipantAvatarsRole: {
        const auto *contacts = ContactResolver::instance();
        QStringList sources;
        for (const auto &p : c.participants) {
            sources << (contacts ? contacts->avatarSource(p.uri) : QString());
        }
        return sources;
    }
    }
    return {};
}

int ChatListModel::totalUnread() const
{
    int n = 0;
    for (const auto &e : m_entries) {
        n += shownUnread(e.chat);
    }
    return n;
}

void ChatListModel::scheduleRefresh()
{
    // Do not push an already pending refresh further out.
    if (!m_refreshTimer.isActive()) {
        m_refreshTimer.start();
    }
}

void ChatListModel::refresh()
{
    m_refreshTimer.stop();
    if (!m_client || !m_client->isAvailable()) {
        return;
    }
    const quint64 gen = ++m_generation;
    m_loading = true;
    Q_EMIT loadingChanged();
    QPointer<ChatListModel> self(this);
    m_client->listChats(500, 0, [self, gen](const QList<Chat> &chats) {
        if (!self || gen != self->m_generation) {
            return;
        }
        // Keep snippets/typing we already have for chats that survive the reload.
        QHash<QString, Entry> old;
        for (const auto &e : std::as_const(self->m_entries)) {
            old.insert(e.chat.id, e);
        }
        // Only chats whose activity moved (or that have no snippet yet) need
        // their newest message fetched again.
        QStringList stale;
        self->beginResetModel();
        self->m_entries.clear();
        for (const auto &c : chats) {
            const auto prev = old.constFind(c.id);
            Entry e = prev != old.cend() ? *prev : Entry{};
            if (prev == old.cend() || e.lastMessageId.isEmpty() || e.chat.lastMessageAt != c.lastMessageAt) {
                stale << c.id;
            }
            e.chat = c;
            self->clampOwnUnread(c);
            self->m_entries.append(e);
        }
        std::stable_sort(self->m_entries.begin(), self->m_entries.end(), [](const Entry &a, const Entry &b) {
            return precedes(a.chat, b.chat);
        });
        self->endResetModel();
        self->updateHasPinned();
        self->m_loading = false;
        self->m_loaded = true;
        Q_EMIT self->loadingChanged();
        Q_EMIT self->countChanged();
        Q_EMIT self->totalUnreadChanged();
        for (const auto &e : std::as_const(self->m_entries)) {
            Q_EMIT self->unreadChanged(e.chat.id, self->shownUnread(e.chat));
        }
        for (const auto &id : std::as_const(stale)) {
            self->fetchSnippet(id);
        }
    }, [self, gen](const QString &, const QString &message) {
        if (!self || gen != self->m_generation) {
            return;
        }
        qWarning() << "ListChats failed:" << message;
        self->m_loading = false;
        Q_EMIT self->loadingChanged();
    });
}

void ChatListModel::fetchSnippet(const QString &chatId)
{
    if (!m_client) {
        return;
    }
    QPointer<ChatListModel> self(this);
    m_client->listMessages(chatId, QString(), 1, [self, chatId](const MessagePage &page) {
        if (!self || page.messages.isEmpty()) {
            return;
        }
        const int row = self->indexOf(chatId);
        if (row < 0) {
            return;
        }
        Entry &e = self->m_entries[row];
        const Message &m = page.messages.last();
        // A live message may have overtaken this reply.
        if (!e.lastMessageId.isEmpty() && e.lastMessageId != m.id && e.chat.lastMessageAt > m.sentAt) {
            return;
        }
        self->applyMessage(e, m);
        self->emitRowChanged(row);
    });
}

void ChatListModel::applyMessage(Entry &e, const Message &msg)
{
    e.snippet = msg.snippet();
    e.lastMessageId = msg.id;
    e.lastFromMe = m_client ? m_client->isFromMyself(msg) : msg.isFromMe;
}

int ChatListModel::indexOf(const QString &chatId) const
{
    for (int i = 0; i < m_entries.size(); ++i) {
        if (m_entries.at(i).chat.id == chatId) {
            return i;
        }
    }
    return -1;
}

std::optional<Chat> ChatListModel::chat(const QString &chatId) const
{
    const int row = indexOf(chatId);
    if (row < 0) {
        return std::nullopt;
    }
    return m_entries.at(row).chat;
}

QVariantMap ChatListModel::get(const QString &chatId) const
{
    const int row = indexOf(chatId);
    QVariantMap out;
    if (row < 0) {
        return out;
    }
    const auto roles = roleNames();
    for (auto it = roles.cbegin(); it != roles.cend(); ++it) {
        out.insert(QString::fromLatin1(it.value()), data(index(row), it.key()));
    }
    return out;
}

QString ChatListModel::titleFor(const QString &chatId) const
{
    const int row = indexOf(chatId);
    if (row >= 0) {
        return m_entries.at(row).chat.title();
    }
    return Handle{chatId, {}}.displayName();
}

QString ChatListModel::backgroundFor(const QString &chatId) const
{
    const int row = indexOf(chatId);
    if (row < 0 || m_entries.at(row).chat.backgroundPath.isEmpty()) {
        return QString();
    }
    return QUrl::fromLocalFile(m_entries.at(row).chat.backgroundPath).toString();
}

int ChatListModel::unreadFor(const QString &chatId) const
{
    const int row = indexOf(chatId);
    return row < 0 ? 0 : shownUnread(m_entries.at(row).chat);
}

int ChatListModel::shownUnread(const Chat &c) const
{
    return qMax(0, int(c.unreadCount) - m_ownUnread.value(c.id));
}

void ChatListModel::clampOwnUnread(const Chat &c)
{
    const auto it = m_ownUnread.find(c.id);
    if (it == m_ownUnread.end()) {
        return;
    }
    if (int(c.unreadCount) <= 0) {
        m_ownUnread.erase(it);
    } else if (*it > int(c.unreadCount)) {
        *it = int(c.unreadCount);
    }
}

void ChatListModel::setOwnUnread(const QString &chatId, int n)
{
    const int row = indexOf(chatId);
    const int before = row >= 0 ? shownUnread(m_entries.at(row).chat) : 0;
    if (n > 0) {
        m_ownUnread.insert(chatId, n);
    } else {
        m_ownUnread.remove(chatId);
    }
    if (row < 0) {
        return;
    }
    const int after = shownUnread(m_entries.at(row).chat);
    if (after != before) {
        emitRowChanged(row);
        Q_EMIT totalUnreadChanged();
        Q_EMIT unreadChanged(chatId, after);
    }
}

bool ChatListModel::isGroupChat(const QString &chatId) const
{
    const int row = indexOf(chatId);
    return row >= 0 ? m_entries.at(row).chat.isGroup : chatId.startsWith("group"_L1);
}

QString ChatListModel::nameForHandle(const QString &uri) const
{
    for (const auto &e : m_entries) {
        for (const auto &p : e.chat.participants) {
            if (p.uri == uri) {
                return p.displayName();
            }
        }
    }
    return Handle{uri, {}}.displayName();
}

void ChatListModel::emitRowChanged(int row)
{
    const auto idx = index(row);
    Q_EMIT dataChanged(idx, idx);
}

bool ChatListModel::precedes(const Chat &a, const Chat &b)
{
    if (a.pinned != b.pinned) {
        return a.pinned;
    }
    if (a.pinned) {
        return a.pinOrder < b.pinOrder;
    }
    return a.lastMessageAt > b.lastMessageAt;
}

int ChatListModel::reposition(int row)
{
    // The list is sorted (precedes()); ours goes before others that tie with it
    // (equal time; a pinned chat's activity does not move it at all).
    const Chat &c = m_entries.at(row).chat;
    int newRow = 0;
    for (int i = 0; i < m_entries.size(); ++i) {
        if (i != row && precedes(m_entries.at(i).chat, c)) {
            ++newRow;
        }
    }
    if (newRow == row) {
        return row;
    }
    // beginMoveRows wants the destination as "insert before" in the old numbering.
    beginMoveRows(QModelIndex(), row, row, QModelIndex(), newRow > row ? newRow + 1 : newRow);
    m_entries.move(row, newRow);
    endMoveRows();
    return newRow;
}

void ChatListModel::resort()
{
    // Selection sort with one row move per misplaced row: stable, keeps the
    // views' items (and the filter proxy) in step; pins change rarely.
    for (int target = 0; target < m_entries.size(); ++target) {
        int best = target;
        for (int i = target + 1; i < m_entries.size(); ++i) {
            if (precedes(m_entries.at(i).chat, m_entries.at(best).chat)) {
                best = i;
            }
        }
        if (best != target) {
            beginMoveRows(QModelIndex(), best, best, QModelIndex(), target);
            m_entries.move(best, target);
            endMoveRows();
        }
    }
}

void ChatListModel::updateHasPinned()
{
    const bool any = std::any_of(m_entries.cbegin(), m_entries.cend(), [](const Entry &e) {
        return e.chat.pinned;
    });
    if (any != m_hasPinned) {
        m_hasPinned = any;
        if (!m_entries.isEmpty()) {
            Q_EMIT dataChanged(index(0), index(int(m_entries.size()) - 1), {SectionRole});
        }
        Q_EMIT hasPinnedChanged();
    }
}

void ChatListModel::upsert(const Chat &chat, bool fetchSnippetIfNew)
{
    int row = indexOf(chat.id);
    if (row < 0) {
        int pos = 0;
        while (pos < m_entries.size() && !precedes(chat, m_entries.at(pos).chat)) {
            ++pos;
        }
        beginInsertRows(QModelIndex(), pos, pos);
        m_entries.insert(pos, Entry{chat, {}, {}, false, {}});
        endInsertRows();
        updateHasPinned();
        Q_EMIT countChanged();
        Q_EMIT totalUnreadChanged();
        Q_EMIT unreadChanged(chat.id, shownUnread(chat));
        if (fetchSnippetIfNew) {
            fetchSnippet(chat.id);
        }
        return;
    }
    Entry &e = m_entries[row];
    const int unreadBefore = shownUnread(e.chat);
    if (chat.unreadCount < e.chat.unreadCount) {
        clampOwnUnread(chat);
    }
    const bool unreadChangedNow = unreadBefore != shownUnread(chat);
    // Never move last activity backwards (a ChatUpdated can lag a MessageReceived).
    const qint64 last = qMax(e.chat.lastMessageAt, chat.lastMessageAt);
    const bool pinChanged = e.chat.pinned != chat.pinned || (chat.pinned && e.chat.pinOrder != chat.pinOrder);
    e.chat = chat;
    e.chat.lastMessageAt = last;
    if (pinChanged) {
        // Pinning renumbers other pins too (each its own ChatUpdated), so the
        // rest of the list may be momentarily out of order: sort it all.
        resort();
        row = indexOf(chat.id);
        updateHasPinned();
    } else {
        row = reposition(row);
    }
    emitRowChanged(row);
    if (unreadChangedNow) {
        Q_EMIT totalUnreadChanged();
        Q_EMIT unreadChanged(chat.id, shownUnread(e.chat));
    }
}

void ChatListModel::onChatUpdated(const Chat &chat)
{
    const bool known = indexOf(chat.id) >= 0;
    upsert(chat, !known);
    // Activity without a MessageReceived we saw (e.g. unsend): refresh the snippet.
    if (known) {
        const int row = indexOf(chat.id);
        if (row >= 0 && m_entries.at(row).chat.lastMessageAt > 0 && m_entries.at(row).lastMessageId.isEmpty()) {
            fetchSnippet(chat.id);
        }
    }
}

void ChatListModel::onMessageReceived(const QString &chatId, const Message &msg)
{
    int row = indexOf(chatId);
    if (row < 0) {
        // Unknown chat: fetch it; ChatUpdated normally follows anyway.
        QPointer<ChatListModel> self(this);
        if (m_client) {
            m_client->getChat(chatId, [self, msg](const Chat &c) {
                if (!self) {
                    return;
                }
                self->upsert(c, false);
                const int r = self->indexOf(c.id);
                if (r >= 0) {
                    self->applyMessage(self->m_entries[r], msg);
                    self->emitRowChanged(r);
                }
            });
        }
        return;
    }
    Entry &e = m_entries[row];
    if (!msg.isFromMe) {
        e.typing.remove(msg.sender.uri);
    }
    if (msg.sentAt >= e.chat.lastMessageAt || e.lastMessageId.isEmpty()) {
        applyMessage(e, msg);
        e.chat.lastMessageAt = qMax(e.chat.lastMessageAt, msg.sentAt);
    }
    row = reposition(row);
    emitRowChanged(row);
}

void ChatListModel::onMessageUpdated(const QString &chatId, const Message &msg)
{
    const int row = indexOf(chatId);
    if (row < 0 || m_entries.at(row).lastMessageId != msg.id) {
        return;
    }
    applyMessage(m_entries[row], msg);
    emitRowChanged(row);
}

void ChatListModel::onTyping(const QString &chatId, const QString &handle, bool typing)
{
    const int row = indexOf(chatId);
    if (row < 0) {
        return;
    }
    Entry &e = m_entries[row];
    const bool before = !e.typing.isEmpty();
    if (typing) {
        e.typing.insert(handle);
    } else {
        e.typing.remove(handle);
    }
    if (before != !e.typing.isEmpty()) {
        const auto idx = index(row);
        Q_EMIT dataChanged(idx, idx, {TypingRole});
    }
}

// ---------------------------------------------------------------------------

ChatFilterModel::ChatFilterModel(ChatListModel *source, QObject *parent)
    : QSortFilterProxyModel(parent)
{
    setSourceModel(source);
    setFilterRole(ChatListModel::SearchTextRole);
    setFilterCaseSensitivity(Qt::CaseInsensitive);
    connect(this, &QAbstractItemModel::rowsInserted, this, &ChatFilterModel::countChanged);
    connect(this, &QAbstractItemModel::rowsRemoved, this, &ChatFilterModel::countChanged);
    connect(this, &QAbstractItemModel::modelReset, this, &ChatFilterModel::countChanged);
    connect(this, &QAbstractItemModel::layoutChanged, this, &ChatFilterModel::countChanged);
}

void ChatFilterModel::setFilterText(const QString &text)
{
    if (text == m_filterText) {
        return;
    }
    m_filterText = text;
    beginFilterChange();
    endFilterChange(QSortFilterProxyModel::Direction::Rows);
    Q_EMIT filterTextChanged();
    Q_EMIT countChanged();
}

QString ChatFilterModel::chatIdAt(int row) const
{
    return data(index(row, 0), ChatListModel::ChatIdRole).toString();
}

bool ChatFilterModel::filterAcceptsRow(int sourceRow, const QModelIndex &sourceParent) const
{
    if (m_filterText.trimmed().isEmpty()) {
        return true;
    }
    const auto idx = sourceModel()->index(sourceRow, 0, sourceParent);
    const QString hay = idx.data(ChatListModel::SearchTextRole).toString();
    const auto words = m_filterText.split(u' ', Qt::SkipEmptyParts);
    for (const auto &w : words) {
        if (!hay.contains(w, Qt::CaseInsensitive)) {
            return false;
        }
    }
    return true;
}

} // namespace km
