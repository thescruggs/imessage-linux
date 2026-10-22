// SPDX-License-Identifier: SSPL-1.0
#include "appcontroller.h"

#include <KIdleTime>
#include <KLocalizedString>
#include <KNotification>
#include <KNotificationReplyAction>
#include <KStatusNotifierItem>

#include "outgoingfiles.h"

#include <QClipboard>
#include <QCoreApplication>
#include <QDateTime>
#include <QGuiApplication>
#include <QImage>
#include <QMenu>
#include <QMimeData>
#include <QPixmap>

using namespace Qt::StringLiterals;

namespace km
{

AppController::AppController(const QDBusConnection &bus, const AppOptions &options, KSharedConfigPtr config, QObject *parent)
    : QObject(parent)
    , m_options(options)
{
    // Pasted images and copies of sandbox-invisible files from earlier runs.
    Outgoing::purge(24 * 60 * 60);

    m_contacts = new ContactResolver(options.kpeople, this);
    m_client = new DaemonClient(bus, this);
    m_chatList = new ChatListModel(m_client, this);
    m_chats = new ChatFilterModel(m_chatList, this);
    m_suggestions = new ChatFilterModel(m_chatList, this);
    m_messages = new MessageListModel(m_client, m_chatList, this);
    m_recipients = new HandleLookupModel(m_client, this);
    m_search = new MessageSearchModel(m_client, m_chatList, this);
    m_settings = new AppSettings(config, this);
    m_macSync = new MacSync(m_client, m_settings, std::move(config), this);
    m_contactSuggestions = new ContactSuggestionModel(m_client, m_contacts, this);
    m_recipientPicker = new RecipientPickerModel(m_contactSuggestions, m_suggestions, this);

    // "My other addresses" (the iPhone's number …) count as the user's own.
    m_client->setOtherOwnHandles(m_settings->otherHandleUris());
    connect(m_settings, &AppSettings::changed, this, [this] {
        m_client->setOtherOwnHandles(m_settings->otherHandleUris());
    });

    // Away from the keyboard: the window may be active on the open chat, but
    // nobody is reading it. Without an idle backend (e.g. offscreen) no
    // timeout ever fires, so the user is never away.
    if (m_options.idleDetection) {
        auto *idle = KIdleTime::instance();
        connect(idle, &KIdleTime::timeoutReached, this, [this](int id) {
            if (id != 0 && id == m_idleTimeoutId) {
                setUserAway(true);
            }
        });
        connect(idle, &KIdleTime::resumingFromIdle, this, [this] {
            setUserAway(false);
        });
    }
    updateIdleWatch();
    connect(m_settings, &AppSettings::changed, this, &AppController::updateIdleWatch);

    m_problemTimer.setSingleShot(true);
    m_problemTimer.setInterval(1500);
    connect(&m_problemTimer, &QTimer::timeout, this, &AppController::checkConnectionProblem);
    connect(m_client, &DaemonClient::connectionStateChanged, &m_problemTimer, qOverload<>(&QTimer::start));
    connect(m_client, &DaemonClient::statusChanged, this, [this] {
        // Back to connected clears at once; problems wait for the timer.
        if (m_client->connectionState() == "connected"_L1) {
            checkConnectionProblem();
        } else if (!m_problemTimer.isActive() && m_client->connectionState() != m_problemState) {
            m_problemTimer.start();
        }
    });
    connect(m_client, &DaemonClient::backgroundError, this, &AppController::onBackgroundError);

    m_markReadTimer.setSingleShot(true);
    m_markReadTimer.setInterval(300);
    connect(&m_markReadTimer, &QTimer::timeout, this, &AppController::markReadNow);

    connect(m_client, &DaemonClient::messageReceived, this, &AppController::onMessageReceived);
    connect(m_chatList, &ChatListModel::unreadChanged, this, [this](const QString &chatId, int unread) {
        if (unread > 0 && chatId == m_currentChatId) {
            scheduleMarkRead();
        }
    });
    connect(m_chatList, &ChatListModel::totalUnreadChanged, this, &AppController::updateTray);
    if (m_options.launcherBadge) {
        m_launcher = new LauncherEntry(bus, u"org.imessage.kmessages"_s, this);
        connect(m_chatList, &ChatListModel::totalUnreadChanged, this, &AppController::updateLauncherBadge);
        connect(m_settings, &AppSettings::changed, this, &AppController::updateLauncherBadge);
        updateLauncherBadge();
    }
    connect(m_chatList, &QAbstractItemModel::dataChanged, this, &AppController::currentChatTitleChanged);
    connect(m_chatList, &QAbstractItemModel::modelReset, this, &AppController::currentChatTitleChanged);
    // Same triggers: chat data, model reset, current chat switch.
    connect(this, &AppController::currentChatTitleChanged, this, &AppController::currentChatBackgroundChanged);
    connect(m_client, &DaemonClient::sendFinished, this, [this](bool ok, const QString &, const QString &, const QString &error) {
        if (!ok) {
            Q_EMIT showPassiveNotification(i18n("Message not sent: %1", error));
        }
    });
    connect(m_client, &DaemonClient::operationFailed, this, [this](const QString &op, const QString &, const QString &message) {
        Q_EMIT showPassiveNotification(i18nc("%1 = D-Bus method, %2 = error", "%1 failed: %2", op, message));
    });

    if (m_options.tray) {
        m_tray = new KStatusNotifierItem(u"org.imessage.kmessages"_s, this);
        m_tray->setCategory(KStatusNotifierItem::Communications);
        m_tray->setIconByName(u"dialog-messages"_s);
        m_tray->setTitle(i18n("Messages"));
        m_tray->setStandardActionsEnabled(true);
        auto *newMessage = m_tray->contextMenu()->addAction(QIcon::fromTheme(u"document-edit"_s), i18n("New Message"));
        connect(newMessage, &QAction::triggered, this, [this] {
            showWindow();
            Q_EMIT newMessageRequested();
        });
        auto *sync = m_tray->contextMenu()->addAction(QIcon::fromTheme(u"view-refresh"_s), i18n("Sync from Mac"));
        connect(sync, &QAction::triggered, m_macSync, &MacSync::start);
        connect(m_macSync, &MacSync::runningChanged, sync, [this, sync] {
            sync->setEnabled(!m_macSync->isRunning());
        });
        updateTray();
    }
}

AppController::~AppController()
{
    if (m_idleTimeoutId != 0) {
        KIdleTime::instance()->removeIdleTimeout(m_idleTimeoutId);
    }
}

void AppController::setWindow(QWindow *window)
{
    m_window = window;
    if (!window) {
        return;
    }
    connect(window, &QWindow::activeChanged, this, &AppController::scheduleMarkRead);
    connect(window, &QWindow::activeChanged, this, [this] {
        // Seen in the window: a background error no longer needs the tray.
        if (windowInFront() && !m_backgroundProblem.isEmpty()) {
            m_backgroundProblem.clear();
            updateTray();
        }
    });
    connect(window, &QWindow::visibleChanged, this, &AppController::scheduleMarkRead);
    if (m_tray) {
        m_tray->setAssociatedWindow(window);
    }
}

void AppController::setCurrentChatId(const QString &chatId)
{
    if (chatId == m_currentChatId) {
        return;
    }
    m_currentChatId = chatId;
    m_messages->setChatId(chatId);
    if (auto n = m_notifications.take(chatId)) {
        n->close();
    }
    Q_EMIT currentChatIdChanged();
    Q_EMIT currentChatTitleChanged();
    scheduleMarkRead();
}

void AppController::setConversationVisible(bool visible)
{
    if (visible != m_conversationVisible) {
        m_conversationVisible = visible;
        Q_EMIT conversationVisibleChanged();
        scheduleMarkRead();
    }
}

QString AppController::currentChatTitle() const
{
    return m_currentChatId.isEmpty() ? QString() : m_chatList->titleFor(m_currentChatId);
}

QString AppController::currentChatBackground() const
{
    return m_currentChatId.isEmpty() ? QString() : m_chatList->backgroundFor(m_currentChatId);
}

void AppController::setUserAway(bool away)
{
    if (away && m_settings->awayAfterSeconds() == 0) {
        return;
    }
    if (away == m_userAway) {
        return;
    }
    m_userAway = away;
    if (away) {
        m_markReadTimer.stop();
        if (m_options.idleDetection) {
            KIdleTime::instance()->catchNextResumeEvent();
        }
    }
    Q_EMIT userAwayChanged();
    if (!away) {
        // Back at the keyboard with the chat in front of them: now it is read.
        scheduleMarkRead();
    }
}

void AppController::updateIdleWatch()
{
    // At most a day (and no int overflow in ms).
    const int ms = qMin(m_settings->awayAfterSeconds(), 24 * 60 * 60) * 1000;
    if (ms == m_idleTimeoutMs) {
        return;
    }
    m_idleTimeoutMs = ms;
    if (ms == 0) {
        setUserAway(false);
    }
    if (!m_options.idleDetection) {
        return;
    }
    auto *idle = KIdleTime::instance();
    if (m_idleTimeoutId != 0) {
        idle->removeIdleTimeout(m_idleTimeoutId);
        m_idleTimeoutId = 0;
    }
    if (ms > 0) {
        m_idleTimeoutId = idle->addIdleTimeout(ms);
    }
}

bool AppController::chatVisible(const QString &chatId) const
{
    return !chatId.isEmpty() && chatId == m_currentChatId && m_conversationVisible && m_window && m_window->isVisible() && m_window->isActive()
        && !m_userAway;
}

void AppController::scheduleMarkRead()
{
    if (chatVisible(m_currentChatId)) {
        m_markReadTimer.start();
    }
}

void AppController::markReadNow()
{
    if (chatVisible(m_currentChatId) && m_chatList->unreadFor(m_currentChatId) > 0) {
        // Notified while the user was away: read now.
        if (auto n = m_notifications.take(m_currentChatId)) {
            n->close();
        }
        markChatRead(m_currentChatId);
    }
}

void AppController::markChatRead(const QString &chatId)
{
    if (!m_client->hasOtherOwnHandles()) {
        m_client->markRead(chatId);
        return;
    }
    // Chat (unread count) and messages straight from the daemon, so both
    // include the message that triggered this.
    QPointer<AppController> self(this);
    m_client->getChat(chatId, [self, chatId](const Chat &chat) {
        if (!self || chat.unreadCount == 0) {
            return;
        }
        const uint limit = qBound(50u, uint(chat.unreadCount) * 2, 500u);
        self->m_client->listMessages(chatId, QString(), limit, [self, chatId, unread = int(chat.unreadCount)](const MessagePage &page) {
            if (!self) {
                return;
            }
            // The daemon counts the newest `unread` incoming messages as
            // unread, own ones from another device included.
            int remaining = unread;
            int ownNewer = 0;
            QString upTo;
            for (auto it = page.messages.crbegin(); it != page.messages.crend() && remaining > 0; ++it) {
                if (it->isFromMe) {
                    continue;
                }
                --remaining;
                if (!self->m_client->isOtherOwnHandle(it->sender.uri)) {
                    upTo = it->id;
                    break;
                }
                ++ownNewer;
            }
            self->m_chatList->setOwnUnread(chatId, ownNewer);
            if (!upTo.isEmpty()) {
                self->m_client->markRead(chatId, upTo);
            }
        });
    });
}

void AppController::onMessageReceived(const QString &chatId, const Message &msg)
{
    if (msg.isFromMe) {
        return;
    }
    if (m_client->isOtherOwnHandle(msg.sender.uri)) {
        // Sent by the user from another device (their phone number is not
        // registered here, so the daemon reports it as incoming): no
        // notification, and like iOS, writing in a chat marks it read.
        if (auto n = m_notifications.take(chatId)) {
            n->close();
        }
        markChatRead(chatId);
        return;
    }
    if (chatVisible(chatId)) {
        scheduleMarkRead();
        return;
    }
    if (!m_settings->notificationsEnabled()) {
        return;
    }
    const QString chatTitle = m_chatList->indexOf(chatId) >= 0 ? m_chatList->titleFor(chatId) : m_chatList->nameForHandle(msg.sender.uri);
    QString text = msg.snippet();
    if (m_chatList->isGroupChat(chatId)) {
        text = i18nc("group notification: sender: text", "%1: %2", m_chatList->nameForHandle(msg.sender.uri), text);
    }
    if (!msg.verified) {
        text = i18nc("notification for a message whose sender could not be verified", "(unverified sender) %1", text);
    }
    Q_EMIT notificationRequested(chatId, chatTitle, text);
    if (!m_options.notifications) {
        return;
    }

    auto *n = new KNotification(u"newMessage"_s, KNotification::CloseOnTimeout | KNotification::CloseWhenWindowActivated);
    n->setTitle(chatTitle);
    n->setText(text.toHtmlEscaped());
    n->setIconName(u"dialog-messages"_s);
    // The sender's contact photo, like Messages.app.
    if (const QImage photo = m_contacts->photoFor(msg.sender.uri); !photo.isNull()) {
        n->setPixmap(QPixmap::fromImage(photo));
    }
    if (m_window) {
        n->setWindow(m_window);
    }
    auto *open = n->addDefaultAction(i18n("Open"));
    connect(open, &KNotificationAction::activated, this, [this, chatId] {
        openChat(chatId);
    });
    auto reply = std::make_unique<KNotificationReplyAction>(i18n("Reply"));
    reply->setPlaceholderText(i18n("Reply to %1…", chatTitle));
    reply->setFallbackBehavior(KNotificationReplyAction::FallbackBehavior::UseRegularAction);
    connect(reply.get(), &KNotificationReplyAction::replied, this, [this, chatId](const QString &replyText) {
        sendReply(chatId, replyText);
    });
    // Servers without inline replies show a plain "Reply" button instead.
    connect(reply.get(), &KNotificationReplyAction::activated, this, [this, chatId] {
        openChat(chatId);
    });
    n->setReplyAction(std::move(reply));

    if (auto old = m_notifications.value(chatId)) {
        old->close();
    }
    m_notifications.insert(chatId, n);
    n->sendEvent();
}

void AppController::sendReply(const QString &chatId, const QString &text)
{
    if (text.trimmed().isEmpty()) {
        return;
    }
    QPointer<AppController> self(this);
    m_client->sendTextThen(chatId, {}, text, QString(), [self, chatId](bool ok, const QString &error) {
        if (!self) {
            return;
        }
        if (ok) {
            // Replying means the chat has been read, as on iOS.
            self->markChatRead(chatId);
            return;
        }
        Q_EMIT self->replyFailed(chatId, error);
        if (!self->m_options.notifications) {
            return;
        }
        auto *n = new KNotification(u"sendFailed"_s, KNotification::Persistent | KNotification::CloseWhenWindowActivated);
        n->setTitle(self->m_chatList->indexOf(chatId) >= 0 ? self->m_chatList->titleFor(chatId) : i18n("Messages"));
        n->setText(i18n("Reply not sent: %1", error).toHtmlEscaped());
        n->setIconName(u"dialog-messages"_s);
        if (self->m_window) {
            n->setWindow(self->m_window);
        }
        auto *open = n->addDefaultAction(i18n("Open"));
        connect(open, &KNotificationAction::activated, self, [self, chatId] {
            if (self) {
                self->openChat(chatId);
            }
        });
        n->sendEvent();
    });
}

bool AppController::windowInFront() const
{
    return m_window && m_window->isVisible() && m_window->isActive();
}

bool AppController::needsAttention() const
{
    return m_chatList->totalUnread() > 0 || !m_problemState.isEmpty() || !m_backgroundProblem.isEmpty();
}

void AppController::checkConnectionProblem()
{
    const QString state = m_client->connectionState();
    if (state == "connected"_L1) {
        if (!m_problemState.isEmpty() || !m_backgroundProblem.isEmpty()) {
            m_problemState.clear();
            m_backgroundProblem.clear();
            if (m_problemNotification) {
                m_problemNotification->close();
            }
            updateTray();
        }
        return;
    }
    const bool problem = state == "needs_login"_L1 || state == "needs_two_factor"_L1 || state == "error"_L1;
    // One notification per problem: an error that keeps retrying
    // (error → connecting → error) is not announced again until connected.
    if (!problem || state == m_problemState) {
        return;
    }
    m_problemState = state;
    updateTray();
    if (state == "needs_login"_L1) {
        notifyProblem(state, i18n("Not signed in to iMessage"), i18n("Sign in with your Apple ID to send and receive messages."), 0);
    } else if (state == "needs_two_factor"_L1) {
        notifyProblem(state, i18n("Verification code needed"), i18n("Enter the code shown on your other Apple device to finish signing in."), 1);
    } else {
        const QString detail = m_client->connectionDetail();
        notifyProblem(state,
                      i18n("iMessage connection problem"),
                      detail.isEmpty() ? i18n("The messaging service reported an error.") : detail,
                      -1);
    }
}

void AppController::onBackgroundError(const QString &message)
{
    // Dropped events are handled by reloading (resyncRequested): nothing for the user.
    if (message.contains("resync"_L1) || message == m_backgroundProblem) {
        return;
    }
    m_backgroundProblem = message;
    updateTray();
    notifyProblem(u"background"_s, i18n("Messages"), message, -1);
}

void AppController::notifyProblem(const QString &key, const QString &title, const QString &text, int loginStep)
{
    if (windowInFront()) {
        return; // the footer / login dialog / passive message already say it
    }
    Q_EMIT connectionProblemNotified(key, text);
    if (!m_options.notifications) {
        return;
    }
    if (m_problemNotification) {
        m_problemNotification->close();
    }
    auto *n = new KNotification(u"connectionProblem"_s, KNotification::Persistent | KNotification::CloseWhenWindowActivated);
    n->setTitle(title);
    n->setText(text.toHtmlEscaped());
    n->setIconName(u"dialog-messages"_s);
    if (m_window) {
        n->setWindow(m_window);
    }
    auto *open = n->addDefaultAction(i18n("Open"));
    connect(open, &KNotificationAction::activated, this, [this, loginStep] {
        showWindow();
        if (loginStep >= 0) {
            Q_EMIT loginRequested(loginStep);
        }
    });
    m_problemNotification = n;
    n->sendEvent();
}

void AppController::updateTray()
{
    const bool attention = needsAttention();
    if (attention != m_lastAttention) {
        m_lastAttention = attention;
        Q_EMIT needsAttentionChanged();
    }
    if (!m_tray) {
        return;
    }
    const int unread = m_chatList->totalUnread();
    m_tray->setStatus(attention ? KStatusNotifierItem::NeedsAttention : KStatusNotifierItem::Active);
    m_tray->setAttentionIconByName(u"dialog-messages"_s);
    m_tray->setOverlayIconByName(attention ? u"emblem-important"_s : QString());
    QString subtitle = unread > 0 ? i18np("%1 unread message", "%1 unread messages", unread) : i18n("No unread messages");
    if (m_problemState == "needs_login"_L1) {
        subtitle = i18n("Not signed in");
    } else if (m_problemState == "needs_two_factor"_L1) {
        subtitle = i18n("Verification code needed");
    } else if (m_problemState == "error"_L1) {
        subtitle = i18n("Connection problem");
    } else if (!m_backgroundProblem.isEmpty()) {
        subtitle = m_backgroundProblem;
    }
    m_tray->setToolTip(u"dialog-messages"_s, i18n("Messages"), subtitle);
}

void AppController::updateLauncherBadge()
{
    if (m_launcher) {
        m_launcher->setCount(m_chatList->totalUnread(), m_settings->unreadBadge());
    }
}

void AppController::showWindow()
{
    if (!m_window) {
        return;
    }
    m_window->show();
    m_window->raise();
    m_window->requestActivate();
}

void AppController::toggleWindow()
{
    if (m_window && m_window->isVisible() && m_window->isActive()) {
        m_window->hide();
    } else {
        showWindow();
    }
}

void AppController::openChat(const QString &chatId)
{
    showWindow();
    Q_EMIT openChatRequested(chatId);
}

void AppController::quit()
{
    QCoreApplication::quit();
}

QList<QUrl> AppController::clipboardFiles() const
{
    const QClipboard *clipboard = QGuiApplication::clipboard();
    const QMimeData *data = clipboard ? clipboard->mimeData() : nullptr;
    if (!data) {
        return {};
    }
    // Files copied in a file manager.
    if (data->hasUrls()) {
        QList<QUrl> files;
        const auto urls = data->urls();
        for (const auto &u : urls) {
            if (!u.isLocalFile()) {
                return {}; // a web link: paste it as text
            }
            files << u;
        }
        return files;
    }
    // A screenshot or "Copy Image"; text that comes with an image (a
    // spreadsheet selection …) is pasted as text.
    if (data->hasImage() && !data->hasText()) {
        const QImage image = qvariant_cast<QImage>(data->imageData());
        const QString name = i18nc("file name of a pasted image; %1 = date and time", "Pasted image %1", QDateTime::currentDateTime().toString(u"yyyy-MM-dd hh.mm.ss"_s));
        const QString path = Outgoing::saveImage(image, name);
        if (!path.isEmpty()) {
            return {QUrl::fromLocalFile(path)};
        }
    }
    return {};
}

} // namespace km
