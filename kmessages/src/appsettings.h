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
    /// Seconds without keyboard or mouse input after which the user counts as
    /// away: the open chat is then notified and left unread (0 = never away).
    Q_PROPERTY(int awayAfterSeconds READ awayAfterSeconds WRITE setAwayAfterSeconds NOTIFY changed)
    /// The unread count on the task manager entry.
    Q_PROPERTY(bool unreadBadge READ unreadBadge WRITE setUnreadBadge NOTIFY changed)
    /// "My other addresses": comma-separated phone numbers / emails of the
    /// user that this computer is not registered for (e.g. the iPhone's number).
    Q_PROPERTY(QString otherHandles READ otherHandles WRITE setOtherHandles NOTIFY changed)
    /// "Sync from Mac": the Mac's ssh destination ("" = the daemon's `mac_host`).
    Q_PROPERTY(QString macHost READ macHost WRITE setMacHost NOTIFY changed)
    /// "Sync from Mac": the `imsg` program to run ("" = `imsg` on PATH).
    Q_PROPERTY(QString imsgPath READ imsgPath WRITE setImsgPath NOTIFY changed)
    /// The composer offers emoji for the word being typed ("snake" → 🐍).
    Q_PROPERTY(bool emojiSuggestions READ emojiSuggestions WRITE setEmojiSuggestions NOTIFY changed)
    /// Emoji inserted from the suggestions or the picker, newest first ([Emoji] Recent).
    Q_PROPERTY(QStringList recentEmoji READ recentEmoji NOTIFY recentEmojiChanged)

public:
    explicit AppSettings(KSharedConfigPtr config = KSharedConfig::openConfig(), QObject *parent = nullptr);

    bool notificationsEnabled() const;
    void setNotificationsEnabled(bool on);
    bool startMinimized() const;
    void setStartMinimized(bool on);
    bool closeToTray() const;
    void setCloseToTray(bool on);
    int awayAfterSeconds() const;
    void setAwayAfterSeconds(int seconds);
    bool unreadBadge() const;
    void setUnreadBadge(bool on);
    QString otherHandles() const;
    void setOtherHandles(const QString &text);
    /// otherHandles() as IDS URIs (entries that do not normalize are skipped).
    QStringList otherHandleUris() const;
    QString macHost() const;
    void setMacHost(const QString &host);
    QString imsgPath() const;
    void setImsgPath(const QString &path);
    bool emojiSuggestions() const;
    void setEmojiSuggestions(bool on);
    QStringList recentEmoji() const;
    /// Moves `emoji` to the front of recentEmoji (at most MaxRecentEmoji kept).
    Q_INVOKABLE void addRecentEmoji(const QString &emoji);

    static constexpr int MaxRecentEmoji = 24;
    static constexpr int DefaultAwayAfterSeconds = 60;

Q_SIGNALS:
    void changed();
    void recentEmojiChanged();

private:
    bool read(const char *key, bool def) const;
    void write(const char *key, bool value);
    QString readText(const char *key) const;
    void writeText(const char *key, const QString &value);
    KSharedConfigPtr m_config;
};

} // namespace km
