// SPDX-License-Identifier: SSPL-1.0
//
// Contact names and photos for IDS handles, the way Messages.app shows
// them. The source is KPeople (vCards in $XDG_DATA_HOME/kpeoplevcard/,
// Akonadi when its KPeople plugin is installed, …); see README "Contacts".
#pragma once

#include <QImage>
#include <QMutex>
#include <QObject>
#include <QQuickImageProvider>
#include <QStringList>
#include <QTimer>

#include <memory>
#include <optional>

class QAbstractItemModel;

namespace km
{

class ContactResolver : public QObject
{
    Q_OBJECT
    /// Number of people indexed (0 until the contact source finished loading).
    Q_PROPERTY(int count READ count NOTIFY contactsChanged)
    /// Bumped on every re-index, so QML bindings over avatarSource() can depend on it.
    Q_PROPERTY(int revision READ revision NOTIFY contactsChanged)

public:
    struct Person {
        QString id;
        QString name; ///< formatted name ("Bob Smith", or an organization)
        QString givenName; ///< first name for group titles; empty = use `name`
        QStringList emails;
        QStringList phones; ///< as written in the address book
        QImage photo;
    };

    struct Match {
        QString name;
        QString givenName; ///< never empty: falls back to `name`
        bool hasPhoto = false;
    };

    /// One reachable address of a person, for recipient suggestions.
    struct Candidate {
        QString name;
        QString uri; ///< IDS URI (mailto: / tel:+…)
        QString address; ///< as shown
    };

    /// With `useKPeople` false (tests, or a build without KPeople) the
    /// resolver only knows what setPeople() gives it.
    explicit ContactResolver(bool useKPeople = true, QObject *parent = nullptr);
    ~ContactResolver() override;

    /// The resolver the models consult (the most recently created one), or null.
    static ContactResolver *instance();

    int count() const;
    int revision() const { return m_revision; }

    std::optional<Match> lookup(const QString &uri) const;
    /// Contact name for `uri`, or an empty string.
    Q_INVOKABLE QString nameFor(const QString &uri) const;
    /// Photo for `uri` (null if none). Thread-safe.
    QImage photoFor(const QString &uri) const;
    /// `image://contact/…` URL for a QML Image/Avatar source, or "" if the handle has no photo.
    Q_INVOKABLE QString avatarSource(const QString &uri) const;
    /// People whose name or address contains every word of `text`, one row
    /// per email / phone number, best matches first.
    QList<Candidate> search(const QString &text, int limit = 30) const;

    /// Replace the index (used by the KPeople loader, and by tests).
    void setPeople(const QList<Person> &people);

    /// Index keys of an IDS URI or an address-book entry: emails lowercased
    /// (`e:bob@x`), phones as all digits (`p:12055550100`) plus their last ten
    /// digits (`t:2055550100`) so national and +1 forms find each other.
    static QStringList keysFor(const QString &uriOrAddress);
    /// IDS URI for an address-book phone number or email (assumes +1 for
    /// ten-digit numbers), or an empty string.
    static QString uriForAddress(const QString &address);

    /// Shared with the image provider, which may run on a loader thread.
    struct Index;
    struct Shared {
        mutable QMutex mutex;
        std::shared_ptr<const Index> index;
    };
    std::shared_ptr<Shared> shared() const { return m_shared; }

Q_SIGNALS:
    void contactsChanged();

private:
    std::shared_ptr<const Index> snapshot() const;
    void scheduleReindex();
    void reindexChunk();

    std::shared_ptr<Shared> m_shared;
    int m_revision = 0;
    QAbstractItemModel *m_persons = nullptr; ///< KPeople::PersonsModel
    QTimer m_reindexTimer;
    // in-progress incremental re-index of m_persons
    QList<Person> m_pending;
    int m_pendingRow = -1;
};

/// "image://contact/<percent-encoded handle URI>[?revision]"
class ContactImageProvider : public QQuickImageProvider
{
public:
    explicit ContactImageProvider(std::shared_ptr<ContactResolver::Shared> shared);
    QImage requestImage(const QString &id, QSize *size, const QSize &requestedSize) override;

private:
    std::shared_ptr<ContactResolver::Shared> m_shared;
};

} // namespace km
