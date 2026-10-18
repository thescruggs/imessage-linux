// SPDX-License-Identifier: SSPL-1.0
//
// "Sync from Mac": runs `imsg sync-mac --json` (contacts and pins from the
// user's Mac over ssh, docs/CLI.md) and turns its JSON summary into one line
// for a passive notification. Contacts land in ~/.local/share/kpeoplevcard,
// which KPeople watches; pins arrive from the daemon as ChatUpdated.
#pragma once

#include <KSharedConfig>

#include <QDateTime>
#include <QJsonObject>
#include <QObject>
#include <QPointer>
#include <QTimer>

class QProcess;

namespace km
{
class AppSettings;
class DaemonClient;

class MacSync : public QObject
{
    Q_OBJECT
    /// `imsg sync-mac` is running.
    Q_PROPERTY(bool running READ isRunning NOTIFY runningChanged)
    /// The Mac synced from: the Settings override, else the daemon's `mac_host`, "" = none set.
    Q_PROPERTY(QString host READ host NOTIFY hostChanged)
    /// End of the last successful sync (invalid = never), and as text ("" = never).
    Q_PROPERTY(QDateTime lastSync READ lastSync NOTIFY lastSyncChanged)
    Q_PROPERTY(QString lastSyncText READ lastSyncText NOTIFY lastSyncChanged)
    /// The message of the last finished run ("" before the first).
    Q_PROPERTY(QString lastMessage READ lastMessage NOTIFY finished)

public:
    /// Exit codes of imsg (docs/CLI.md).
    static constexpr int ExitUsage = 2;
    static constexpr int ExitNoDaemon = 5;

    MacSync(DaemonClient *client, AppSettings *settings, KSharedConfigPtr config, QObject *parent = nullptr);
    ~MacSync() override;

    bool isRunning() const { return m_process != nullptr; }
    QString host() const;
    QDateTime lastSync() const { return m_lastSync; }
    QString lastSyncText() const;
    QString lastMessage() const { return m_lastMessage; }

    /// The imsg to run: $KMESSAGES_IMSG, else the Settings path, else `imsg`
    /// on PATH; "" = not found.
    QString program() const;
    /// `sync-mac --json [-- HOST]` (HOST only when set in Settings: otherwise
    /// imsg asks the daemon for its `mac_host` itself).
    QStringList arguments() const;

    /// Start a sync (no-op while one is running). Ends with finished().
    Q_INVOKABLE void start();

    /// The notification text for a finished run: `json` is imsg's stdout
    /// (may be empty or not JSON), `stderrText` its diagnostics. `ok` is set
    /// to whether everything asked for was synced.
    static QString summarize(const QByteArray &json, int exitCode, const QString &stderrText, bool *ok);

    /// Kill a sync that has not finished after this long (ms).
    void setTimeout(int ms) { m_timer.setInterval(ms); }

Q_SIGNALS:
    void runningChanged();
    void hostChanged();
    void lastSyncChanged();
    /// A run ended; `message` is for a passive notification.
    void finished(bool ok, const QString &message);

private:
    void finish(bool ok, const QString &message);
    void onProcessFinished(int exitCode, bool crashed);

    QPointer<DaemonClient> m_client;
    QPointer<AppSettings> m_settings;
    KSharedConfigPtr m_config;
    QProcess *m_process = nullptr;
    QTimer m_timer;
    bool m_timedOut = false;
    QDateTime m_lastSync;
    QString m_lastMessage;
};

} // namespace km
