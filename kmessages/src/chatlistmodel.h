// SPDX-License-Identifier: SSPL-1.0
#pragma once

#include "types.h"

#include <QAbstractListModel>
#include <QHash>
#include <QPointer>
#include <QSet>
#include <QSortFilterProxyModel>
#include <QTimer>

namespace km
{
class DaemonClient;

/// All conversations, newest activity first (ListChats + live ChatUpdated /
/// MessageReceived / Typing). The snippet is the newest message's preview,
/// fetched with ListMessages(limit 1) for chats whose activity changed.
class ChatListModel : public QAbstractListModel
{
    Q_OBJECT
    Q_PROPERTY(int count READ rowCount NOTIFY countChanged)
    Q_PROPERTY(int totalUnread READ totalUnread NOTIFY totalUnreadChanged)
    Q_PROPERTY(bool loading READ isLoading NOTIFY loadingChanged)
    Q_PROPERTY(bool loaded READ isLoaded NOTIFY loadingChanged)
    /// Bumped on every change to any row (reset, insert, remove, move, dataChanged),
    /// so QML bindings over get()/titleFor() can depend on it.
    Q_PROPERTY(int revision READ revision NOTIFY revisionChanged)

public:
    enum Roles {
        ChatIdRole = Qt::UserRole + 1,
        NameRole,
        SnippetRole,
        TimeRole,
        TimestampRole,
        UnreadCountRole,
        HasUnreadRole,
        InitialsRole,
        AvatarColorRole,
        IsGroupRole,
        TypingRole,
        ParticipantsRole, ///< display names
        ParticipantUrisRole,
        SearchTextRole,
        LastMessageFromMeRole,
        AvatarSourceRole, ///< contact photo URL for 1:1 chats, else ""
        ParticipantAvatarsRole, ///< contact photo URL per participant ("" = none)
        ParticipantAddressesRole, ///< formatted address per participant
    };
    Q_ENUM(Roles)

    explicit ChatListModel(DaemonClient *client, QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role) const override;
    QHash<int, QByteArray> roleNames() const override;

    int totalUnread() const;
    bool isLoading() const { return m_loading; }
    bool isLoaded() const { return m_loaded; }
    int revision() const { return m_revision; }

    Q_INVOKABLE void refresh();
    /// refresh() after a short delay, coalescing several triggers (daemon
    /// reappearance emits both resyncRequested and a state change).
    void scheduleRefresh();
    Q_INVOKABLE int indexOf(const QString &chatId) const;
    /// Role values of a chat as a map (empty if unknown).
    Q_INVOKABLE QVariantMap get(const QString &chatId) const;
    Q_INVOKABLE QString titleFor(const QString &chatId) const;
    /// File URL of the chat's transcript background image, "" = none.
    Q_INVOKABLE QString backgroundFor(const QString &chatId) const;
    /// Unread count as shown: the daemon's, less the user's own messages from
    /// another device that it still counts (setOwnUnread()).
    Q_INVOKABLE int unreadFor(const QString &chatId) const;
    /// `n` of the chat's daemon-unread messages are the user's own (sent from
    /// a "My other addresses" handle) and must not show as unread. Clamped to
    /// the daemon's count whenever that drops.
    void setOwnUnread(const QString &chatId, int n);
    Q_INVOKABLE bool isGroupChat(const QString &chatId) const;
    /// Display name for a handle, using what the chats know about it.
    Q_INVOKABLE QString nameForHandle(const QString &uri) const;
    std::optional<Chat> chat(const QString &chatId) const;

Q_SIGNALS:
    void countChanged();
    void totalUnreadChanged();
    void loadingChanged();
    void revisionChanged();
    /// A chat's unread count changed (also emitted for new chats).
    void unreadChanged(const QString &chatId, int unread);

private:
    struct Entry {
        Chat chat;
        QString snippet;
        QString lastMessageId;
        bool lastFromMe = false;
        QSet<QString> typing; ///< handles currently typing
    };
    QHash<QString, int> m_ownUnread; ///< chat id → see setOwnUnread()

    int shownUnread(const Chat &c) const;
    void clampOwnUnread(const Chat &c);
    void onChatUpdated(const Chat &chat);
    void onMessageReceived(const QString &chatId, const Message &msg);
    void onMessageUpdated(const QString &chatId, const Message &msg);
    void onTyping(const QString &chatId, const QString &handle, bool typing);
    void fetchSnippet(const QString &chatId);
    void applyMessage(Entry &e, const Message &msg);
    void upsert(const Chat &chat, bool fetchSnippetIfNew);
    /// Move row `row` to keep the newest-first order; returns its new row.
    int reposition(int row);
    void emitRowChanged(int row);

    QPointer<DaemonClient> m_client;
    QList<Entry> m_entries;
    bool m_loading = false;
    bool m_loaded = false;
    quint64 m_generation = 0;
    int m_revision = 0;
    QTimer m_refreshTimer;
};

/// Search filter over ChatListModel (name, handles, snippet).
class ChatFilterModel : public QSortFilterProxyModel
{
    Q_OBJECT
    Q_PROPERTY(QString filterText READ filterText WRITE setFilterText NOTIFY filterTextChanged)
    Q_PROPERTY(int count READ count NOTIFY countChanged)

public:
    explicit ChatFilterModel(ChatListModel *source, QObject *parent = nullptr);
    QString filterText() const { return m_filterText; }
    void setFilterText(const QString &text);
    int count() const { return rowCount(); }
    Q_INVOKABLE QString chatIdAt(int row) const;

Q_SIGNALS:
    void filterTextChanged();
    void countChanged();

protected:
    bool filterAcceptsRow(int sourceRow, const QModelIndex &sourceParent) const override;

private:
    QString m_filterText;
};

} // namespace km
