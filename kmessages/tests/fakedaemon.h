// SPDX-License-Identifier: SSPL-1.0
//
// Runs tests/fake_daemon.py on the (private) session bus the test runs on.
// CTest wraps every test in `dbus-run-session --config-file=session.conf`.
#pragma once

#include <QDBusMessage>
#include <QJsonArray>
#include <QProcess>
#include <QStringList>

class FakeDaemon
{
public:
    FakeDaemon() = default;
    ~FakeDaemon();

    /// Start with extra arguments (e.g. {"--state", "needs_login", "--empty"}); waits for the bus name.
    bool start(const QStringList &args = {});
    void stop();
    bool isRunning() const;

    /// Synchronous call on org.imessage.FakeControl.
    QDBusMessage control(const QString &method, const QVariantList &args = {});
    /// Method calls the fake has seen: [[method, args…], …]
    QJsonArray calls();
    /// Calls named `method` only.
    QList<QJsonArray> callsOf(const QString &method);

    static bool nameOwned();
    /// Refuses to run on what looks like the user's real session bus.
    static bool onPrivateBus();

private:
    QProcess m_proc;
};

/// Point XDG_CONFIG_HOME / XDG_DATA_HOME / XDG_CACHE_HOME / XDG_STATE_HOME at a
/// temporary directory removed at exit, so tests never write into the
/// user's home. Call first thing in main(), before the Q*Application exists.
void useTemporaryXdgDirs();
