// SPDX-License-Identifier: SSPL-1.0
#pragma once

#include "types.h"

#include <QAbstractListModel>
#include <QHash>
#include <QPointer>
#include <QSet>
#include <QUrl>

namespace km
{
class DaemonClient;
class ChatListModel;

/// Transcript of one chat, **newest first** (row 0 = newest), so a
/// bottom-to-top ListView shows it chronologically and loadOlder() appends
/// older pages (ListMessages with the previous page's cursor). Live
/// MessageReceived / MessageUpdated / Typing signals are applied in place.
/// Grouping, separators and the delivery label are computed per row.
class MessageListModel : public QAbstractListModel
{
    Q_OBJECT
    Q_PROPERTY(QString chatId READ chatId WRITE setChatId NOTIFY chatIdChanged)
    Q_PROPERTY(bool isGroup READ isGroup NOTIFY chatIdChanged)
    Q_PROPERTY(int count READ rowCount NOTIFY countChanged)
    Q_PROPERTY(bool loading READ isLoading NOTIFY loadingChanged)
    Q_PROPERTY(bool loaded READ isLoaded NOTIFY loadingChanged)
    Q_PROPERTY(bool canLoadMore READ canLoadMore NOTIFY canLoadMoreChanged)
    Q_PROPERTY(int pageSize READ pageSize WRITE setPageSize NOTIFY pageSizeChanged)
    Q_PROPERTY(bool someoneTyping READ someoneTyping NOTIFY typingChanged)
    Q_PROPERTY(QString typingText READ typingText NOTIFY typingChanged)
    Q_PROPERTY(QString lastError READ lastError NOTIFY lastErrorChanged)
    /// Sending here goes out as a text message (SMS via the iPhone): Text
    /// Message Forwarding is active, the chat is with phone numbers, and one
    /// of them is not on iMessage (or, until that is known, the newest
    /// message was a text). Green composer, no typing indicators.
    Q_PROPERTY(bool smsMode READ smsMode NOTIFY smsModeChanged)

public:
    enum Roles {
        MessageIdRole = Qt::UserRole + 1,
        TextRole,
        RichTextRole,
        HasTextRole,
        IsEmojiOnlyRole,
        SenderRole,
        SenderNameRole,
        IsFromMeRole,
        TimestampRole,
        TimeRole,
        ShowSeparatorRole,
        SeparatorLabelRole,
        FirstInGroupRole, ///< top bubble of a run from one sender
        LastInGroupRole, ///< bottom bubble of a run: gets the tail
        ShowSenderNameRole,
        ShowAvatarRole,
        AvatarColorRole,
        AvatarSourceRole, ///< sender's contact photo URL, or ""
        InitialsRole,
        DeliveryRole,
        DeliveryLabelRole,
        EditedRole,
        UnsentRole,
        VerifiedRole,
        ReactionsRole,
        MyReactionRole,
        AttachmentsRole,
        HasAttachmentsRole,
        ReplyToRole,
        ReplyToTextRole,
        ReplyToSenderRole,
        SubjectRole,
        EffectRole,
        UnsupportedRole,
        IsSmsRole, ///< a text message relayed by the iPhone (green bubble, no tapbacks / edit)
        LinkRole, ///< the link preview (QVariantMap, see link()), empty map = none
        LinkOnlyRole, ///< the text is just the link's URL: the card replaces it
    };
    Q_ENUM(Roles)

    explicit MessageListModel(DaemonClient *client, ChatListModel *chats = nullptr, QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role) const override;
    QHash<int, QByteArray> roleNames() const override;
    bool canFetchMore(const QModelIndex &parent) const override;
    void fetchMore(const QModelIndex &parent) override;

    QString chatId() const { return m_chatId; }
    void setChatId(const QString &chatId);
    bool isGroup() const;
    bool isLoading() const { return m_loading; }
    bool isLoaded() const { return m_loaded; }
    bool canLoadMore() const { return m_loaded && !m_cursor.isEmpty(); }
    int pageSize() const { return m_pageSize; }
    void setPageSize(int size);
    bool someoneTyping() const { return !m_typing.isEmpty(); }
    QString typingText() const;
    QString lastError() const { return m_lastError; }
    bool smsMode() const;
    /// The phone numbers this chat is with (1:1 `tel:` or a `group-p:` of
    /// numbers, our own left out); empty for anything else.
    QStringList phoneRecipients() const;
    const QList<Message> &messages() const { return m_messages; }
    /// Wait before the one retry of a DownloadAttachment that timed out (default 30 s).
    void setDownloadRetryDelay(int ms) { m_downloadRetryMs = ms; }

    Q_INVOKABLE void reload();
    Q_INVOKABLE void loadOlder();
    Q_INVOKABLE int rowOf(const QString &messageId) const;
    Q_INVOKABLE QVariantMap get(int row) const;

    // Actions on the current chat (results arrive as daemon signals).
    Q_INVOKABLE void sendText(const QString &text, const QString &replyTo = QString());
    /// Send a file; once sent, the bubble shows it from `file` itself (no download).
    Q_INVOKABLE void sendFile(const QUrl &file);
    /// Toggle our tapback `kind` (`love`, …, `emoji:🔥`) on a message.
    Q_INVOKABLE void react(const QString &messageId, const QString &kind);
    Q_INVOKABLE void edit(const QString &messageId, const QString &text);
    Q_INVOKABLE void unsend(const QString &messageId);
    Q_INVOKABLE void setTyping(bool typing);
    Q_INVOKABLE void markRead();
    Q_INVOKABLE void downloadAttachment(const QString &attachmentId);
    /// The local file of an attachment could not be loaded (moved, deleted):
    /// forget it and download the daemon's copy (once per attachment).
    Q_INVOKABLE void reloadAttachment(const QString &attachmentId);
    /// Download if needed, then open with the default application.
    Q_INVOKABLE void openAttachment(const QString &attachmentId);
    Q_INVOKABLE void copyText(const QString &messageId);
    /// Copy the URL of the message's link preview.
    Q_INVOKABLE void copyLink(const QString &messageId);

Q_SIGNALS:
    void chatIdChanged();
    void countChanged();
    void loadingChanged();
    void canLoadMoreChanged();
    void pageSizeChanged();
    void typingChanged();
    void lastErrorChanged();
    void smsModeChanged();
    void attachmentReady(const QString &attachmentId, const QString &path);
    /// A new message was added at the bottom (row 0).
    void newestMessageAdded(bool fromMe);
    /// sendText() failed; the UI can put `text` back into the composer.
    void sendTextFailed(const QString &chatId, const QString &text, const QString &replyTo, const QString &error);
    /// edit() failed; the UI can put the edit back into the composer.
    void editFailed(const QString &chatId, const QString &messageId, const QString &text, const QString &error);

private:
    void onMessageReceived(const QString &chatId, const Message &msg);
    void onMessageUpdated(const QString &chatId, const Message &msg);
    void onTyping(const QString &chatId, const QString &handle, bool typing);
    void setLoading(bool loading);
    void setLastError(const QString &error);
    void recomputeLastOwn();
    /// LookupHandles for phoneRecipients() when SMS forwarding is active.
    void checkReachability();
    /// Emit dataChanged for rows [from, to] (clamped).
    void touchRows(int from, int to);
    /// Emit dataChanged(AttachmentsRole) for the rows holding `attachmentId`.
    void touchAttachment(const QString &attachmentId);
    /// Our own message `msg` carries a file we are sending: use the local file.
    void adoptOutgoingPaths(const QString &chatId, const Message &msg);

    bool sameGroup(int older, int newer) const;
    bool showSeparator(int row) const;
    bool firstInGroup(int row) const;
    bool lastInGroup(int row) const;
    QString senderName(const Message &m) const;
    QString deliveryLabel(int row) const;
    QVariantList reactions(const Message &m) const;
    QString myReaction(const Message &m) const;
    QVariantList attachments(const Message &m) const;
    /// url, title, summary, siteName, host, image / icon (each {id,
    /// localPath, downloading, error} or an empty map).
    QVariantMap link(const Message &m) const;
    QVariantMap attachmentState(const QString &id) const;
    const Message *findMessage(const QString &id) const;
    static QString richText(const Message &m);
    static QString linkify(const QString &plain);
    static bool isEmojiOnly(const QString &text);

    QPointer<DaemonClient> m_client;
    QPointer<ChatListModel> m_chats;
    QString m_chatId;
    QList<Message> m_messages; ///< newest first
    QString m_cursor; ///< next_cursor of the oldest page loaded; empty = no more
    bool m_loading = false;
    bool m_loaded = false;
    int m_pageSize = 50;
    int m_lastOwnRow = -1;
    quint64 m_generation = 0;
    QSet<QString> m_typing;
    QStringList m_ownHandles; ///< last seen DaemonClient::ownHandles()
    QString m_lastError;
    int m_reachable = -1; ///< phoneRecipients() all on iMessage: 1, not all: 0, unknown: -1
    bool m_lastSmsActive = false;
    QHash<QString, QString> m_paths; ///< attachment id → local path
    QSet<QString> m_downloading;
    QHash<QString, QString> m_downloadErrors;
    QSet<QString> m_openWhenReady;
    QSet<QString> m_reloaded; ///< reloadAttachment() done once
    QSet<QString> m_downloadRetried; ///< timed out once already
    int m_downloadRetryMs = 30 * 1000;
    /// Files whose SendFile has not been answered yet: the MessageReceived for
    /// them usually comes first, matched by chat, file name and size; each
    /// entry is used for one attachment only.
    struct OutgoingFile {
        quint64 token;
        QString chatId;
        QString fileName;
        QString localPath;
        qint64 size = -1;
        bool adopted = false;
    };
    QList<OutgoingFile> m_outgoing;
    quint64 m_outgoingSeq = 0;
};

} // namespace km
