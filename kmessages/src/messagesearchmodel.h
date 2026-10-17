// SPDX-License-Identifier: SSPL-1.0
#pragma once

#include "types.h"

#include <QAbstractListModel>
#include <QPointer>
#include <QTimer>

namespace km
{
class DaemonClient;
class ChatListModel;

/// Full-text message search (daemon `Search`), debounced.
class MessageSearchModel : public QAbstractListModel
{
    Q_OBJECT
    Q_PROPERTY(QString query READ query WRITE setQuery NOTIFY queryChanged)
    Q_PROPERTY(int count READ rowCount NOTIFY countChanged)
    Q_PROPERTY(bool searching READ isSearching NOTIFY searchingChanged)

public:
    enum Roles { ChatIdRole = Qt::UserRole + 1, ChatNameRole, MessageIdRole, TextRole, TimeRole, SenderNameRole, AvatarSourceRole };

    MessageSearchModel(DaemonClient *client, ChatListModel *chats, QObject *parent = nullptr);
    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role) const override;
    QHash<int, QByteArray> roleNames() const override;

    QString query() const { return m_query; }
    void setQuery(const QString &query);
    bool isSearching() const { return m_searching; }

Q_SIGNALS:
    void queryChanged();
    void countChanged();
    void searchingChanged();

private:
    void run();

    QPointer<DaemonClient> m_client;
    QPointer<ChatListModel> m_chats;
    QString m_query;
    QList<Message> m_results;
    QTimer m_debounce;
    bool m_searching = false;
    quint64 m_generation = 0;
};

} // namespace km
