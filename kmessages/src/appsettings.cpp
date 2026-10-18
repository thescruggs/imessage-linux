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

QString AppSettings::readText(const char *key) const
{
    return m_config->group(QStringLiteral("General")).readEntry(key, QString());
}

void AppSettings::writeText(const char *key, const QString &text)
{
    const QString value = text.trimmed();
    if (value == readText(key)) {
        return;
    }
    auto group = m_config->group(QStringLiteral("General"));
    group.writeEntry(key, value);
    group.sync();
    Q_EMIT changed();
}

QString AppSettings::otherHandles() const
{
    return readText("OtherHandles");
}

void AppSettings::setOtherHandles(const QString &text)
{
    writeText("OtherHandles", text);
}

QString AppSettings::macHost() const
{
    return readText("MacHost");
}

void AppSettings::setMacHost(const QString &host)
{
    writeText("MacHost", host);
}

QString AppSettings::imsgPath() const
{
    return readText("ImsgPath");
}

void AppSettings::setImsgPath(const QString &path)
{
    writeText("ImsgPath", path);
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
