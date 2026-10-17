// SPDX-License-Identifier: SSPL-1.0
#include "appsettings.h"
#include "types.h"

#include <KConfigGroup>

namespace km
{

AppSettings::AppSettings(KSharedConfigPtr config, QObject *parent)
    : QObject(parent)
    , m_config(std::move(config))
{
}

bool AppSettings::read(const char *key, bool def) const
{
    return m_config->group(QStringLiteral("General")).readEntry(key, def);
}

void AppSettings::write(const char *key, bool value)
{
    if (read(key, !value) == value) {
        return;
    }
    auto group = m_config->group(QStringLiteral("General"));
    group.writeEntry(key, value);
    group.sync();
    Q_EMIT changed();
}

bool AppSettings::notificationsEnabled() const
{
    return read("NotificationsEnabled", true);
}

void AppSettings::setNotificationsEnabled(bool on)
{
    write("NotificationsEnabled", on);
}

bool AppSettings::startMinimized() const
{
    return read("StartMinimized", false);
}

void AppSettings::setStartMinimized(bool on)
{
    write("StartMinimized", on);
}

bool AppSettings::closeToTray() const
{
    return read("CloseToTray", true);
}

void AppSettings::setCloseToTray(bool on)
{
    write("CloseToTray", on);
}

QString AppSettings::otherHandles() const
{
    return m_config->group(QStringLiteral("General")).readEntry("OtherHandles", QString());
}

void AppSettings::setOtherHandles(const QString &text)
{
    const QString value = text.trimmed();
    if (value == otherHandles()) {
        return;
    }
    auto group = m_config->group(QStringLiteral("General"));
    group.writeEntry("OtherHandles", value);
    group.sync();
    Q_EMIT changed();
}

QStringList AppSettings::otherHandleUris() const
{
    QStringList out;
    const auto parts = otherHandles().split(QLatin1Char(','), Qt::SkipEmptyParts);
    for (const auto &p : parts) {
        const QString uri = normalizeHandle(p.trimmed());
        if (!uri.isEmpty() && !out.contains(uri)) {
            out << uri;
        }
    }
    return out;
}

} // namespace km
