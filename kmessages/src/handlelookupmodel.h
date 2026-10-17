// SPDX-License-Identifier: SSPL-1.0
#pragma once

#include <QAbstractListModel>
#include <QPointer>

namespace km
{
class DaemonClient;

/// Recipients being entered on the new-message page. Each entry is
/// normalized locally (same rules as the daemon) and then checked with
/// LookupHandles; sending is allowed once every recipient is reachable.
class HandleLookupModel : public QAbstractListModel
{
    Q_OBJECT
    Q_PROPERTY(int count READ rowCount NOTIFY changed)
    Q_PROPERTY(QStringList handles READ handles NOTIFY changed)
    Q_PROPERTY(bool checking READ isChecking NOTIFY changed)
    Q_PROPERTY(bool canSend READ canSend NOTIFY changed)
    Q_PROPERTY(QString problem READ problem NOTIFY changed)
    /// The message will go out as a text message (SMS via the iPhone): Text
    /// Message Forwarding is active and a recipient is a phone number that is
    /// not on iMessage.
    Q_PROPERTY(bool smsMode READ smsMode NOTIFY changed)

public:
    enum State { Checking, Reachable, Unreachable, Invalid, Error };
    Q_ENUM(State)

    enum Roles {
        InputRole = Qt::UserRole + 1,
        HandleRole,
        LabelRole,
        StateRole,
        StateNameRole,
        MessageRole,
    };

    explicit HandleLookupModel(DaemonClient *client, QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role) const override;
    QHash<int, QByteArray> roleNames() const override;

    QStringList handles() const;
    bool isChecking() const;
    bool canSend() const;
    bool smsMode() const;
    /// Human-readable reason sending is blocked (empty if none).
    QString problem() const;

    /// Add one or more recipients (split on `,` `;` and newlines). Returns false if nothing was added.
    Q_INVOKABLE bool add(const QString &input);
    Q_INVOKABLE void remove(int row);
    Q_INVOKABLE void removeLast();
    Q_INVOKABLE void clear();

Q_SIGNALS:
    void changed();

private:
    struct Entry {
        QString input;
        QString handle;
        State state = Checking;
        QString message;
    };
    void lookup(const QString &handle);
    /// An unreachable phone number can still get a text message.
    bool textable(const QString &handle) const;
    void setState(const QString &handle, State state, const QString &message = QString());

    QPointer<DaemonClient> m_client;
    QList<Entry> m_entries;
};

} // namespace km
