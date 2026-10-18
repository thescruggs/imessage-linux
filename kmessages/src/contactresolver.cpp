// SPDX-License-Identifier: SSPL-1.0
#include "contactresolver.h"
#include "types.h"

#include <QAbstractItemModel>
#include <QHash>
#include <QLoggingCategory>
#include <QPixmap>
#include <QRegularExpression>
#include <QSet>
#include <QUrl>

#if KMESSAGES_WITH_KPEOPLE
#include <KPeople/PersonsModel>
#include <KPeopleBackend/AbstractContact>
#endif

using namespace Qt::StringLiterals;

namespace km
{

namespace
{
ContactResolver *s_instance = nullptr;
constexpr int PhotoMaxSize = 256; ///< px; avatars are at most ~4 grid units
constexpr int RowsPerChunk = 40; ///< KPeople rows indexed per event-loop turn

QString digitsOf(const QString &s)
{
    QString out;
    for (const QChar c : s) {
        if (c >= u'0' && c <= u'9') {
            out += c;
        }
    }
    return out;
}

QString firstWord(const QString &name)
{
    return name.trimmed().section(u' ', 0, 0);
}
}

struct ContactResolver::Index {
    QList<Person> people;
    QHash<QString, int> byKey; ///< keysFor() → people index
    int revision = 0;

    const Person *find(const QString &uri) const
    {
        const auto keys = keysFor(uri);
        for (const auto &k : keys) {
            const auto it = byKey.constFind(k);
            if (it != byKey.cend()) {
                return &people.at(*it);
            }
        }
        return nullptr;
    }
};

ContactResolver::ContactResolver(bool useKPeople, QObject *parent)
    : QObject(parent)
    , m_shared(std::make_shared<Shared>())
{
    m_shared->index = std::make_shared<Index>();
    s_instance = this;
    m_reindexTimer.setSingleShot(true);
    m_reindexTimer.setInterval(500);
    connect(&m_reindexTimer, &QTimer::timeout, this, [this] {
        m_pending.clear();
        m_pendingRow = 0;
        reindexChunk();
    });
#if KMESSAGES_WITH_KPEOPLE
    if (useKPeople) {
        // PersonsModel loads its data sources asynchronously; until it has,
        // lookups simply miss and the UI shows handles.
        auto *model = new KPeople::PersonsModel(this);
        m_persons = model;
        connect(model, &KPeople::PersonsModel::modelInitialized, this, &ContactResolver::scheduleReindex);
        connect(model, &QAbstractItemModel::rowsInserted, this, &ContactResolver::scheduleReindex);
        connect(model, &QAbstractItemModel::rowsRemoved, this, &ContactResolver::scheduleReindex);
        connect(model, &QAbstractItemModel::dataChanged, this, &ContactResolver::scheduleReindex);
        connect(model, &QAbstractItemModel::modelReset, this, &ContactResolver::scheduleReindex);
        connect(model, &QAbstractItemModel::layoutChanged, this, &ContactResolver::scheduleReindex);
    }
#else
    Q_UNUSED(useKPeople)
#endif
}

ContactResolver::~ContactResolver()
{
    if (s_instance == this) {
        s_instance = nullptr;
    }
}

ContactResolver *ContactResolver::instance()
{
    return s_instance;
}

std::shared_ptr<const ContactResolver::Index> ContactResolver::snapshot() const
{
    QMutexLocker lock(&m_shared->mutex);
    return m_shared->index;
}

int ContactResolver::count() const
{
    return int(snapshot()->people.size());
}

QStringList ContactResolver::keysFor(const QString &uriOrAddress)
{
    QString a = uriOrAddress.trimmed();
    if (a.startsWith("mailto:"_L1, Qt::CaseInsensitive)) {
        a = a.mid(7);
    } else if (a.startsWith("tel:"_L1, Qt::CaseInsensitive)) {
        a = a.mid(4);
    }
    if (a.contains(u'@')) {
        return {"e:"_L1 + a.trimmed().toLower()};
    }
    const QString d = digitsOf(a);
    if (d.isEmpty()) {
        return {};
    }
    QStringList keys{"p:"_L1 + d};
    // The last ten digits match "+1 555 …" with "(555) …", but would also
    // match +44 7911 123456 with +1 791-112-3456: only without a country
    // code, or with +1 (the only country code that starts with 1).
    const bool international = a.trimmed().startsWith(u'+');
    if (d.size() >= 10 && (!international || d.startsWith(u'1'))) {
        keys << "t:"_L1 + d.right(10);
    }
    return keys;
}

QString ContactResolver::uriForAddress(const QString &address)
{
    const QString a = address.trimmed();
    if (a.contains(u'@')) {
        return normalizeHandle(a);
    }
    const QString base = normalizeHandle(a);
    if (base.isEmpty()) {
        return {};
    }
    const QString d = digitsOf(a);
    if (base.startsWith("tel:+"_L1)) {
        return base;
    }
    if (d.size() == 10) {
        return "tel:+1"_L1 + d;
    }
    if (d.size() == 11 && d.startsWith(u'1')) {
        return "tel:+"_L1 + d;
    }
    return base;
}

std::optional<ContactResolver::Match> ContactResolver::lookup(const QString &uri) const
{
    const auto idx = snapshot();
    const Person *p = idx->find(uri);
    const auto shared = sharedProfile(uri);
    const bool sharedPhoto = shared && !shared->photo.isEmpty();
    if (p) {
        return Match{p->name, p->givenName.isEmpty() ? firstWord(p->name) : p->givenName, !p->photo.isNull() || sharedPhoto};
    }
    if (shared && !shared->displayName().isEmpty()) {
        const QString name = shared->displayName();
        const QString given = !shared->first.trimmed().isEmpty() && shared->name.trimmed().isEmpty() ? shared->first.trimmed() : firstWord(name);
        return Match{name, given, sharedPhoto};
    }
    return std::nullopt;
}

std::optional<SharedProfile> ContactResolver::sharedProfile(const QString &uri) const
{
    QMutexLocker lock(&m_profilesMutex);
    const auto it = m_profiles.constFind(uri);
    if (it == m_profiles.cend()) {
        return std::nullopt;
    }
    return *it;
}

QString ContactResolver::sharedNameFor(const QString &uri) const
{
    const auto p = sharedProfile(uri);
    return p ? p->displayName() : QString();
}

QString ContactResolver::sharedPhotoSource(const QString &uri) const
{
    const auto p = sharedProfile(uri);
    if (!p || p->photo.isEmpty()) {
        return {};
    }
    // `u` busts QML's image cache when the photo was fetched again.
    return u"image://attachment/%1?u=%2"_s.arg(QString::fromLatin1(QUrl::toPercentEncoding(p->photo)), QString::number(p->updatedAt));
}

bool ContactResolver::applySharedProfile(const QString &uri, const std::optional<SharedProfile> &profile)
{
    if (uri.isEmpty()) {
        return false;
    }
    QMutexLocker lock(&m_profilesMutex);
    const auto it = m_profiles.find(uri);
    if (!profile || profile->isEmpty()) {
        if (it == m_profiles.end()) {
            return false;
        }
        m_profiles.erase(it);
        return true;
    }
    if (it != m_profiles.end() && *it == *profile) {
        return false;
    }
    m_profiles.insert(uri, *profile);
    return true;
}

void ContactResolver::sharedProfilesChanged()
{
    ++m_revision;
    Q_EMIT contactsChanged();
}

void ContactResolver::setSharedProfile(const QString &uri, const std::optional<SharedProfile> &profile)
{
    if (applySharedProfile(uri, profile)) {
        sharedProfilesChanged();
    }
}

void ContactResolver::setSharedProfiles(const QList<Handle> &handles)
{
    bool changed = false;
    for (const auto &h : handles) {
        changed |= applySharedProfile(h.uri, h.profile);
    }
    if (changed) {
        sharedProfilesChanged();
    }
}

QString ContactResolver::nameFor(const QString &uri) const
{
    const auto idx = snapshot();
    const Person *p = idx->find(uri);
    return p ? p->name : QString();
}

QImage ContactResolver::photoFor(const QString &uri) const
{
    const auto idx = snapshot();
    const Person *p = idx->find(uri);
    if (p && !p->photo.isNull()) {
        return p->photo;
    }
    const auto shared = sharedProfile(uri);
    QImage img;
    if (shared && !shared->photo.isEmpty() && img.load(shared->photo)) {
        if (img.width() > PhotoMaxSize || img.height() > PhotoMaxSize) {
            img = img.scaled(PhotoMaxSize, PhotoMaxSize, Qt::KeepAspectRatioByExpanding, Qt::SmoothTransformation);
        }
    }
    return img;
}

QString ContactResolver::avatarSource(const QString &uri) const
{
    const auto idx = snapshot();
    const Person *p = idx->find(uri);
    if (!p || p->photo.isNull()) {
        return sharedPhotoSource(uri);
    }
    // The revision busts QML's image cache when the photo may have changed.
    return u"image://contact/%1?%2"_s.arg(QString::fromLatin1(QUrl::toPercentEncoding(uri)), QString::number(idx->revision));
}

QList<ContactResolver::Candidate> ContactResolver::search(const QString &text, int limit) const
{
    const auto words = text.simplified().split(u' ', Qt::SkipEmptyParts);
    if (words.isEmpty()) {
        return {};
    }
    const auto idx = snapshot();
    struct Hit {
        int rank;
        const Person *p;
    };
    QList<Hit> hits;
    for (const auto &p : idx->people) {
        const QString addresses = (p.emails + p.phones).join(u'\n');
        const QString digits = digitsOf(p.phones.join(u' '));
        bool all = true;
        for (const auto &w : words) {
            static const QRegularExpression phoneChars(u"^[0-9+() .-]+$"_s);
            const QString wd = digitsOf(w);
            const bool phoneLike = wd.size() >= 3 && phoneChars.match(w).hasMatch();
            if (!p.name.contains(w, Qt::CaseInsensitive) && !addresses.contains(w, Qt::CaseInsensitive) && !(phoneLike && digits.contains(wd))) {
                all = false;
                break;
            }
        }
        if (!all) {
            continue;
        }
        int rank = 2;
        if (p.name.startsWith(words.first(), Qt::CaseInsensitive)) {
            rank = 0;
        } else {
            const auto nameWords = p.name.split(u' ', Qt::SkipEmptyParts);
            for (const auto &nw : nameWords) {
                if (nw.startsWith(words.first(), Qt::CaseInsensitive)) {
                    rank = 1;
                    break;
                }
            }
        }
        hits.append({rank, &p});
    }
    std::stable_sort(hits.begin(), hits.end(), [](const Hit &a, const Hit &b) {
        if (a.rank != b.rank) {
            return a.rank < b.rank;
        }
        return QString::localeAwareCompare(a.p->name, b.p->name) < 0;
    });
    // Per person: a complete address typed is the only one offered; else
    // the addresses matched by words that are not in the name come first
    // (then phones, emails, as written).
    const QString typedUri = uriForAddress(text);
    const auto addressMatches = [&words](const QString &name, const QString &addr) {
        static const QRegularExpression phoneChars(u"^[0-9+() .-]+$"_s);
        const QString ad = digitsOf(addr);
        for (const auto &w : words) {
            if (name.contains(w, Qt::CaseInsensitive)) {
                continue;
            }
            const QString wd = digitsOf(w);
            const bool phoneLike = wd.size() >= 3 && phoneChars.match(w).hasMatch();
            if (addr.contains(w, Qt::CaseInsensitive) || (phoneLike && ad.contains(wd))) {
                return true;
            }
        }
        return false;
    };
    QList<Candidate> out;
    QSet<QString> seen;
    for (const auto &h : std::as_const(hits)) {
        const QStringList all = h.p->phones + h.p->emails;
        QStringList ordered;
        for (const auto &addr : all) {
            if (!typedUri.isEmpty() && uriForAddress(addr) == typedUri) {
                ordered = {addr};
                break;
            }
        }
        if (ordered.isEmpty()) {
            QStringList rest;
            for (const auto &addr : all) {
                (addressMatches(h.p->name, addr) ? ordered : rest) << addr;
            }
            ordered << rest;
        }
        for (const auto &addr : std::as_const(ordered)) {
            const QString uri = uriForAddress(addr);
            if (uri.isEmpty() || seen.contains(uri)) {
                continue;
            }
            seen.insert(uri);
            out.append({h.p->name, uri, addr.trimmed()});
            if (out.size() >= limit) {
                return out;
            }
        }
    }
    return out;
}

void ContactResolver::setPeople(const QList<Person> &people)
{
    auto idx = std::make_shared<Index>();
    idx->revision = ++m_revision;
    for (const auto &p : people) {
        if (p.name.trimmed().isEmpty()) {
            continue;
        }
        const int i = int(idx->people.size());
        idx->people.append(p);
        for (const auto &addr : p.emails + p.phones) {
            const auto keys = keysFor(addr);
            for (const auto &k : keys) {
                if (!idx->byKey.contains(k)) { // the first person with an address wins
                    idx->byKey.insert(k, i);
                }
            }
        }
    }
    {
        QMutexLocker lock(&m_shared->mutex);
        m_shared->index = std::move(idx);
    }
    Q_EMIT contactsChanged();
}

void ContactResolver::scheduleReindex()
{
    m_pendingRow = -1; // abandon a re-index in progress; the timer starts over
    m_reindexTimer.start();
}

void ContactResolver::reindexChunk()
{
#if KMESSAGES_WITH_KPEOPLE
    if (!m_persons || m_pendingRow < 0) {
        return;
    }
    const int rows = m_persons->rowCount();
    const int end = qMin(rows, m_pendingRow + RowsPerChunk);
    for (int r = m_pendingRow; r < end; ++r) {
        const QModelIndex idx = m_persons->index(r, 0);
        Person p;
        p.id = idx.data(KPeople::PersonsModel::PersonUriRole).toString();
        p.name = idx.data(KPeople::PersonsModel::FormattedNameRole).toString().trimmed();
        const auto contact = idx.data(KPeople::PersonsModel::PersonVCardRole).value<KPeople::AbstractContact::Ptr>();
        if (contact) {
            p.emails = contact->customProperty(KPeople::AbstractContact::AllEmailsProperty).toStringList();
            const auto phones = contact->customProperty(KPeople::AbstractContact::AllPhoneNumbersProperty).toList();
            for (const auto &v : phones) {
                p.phones << v.toString();
            }
            const QVariant pic = contact->customProperty(KPeople::AbstractContact::PictureProperty);
            if (pic.metaType() == QMetaType::fromType<QImage>()) {
                p.photo = pic.value<QImage>();
            } else if (pic.metaType() == QMetaType::fromType<QPixmap>()) {
                p.photo = pic.value<QPixmap>().toImage();
            } else if (pic.metaType() == QMetaType::fromType<QUrl>() && pic.toUrl().isLocalFile()) {
                p.photo.load(pic.toUrl().toLocalFile());
            }
        }
        // A data source names nameless contacts after an address; that is no name.
        if (p.emails.contains(p.name, Qt::CaseInsensitive) || p.phones.contains(p.name)) {
            p.name.clear();
        }
        if (!p.photo.isNull() && (p.photo.width() > PhotoMaxSize || p.photo.height() > PhotoMaxSize)) {
            p.photo = p.photo.scaled(PhotoMaxSize, PhotoMaxSize, Qt::KeepAspectRatioByExpanding, Qt::SmoothTransformation);
        }
        if (!p.name.isEmpty()) {
            m_pending.append(std::move(p));
        }
    }
    m_pendingRow = end;
    if (end < rows) {
        // Yield to the event loop between chunks so the UI never stalls.
        const int expected = end;
        QTimer::singleShot(0, this, [this, expected] {
            if (m_pendingRow == expected) {
                reindexChunk();
            }
        });
        return;
    }
    m_pendingRow = -1;
    const auto people = std::exchange(m_pending, {});
    qDebug() << "contacts indexed:" << people.size();
    setPeople(people);
#endif
}

// ---------------------------------------------------------------------------

ContactImageProvider::ContactImageProvider(std::shared_ptr<ContactResolver::Shared> shared)
    : QQuickImageProvider(QQuickImageProvider::Image)
    , m_shared(std::move(shared))
{
}

QImage ContactImageProvider::requestImage(const QString &id, QSize *size, const QSize &requestedSize)
{
    const QString uri = QUrl::fromPercentEncoding(id.section(u'?', 0, 0).toUtf8());
    std::shared_ptr<const ContactResolver::Index> idx;
    {
        QMutexLocker lock(&m_shared->mutex);
        idx = m_shared->index;
    }
    const ContactResolver::Person *p = idx ? idx->find(uri) : nullptr;
    QImage img = p ? p->photo : QImage();
    if (!img.isNull() && requestedSize.width() > 0 && requestedSize.height() > 0 && (requestedSize.width() < img.width() || requestedSize.height() < img.height())) {
        img = img.scaled(requestedSize, Qt::KeepAspectRatioByExpanding, Qt::SmoothTransformation);
    }
    if (size) {
        *size = img.size();
    }
    return img;
}

} // namespace km
