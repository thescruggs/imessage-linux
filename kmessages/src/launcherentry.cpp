// SPDX-License-Identifier: SSPL-1.0
#include "launcherentry.h"

#include <QDBusMessage>

using namespace Qt::StringLiterals;

namespace km
{

LauncherEntry::LauncherEntry(const QDBusConnection &bus, const QString &desktopFileId, QObject *parent)
    : QObject(parent)
    , m_bus(bus)
    , m_appUri(u"application://"_s + desktopFileId + u".desktop"_s)
{
    // Without the object the Update signal still works; only Query is lost.
    m_registered = m_bus.isConnected() && m_bus.registerObject(QLatin1StringView(ObjectPath), this, QDBusConnection::ExportScriptableSlots);
}

LauncherEntry::~LauncherEntry()
{
    if (m_registered) {
        m_bus.unregisterObject(QLatin1StringView(ObjectPath));
    }
}

QVariantMap LauncherEntry::properties() const
{
    return {
        {u"count"_s, QVariant::fromValue(qint64(qMax(m_count, 0)))},
        {u"count-visible"_s, isCountVisible()},
    };
}

void LauncherEntry::setCount(int count, bool visible)
{
    count = qMax(count, 0);
    if (count == m_count && visible == m_visible) {
        return;
    }
    m_count = count;
    m_visible = visible;
    if (m_bus.isConnected()) {
        auto signal = QDBusMessage::createSignal(QLatin1StringView(ObjectPath), QLatin1StringView(Interface), u"Update"_s);
        signal << m_appUri << properties();
        m_bus.send(signal);
    }
    Q_EMIT updated(m_count, isCountVisible());
}

QString LauncherEntry::Query(QVariantMap &properties) const
{
    properties = this->properties();
    return m_appUri;
}

} // namespace km
