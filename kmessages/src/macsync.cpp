// SPDX-License-Identifier: SSPL-1.0
#include "macsync.h"
#include "appsettings.h"
#include "daemonclient.h"

#include <KConfigGroup>
#include <KLocalizedString>

#include <QJsonArray>
#include <QJsonDocument>
#include <QLocale>
#include <QProcess>
#include <QStandardPaths>

using namespace Qt::StringLiterals;

namespace km
{

namespace
{
constexpr int DefaultTimeoutMs = 10 * 60 * 1000; // a big address book over a slow link
constexpr int KillGraceMs = 3000; // after SIGTERM, before SIGKILL

/// "312 contacts (3 updated, 1 removed)"; just `head` without details.
QString withDetails(const QString &head, const QStringList &details)
{
    return details.isEmpty() ? head : i18nc("sync from Mac: %1 = '312 contacts', %2 = details", "%1 (%2)", head, details.join(u", "_s));
}

/// The last non-empty line of imsg's / ssh's diagnostics, without imsg's prefix.
QString lastLine(const QString &text)
{
    const auto lines = text.split(u'\n', Qt::SkipEmptyParts);
    for (auto it = lines.crbegin(); it != lines.crend(); ++it) {
        QString line = it->trimmed();
        if (line.isEmpty()) {
            continue;
        }
        if (line.startsWith("imsg: error: "_L1)) {
            line = line.mid(13);
        }
        return line;
    }
    return {};
}
}

MacSync::MacSync(DaemonClient *client, AppSettings *settings, KSharedConfigPtr config, QObject *parent)
    : QObject(parent)
    , m_client(client)
    , m_settings(settings)
    , m_config(std::move(config))
{
    m_lastSync = m_config->group(u"MacSync"_s).readEntry("LastSync", QDateTime());
    m_timer.setSingleShot(true);
    m_timer.setInterval(DefaultTimeoutMs);
    connect(&m_timer, &QTimer::timeout, this, [this] {
        if (m_process) {
            m_timedOut = true;
            terminate();
        }
    });
    m_killTimer.setSingleShot(true);
    m_killTimer.setInterval(KillGraceMs);
    connect(&m_killTimer, &QTimer::timeout, this, [this] {
        if (m_process) {
            m_process->kill();
        }
    });
    if (client) {
        connect(client, &DaemonClient::statusChanged, this, &MacSync::hostChanged);
    }
    if (settings) {
        connect(settings, &AppSettings::changed, this, &MacSync::hostChanged);
    }
}

MacSync::~MacSync()
{
    if (m_process) {
        m_process->disconnect(this);
        m_process->kill();
        m_process->waitForFinished(1000);
    }
}

QString MacSync::host() const
{
    const QString own = m_settings ? m_settings->macHost() : QString();
    if (!own.isEmpty()) {
        return own;
    }
    return m_client ? m_client->macHost() : QString();
}

QString MacSync::lastSyncText() const
{
    return m_lastSync.isValid() ? QLocale().toString(m_lastSync.toLocalTime(), QLocale::ShortFormat) : QString();
}

QString MacSync::program() const
{
    const QString env = qEnvironmentVariable("KMESSAGES_IMSG");
    if (!env.isEmpty()) {
        return env;
    }
    const QString configured = m_settings ? m_settings->imsgPath() : QString();
    if (!configured.isEmpty()) {
        return configured;
    }
    return QStandardPaths::findExecutable(u"imsg"_s);
}

QStringList MacSync::arguments() const
{
    QStringList args{u"sync-mac"_s, u"--json"_s};
    const QString own = m_settings ? m_settings->macHost() : QString();
    if (!own.isEmpty()) {
        args << u"--"_s << own; // a host is never an option
    }
    return args;
}

void MacSync::start()
{
    if (m_process) {
        return;
    }
    const QString prog = program();
    if (prog.isEmpty()) {
        finish(false, i18n("Sync from Mac: the imsg program was not found. Install it, or set its path in Settings."));
        return;
    }
    // Without a host imsg would only say so; say it here, with where to set it.
    // (Unknown while the daemon is away: imsg then reports that itself.)
    if (host().isEmpty() && m_client && m_client->isStatusKnown()) {
        finish(false, i18n("Sync from Mac: no Mac set. Enter its ssh address in Settings."));
        return;
    }
    m_process = new QProcess(this);
    m_process->setProgram(prog);
    m_process->setArguments(arguments());
    m_process->setStandardInputFile(QProcess::nullDevice());
    m_timedOut = false;
    m_stopped = false;
    connect(m_process, &QProcess::finished, this, [this](int exitCode, QProcess::ExitStatus status) {
        onProcessFinished(exitCode, status == QProcess::CrashExit);
    });
    connect(m_process, &QProcess::errorOccurred, this, [this](QProcess::ProcessError error) {
        if (error != QProcess::FailedToStart || !m_process) {
            return; // crashes and kills end in finished()
        }
        const QString what = m_process->errorString();
        m_process->deleteLater();
        m_process = nullptr;
        m_timer.stop();
        m_killTimer.stop();
        Q_EMIT runningChanged();
        finish(false, i18n("Sync from Mac: cannot run imsg (%1).", what));
    });
    // Running from here on; a start failure (reported synchronously or later)
    // undoes it in the errorOccurred handler above.
    m_timer.start();
    Q_EMIT runningChanged();
    m_process->start();
}

void MacSync::stop()
{
    if (!m_process || m_stopped) {
        return;
    }
    m_stopped = true;
    terminate();
}

void MacSync::terminate()
{
    m_process->terminate();
    m_killTimer.start();
}

void MacSync::onProcessFinished(int exitCode, bool crashed)
{
    if (!m_process) {
        return;
    }
    const QByteArray out = m_process->readAllStandardOutput();
    const QString err = QString::fromUtf8(m_process->readAllStandardError());
    m_process->deleteLater();
    m_process = nullptr;
    m_timer.stop();
    m_killTimer.stop();
    Q_EMIT runningChanged();

    if (m_stopped) {
        finish(false, i18n("Sync from Mac stopped."));
        return;
    }
    if (m_timedOut) {
        finish(false, i18n("Sync from Mac: no answer from the Mac in time; stopped."));
        return;
    }
    if (crashed) {
        finish(false, i18n("Sync from Mac: imsg stopped unexpectedly."));
        return;
    }
    bool ok = false;
    const QString message = summarize(out, exitCode, err, &ok);
    if (ok) {
        m_lastSync = QDateTime::currentDateTimeUtc();
        auto group = m_config->group(u"MacSync"_s);
        group.writeEntry("LastSync", m_lastSync);
        group.sync();
        Q_EMIT lastSyncChanged();
    }
    finish(ok, message);
}

void MacSync::finish(bool ok, const QString &message)
{
    m_lastMessage = message;
    Q_EMIT finished(ok, message);
}

QString MacSync::summarize(const QByteArray &json, int exitCode, const QString &stderrText, bool *ok)
{
    *ok = false;
    if (exitCode == ExitNoDaemon) {
        return i18n("Sync from Mac: imessaged is not running. Start it and try again.");
    }
    QJsonParseError parseError;
    const QJsonDocument doc = QJsonDocument::fromJson(json, &parseError);
    if (!doc.isObject()) {
        // Not our imsg (too old for sync-mac), or it died before printing.
        const QString why = lastLine(stderrText);
        return why.isEmpty() ? i18n("Sync from Mac failed (imsg exited with code %1).", exitCode) : i18n("Sync from Mac failed: %1", why);
    }
    const QJsonObject o = doc.object();
    if (o.contains("error"_L1)) {
        return i18n("Sync from Mac failed: %1", o.value("error"_L1).toString());
    }

    QStringList parts;
    QStringList errors;
    const QJsonValue contactsValue = o.value("contacts"_L1);
    if (contactsValue.isObject()) {
        const QJsonObject c = contactsValue.toObject();
        if (c.contains("error"_L1)) {
            errors << i18nc("sync from Mac: a part failed", "contacts: %1", c.value("error"_L1).toString());
        } else {
            const int received = c.value("received"_L1).toInt();
            const int written = c.value("written"_L1).toInt();
            const int removed = c.value("removed"_L1).toInt();
            QStringList details;
            if (written > 0) {
                details << i18ncp("sync from Mac: contacts", "%1 updated", "%1 updated", written);
            }
            if (removed > 0) {
                details << i18ncp("sync from Mac: contacts", "%1 removed", "%1 removed", removed);
            }
            parts << withDetails(i18ncp("sync from Mac", "%1 contact", "%1 contacts", received), details);
        }
    }
    const QJsonValue pinsValue = o.value("pins"_L1);
    if (pinsValue.isObject()) {
        const QJsonObject p = pinsValue.toObject();
        if (p.contains("error"_L1)) {
            errors << i18nc("sync from Mac: a part failed", "pins: %1", p.value("error"_L1).toString());
        } else {
            int pinned = 0;
            int missing = 0;
            int duplicate = 0;
            for (const auto &v : p.value("mac"_L1).toArray()) {
                const QString result = v.toObject().value("result"_L1).toString();
                if (result == "pinned"_L1 || result == "would-pin"_L1) {
                    ++pinned;
                } else if (result == "not-in-daemon"_L1 || result == "unknown"_L1) {
                    ++missing;
                } else if (result == "duplicate"_L1) {
                    ++duplicate;
                }
            }
            const int unpinned = int(p.value("unpin"_L1).toArray().size());
            const int keptLocal = int(p.value("kept_local"_L1).toArray().size());
            const int keptUnmapped = int(p.value("kept_unmapped"_L1).toArray().size());
            QStringList details;
            if (missing > 0) {
                details << i18ncp("sync from Mac: pins", "%1 not found", "%1 not found", missing);
            }
            if (duplicate > 0) {
                details << i18ncp("sync from Mac: pins", "%1 duplicate", "%1 duplicates", duplicate);
            }
            if (unpinned > 0) {
                details << i18ncp("sync from Mac: pins the Mac no longer has", "%1 unpinned", "%1 unpinned", unpinned);
            }
            if (keptLocal > 0) {
                details << i18ncp("sync from Mac: pins made on this computer", "%1 local kept", "%1 local kept", keptLocal);
            }
            if (keptUnmapped > 0) {
                details << i18ncp("sync from Mac: earlier Mac pins kept because some pins could not be mapped",
                                  "%1 kept (unmapped)",
                                  "%1 kept (unmapped)",
                                  keptUnmapped);
            }
            const QString lookupError = p.value("lookup_error"_L1).toString();
            if (!lookupError.isEmpty()) {
                details << i18nc("sync from Mac: the Mac's chat.db could not be read", "group lookup failed: %1", lookupError);
            }
            parts << withDetails(i18ncp("sync from Mac", "%1 pin", "%1 pins", pinned), details);
        }
    }
    const QJsonValue backgroundsValue = o.value("backgrounds"_L1);
    if (backgroundsValue.isObject()) {
        const QJsonObject b = backgroundsValue.toObject();
        if (b.contains("error"_L1)) {
            errors << i18nc("sync from Mac: a part failed", "backgrounds: %1", b.value("error"_L1).toString());
        } else {
            parts << i18ncp("sync from Mac", "%1 background", "%1 backgrounds", int(b.value("chats"_L1).toArray().size()));
        }
    }

    if (!errors.isEmpty() || exitCode != 0) {
        if (errors.isEmpty()) {
            const QString why = lastLine(stderrText);
            errors << (why.isEmpty() ? i18n("imsg exited with code %1", exitCode) : why);
        }
        return i18n("Sync from Mac incomplete: %1", (parts + errors).join(u"; "_s));
    }
    *ok = true;
    const QString from = o.value("host"_L1).toString();
    const QString what = parts.isEmpty() ? i18nc("sync from Mac: nothing reported", "nothing to do") : parts.join(u", "_s);
    return from.isEmpty() ? i18n("Synced from your Mac: %1", what) : i18nc("%1 = host, %2 = summary", "Synced from %1: %2", from, what);
}

} // namespace km
