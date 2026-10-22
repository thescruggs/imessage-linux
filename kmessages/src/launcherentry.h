// SPDX-License-Identifier: SSPL-1.0
//
// The unread count on the task manager entry: the Unity LauncherEntry D-Bus
// API that Plasma's task manager (and docks such as Latte, Dash to Dock)
// read. An Update signal goes out whenever the count changes; Query answers
// with the same properties.
#pragma once

#include <QDBusConnection>
#include <QObject>
#include <QVariantMap>

namespace km
{

class LauncherEntry : public QObject
{
    Q_OBJECT
    Q_CLASSINFO("D-Bus Interface", "com.canonical.Unity.LauncherEntry")

public:
    static constexpr auto ObjectPath = "/org/imessage/kmessages/UnityLauncher";
    static constexpr auto Interface = "com.canonical.Unity.LauncherEntry";

    /// Registers ObjectPath on `bus`; `desktopFileId` without ".desktop".
    LauncherEntry(const QDBusConnection &bus, const QString &desktopFileId, QObject *parent = nullptr);
    ~LauncherEntry() override;

    /// "application://<id>.desktop"
    QString appUri() const { return m_appUri; }
    int count() const { return m_count; }
    bool isCountVisible() const { return m_visible && m_count > 0; }
    /// The Update properties: {"count": x, "count-visible": b}.
    QVariantMap properties() const;

    /// Emits Update when the shown badge changes (`visible` false: no badge
    /// whatever the count).
    void setCount(int count, bool visible = true);

public Q_SLOTS:
    /// com.canonical.Unity.LauncherEntry.Query → (app_uri, properties).
    Q_SCRIPTABLE QString Query(QVariantMap &properties) const;

Q_SIGNALS:
    /// Emitted with each Update on the bus (for tests).
    void updated(int count, bool visible);

private:
    QDBusConnection m_bus;
    QString m_appUri;
    int m_count = -1; ///< -1: nothing sent yet
    bool m_visible = false;
    bool m_registered = false;
};

} // namespace km
