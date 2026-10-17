// SPDX-License-Identifier: SSPL-1.0
#include "fakedaemon.h"

#include <QDBusConnection>
#include <QDBusConnectionInterface>
#include <QDeadlineTimer>
#include <QDir>
#include <QFile>
#include <QJsonDocument>
#include <QTemporaryDir>
#include <QTest>

using namespace Qt::StringLiterals;

FakeDaemon::~FakeDaemon()
{
    stop();
}

bool FakeDaemon::nameOwned()
{
    auto *iface = QDBusConnection::sessionBus().interface();
    return iface && iface->isServiceRegistered(u"org.imessage.Daemon1"_s).value();
}

bool FakeDaemon::onPrivateBus()
{
    return qEnvironmentVariableIsSet("KMESSAGES_TEST_PRIVATE_BUS") && qEnvironmentVariableIsSet("DBUS_SESSION_BUS_ADDRESS");
}

bool FakeDaemon::start(const QStringList &args)
{
    if (!onPrivateBus()) {
        qWarning("not on a private test bus (run through ctest)");
        return false;
    }
    stop();
    m_proc.setProcessChannelMode(QProcess::ForwardedErrorChannel);
    m_proc.start(QStringLiteral(PYTHON3_EXECUTABLE), QStringList{QStringLiteral(FAKE_DAEMON_PY)} + args);
    if (!m_proc.waitForStarted(5000)) {
        qWarning() << "cannot start fake daemon:" << m_proc.errorString();
        return false;
    }
    QDeadlineTimer deadline(10000);
    while (!deadline.hasExpired()) {
        if (nameOwned()) {
            return true;
        }
        if (m_proc.state() == QProcess::NotRunning) {
            qWarning() << "fake daemon exited:" << m_proc.readAllStandardOutput();
            return false;
        }
        QTest::qWait(50);
    }
    qWarning("fake daemon did not claim its name");
    return false;
}

void FakeDaemon::stop()
{
    if (m_proc.state() == QProcess::NotRunning) {
        return;
    }
    m_proc.terminate();
    if (!m_proc.waitForFinished(5000)) {
        m_proc.kill();
        m_proc.waitForFinished(2000);
    }
    QDeadlineTimer deadline(5000);
    while (nameOwned() && !deadline.hasExpired()) {
        QTest::qWait(20);
    }
}

bool FakeDaemon::isRunning() const
{
    return m_proc.state() == QProcess::Running;
}

QDBusMessage FakeDaemon::control(const QString &method, const QVariantList &args)
{
    auto msg = QDBusMessage::createMethodCall(u"org.imessage.Daemon1"_s, u"/org/imessage/Daemon1"_s, u"org.imessage.FakeControl"_s, method);
    msg.setArguments(args);
    const auto reply = QDBusConnection::sessionBus().call(msg, QDBus::Block, 5000);
    if (reply.type() == QDBusMessage::ErrorMessage) {
        qWarning() << "control" << method << "failed:" << reply.errorMessage();
    }
    return reply;
}

QJsonArray FakeDaemon::calls()
{
    const auto reply = control(u"Calls"_s);
    if (reply.arguments().isEmpty()) {
        return {};
    }
    return QJsonDocument::fromJson(reply.arguments().first().toString().toUtf8()).array();
}

QList<QJsonArray> FakeDaemon::callsOf(const QString &method)
{
    QList<QJsonArray> out;
    const auto all = calls();
    for (const auto &c : all) {
        const auto a = c.toArray();
        if (!a.isEmpty() && a.first().toString() == method) {
            out << a;
        }
    }
    return out;
}

void useTemporaryXdgDirs()
{
    static QTemporaryDir dir(QDir::tempPath() + u"/kmessages-test-XXXXXX"_s);
    if (!dir.isValid()) {
        qFatal("cannot create a temporary XDG directory");
    }
    for (const char *var : {"XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME"}) {
        const QString path = dir.filePath(QString::fromLatin1(var).toLower());
        QDir().mkpath(path);
        qputenv(var, QFile::encodeName(path));
    }
}
