// SPDX-License-Identifier: SSPL-1.0
#pragma once

#include <QAbstractListModel>
#include <QHash>
#include <QPointer>

namespace km
{

/// The new-message page's suggestion list as one model: the address-book
/// matches (ContactSuggestionModel, one row per address) first, then the
/// matching existing conversations (a ChatFilterModel). One list means one
/// `currentIndex` for the keyboard highlight. Structural changes of either
/// source are forwarded (inserts / removals with the offset; resets, layout
/// changes and moves, which neither source makes, as resets), data changes
/// per row.
class RecipientPickerModel : public QAbstractListModel
{
    Q_OBJECT
    Q_PROPERTY(int count READ count NOTIFY countChanged)

public:
    enum Roles {
        KindRole = Qt::UserRole + 1, ///< "contact" or "conversation"
        NameRole,
        UriRole, ///< contact: its IDS URI; conversation: "" (see participantUris)
        AddressRole, ///< contact: formatted address
        AvatarSourceRole,
        AvatarColorRole,
        ReachabilityRole, ///< contact rows (ContactSuggestionModel), "" for conversations
        ChatIdRole, ///< conversation rows
        ParticipantUrisRole,
        ParticipantAddressesRole,
        IsGroupRole,
        SectionRole, ///< "Contacts" / "Conversations" (translated); "" while there are no contact rows
    };
    Q_ENUM(Roles)

    RecipientPickerModel(QAbstractItemModel *contacts, QAbstractItemModel *conversations, QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role) const override;
    QHash<int, QByteArray> roleNames() const override;
    int count() const { return rowCount(); }

    /// What activating `row` means: {key, kind, uri, chatId, isGroup,
    /// participantUris}; empty for a row out of range. `key` identifies the
    /// item across resets ("c:<uri>" / "v:<chat id>").
    Q_INVOKABLE QVariantMap rowData(int row) const;
    /// The row of the item with `key`, -1 = gone.
    Q_INVOKABLE int indexOfKey(const QString &key) const;
    /// `text` is a complete email address or phone number (≥ 7 digits).
    Q_INVOKABLE static bool looksLikeAddress(const QString &text);

Q_SIGNALS:
    void countChanged();
    /// After every structural change (reset, rows added or removed).
    void rowsChanged();

private:
    int contactCount() const;
    void connectSource(QAbstractItemModel *source, bool isContacts);
    QVariant sourceData(QAbstractItemModel *source, const QHash<QByteArray, int> &roles, int row, const QByteArray &name) const;

    QPointer<QAbstractItemModel> m_contacts;
    QPointer<QAbstractItemModel> m_conversations;
    QHash<QByteArray, int> m_contactRoles; ///< role name → source role
    QHash<QByteArray, int> m_conversationRoles;
    bool m_resetting = false;
    int m_contactsBefore = 0; ///< contact rows before a structural contact change
};

} // namespace km
