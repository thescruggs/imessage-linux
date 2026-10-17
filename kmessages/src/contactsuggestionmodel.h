// SPDX-License-Identifier: SSPL-1.0
#pragma once

#include <QAbstractListModel>
#include <QElapsedTimer>
#include <QHash>
#include <QPointer>
#include <QSet>
#include <QTimer>

namespace km
{
class DaemonClient;
class ContactResolver;

/// Address-book people matching what is typed into the new-message "To:"
/// field: one row per email / phone number. Shown rows are checked with
/// LookupHandles so the page can mark the iMessage addresses: results are
/// kept for the app session and asked again after an hour at the earliest;
/// at most MaxLookupBurst handles go out per call, one call at a time, the
/// rest queued.
class ContactSuggestionModel : public QAbstractListModel
{
    Q_OBJECT
    Q_PROPERTY(QString filterText READ filterText WRITE setFilterText NOTIFY filterTextChanged)
    Q_PROPERTY(int count READ rowCount NOTIFY countChanged)

public:
    enum Roles {
        NameRole = Qt::UserRole + 1,
        UriRole,
        AddressRole,
        AvatarSourceRole,
        AvatarColorRole,
        /// "checking", "reachable", "unreachable" or "unknown"
        ReachabilityRole,
    };
    Q_ENUM(Roles)

    ContactSuggestionModel(DaemonClient *client, ContactResolver *contacts, QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role) const override;
    QHash<int, QByteArray> roleNames() const override;

    QString filterText() const { return m_filterText; }
    void setFilterText(const QString &text);

    static constexpr int MaxRows = 20;
    static constexpr int MaxLookupBurst = 5;
    static constexpr qint64 DefaultCacheTtlMs = 60 * 60 * 1000;
    /// Tests: how long a LookupHandles result stays fresh.
    void setCacheTtlMs(qint64 ms) { m_cacheTtlMs = ms; }

Q_SIGNALS:
    void filterTextChanged();
    void countChanged();

private:
    struct Row {
        QString name;
        QString uri;
        QString address;
    };
    void rebuild();
    void lookupVisible();
    void lookupNextBurst();
    bool isFresh(const QString &uri) const;
    void touchUri(const QString &uri);

    QPointer<DaemonClient> m_client;
    QPointer<ContactResolver> m_contacts;
    QString m_filterText;
    QList<Row> m_rows;
    struct Cached {
        bool reachable = false;
        qint64 atMs = 0; ///< m_clock time of the answer
    };
    QHash<QString, Cached> m_reachable; ///< LookupHandles results, by URI
    QSet<QString> m_checking; ///< in the call in flight
    QStringList m_queue; ///< shown, not fresh, waiting for a burst
    bool m_inFlight = false;
    qint64 m_cacheTtlMs = DefaultCacheTtlMs;
    QElapsedTimer m_clock;
    QTimer m_lookupTimer;
};

} // namespace km
