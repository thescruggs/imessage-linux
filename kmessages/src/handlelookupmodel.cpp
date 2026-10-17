// SPDX-License-Identifier: SSPL-1.0
#include "handlelookupmodel.h"
#include "contactresolver.h"
#include "daemonclient.h"
#include "types.h"

#include <KLocalizedString>

#include <QRegularExpression>

using namespace Qt::StringLiterals;

namespace km
{

HandleLookupModel::HandleLookupModel(DaemonClient *client, QObject *parent)
    : QAbstractListModel(parent)
    , m_client(client)
{
    if (client) {
        connect(client, &DaemonClient::statusChanged, this, &HandleLookupModel::changed);
    }
    if (auto *contacts = ContactResolver::instance()) {
        connect(contacts, &ContactResolver::contactsChanged, this, [this] {
            if (!m_entries.isEmpty()) {
                Q_EMIT dataChanged(index(0), index(int(m_entries.size()) - 1));
                Q_EMIT changed();
            }
        });
    }
}

int HandleLookupModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : int(m_entries.size());
}

QHash<int, QByteArray> HandleLookupModel::roleNames() const
{
    return {{InputRole, "input"}, {HandleRole, "handle"}, {LabelRole, "label"}, {StateRole, "state"}, {StateNameRole, "stateName"}, {MessageRole, "message"}};
}

QVariant HandleLookupModel::data(const QModelIndex &index, int role) const
{
    if (!checkIndex(index, CheckIndexOption::IndexIsValid | CheckIndexOption::ParentIsInvalid)) {
        return {};
    }
    const Entry &e = m_entries.at(index.row());
    switch (role) {
    case InputRole:
        return e.input;
    case HandleRole:
        return e.handle;
    case Qt::DisplayRole:
    case LabelRole:
        return e.handle.isEmpty() ? e.input : Handle{e.handle, {}}.displayName();
    case StateRole:
        return int(e.state);
    case StateNameRole:
        switch (e.state) {
        case Checking:
            return u"checking"_s;
        case Reachable:
            return u"reachable"_s;
        case Unreachable:
            return u"unreachable"_s;
        case Invalid:
            return u"invalid"_s;
        case Error:
            return u"error"_s;
        }
        return QString();
    case MessageRole:
        return e.message;
    }
    return {};
}

QStringList HandleLookupModel::handles() const
{
    QStringList out;
    for (const auto &e : m_entries) {
        if (!e.handle.isEmpty()) {
            out << e.handle;
        }
    }
    return out;
}

bool HandleLookupModel::isChecking() const
{
    return std::any_of(m_entries.cbegin(), m_entries.cend(), [](const Entry &e) {
        return e.state == Checking;
    });
}

bool HandleLookupModel::textable(const QString &handle) const
{
    return m_client && m_client->isSmsActive() && handle.startsWith("tel:"_L1);
}

bool HandleLookupModel::canSend() const
{
    return !m_entries.isEmpty() && std::all_of(m_entries.cbegin(), m_entries.cend(), [this](const Entry &e) {
        return e.state == Reachable || (e.state == Unreachable && textable(e.handle));
    });
}

bool HandleLookupModel::smsMode() const
{
    return canSend() && std::any_of(m_entries.cbegin(), m_entries.cend(), [](const Entry &e) {
        return e.state == Unreachable;
    });
}

QString HandleLookupModel::problem() const
{
    for (const auto &e : m_entries) {
        switch (e.state) {
        case Invalid:
            return i18n("“%1” is not an email address or phone number.", e.input);
        case Unreachable:
            if (textable(e.handle)) {
                break; // goes out as a text message
            }
            return i18n("%1 is not registered with iMessage.", Handle{e.handle, {}}.displayName());
        case Error:
            return i18n("Could not check %1: %2", e.input, e.message);
        case Checking:
        case Reachable:
            break;
        }
    }
    return {};
}

bool HandleLookupModel::add(const QString &input)
{
    static const QRegularExpression sep(u"[,;\\n]"_s);
    bool added = false;
    const auto pieces = input.split(sep, Qt::SkipEmptyParts);
    for (const auto &piece : pieces) {
        const QString raw = piece.trimmed();
        if (raw.isEmpty()) {
            continue;
        }
        const QString handle = normalizeHandle(raw);
        if (!handle.isEmpty() && handles().contains(handle)) {
            continue;
        }
        const int row = int(m_entries.size());
        beginInsertRows(QModelIndex(), row, row);
        m_entries.append({raw, handle, handle.isEmpty() ? Invalid : Checking, {}});
        endInsertRows();
        added = true;
        if (!handle.isEmpty()) {
            lookup(handle);
        }
    }
    if (added) {
        Q_EMIT changed();
    }
    return added;
}

void HandleLookupModel::lookup(const QString &handle)
{
    if (!m_client) {
        setState(handle, Error, i18n("not connected"));
        return;
    }
    QPointer<HandleLookupModel> self(this);
    m_client->lookupHandles({handle}, [self, handle](const QList<HandleReachability> &result) {
        if (!self) {
            return;
        }
        const bool reachable = !result.isEmpty() && result.first().reachable;
        self->setState(handle, reachable ? Reachable : Unreachable);
    }, [self, handle](const QString &name, const QString &message) {
        if (self) {
            self->setState(handle, name == "InvalidHandle"_L1 ? Invalid : Error, message);
        }
    });
}

void HandleLookupModel::setState(const QString &handle, State state, const QString &message)
{
    for (int i = 0; i < m_entries.size(); ++i) {
        if (m_entries.at(i).handle == handle) {
            m_entries[i].state = state;
            m_entries[i].message = message;
            const auto idx = index(i);
            Q_EMIT dataChanged(idx, idx);
        }
    }
    Q_EMIT changed();
}

void HandleLookupModel::remove(int row)
{
    if (row < 0 || row >= m_entries.size()) {
        return;
    }
    beginRemoveRows(QModelIndex(), row, row);
    m_entries.removeAt(row);
    endRemoveRows();
    Q_EMIT changed();
}

void HandleLookupModel::removeLast()
{
    remove(int(m_entries.size()) - 1);
}

void HandleLookupModel::clear()
{
    beginResetModel();
    m_entries.clear();
    endResetModel();
    Q_EMIT changed();
}

} // namespace km
