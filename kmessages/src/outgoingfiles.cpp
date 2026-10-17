// SPDX-License-Identifier: SSPL-1.0
#include "outgoingfiles.h"

#include <KLocalizedString>

#include <QDateTime>
#include <QDir>
#include <QFile>
#include <QFileInfo>
#include <QImage>
#include <QLoggingCategory>
#include <QStandardPaths>
#include <QUuid>

using namespace Qt::StringLiterals;

Q_LOGGING_CATEGORY(KM_OUTGOING, "kmessages.outgoing", QtInfoMsg)

namespace km::Outgoing
{

namespace
{
bool isUnder(const QString &path, const QString &dir)
{
    if (dir.isEmpty()) {
        return false;
    }
    const QString d = dir.endsWith(u'/') ? dir : dir + u'/';
    return path.startsWith(d);
}

/// Canonical when it exists, else cleaned.
QString resolved(const QString &path)
{
    const QString canonical = QFileInfo(path).canonicalFilePath();
    return canonical.isEmpty() ? QDir::cleanPath(path) : canonical;
}

/// A fresh private directory stagingDir()/<uuid>; "" on failure.
QString makeStagingSubdir()
{
    const QString base = stagingDir();
    if (!QDir().mkpath(base)) {
        return {};
    }
    QFile::setPermissions(base, QFileDevice::ReadOwner | QFileDevice::WriteOwner | QFileDevice::ExeOwner);
    const QString dir = base + u'/' + QUuid::createUuid().toString(QUuid::WithoutBraces);
    if (!QDir().mkdir(dir)) {
        return {};
    }
    QFile::setPermissions(dir, QFileDevice::ReadOwner | QFileDevice::WriteOwner | QFileDevice::ExeOwner);
    return dir;
}
}

QString stagingDir()
{
    return QStandardPaths::writableLocation(QStandardPaths::GenericCacheLocation) + "/kmessages/outgoing"_L1;
}

bool needsStaging(const QString &absPath)
{
    const QString path = resolved(absPath);
    if (isUnder(path, resolved(stagingDir()))) {
        return false;
    }
    // Private or hidden from the daemon's sandbox, whatever $HOME is.
    for (const auto prefix : {"/tmp"_L1, "/var/tmp"_L1, "/run"_L1, "/dev/shm"_L1}) {
        if (isUnder(path, prefix)) {
            return true;
        }
    }
    return !isUnder(path, resolved(QDir::homePath()));
}

Staged stage(const QString &absPath)
{
    Staged out;
    const QFileInfo info(absPath);
    if (!info.isFile()) {
        out.error = i18n("%1 is not a file.", absPath);
        return out;
    }
    out.dir = makeStagingSubdir();
    if (out.dir.isEmpty()) {
        out.error = i18n("Cannot create %1.", stagingDir());
        return out;
    }
    out.path = out.dir + u'/' + info.fileName();
    QFile src(absPath);
    if (!src.copy(out.path)) {
        out.error = i18n("Cannot copy %1 for sending: %2", absPath, src.errorString());
        removeStaged(out.dir);
        out.dir.clear();
        out.path.clear();
        return out;
    }
    qCDebug(KM_OUTGOING) << "staged" << absPath << "as" << out.path;
    return out;
}

void removeStaged(const QString &dir)
{
    if (dir.isEmpty() || !isUnder(QDir::cleanPath(dir), QDir::cleanPath(stagingDir()))) {
        return;
    }
    QDir(dir).removeRecursively();
}

QString saveImage(const QImage &image, const QString &baseName)
{
    if (image.isNull()) {
        return {};
    }
    const QString dir = makeStagingSubdir();
    if (dir.isEmpty()) {
        return {};
    }
    const QString path = dir + u'/' + baseName + ".png"_L1;
    if (!image.save(path, "PNG")) {
        removeStaged(dir);
        return {};
    }
    return path;
}

void purge(qint64 maxAgeSecs)
{
    const QDir base(stagingDir());
    if (!base.exists()) {
        return;
    }
    const QDateTime cutoff = QDateTime::currentDateTime().addSecs(-maxAgeSecs);
    // Our own subdirectories only: a symlink planted here must not make us
    // empty its target (removeRecursively() itself does not follow links).
    const auto entries = base.entryInfoList(QDir::Dirs | QDir::NoDotAndDotDot | QDir::NoSymLinks);
    for (const auto &e : entries) {
        if (e.isSymLink()) {
            continue;
        }
        if (e.lastModified() < cutoff) {
            QDir(e.absoluteFilePath()).removeRecursively();
        }
    }
}

} // namespace km::Outgoing
