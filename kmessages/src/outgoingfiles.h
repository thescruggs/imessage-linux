// SPDX-License-Identifier: SSPL-1.0
//
// Files on their way to SendFile. The daemon runs sandboxed (systemd:
// PrivateTmp, ProtectSystem=strict, ProtectHome=read-only), so it cannot
// see /tmp, /run/user/… (portal documents) and the like: such files are
// copied into a staging directory under ~/.cache first. Clipboard images
// are saved there too.
#pragma once

#include <QString>

class QImage;

namespace km::Outgoing
{

/// `$XDG_CACHE_HOME/kmessages/outgoing`.
QString stagingDir();

/// The daemon may not be able to read `absPath`: it is outside the home
/// directory, or in /tmp, /var/tmp, /run or /dev/shm. Files already in
/// stagingDir() never need staging.
bool needsStaging(const QString &absPath);

struct Staged {
    QString path; ///< the copy to send
    QString dir; ///< its private directory (removeStaged() deletes it)
    QString error; ///< non-empty on failure
};
/// Copy `absPath` to stagingDir()/<uuid>/<file name>.
Staged stage(const QString &absPath);
/// Delete a directory returned in Staged::dir (anything else is refused).
void removeStaged(const QString &dir);

/// Save `image` as stagingDir()/<uuid>/<baseName>.png; "" on failure.
QString saveImage(const QImage &image, const QString &baseName);

/// Delete staging directories last modified more than `maxAgeSecs` ago.
void purge(qint64 maxAgeSecs);

} // namespace km::Outgoing
