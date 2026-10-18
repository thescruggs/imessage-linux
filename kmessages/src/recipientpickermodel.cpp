// SPDX-License-Identifier: SSPL-1.0
#include "recipientpickermodel.h"

#include "types.h"

#include <KLocalizedString>

using namespace Qt::StringLiterals;

namespace km
{

namespace
{
QHash<QByteArray, int> invert(const QHash<int, QByteArray> &names)
{
    QHash<QByteArray, int> out;
    for (auto it = names.cbegin(); it != names.cend(); ++it) {
        out.insert(it.value(), it.key());
    }
    return out;
}
}

RecipientPickerModel::RecipientPickerModel(QAbstractItemModel *contacts, QAbstractItemModel *conversations, QObject *parent)
    : QAbstractListModel(parent)
    , m_contacts(contacts)
    , m_conversations(conversations)
{
    if (contacts) {
        m_contactRoles = invert(contacts->roleNames());
        connectSource(contacts, true);
    }
    if (conversations) {
        m_conversationRoles = invert(conversations->roleNames());
        connectSource(conversations, false);
    }
}

int RecipientPickerModel::contactCount() const
{
    return m_contacts ? m_contacts->rowCount() : 0;
}

void RecipientPickerModel::connectSource(QAbstractItemModel *source, bool isContacts)
{
    // Offset of the source's rows in this model, read while the signal is
    // delivered (the contact rows come first).
    const auto offset = [this, isContacts] {
        return isContacts ? 0 : contactCount();
    };
    // A change in the number of contact rows can add or drop the section
    // headers of the conversation rows.
    const auto sectionsMaybeChanged = [this, isContacts] {
        if (!isContacts) {
            return;
        }
        const int now = contactCount();
        if ((now == 0) != (m_contactsBefore == 0) && rowCount() > now) {
            Q_EMIT dataChanged(index(now), index(rowCount() - 1), {SectionRole});
        }
    };
    const auto beginReset = [this] {
        if (!m_resetting) {
            m_resetting = true;
            beginResetModel();
        }
    };
    const auto endReset = [this] {
        if (m_resetting) {
            m_resetting = false;
            endResetModel();
            Q_EMIT countChanged();
            Q_EMIT rowsChanged();
        }
    };
    connect(source, &QAbstractItemModel::modelAboutToBeReset, this, beginReset);
    connect(source, &QAbstractItemModel::modelReset, this, endReset);
    connect(source, &QAbstractItemModel::layoutAboutToBeChanged, this, beginReset);
    connect(source, &QAbstractItemModel::layoutChanged, this, endReset);
    connect(source, &QAbstractItemModel::rowsAboutToBeInserted, this, [this, offset](const QModelIndex &parent, int first, int last) {
        if (!parent.isValid()) {
            m_contactsBefore = contactCount();
            beginInsertRows({}, offset() + first, offset() + last);
        }
    });
    connect(source, &QAbstractItemModel::rowsInserted, this, [this, sectionsMaybeChanged](const QModelIndex &parent) {
        if (!parent.isValid()) {
            endInsertRows();
            Q_EMIT countChanged();
            Q_EMIT rowsChanged();
            sectionsMaybeChanged();
        }
    });
    connect(source, &QAbstractItemModel::rowsAboutToBeRemoved, this, [this, offset](const QModelIndex &parent, int first, int last) {
        if (!parent.isValid()) {
            m_contactsBefore = contactCount();
            beginRemoveRows({}, offset() + first, offset() + last);
        }
    });
    connect(source, &QAbstractItemModel::rowsRemoved, this, [this, sectionsMaybeChanged](const QModelIndex &parent) {
        if (!parent.isValid()) {
            endRemoveRows();
            Q_EMIT countChanged();
            Q_EMIT rowsChanged();
            sectionsMaybeChanged();
        }
    });
    // Neither source moves rows (ContactSuggestionModel resets, a
    // QSortFilterProxyModel turns moves into layout changes); should one
    // ever do, a reset is always correct.
    connect(source, &QAbstractItemModel::rowsAboutToBeMoved, this, beginReset);
    connect(source, &QAbstractItemModel::rowsMoved, this, endReset);
    connect(source, &QAbstractItemModel::dataChanged, this, [this, offset](const QModelIndex &tl, const QModelIndex &br) {
        if (m_resetting || tl.parent().isValid()) {
            return;
        }
        // Role numbers differ between the sources: refresh every role of the rows.
        Q_EMIT dataChanged(index(offset() + tl.row()), index(offset() + br.row()));
    });
}

int RecipientPickerModel::rowCount(const QModelIndex &parent) const
{
    if (parent.isValid()) {
        return 0;
    }
    return contactCount() + (m_conversations ? m_conversations->rowCount() : 0);
}

QHash<int, QByteArray> RecipientPickerModel::roleNames() const
{
    return {
        {KindRole, "kind"},
        {NameRole, "name"},
        {UriRole, "uri"},
        {AddressRole, "address"},
        {AvatarSourceRole, "avatarSource"},
        {AvatarColorRole, "avatarColor"},
        {ReachabilityRole, "reachability"},
        {ChatIdRole, "chatId"},
        {ParticipantUrisRole, "participantUris"},
        {ParticipantAddressesRole, "participantAddresses"},
        {IsGroupRole, "isGroup"},
        {SectionRole, "section"},
    };
}

QVariant RecipientPickerModel::sourceData(QAbstractItemModel *source, const QHash<QByteArray, int> &roles, int row, const QByteArray &name) const
{
    const auto it = roles.constFind(name);
    if (!source || it == roles.cend()) {
        return {};
    }
    return source->data(source->index(row, 0), it.value());
}

QVariant RecipientPickerModel::data(const QModelIndex &index, int role) const
{
    if (!checkIndex(index, CheckIndexOption::IndexIsValid | CheckIndexOption::ParentIsInvalid)) {
        return {};
    }
    const int contacts = contactCount();
    const bool isContact = index.row() < contacts;
    const int row = isContact ? index.row() : index.row() - contacts;
    QAbstractItemModel *source = isContact ? m_contacts.data() : m_conversations.data();
    const auto &roles = isContact ? m_contactRoles : m_conversationRoles;
    const auto get = [&](const char *name) {
        return sourceData(source, roles, row, QByteArray(name));
    };
    const auto text = [&](const char *name) {
        return get(name).toString();
    };

    switch (role) {
    case Qt::DisplayRole:
    case NameRole:
        return text("name");
    case KindRole:
        return isContact ? u"contact"_s : u"conversation"_s;
    case UriRole:
        return isContact ? text("uri") : QString();
    case AddressRole:
        return isContact ? text("address") : get("participantAddresses").toStringList().join(u", "_s);
    case AvatarSourceRole:
        return text("avatarSource");
    case AvatarColorRole:
        return get("avatarColor");
    case ReachabilityRole:
        return isContact ? text("reachability") : QString();
    case ChatIdRole:
        return isContact ? QString() : text("chatId");
    case ParticipantUrisRole:
        return isContact ? QStringList{text("uri")} : get("participantUris").toStringList();
    case ParticipantAddressesRole:
        return isContact ? QStringList{text("address")} : get("participantAddresses").toStringList();
    case IsGroupRole:
        return isContact ? false : get("isGroup").toBool();
    case SectionRole:
        if (contacts == 0) {
            return QString();
        }
        return isContact ? i18n("Contacts") : i18n("Conversations");
    }
    return {};
}

QVariantMap RecipientPickerModel::rowData(int row) const
{
    if (row < 0 || row >= rowCount()) {
        return {};
    }
    const QModelIndex i = index(row);
    const bool contact = data(i, KindRole).toString() == "contact"_L1;
    return {
        {u"key"_s, QString(contact ? u"c:"_s + data(i, UriRole).toString() : u"v:"_s + data(i, ChatIdRole).toString())},
        {u"kind"_s, data(i, KindRole)},
        {u"uri"_s, data(i, UriRole)},
        {u"chatId"_s, data(i, ChatIdRole)},
        {u"isGroup"_s, data(i, IsGroupRole)},
        {u"participantUris"_s, data(i, ParticipantUrisRole)},
    };
}

int RecipientPickerModel::indexOfKey(const QString &key) const
{
    if (key.isEmpty()) {
        return -1;
    }
    for (int row = 0; row < rowCount(); ++row) {
        if (rowData(row).value(u"key"_s).toString() == key) {
            return row;
        }
    }
    return -1;
}

bool RecipientPickerModel::looksLikeAddress(const QString &text)
{
    const QString t = text.trimmed();
    const QString uri = normalizeHandle(t);
    if (uri.startsWith("mailto:"_L1)) {
        return uri.indexOf(u'.', uri.indexOf(u'@')) > 0; // a dot in the domain
    }
    if (uri.startsWith("tel:"_L1)) {
        int digits = 0;
        for (const QChar c : uri) {
            digits += c.isDigit() ? 1 : 0;
        }
        return digits >= 7;
    }
    return false;
}

} // namespace km
