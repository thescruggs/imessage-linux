// SPDX-License-Identifier: SSPL-1.0
#include "contactsuggestionmodel.h"
#include "contactresolver.h"
#include "daemonclient.h"
#include "types.h"

using namespace Qt::StringLiterals;

namespace km
{

ContactSuggestionModel::ContactSuggestionModel(DaemonClient *client, ContactResolver *contacts, QObject *parent)
    : QAbstractListModel(parent)
    , m_client(client)
    , m_contacts(contacts)
{
    // Do not query the daemon for every keystroke.
    m_lookupTimer.setSingleShot(true);
    m_lookupTimer.setInterval(400);
    connect(&m_lookupTimer, &QTimer::timeout, this, &ContactSuggestionModel::lookupVisible);
    m_clock.start();
    if (contacts) {
        connect(contacts, &ContactResolver::contactsChanged, this, &ContactSuggestionModel::rebuild);
    }
}

int ContactSuggestionModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : int(m_rows.size());
}

QHash<int, QByteArray> ContactSuggestionModel::roleNames() const
{
    return {
        {NameRole, "name"},
        {UriRole, "uri"},
        {AddressRole, "address"},
        {AvatarSourceRole, "avatarSource"},
        {AvatarColorRole, "avatarColor"},
        {ReachabilityRole, "reachability"},
    };
}

QVariant ContactSuggestionModel::data(const QModelIndex &index, int role) const
{
    if (!checkIndex(index, CheckIndexOption::IndexIsValid | CheckIndexOption::ParentIsInvalid)) {
        return {};
    }
    const Row &r = m_rows.at(index.row());
    switch (role) {
    case Qt::DisplayRole:
    case NameRole:
        return r.name;
    case UriRole:
        return r.uri;
    case AddressRole:
        return r.address;
    case AvatarSourceRole:
        return m_contacts ? m_contacts->avatarSource(r.uri) : QString();
    case AvatarColorRole:
        return avatarColorFor(r.uri);
    case ReachabilityRole: {
        const auto it = m_reachable.constFind(r.uri);
        if (it != m_reachable.cend()) {
            return it->reachable ? u"reachable"_s : u"unreachable"_s;
        }
        return m_checking.contains(r.uri) ? u"checking"_s : u"unknown"_s;
    }
    }
    return {};
}

void ContactSuggestionModel::setFilterText(const QString &text)
{
    if (text == m_filterText) {
        return;
    }
    m_filterText = text;
    Q_EMIT filterTextChanged();
    rebuild();
}

void ContactSuggestionModel::rebuild()
{
    beginResetModel();
    m_rows.clear();
    if (m_contacts && !m_filterText.trimmed().isEmpty()) {
        const auto hits = m_contacts->search(m_filterText, MaxRows);
        for (const auto &h : hits) {
            m_rows.append({h.name, h.uri, h.address});
        }
    }
    endResetModel();
    Q_EMIT countChanged();
    if (!m_rows.isEmpty()) {
        m_lookupTimer.start();
    }
}

bool ContactSuggestionModel::isFresh(const QString &uri) const
{
    const auto it = m_reachable.constFind(uri);
    return it != m_reachable.cend() && m_clock.elapsed() - it->atMs < m_cacheTtlMs;
}

void ContactSuggestionModel::lookupVisible()
{
    // What is shown now replaces what was queued for an earlier filter.
    m_queue.clear();
    for (const auto &r : std::as_const(m_rows)) {
        if (!isFresh(r.uri) && !m_checking.contains(r.uri) && !m_queue.contains(r.uri)) {
            m_queue << r.uri;
        }
    }
    lookupNextBurst();
}

void ContactSuggestionModel::lookupNextBurst()
{
    if (m_inFlight || m_queue.isEmpty() || !m_client || !m_client->isAvailable()) {
        return;
    }
    QStringList todo;
    while (!m_queue.isEmpty() && todo.size() < MaxLookupBurst) {
        const QString u = m_queue.takeFirst();
        if (!isFresh(u) && !m_checking.contains(u)) {
            todo << u;
        }
    }
    if (todo.isEmpty()) {
        return;
    }
    m_inFlight = true;
    for (const auto &u : std::as_const(todo)) {
        m_checking.insert(u);
        touchUri(u);
    }
    QPointer<ContactSuggestionModel> self(this);
    m_client->lookupHandles(todo, [self, todo](const QList<HandleReachability> &result) {
        if (!self) {
            return;
        }
        self->m_inFlight = false;
        const qint64 now = self->m_clock.elapsed();
        for (const auto &u : todo) {
            self->m_checking.remove(u);
        }
        // Answers come in input order; the daemon may spell a handle differently.
        for (int i = 0; i < result.size(); ++i) {
            const QString key = result.size() == todo.size() ? todo.at(i) : result.at(i).handle;
            self->m_reachable.insert(key, Cached{result.at(i).reachable, now});
        }
        for (const auto &u : todo) {
            self->touchUri(u);
        }
        self->lookupNextBurst();
    }, [self, todo](const QString &, const QString &) {
        if (!self) {
            return;
        }
        // Unknown again; the next filter change retries. Drop the rest too:
        // the daemon is unlikely to do better for them right now.
        self->m_inFlight = false;
        self->m_queue.clear();
        for (const auto &u : todo) {
            self->m_checking.remove(u);
            self->touchUri(u);
        }
    });
}

void ContactSuggestionModel::touchUri(const QString &uri)
{
    for (int i = 0; i < m_rows.size(); ++i) {
        if (m_rows.at(i).uri == uri) {
            Q_EMIT dataChanged(index(i), index(i), {ReachabilityRole});
        }
    }
}

} // namespace km
