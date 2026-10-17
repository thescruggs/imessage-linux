// SPDX-License-Identifier: SSPL-1.0
#include "messagesearchmodel.h"
#include "chatlistmodel.h"
#include "contactresolver.h"
#include "daemonclient.h"

#include <KLocalizedString>

using namespace Qt::StringLiterals;

namespace km
{

MessageSearchModel::MessageSearchModel(DaemonClient *client, ChatListModel *chats, QObject *parent)
    : QAbstractListModel(parent)
    , m_client(client)
    , m_chats(chats)
{
    m_debounce.setSingleShot(true);
    m_debounce.setInterval(250);
    connect(&m_debounce, &QTimer::timeout, this, &MessageSearchModel::run);
    if (auto *contacts = ContactResolver::instance()) {
        connect(contacts, &ContactResolver::contactsChanged, this, [this] {
            if (!m_results.isEmpty()) {
                Q_EMIT dataChanged(index(0), index(int(m_results.size()) - 1));
            }
        });
    }
}

int MessageSearchModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : int(m_results.size());
}

QHash<int, QByteArray> MessageSearchModel::roleNames() const
{
    return {{ChatIdRole, "chatId"}, {ChatNameRole, "chatName"}, {MessageIdRole, "messageId"}, {TextRole, "snippet"}, {TimeRole, "time"}, {SenderNameRole, "senderName"}, {AvatarSourceRole, "avatarSource"}};
}

QVariant MessageSearchModel::data(const QModelIndex &index, int role) const
{
    if (!checkIndex(index, CheckIndexOption::IndexIsValid | CheckIndexOption::ParentIsInvalid)) {
        return {};
    }
    const Message &m = m_results.at(index.row());
    switch (role) {
    case ChatIdRole:
        return m.chatId;
    case ChatNameRole:
        return m_chats ? m_chats->titleFor(m.chatId) : m.chatId;
    case MessageIdRole:
        return m.id;
    case Qt::DisplayRole:
    case TextRole:
        return m.snippet();
    case TimeRole:
        return formatListTime(m.sentAt);
    case SenderNameRole:
        return m.isFromMe ? i18n("You") : (m_chats ? m_chats->nameForHandle(m.sender.uri) : m.sender.displayName());
    case AvatarSourceRole: {
        const auto *contacts = ContactResolver::instance();
        const auto chat = m_chats ? m_chats->chat(m.chatId) : std::nullopt;
        if (!contacts || !chat || chat->isGroup || chat->participants.size() != 1) {
            return QString();
        }
        return contacts->avatarSource(chat->participants.first().uri);
    }
    }
    return {};
}

void MessageSearchModel::setQuery(const QString &query)
{
    if (query == m_query) {
        return;
    }
    m_query = query;
    Q_EMIT queryChanged();
    if (query.trimmed().size() < 2) {
        ++m_generation;
        m_debounce.stop();
        beginResetModel();
        m_results.clear();
        endResetModel();
        Q_EMIT countChanged();
        return;
    }
    m_debounce.start();
}

void MessageSearchModel::run()
{
    if (!m_client) {
        return;
    }
    const quint64 gen = ++m_generation;
    m_searching = true;
    Q_EMIT searchingChanged();
    QPointer<MessageSearchModel> self(this);
    auto done = [self, gen](const QList<Message> &results) {
        if (!self || gen != self->m_generation) {
            return;
        }
        self->beginResetModel();
        self->m_results = results;
        self->endResetModel();
        self->m_searching = false;
        Q_EMIT self->searchingChanged();
        Q_EMIT self->countChanged();
    };
    m_client->search(m_query.trimmed(), 30, done, [done](const QString &, const QString &) {
        done({});
    });
}

} // namespace km
