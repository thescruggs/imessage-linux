// SPDX-License-Identifier: SSPL-1.0
#pragma once

#include <KSharedConfig>
#include <QObject>

namespace km
{

/// User settings, stored in kmessagesrc ([General]).
class AppSettings : public QObject
{
    Q_OBJECT
    Q_PROPERTY(bool notificationsEnabled READ notificationsEnabled WRITE setNotificationsEnabled NOTIFY changed)
    Q_PROPERTY(bool startMinimized READ startMinimized WRITE setStartMinimized NOTIFY changed)
    Q_PROPERTY(bool closeToTray READ closeToTray WRITE setCloseToTray NOTIFY changed)
    /// "My other addresses": comma-separated phone numbers / emails of the
    /// user that this computer is not registered for (e.g. the iPhone's number).
    Q_PROPERTY(QString otherHandles READ otherHandles WRITE setOtherHandles NOTIFY changed)
    /// "Sync from Mac": the Mac's ssh destination ("" = the daemon's `mac_host`).
    Q_PROPERTY(QString macHost READ macHost WRITE setMacHost NOTIFY changed)
    /// "Sync from Mac": the `imsg` program to run ("" = `imsg` on PATH).
    Q_PROPERTY(QString imsgPath READ imsgPath WRITE setImsgPath NOTIFY changed)

public:
    explicit AppSettings(KSharedConfigPtr config = KSharedConfig::openConfig(), QObject *parent = nullptr);

    bool notificationsEnabled() const;
    void setNotificationsEnabled(bool on);
    bool startMinimized() const;
    void setStartMinimized(bool on);
    bool closeToTray() const;
    void setCloseToTray(bool on);
    QString otherHandles() const;
    void setOtherHandles(const QString &text);
    /// otherHandles() as IDS URIs (entries that do not normalize are skipped).
    QStringList otherHandleUris() const;
    QString macHost() const;
    void setMacHost(const QString &host);
    QString imsgPath() const;
    void setImsgPath(const QString &path);

Q_SIGNALS:
    void changed();

private:
    bool read(const char *key, bool def) const;
    void write(const char *key, bool value);
    QString readText(const char *key) const;
    void writeText(const char *key, const QString &value);
    KSharedConfigPtr m_config;
};

} // namespace km
