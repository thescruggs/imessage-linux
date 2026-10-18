// SPDX-License-Identifier: SSPL-1.0
//
// Offscreen UI test: the real QML window against the fake daemon. Each
// screen is grabbed to build/screenshots/<name>.png and checked for being
// non-blank; QML warnings from our own files fail the test.
#include "appcontroller.h"
#include "fakedaemon.h"
#include "qmlsetup.h"

#include <KIconTheme>

#include <QApplication>
#include <QClipboard>
#include <QDir>
#include <functional>
#include <QFile>
#include <QPainter>
#include <QQmlApplicationEngine>
#include <QQuickItem>
#include <QQuickStyle>
#include <QQuickWindow>
#include <QSignalSpy>
#include <QTemporaryDir>
#include <QTest>

using namespace Qt::StringLiterals;
using namespace km;

namespace
{
const QString Bob = u"mailto:bob@example.com"_s;
const QString Team = u"group:5B0E2C1A-3F4D-4E21-9A55-0C7E1B2D3F40"_s;
const QString Dave = u"tel:+15551234567"_s;
const QString Carol = u"mailto:carol@example.com"_s;
constexpr int Timeout = 8000;

QStringList g_qmlWarnings;

/// A stand-in contact photo: a head and shoulders on a coloured background.
QImage photo(QColor background, QColor skin)
{
    QImage img(128, 128, QImage::Format_RGB32);
    img.fill(background);
    QPainter p(&img);
    p.setRenderHint(QPainter::Antialiasing);
    p.setPen(Qt::NoPen);
    p.setBrush(skin);
    p.drawEllipse(QRectF(40, 22, 48, 54));
    p.setBrush(background.darker(160));
    p.drawEllipse(QRectF(16, 84, 96, 90));
    return img;
}
QtMessageHandler g_previousHandler = nullptr;

void messageHandler(QtMsgType type, const QMessageLogContext &ctx, const QString &msg)
{
    // Kirigami's PageRow creates pushed Components with a non-Item parent
    // (PageRow.qml: createObject(pagesLogic, …)), which Qt reports against our
    // Component's location; that one is not ours to fix.
    const bool kirigamiPush = msg.contains(u"Created graphical object was not placed in the graphics scene"_s);
    if (type != QtDebugMsg && type != QtInfoMsg && msg.contains(u"qrc:/kmessages/"_s) && !kirigamiPush) {
        g_qmlWarnings << msg;
    }
    g_previousHandler(type, ctx, msg);
}
}

class TestScreens : public QObject
{
    Q_OBJECT

    FakeDaemon m_fake;
    std::unique_ptr<AppController> m_controller;
    std::unique_ptr<QQmlApplicationEngine> m_engine;
    QQuickWindow *m_window = nullptr;
    QStringList m_shots;

    void settle(int ms = 400)
    {
        // Let animations, async image loads and layouts finish.
        QTest::qWait(ms);
        QCoreApplication::processEvents();
    }

    // Main.qml's functions are typed (string / int).
    void invoke(const char *function)
    {
        QVERIFY(QMetaObject::invokeMethod(m_window, function));
    }
    void invoke(const char *function, const QString &arg)
    {
        QVERIFY(QMetaObject::invokeMethod(m_window, function, Q_ARG(QString, arg)));
    }
    void invoke(const char *function, int arg)
    {
        QVERIFY(QMetaObject::invokeMethod(m_window, function, Q_ARG(int, arg)));
    }

    QObject *pageStack() const
    {
        return m_window->property("pageStack").value<QObject *>();
    }

    /// The composer of the (single, live) conversation page.
    QObject *conversationComposer()
    {
        QObject *found = nullptr;
        (void)QTest::qWaitFor([&] {
            const auto all = m_window->findChildren<QObject *>(u"conversationComposer"_s);
            found = all.size() == 1 ? all.first() : nullptr;
            return found != nullptr;
        }, Timeout);
        return found;
    }

    /// ListMessages calls for `chatId` that load a transcript page (not a limit-1 snippet).
    int transcriptLoads(const QString &chatId, bool paged)
    {
        int n = 0;
        const auto calls = m_fake.callsOf(u"ListMessages"_s);
        for (const auto &c : calls) {
            if (c.at(1).toString() == chatId && c.at(3).toInt() > 1 && paged == !c.at(2).toString().isEmpty()) {
                ++n;
            }
        }
        return n;
    }

    /// Visible items in the scene (view delegates and toolbar items have no
    /// QObject parent in the window, so findChildren() misses them).
    QList<QQuickItem *> visibleItems(const QString &objectName = QString())
    {
        QList<QQuickItem *> out;
        std::function<void(QQuickItem *)> walk = [&](QQuickItem *item) {
            if (!item->isVisible()) {
                return;
            }
            if (objectName.isEmpty() || item->objectName() == objectName) {
                out << item;
            }
            const auto children = item->childItems();
            for (auto *child : children) {
                walk(child);
            }
        };
        walk(m_window->contentItem());
        return out;
    }

    /// The visible item named `objectName` (first one), or null.
    QQuickItem *visibleItem(const QString &objectName)
    {
        const auto items = visibleItems(objectName);
        return items.isEmpty() ? nullptr : items.first();
    }

    /// The chat list row (ChatListDelegate) of `chatId`.
    QQuickItem *chatRow(const QString &chatId)
    {
        QQuickItem *found = nullptr;
        (void)QTest::qWaitFor([&] {
            const auto rows = visibleItems(u"chatListDelegate"_s);
            for (auto *row : rows) {
                if (row->property("chatId").toString() == chatId) {
                    found = row;
                    return true;
                }
            }
            return false;
        }, Timeout);
        return found;
    }

    /// A visible item showing exactly `text` (headers, passive notifications).
    bool showsText(const QString &text)
    {
        const auto items = visibleItems();
        for (auto *item : items) {
            if (item->height() > 0 && item->property("text").toString() == text) {
                return true;
            }
        }
        return false;
    }

    void rightClick(QQuickItem *item)
    {
        const QPointF centre = item->mapToScene(QPointF(item->width() / 2, item->height() / 2));
        QTest::mouseClick(m_window, Qt::RightButton, {}, centre.toPoint());
    }

    /// QTest::keyClicks() takes only widgets: one key event per (ASCII) character.
    void typeKeys(const QString &text)
    {
        for (const QChar c : text) {
            QTest::keyClick(m_window, c.toLatin1());
        }
    }

    void click(QObject *button)
    {
        QVERIFY(button);
        QVERIFY(QMetaObject::invokeMethod(button, "click"));
    }

    void shoot(const QString &name)
    {
        settle();
        const QImage img = m_window->grabWindow();
        QVERIFY2(!img.isNull(), "grabWindow returned nothing");
        QDir().mkpath(QStringLiteral(SCREENSHOT_DIR));
        const QString path = QStringLiteral(SCREENSHOT_DIR) + u'/' + name + u".png"_s;
        QVERIFY(img.save(path));
        m_shots << path;
        // Non-blank: plenty of distinct colours (text, avatars, bubbles).
        QSet<QRgb> colours;
        for (int y = 0; y < img.height(); y += 3) {
            for (int x = 0; x < img.width(); x += 3) {
                colours.insert(img.pixel(x, y));
            }
        }
        QVERIFY2(colours.size() > 40, qPrintable(u"%1 looks blank (%2 colours)"_s.arg(name).arg(colours.size())));
        qInfo().noquote() << "screenshot" << path << img.size() << colours.size() << "colours";
    }

private Q_SLOTS:
    void initTestCase()
    {
        if (!FakeDaemon::onPrivateBus()) {
            QSKIP("needs the private test bus; run through ctest");
        }
        g_previousHandler = qInstallMessageHandler(messageHandler);
        QVERIFY(m_fake.start());

        AppOptions options;
        options.tray = false;
        options.notifications = false;
        options.kpeople = false; // contacts below, not from the (empty) temporary address book
        m_controller = std::make_unique<AppController>(QDBusConnection::sessionBus(), options, KSharedConfig::openConfig(u"kmessages-testrc"_s));
        m_controller->settings()->setNotificationsEnabled(true);
        m_controller->contacts()->setPeople({
            {u"1"_s, u"Bob Smith"_s, u"Bob"_s, {u"bob@example.com"_s}, {}, photo(QColor(0x3d, 0xae, 0xe9), QColor(0xf5, 0xd0, 0xa9))},
            {u"2"_s, u"Dave Miller"_s, u"Dave"_s, {}, {u"(555) 123-4567"_s, u"dave@example.net"_s}, photo(QColor(0x2e, 0xcc, 0x71), QColor(0x8d, 0x5b, 0x3c))},
            {u"3"_s, u"Carol Jones"_s, u"Carol"_s, {u"carol@example.com"_s}, {}, {}},
        });
        m_engine = std::make_unique<QQmlApplicationEngine>();
        setupQmlEngine(*m_engine, m_controller.get());
        m_engine->load(mainQmlUrl());
        QCOMPARE(m_engine->rootObjects().size(), 1);
        m_window = qobject_cast<QQuickWindow *>(m_engine->rootObjects().first());
        QVERIFY(m_window);
        m_controller->setWindow(m_window);
        m_window->resize(1280, 820);
        m_controller->showWindow();
        QVERIFY(QTest::qWaitForWindowExposed(m_window));

        QTRY_COMPARE_WITH_TIMEOUT(m_controller->chatList()->rowCount(), 4, Timeout);
        // wait for all snippets
        QTRY_VERIFY_WITH_TIMEOUT(m_controller->chatList()->data(m_controller->chatList()->index(3), ChatListModel::SnippetRole).toString().size() > 0, Timeout);
    }

    void chatList()
    {
        shoot(u"chat-list"_s);
    }

    void transcript()
    {
        invoke("openChat", Bob);
        QCOMPARE(m_controller->currentChatId(), Bob);
        auto *msgs = m_controller->messages();
        QTRY_VERIFY_WITH_TIMEOUT(msgs->isLoaded() && msgs->rowCount() > 0, Timeout);
        // the image attachment is downloaded on demand by its delegate
        QTRY_VERIFY_WITH_TIMEOUT(!msgs->data(msgs->index(0), MessageListModel::AttachmentsRole).toList().first().toMap().value(u"localPath"_s).toString().isEmpty(),
                                 Timeout);
        QVERIFY(m_fake.callsOf(u"DownloadAttachment"_s).size() >= 1);
        m_fake.control(u"InjectTyping"_s, {Bob, Bob, true});
        QTRY_VERIFY_WITH_TIMEOUT(msgs->someoneTyping(), Timeout);
        settle(800);
        shoot(u"transcript"_s);
        m_fake.control(u"InjectTyping"_s, {Bob, Bob, false});
    }

    void reopenOpenChat()
    {
        // Re-opening the chat that is already open (sidebar row, notification
        // "Open", search hit, --chat) must keep the transcript.
        invoke("openChat", Bob);
        auto *msgs = m_controller->messages();
        QTRY_VERIFY_WITH_TIMEOUT(msgs->isLoaded() && msgs->chatId() == Bob && msgs->rowCount() > 0, Timeout);
        settle();
        const int rows = msgs->rowCount();
        const QString title = m_controller->currentChatTitle();
        const QString windowTitle = m_window->title();
        QObject *page = pageStack()->property("currentItem").value<QObject *>();
        QVERIFY(page);
        m_fake.control(u"ClearCalls"_s);

        auto same = [&](const char *how) {
            settle(); // deferred deletes, callLater
            QVERIFY2(m_controller->currentChatId() == Bob, how);
            QVERIFY2(msgs->chatId() == Bob && msgs->isLoaded(), how);
            QVERIFY2(msgs->rowCount() == rows, qPrintable(u"%1: %2 rows, expected %3"_s.arg(QLatin1String(how)).arg(msgs->rowCount()).arg(rows)));
            QVERIFY2(m_controller->currentChatTitle() == title, how);
            QVERIFY2(m_window->title() == windowTitle, how);
            QVERIFY2(pageStack()->property("currentItem").value<QObject *>() == page, how);
            QVERIFY2(pageStack()->property("depth").toInt() == 2, how);
        };
        invoke("openChat", Bob);
        same("openChat twice");
        if (QTest::currentTestFailed()) {
            return;
        }
        m_controller->openChat(Bob); // notification "Open" / --chat / second launch
        same("AppController::openChat");
        if (QTest::currentTestFailed()) {
            return;
        }
        invoke("toggleDetails");
        settle();
        QCOMPARE(pageStack()->property("depth").toInt(), 3);
        invoke("openChat", Bob);
        same("openChat with details open");
        if (QTest::currentTestFailed()) {
            return;
        }
        QCOMPARE(transcriptLoads(Bob, false), 0); // nothing reloaded
    }

    void groupTranscript()
    {
        invoke("openChat", Team);
        auto *msgs = m_controller->messages();
        QTRY_VERIFY_WITH_TIMEOUT(msgs->isLoaded() && msgs->chatId() == Team && msgs->rowCount() == 7, Timeout);
        shoot(u"transcript-group"_s);

        invoke("toggleDetails");
        shoot(u"transcript-group-details"_s);
        invoke("toggleDetails");
    }

    void conversationBackground()
    {
        invoke("openChat", Bob);
        auto *msgs = m_controller->messages();
        QTRY_VERIFY_WITH_TIMEOUT(msgs->isLoaded() && msgs->chatId() == Bob && msgs->rowCount() > 0, Timeout);
        QVERIFY(m_controller->currentChatBackground().isEmpty());
        m_fake.control(u"SetBackground"_s, {Bob, true});
        QTRY_VERIFY_WITH_TIMEOUT(m_controller->currentChatBackground().startsWith(u"file:///"_s), Timeout);
        QVERIFY(m_controller->currentChatBackground().endsWith(u".png"_s));
        settle(800);
        shoot(u"conversation-background"_s);
        // Removing it takes effect live too.
        m_fake.control(u"SetBackground"_s, {Bob, false});
        QTRY_VERIFY_WITH_TIMEOUT(m_controller->currentChatBackground().isEmpty(), Timeout);
    }

    void sharedProfilesAndGroupPhoto()
    {
        const QString eve = u"mailto:eve@example.org"_s;
        // Carol is in the address book without a photo, Eve not at all.
        m_fake.control(u"SetProfile"_s, {Carol, u"Caz"_s, u"Carol"_s, true});
        m_fake.control(u"SetProfile"_s, {eve, u"Evie Adams"_s, u"Evie"_s, true});
        m_fake.control(u"SetGroupIcon"_s, {Team, true});
        auto *chats = m_controller->chatList();
        const auto role = [&](const QString &chatId, int r) {
            return chats->data(chats->index(chats->indexOf(chatId)), r);
        };
        QTRY_COMPARE_WITH_TIMEOUT(role(eve, ChatListModel::NameRole).toString(), u"Evie Adams"_s, Timeout);
        QTRY_VERIFY_WITH_TIMEOUT(role(Team, ChatListModel::AvatarSourceRole).toString().startsWith(u"image://attachment/"_s), Timeout);
        QVERIFY(role(eve, ChatListModel::AvatarSourceRole).toString().startsWith(u"image://attachment/"_s));

        invoke("openChat", Team);
        auto *msgs = m_controller->messages();
        QTRY_VERIFY_WITH_TIMEOUT(msgs->isLoaded() && msgs->chatId() == Team && msgs->rowCount() == 7, Timeout);
        invoke("toggleDetails");
        QCOMPARE(role(Team, ChatListModel::ParticipantSharedNamesRole).toStringList().at(1), u"Caz"_s);
        settle(800);
        shoot(u"shared-profiles"_s);
        invoke("toggleDetails");

        // Back to how the other screenshots expect it.
        m_fake.control(u"SetProfile"_s, {Carol, QString(), QString(), false});
        m_fake.control(u"SetProfile"_s, {eve, QString(), QString(), false});
        m_fake.control(u"SetGroupIcon"_s, {Team, false});
        QTRY_COMPARE_WITH_TIMEOUT(role(eve, ChatListModel::NameRole).toString(), u"Eve Adams"_s, Timeout);
        QTRY_VERIFY_WITH_TIMEOUT(role(Team, ChatListModel::AvatarSourceRole).toString().isEmpty(), Timeout);
    }

    void unverifiedAndUnsent()
    {
        invoke("openChat", Dave);
        auto *msgs = m_controller->messages();
        QTRY_VERIFY_WITH_TIMEOUT(msgs->isLoaded() && msgs->chatId() == Dave && msgs->rowCount() == 3, Timeout);
        shoot(u"transcript-unverified"_s);
    }

    void linkPreview()
    {
        const QString eve = u"mailto:eve@example.org"_s;
        invoke("openChat", eve);
        auto *msgs = m_controller->messages();
        QTRY_VERIFY_WITH_TIMEOUT(msgs->isLoaded() && msgs->chatId() == eve && msgs->rowCount() == 5, Timeout);
        // two cards; the link-only message shows its card, not the bare URL
        QTRY_COMPARE_WITH_TIMEOUT(visibleItems(u"linkPreview"_s).size(), 2, Timeout);
        QTRY_COMPARE_WITH_TIMEOUT(visibleItems(u"linkPreviewImage"_s).size(), 1, Timeout);
        QVERIFY(showsText(u"Plasma: what's new this spring"_s));
        QVERIFY(showsText(u"Fluffy pancakes"_s));
        // the bare URL (as typed; the card's page is where it redirected) is hidden
        QVERIFY(!showsText(u"https://example.org/plasma"_s));
        shoot(u"link-preview"_s);
    }

    void olderPagesWhenContentFits()
    {
        // 7 messages in pages of 5: the first page fits the viewport, so no
        // scrolling would ever ask for the second one.
        auto *msgs = m_controller->messages();
        msgs->setPageSize(5);
        m_fake.control(u"ClearCalls"_s);
        invoke("openChat", Team);
        QTRY_VERIFY_WITH_TIMEOUT(msgs->chatId() == Team && msgs->isLoaded() && msgs->rowCount() == 7 && !msgs->canLoadMore(), Timeout);
        QCOMPARE(transcriptLoads(Team, true), 1);
        settle();
        QCOMPARE(transcriptLoads(Team, true), 1); // and no loop once all is loaded
        msgs->setPageSize(50);
    }

    void scrolledUpStaysUp()
    {
        // Wheel and scroll-bar scrolling set contentY directly (the desktop
        // style's Kirigami.WheelHandler), with no movementEnded. Scrolled up,
        // the view must stay put when the older page lands and when the
        // typing bubble changes the content height; back at the newest
        // message, it follows those changes again.
        auto *msgs = m_controller->messages();
        msgs->setPageSize(20); // Bob has 31 messages
        invoke("openChat", Team); // re-opening the open chat keeps its transcript
        QTRY_VERIFY_WITH_TIMEOUT(msgs->chatId() == Team && msgs->isLoaded(), Timeout);
        invoke("openChat", Bob);
        QTRY_VERIFY_WITH_TIMEOUT(msgs->chatId() == Bob && msgs->isLoaded() && msgs->rowCount() == 20 && msgs->canLoadMore(), Timeout);
        auto *transcript = qobject_cast<QQuickItem *>(m_window->findChild<QObject *>(u"transcript"_s));
        QVERIFY(transcript);
        settle();
        QVERIFY(transcript->property("contentHeight").toReal() > transcript->height()); // else paging needs no scrolling
        QVERIFY(transcript->property("atYEnd").toBool());
        QVERIFY(transcript->property("pinnedToNewest").toBool());

        auto rowInView = [&](int row) {
            QQuickItem *item = nullptr;
            if (!QMetaObject::invokeMethod(transcript, "itemAtIndex", Q_RETURN_ARG(QQuickItem *, item), Q_ARG(int, row)) || !item) {
                return false;
            }
            const qreal y = transcript->property("contentY").toReal();
            return item->y() + item->height() > y && item->y() < y + transcript->height();
        };

        // Scroll to the oldest loaded message the way the wheel handler does.
        transcript->setProperty("contentY", transcript->property("originY").toReal() - transcript->property("topMargin").toReal());
        QVERIFY(transcript->property("atYBeginning").toBool());
        QVERIFY(!transcript->property("pinnedToNewest").toBool());
        QTRY_VERIFY_WITH_TIMEOUT(msgs->rowCount() == 31 && !msgs->canLoadMore(), Timeout);
        settle();
        // The older page went above; the view stayed on the oldest row of the first page.
        QVERIFY(rowInView(19));
        QVERIFY(!rowInView(0));
        QVERIFY(!transcript->property("atYBeginning").toBool());
        m_fake.control(u"InjectTyping"_s, {Bob, Bob, true});
        QTRY_VERIFY_WITH_TIMEOUT(msgs->someoneTyping(), Timeout);
        settle();
        QVERIFY(rowInView(19));
        m_fake.control(u"InjectTyping"_s, {Bob, Bob, false});
        QTRY_VERIFY_WITH_TIMEOUT(!msgs->someoneTyping(), Timeout);

        // Scrolled back to the newest message, the view sticks to it again.
        transcript->setProperty("contentY", transcript->property("originY").toReal() + transcript->property("contentHeight").toReal()
                                                + transcript->property("bottomMargin").toReal() - transcript->height());
        settle();
        QVERIFY(transcript->property("atYEnd").toBool());
        QVERIFY(transcript->property("pinnedToNewest").toBool());
        m_fake.control(u"InjectTyping"_s, {Bob, Bob, true});
        QTRY_VERIFY_WITH_TIMEOUT(msgs->someoneTyping(), Timeout);
        settle();
        QVERIFY(transcript->property("atYEnd").toBool());
        m_fake.control(u"InjectTyping"_s, {Bob, Bob, false});
        QTRY_VERIFY_WITH_TIMEOUT(!msgs->someoneTyping(), Timeout);
        msgs->setPageSize(50);
    }

    void composerKeepsFailedText()
    {
        invoke("openChat", Bob);
        auto *msgs = m_controller->messages();
        QTRY_VERIFY_WITH_TIMEOUT(msgs->isLoaded() && msgs->chatId() == Bob, Timeout);
        QObject *composer = conversationComposer();
        QVERIFY(composer);

        composer->setProperty("text", u"__fail__"_s); // the fake refuses this text
        QVERIFY(QMetaObject::invokeMethod(composer, "send"));
        QCOMPARE(composer->property("text").toString(), QString()); // cleared while sending
        QTRY_COMPARE_WITH_TIMEOUT(composer->property("text").toString(), u"__fail__"_s, Timeout);

        // A failed edit (Bob's message is not ours) comes back in edit mode.
        composer->setProperty("text", QString());
        const QString theirs = msgs->data(msgs->index(0), MessageListModel::MessageIdRole).toString();
        QVERIFY(QMetaObject::invokeMethod(composer, "startEdit", Q_ARG(QString, theirs), Q_ARG(QString, u"changed"_s)));
        QVERIFY(QMetaObject::invokeMethod(composer, "send"));
        QCOMPARE(composer->property("editingId").toString(), QString());
        QTRY_COMPARE_WITH_TIMEOUT(composer->property("text").toString(), u"changed"_s, Timeout);
        QCOMPARE(composer->property("editingId").toString(), theirs);
        QVERIFY(QMetaObject::invokeMethod(composer, "cancel"));
        QCOMPARE(composer->property("text").toString(), QString());
        // Drop the two "… failed" toasts so they stay out of later screenshots.
        for (int i = 0; i < 2; ++i) {
            QVERIFY(QMetaObject::invokeMethod(m_window, "hidePassiveNotification", Q_ARG(QVariant, QVariant(0))));
        }
    }

    void typingRefresh()
    {
        invoke("openChat", Bob);
        QObject *composer = conversationComposer();
        QVERIFY(composer);
        composer->setProperty("typingRefreshMs", 400);
        m_fake.control(u"ClearCalls"_s);
        auto typingCalls = [&] {
            QStringList out;
            const auto calls = m_fake.callsOf(u"SetTyping"_s);
            for (const auto &c : calls) {
                out << (c.at(2).toBool() ? u"on"_s : u"off"_s);
            }
            return out;
        };
        composer->setProperty("text", u"h"_s);
        QTRY_COMPARE_WITH_TIMEOUT(typingCalls(), QStringList{u"on"_s}, Timeout);
        composer->setProperty("text", u"he"_s); // within the refresh interval: nothing new
        QTest::qWait(500);
        QCOMPARE(typingCalls(), QStringList{u"on"_s});
        composer->setProperty("text", u"hel"_s); // still typing after it: said again
        QTRY_COMPARE_WITH_TIMEOUT(typingCalls(), (QStringList{u"on"_s, u"on"_s}), Timeout);
        composer->setProperty("text", QString());
        QTRY_COMPARE_WITH_TIMEOUT(typingCalls(), (QStringList{u"on"_s, u"on"_s, u"off"_s}), Timeout);
        composer->setProperty("typingRefreshMs", 30000);
    }

    void composerAttachments()
    {
        invoke("openChat", Bob);
        auto *msgs = m_controller->messages();
        QTRY_VERIFY_WITH_TIMEOUT(msgs->isLoaded() && msgs->chatId() == Bob, Timeout);
        QObject *composer = conversationComposer();
        QVERIFY(composer);

        // A file from /tmp (the daemon cannot see it: sent from a copy).
        QTemporaryDir outside;
        const QString file = outside.filePath(u"sunset.png"_s);
        QVERIFY(photo(QColor(0xf3, 0x9c, 0x12), QColor(0xc0, 0x39, 0x2b)).scaled(320, 200).save(file));
        m_fake.control(u"SetUnreadablePrefixes"_s, {QStringList{outside.path()}});
        QVERIFY(QMetaObject::invokeMethod(composer, "addAttachments", Q_ARG(QVariant, QVariant(QVariantList{QUrl::fromLocalFile(file)}))));

        // Ctrl+V with an image on the clipboard attaches it as a PNG.
        QGuiApplication::clipboard()->setImage(photo(QColor(0x9b, 0x59, 0xb6), QColor(0xf5, 0xd0, 0xa9)));
        auto *input = qobject_cast<QQuickItem *>(m_window->findChild<QObject *>(u"composerInput"_s));
        QVERIFY(input);
        input->forceActiveFocus();
        QTest::keyClick(m_window, Qt::Key_V, Qt::ControlModifier);
        if (composer->property("attachments").toList().size() < 2) {
            // The offscreen window may not take key focus: same path by hand.
            const auto files = m_controller->clipboardFiles();
            QCOMPARE(files.size(), 1);
            QVERIFY(QMetaObject::invokeMethod(composer, "addAttachments", Q_ARG(QVariant, QVariant::fromValue(files))));
        }
        const auto pending = composer->property("attachments").toList();
        QCOMPARE(pending.size(), 2);
        QVERIFY(pending.at(1).toString().endsWith(u".png"_s));
        QCOMPARE(input->property("text").toString(), QString()); // nothing pasted as text
        composer->setProperty("text", u"Two pictures"_s);
        shoot(u"composer-attachments"_s);

        m_fake.control(u"ClearCalls"_s);
        const int before = msgs->rowCount();
        QVERIFY(QMetaObject::invokeMethod(composer, "send"));
        QVERIFY(composer->property("attachments").toList().isEmpty());
        QTRY_COMPARE_WITH_TIMEOUT(msgs->rowCount(), before + 3, Timeout);
        QCOMPARE(m_fake.callsOf(u"SendFile"_s).size(), 2);
        QTRY_COMPARE_WITH_TIMEOUT(m_fake.callsOf(u"SendText"_s).size(), 1, Timeout);
        // Our own images show from the local files, without DownloadAttachment.
        for (int row = 0; row < 3; ++row) {
            const auto atts = msgs->data(msgs->index(row), MessageListModel::AttachmentsRole).toList();
            if (!atts.isEmpty()) {
                QTRY_VERIFY_WITH_TIMEOUT(!msgs->data(msgs->index(row), MessageListModel::AttachmentsRole).toList().first().toMap().value(u"localPath"_s).toString().isEmpty(), Timeout);
            }
        }
        QVERIFY(m_fake.callsOf(u"DownloadAttachment"_s).isEmpty());
        m_fake.control(u"SetUnreadablePrefixes"_s, {QStringList{}});
        shoot(u"transcript-sent-images"_s);
    }

    void notifications()
    {
        // Bob's chat is open; a message elsewhere asks for a notification.
        invoke("openChat", Bob);
        QSignalSpy notify(m_controller.get(), &AppController::notificationRequested);
        m_fake.control(u"InjectMessage"_s, {Team, u"mailto:carol@example.com"_s, u"are we still on?"_s});
        QTRY_COMPARE_WITH_TIMEOUT(notify.count(), 1, Timeout);
        QCOMPARE(notify.first().at(0).toString(), Team);
        QCOMPARE(notify.first().at(1).toString(), u"Weekend Trip"_s);
        QCOMPARE(notify.first().at(2).toString(), u"Carol Jones: are we still on?"_s); // contact name

        m_controller->settings()->setNotificationsEnabled(false);
        const QString hello = m_fake.control(u"InjectMessage"_s, {Team, u"mailto:carol@example.com"_s, u"hello?"_s}).arguments().value(0).toString();
        QTest::qWait(500);
        QCOMPARE(notify.count(), 1);
        m_controller->settings()->setNotificationsEnabled(true);

        // "My other addresses": what the user sends from their iPhone's number
        // reaches us as incoming, but is neither notified nor left unread.
        m_controller->settings()->setOtherHandles(u"+1 555 000 1111"_s);
        QCOMPARE(m_controller->daemon()->otherOwnHandles(), QStringList{u"tel:+15550001111"_s});
        m_fake.control(u"ClearCalls"_s);
        m_fake.control(u"InjectMessage"_s, {Team, u"tel:+15550001111"_s, u"from my phone"_s});
        QTRY_VERIFY_WITH_TIMEOUT(!m_fake.callsOf(u"MarkRead"_s).isEmpty(), Timeout);
        QCOMPARE(m_fake.callsOf(u"MarkRead"_s).first().at(1).toString(), Team);
        // Read up to Carol's message only: no read receipt for the user's own one.
        QCOMPARE(m_fake.callsOf(u"MarkRead"_s).first().at(2).toString(), hello);
        QTRY_COMPARE_WITH_TIMEOUT(m_controller->chatList()->unreadFor(Team), 0, Timeout);
        QCOMPARE(m_controller->chatList()->chat(Team)->unreadCount, 1u); // the daemon still counts it
        // Nothing unread from anyone else: no MarkRead at all.
        m_fake.control(u"ClearCalls"_s);
        m_fake.control(u"InjectMessage"_s, {Team, u"tel:+15550001111"_s, u"me again"_s});
        QTRY_COMPARE_WITH_TIMEOUT(m_controller->chatList()->chat(Team)->unreadCount, 2u, Timeout);
        QTest::qWait(500);
        QVERIFY(m_fake.callsOf(u"MarkRead"_s).isEmpty());
        QCOMPARE(m_controller->chatList()->unreadFor(Team), 0);
        // Someone else writes: their message counts again.
        const QString later = m_fake.control(u"InjectMessage"_s, {Team, u"mailto:carol@example.com"_s, u"ok"_s}).arguments().value(0).toString();
        QTRY_COMPARE_WITH_TIMEOUT(m_controller->chatList()->unreadFor(Team), 1, Timeout);
        m_fake.control(u"InjectMessage"_s, {Team, u"tel:+15550001111"_s, u"seen it"_s});
        QTRY_VERIFY_WITH_TIMEOUT(!m_fake.callsOf(u"MarkRead"_s).isEmpty(), Timeout);
        QCOMPARE(m_fake.callsOf(u"MarkRead"_s).first().at(2).toString(), later);
        QTRY_COMPARE_WITH_TIMEOUT(m_controller->chatList()->unreadFor(Team), 0, Timeout);
        QCOMPARE(notify.count(), 2); // "ok" was notified
        m_controller->settings()->setOtherHandles(QString());
        QVERIFY(m_controller->daemon()->otherOwnHandles().isEmpty());

        // While the window is active, a message in the open chat is marked read instead.
        m_window->requestActivate();
        if (QTest::qWaitFor([&] {
                return m_window->isActive();
            }, 2000)) {
            m_fake.control(u"ClearCalls"_s);
            m_fake.control(u"InjectMessage"_s, {Bob, Bob, u"you there?"_s});
            QTRY_VERIFY_WITH_TIMEOUT(!m_fake.callsOf(u"MarkRead"_s).isEmpty(), Timeout);
            QCOMPARE(m_fake.callsOf(u"MarkRead"_s).first().at(1).toString(), Bob);
            QCOMPARE(notify.count(), 2); // unchanged since "ok"
            QTRY_COMPARE_WITH_TIMEOUT(m_controller->chatList()->unreadFor(Bob), 0, Timeout);
        } else {
            qInfo("offscreen window cannot become active; skipping the mark-read half");
        }
    }

    void newMessage()
    {
        invoke("showNewMessage");
        QVERIFY(m_controller->currentChatId().isEmpty());
        auto *r = m_controller->recipients();
        QCOMPARE(r->rowCount(), 0);
        r->add(u"bob@example.com"_s);
        r->add(u"android@example.com"_s);
        QTRY_VERIFY_WITH_TIMEOUT(!r->isChecking(), Timeout);
        QVERIFY(!r->canSend());
        shoot(u"new-message"_s);
        r->removeLast();
        QVERIFY(r->canSend());

        // Typing a name lists the contact's addresses.
        auto *field = m_window->findChild<QObject *>(u"recipientField"_s);
        QVERIFY(field);
        field->setProperty("text", u"dav"_s);
        QCOMPARE(m_controller->contactSuggestions()->rowCount(), 2);
        QTRY_COMPARE_WITH_TIMEOUT(m_controller->contactSuggestions()->index(0).data(ContactSuggestionModel::ReachabilityRole).toString(), u"reachable"_s, Timeout);
        shoot(u"new-message-contacts"_s);
        field->setProperty("text", QString());
    }

    void newMessageKeyboard()
    {
        invoke("showNewMessage");
        auto *r = m_controller->recipients();
        auto *picker = m_controller->recipientPicker();
        QTRY_VERIFY_WITH_TIMEOUT(visibleItem(u"recipientField"_s), Timeout);
        auto *field = visibleItem(u"recipientField"_s);
        QObject *page = field;
        while (page && !page->property("highlightedIndex").isValid()) {
            page = page->parent();
        }
        QVERIFY(page);
        field->forceActiveFocus();
        QTRY_VERIFY_WITH_TIMEOUT(field->hasActiveFocus(), Timeout);
        QCOMPARE(r->rowCount(), 0);

        // "dav": Dave's two addresses (contacts), then the conversations
        // with him (the group first, then the 1:1 chat).
        typeKeys(u"dav"_s);
        QCOMPARE(field->property("text").toString(), u"dav"_s);
        QCOMPARE(m_controller->contactSuggestions()->rowCount(), 2);
        QCOMPARE(picker->rowCount(), 4);
        QCOMPARE(picker->rowData(0).value(u"kind"_s).toString(), u"contact"_s);
        QCOMPARE(picker->rowData(1).value(u"uri"_s).toString(), u"mailto:dave@example.net"_s);
        QCOMPARE(picker->rowData(2).value(u"chatId"_s).toString(), Team);
        QCOMPARE(picker->rowData(3).value(u"chatId"_s).toString(), Dave);
        QCOMPARE(page->property("highlightedIndex").toInt(), 0);
        QTest::keyClick(m_window, Qt::Key_Up); // wraps to the last row
        QCOMPARE(page->property("highlightedIndex").toInt(), 3);
        QTest::keyClick(m_window, Qt::Key_Down); // and back to the first
        QCOMPARE(page->property("highlightedIndex").toInt(), 0);
        QTest::keyClick(m_window, Qt::Key_Down);
        QTest::keyClick(m_window, Qt::Key_Down);
        shoot(u"new-message-keyboard"_s);
        QTest::keyClick(m_window, Qt::Key_Down);
        QCOMPARE(page->property("highlightedIndex").toInt(), 3);
        // The 1:1 conversation with Dave adds his number.
        QTest::keyClick(m_window, Qt::Key_Return);
        QCOMPARE(r->handles(), QStringList{Dave});
        QCOMPARE(field->property("text").toString(), QString());

        // Typing again highlights the first row: Down, Enter → the second address.
        typeKeys(u"dav"_s);
        QCOMPARE(page->property("highlightedIndex").toInt(), 0);
        QTest::keyClick(m_window, Qt::Key_Down);
        QTest::keyClick(m_window, Qt::Key_Return);
        QCOMPARE(r->handles(), (QStringList{Dave, u"mailto:dave@example.net"_s}));
        r->removeLast();

        // A contact's address: "carol", Enter → the first row.
        typeKeys(u"carol"_s);
        QCOMPARE(page->property("highlightedIndex").toInt(), 0);
        QCOMPARE(picker->rowData(0).value(u"uri"_s).toString(), Carol);
        QTest::keyClick(m_window, Qt::Key_Enter);
        QCOMPARE(r->handles(), (QStringList{Dave, Carol}));

        // Nothing matches: Enter adds what was typed.
        typeKeys(u"zed@example.org"_s);
        QCOMPARE(picker->rowCount(), 0);
        QCOMPARE(page->property("highlightedIndex").toInt(), -1);
        QTest::keyClick(m_window, Qt::Key_Return);
        QCOMPARE(r->handles(), (QStringList{Dave, Carol, u"mailto:zed@example.org"_s}));

        // Escape clears the text; with nothing typed it goes back to the list.
        typeKeys(u"bo"_s);
        QVERIFY(picker->rowCount() > 0);
        QTest::keyClick(m_window, Qt::Key_Escape);
        QCOMPARE(field->property("text").toString(), QString());
        QCOMPARE(r->rowCount(), 3);
        QTest::keyClick(m_window, Qt::Key_Escape);
        QTRY_COMPARE_WITH_TIMEOUT(pageStack()->property("depth").toInt(), 1, Timeout);
        r->clear();
    }

    void login()
    {
        invoke("showLogin", 0);
        shoot(u"login"_s);
        invoke("showLogin", 1);
        shoot(u"login-2fa"_s);
        invoke("closeOverlays");
    }

    void settings()
    {
        invoke("showSettings");
        shoot(u"settings"_s);
        invoke("closeOverlays");
    }

    void narrow()
    {
        m_window->resize(420, 820);
        settle();
        invoke("openChat", Bob);
        QTRY_VERIFY_WITH_TIMEOUT(m_controller->messages()->isLoaded() && m_controller->messages()->chatId() == Bob, Timeout);
        shoot(u"narrow-transcript"_s);
        invoke("popToList");
        QVERIFY(!m_controller->conversationVisible());
        settle();
        QCOMPARE(m_controller->currentChatId(), QString()); // Back closes the chat
        shoot(u"narrow-list"_s);
        m_window->resize(1280, 820);
    }

    void inlineReply()
    {
        QSignalSpy failed(m_controller.get(), &AppController::replyFailed);
        m_fake.control(u"ClearCalls"_s);
        // Success: sent, and the chat is marked read like on iOS.
        m_controller->sendReply(Team, u"on my way"_s);
        QTRY_VERIFY_WITH_TIMEOUT(!m_fake.callsOf(u"MarkRead"_s).isEmpty(), Timeout);
        QCOMPARE(m_fake.callsOf(u"SendText"_s).size(), 1);
        QCOMPARE(m_fake.callsOf(u"MarkRead"_s).first().at(1).toString(), Team);
        QCOMPARE(failed.count(), 0);
        // Failure: reported (a notification with an Open action).
        m_fake.control(u"ClearCalls"_s);
        m_controller->sendReply(Team, u"__fail__"_s);
        QTRY_COMPARE_WITH_TIMEOUT(failed.count(), 1, Timeout);
        QCOMPARE(failed.first().at(0).toString(), Team);
        QVERIFY(!failed.first().at(1).toString().isEmpty());
        QVERIFY(m_fake.callsOf(u"MarkRead"_s).isEmpty());
        QVERIFY(QMetaObject::invokeMethod(m_window, "hidePassiveNotification", Q_ARG(QVariant, QVariant(0))));
    }

    void connectionProblems()
    {
        // In the tray: the daemon needing the user is notified once per problem.
        m_window->hide();
        QSignalSpy problem(m_controller.get(), &AppController::connectionProblemNotified);
        QSignalSpy login(m_controller.get(), &AppController::loginRequested);
        QVERIFY(!m_controller->needsAttention() || m_controller->chatList()->totalUnread() > 0);
        const int unread = m_controller->chatList()->totalUnread();
        if (unread > 0) {
            for (const QString &chat : {Bob, Team, Dave}) {
                m_controller->daemon()->markRead(chat);
            }
            QTRY_COMPARE_WITH_TIMEOUT(m_controller->chatList()->totalUnread(), 0, Timeout);
        }
        QTRY_VERIFY_WITH_TIMEOUT(!m_controller->needsAttention(), Timeout);

        m_fake.control(u"SetConnectionState"_s, {u"needs_two_factor"_s});
        QTRY_COMPARE_WITH_TIMEOUT(problem.count(), 1, Timeout);
        QCOMPARE(problem.first().at(0).toString(), u"needs_two_factor"_s);
        QVERIFY(m_controller->needsAttention());

        m_fake.control(u"SetConnectionState"_s, {u"error"_s});
        QTRY_COMPARE_WITH_TIMEOUT(problem.count(), 2, Timeout);
        QCOMPARE(problem.at(1).at(1).toString(), u"registration failing, retrying: fake"_s); // the detail
        // A retry loop (error → connecting → error) is not announced again.
        m_fake.control(u"SetConnectionState"_s, {u"connecting"_s});
        QTest::qWait(2000);
        QVERIFY(m_controller->needsAttention()); // still not connected
        m_fake.control(u"SetConnectionState"_s, {u"error"_s});
        QTest::qWait(2000);
        QCOMPARE(problem.count(), 2);

        m_fake.control(u"SetConnectionState"_s, {u"connected"_s});
        QTRY_VERIFY_WITH_TIMEOUT(!m_controller->needsAttention(), Timeout);

        // Background errors: once per message; dropped-event resyncs are not the user's business.
        m_fake.control(u"EmitBackgroundError"_s, {u"attachment upload failed: quota"_s});
        QTRY_COMPARE_WITH_TIMEOUT(problem.count(), 3, Timeout);
        QCOMPARE(problem.at(2).at(0).toString(), u"background"_s);
        QVERIFY(m_controller->needsAttention());
        m_fake.control(u"EmitBackgroundError"_s, {u"attachment upload failed: quota"_s});
        m_fake.control(u"EmitBackgroundError"_s, {u"2 engine event(s) were dropped; clients should resync"_s});
        QTest::qWait(500);
        QCOMPARE(problem.count(), 3);
        QCOMPARE(login.count(), 0); // only the notification's Open action asks for the dialog

        m_controller->showWindow();
        QVERIFY(QTest::qWaitForWindowExposed(m_window));
        for (int i = 0; i < 3; ++i) {
            QMetaObject::invokeMethod(m_window, "hidePassiveNotification", Q_ARG(QVariant, QVariant(0)));
        }
    }

    void smsChat()
    {
        m_fake.control(u"SetSms"_s, {u"active"_s, u"tel:+15555550100"_s});
        QTRY_VERIFY_WITH_TIMEOUT(m_controller->daemon()->isSmsActive(), Timeout);
        const QString sam = m_fake.control(u"AddSmsChat"_s, {u"+15555550142"_s, u"Sam Lee"_s}).arguments().value(0).toString();
        QTRY_VERIFY_WITH_TIMEOUT(m_controller->chatList()->indexOf(sam) >= 0, Timeout);
        invoke("openChat", sam);
        auto *msgs = m_controller->messages();
        QTRY_VERIFY_WITH_TIMEOUT(msgs->isLoaded() && msgs->chatId() == sam && msgs->rowCount() == 4, Timeout);
        QTRY_VERIFY_WITH_TIMEOUT(msgs->smsMode(), Timeout);
        QObject *composer = conversationComposer();
        QVERIFY(composer);
        QTRY_VERIFY_WITH_TIMEOUT(composer->property("sms").toBool(), Timeout);
        QCOMPARE(composer->property("placeholderText").toString(), u"Text Message"_s);
        shoot(u"sms-chat"_s);

        invoke("showSettings");
        shoot(u"settings-sms"_s);
        invoke("closeOverlays");
        m_fake.control(u"SetSms"_s, {u"off"_s, u"tel:+15555550100"_s});
        QTRY_VERIFY_WITH_TIMEOUT(!msgs->smsMode(), Timeout);
        QCOMPARE(composer->property("placeholderText").toString(), u"iMessage"_s);
    }

    // Late: the message injected below would change the transcripts above.
    void pinnedChats()
    {
        invoke("openChat", Bob);
        settle();
        auto *chats = m_controller->chatList();
        QVERIFY(!chats->hasPinned());
        QVERIFY(!showsText(u"Pinned"_s));
        m_fake.control(u"ClearCalls"_s);

        // Right-click a row → Pin.
        QQuickItem *dave = chatRow(Dave);
        QVERIFY(dave);
        rightClick(dave);
        auto *menu = m_window->findChild<QObject *>(u"chatContextMenu"_s);
        QVERIFY(menu);
        QTRY_VERIFY_WITH_TIMEOUT(menu->property("opened").toBool(), Timeout);
        auto *pin = m_window->findChild<QObject *>(u"pinChatAction"_s);
        QVERIFY(pin);
        QCOMPARE(pin->property("text").toString(), u"Pin"_s);
        click(pin);
        QTRY_COMPARE_WITH_TIMEOUT(m_fake.callsOf(u"SetChatPinned"_s).size(), 1, Timeout);
        const auto call = m_fake.callsOf(u"SetChatPinned"_s).first();
        QCOMPARE(call.at(1).toString(), Dave);
        QCOMPARE(call.at(2).toBool(), true);
        QCOMPARE(call.at(3).toInt(), -1);
        QTRY_COMPARE_WITH_TIMEOUT(chats->data(chats->index(0), ChatListModel::ChatIdRole).toString(), Dave, Timeout);
        QTRY_VERIFY_WITH_TIMEOUT(showsText(u"Pinned"_s) && showsText(u"Conversations"_s), Timeout);

        // The details page has the same toggle.
        invoke("openChat", Team);
        QTRY_VERIFY_WITH_TIMEOUT(m_controller->messages()->isLoaded() && m_controller->messages()->chatId() == Team, Timeout);
        invoke("toggleDetails");
        settle();
        auto *detailsPin = m_window->findChild<QObject *>(u"detailsPinButton"_s);
        QVERIFY(detailsPin);
        QCOMPARE(detailsPin->property("text").toString(), u"Pin"_s);
        click(detailsPin);
        QTRY_COMPARE_WITH_TIMEOUT(chats->data(chats->index(1), ChatListModel::ChatIdRole).toString(), Team, Timeout);
        QTRY_COMPARE_WITH_TIMEOUT(detailsPin->property("text").toString(), u"Unpin"_s, Timeout);
        // A new message elsewhere does not push the pins down.
        m_fake.control(u"InjectMessage"_s, {Bob, Bob, u"still below the pins"_s});
        QTRY_COMPARE_WITH_TIMEOUT(chats->data(chats->index(2), ChatListModel::SnippetRole).toString(), u"still below the pins"_s, Timeout);
        QCOMPARE(chats->data(chats->index(0), ChatListModel::ChatIdRole).toString(), Dave);
        QCOMPARE(visibleItems(u"pinnedIcon"_s).size(), 2);
        settle(800);
        shoot(u"pinned-chats"_s);

        // Unpin again: context menu says "Unpin" now; the details toggle too.
        rightClick(chatRow(Dave));
        QTRY_VERIFY_WITH_TIMEOUT(menu->property("opened").toBool(), Timeout);
        QCOMPARE(pin->property("text").toString(), u"Unpin"_s);
        click(pin);
        click(detailsPin);
        QTRY_VERIFY_WITH_TIMEOUT(!chats->hasPinned(), Timeout);
        const auto calls = m_fake.callsOf(u"SetChatPinned"_s);
        QCOMPARE(calls.size(), 4);
        QCOMPARE(calls.at(2).at(2).toBool(), false);
        QTRY_VERIFY_WITH_TIMEOUT(!showsText(u"Pinned"_s), Timeout);
        invoke("toggleDetails");
        invoke("popToList");
        settle();

        // A plain left click on a row still opens the chat.
        QVERIFY(m_controller->currentChatId().isEmpty());
        QQuickItem *row = chatRow(Dave);
        QVERIFY(row);
        QTest::mouseClick(m_window, Qt::LeftButton, {}, row->mapToScene(QPointF(row->width() / 2, row->height() / 2)).toPoint());
        QTRY_COMPARE_WITH_TIMEOUT(m_controller->currentChatId(), Dave, Timeout);
        QVERIFY(!menu->property("opened").toBool());
        invoke("popToList");
        settle();
    }

    void reorderKeepsListScroll()
    {
        // Reorders arrive as VerticalSortHint layout changes on the model the
        // list binds directly: no reset, so the view keeps its scroll position.
        const QString eve = u"mailto:eve@example.org"_s;
        m_window->resize(1280, 330);
        settle();
        auto *view = visibleItem(u"chatListView"_s);
        QVERIFY(view);
        const qreal maxY = view->property("contentHeight").toReal() - view->height() + view->property("originY").toReal();
        QVERIFY2(maxY > 20, "the chat list must scroll for this test");
        view->setProperty("contentY", maxY);
        settle();
        const qreal y = view->property("contentY").toReal();
        auto *chats = m_controller->chatList();
        const QString bottom = chats->data(chats->index(chats->rowCount() - 1), ChatListModel::ChatIdRole).toString();
        m_fake.control(u"InjectMessage"_s, {bottom, bottom, u"up you go"_s});
        QTRY_COMPARE_WITH_TIMEOUT(chats->data(chats->index(0), ChatListModel::ChatIdRole).toString(), bottom, Timeout);
        settle();
        QCOMPARE(view->property("contentY").toReal(), y);
        m_controller->daemon()->setChatPinned(eve, true);
        QTRY_VERIFY_WITH_TIMEOUT(chats->isPinned(eve), Timeout);
        settle();
        QCOMPARE(view->property("contentY").toReal(), y);
        m_controller->daemon()->setChatPinned(eve, false);
        QTRY_VERIFY_WITH_TIMEOUT(!chats->hasPinned(), Timeout);
        m_window->resize(1280, 820);
        settle();
    }

    void syncFromMac()
    {
        // A stand-in imsg on PATH: prints a summary (or fails) after a moment.
        QTemporaryDir bin;
        QVERIFY(bin.isValid());
        const QString script = bin.filePath(u"imsg"_s);
        {
            QFile f(script);
            QVERIFY(f.open(QIODevice::WriteOnly));
            f.write(R"SH(#!/bin/sh
echo "$@" > "$(dirname "$0")/args"
if [ "$FAKE_IMSG_MODE" = slow ]; then exec sleep 30; fi
sleep 1
if [ "$FAKE_IMSG_MODE" = fail ]; then
    echo "ssh: connect to host mac.local port 22: Connection refused" >&2
    echo '{"host": "mac.local", "error": "ssh mac.local failed (exit 255)"}'
    exit 1
fi
echo '{"host": "mac.local", "dry_run": false,'
echo ' "contacts": {"dir": "/tmp/x", "received": 312, "photos": 200, "written": 3, "unchanged": 308, "removed": 1},'
echo ' "pins": {"mac": [{"pin": "a", "chat_id": "mailto:a@x", "result": "pinned"}, {"pin": "b", "chat_id": "mailto:b@x", "result": "pinned"},'
echo '                  {"pin": "c", "chat_id": "mailto:c@x", "result": "pinned"}], "order": [], "kept_local": [], "unpin": []}}'
)SH");
            f.close();
            QVERIFY(f.setPermissions(QFile::ReadOwner | QFile::WriteOwner | QFile::ExeOwner));
        }
        const QByteArray oldPath = qgetenv("PATH");
        qputenv("PATH", QByteArray(QFile::encodeName(bin.path()) + ':' + oldPath));
        qunsetenv("KMESSAGES_IMSG");
        qunsetenv("FAKE_IMSG_MODE");
        auto *sync = m_controller->macSync();
        QCOMPARE(sync->program(), script);
        m_fake.control(u"SetMacHost"_s, {u"mac.local"_s});
        m_controller->daemon()->refreshStatus();
        QTRY_COMPARE_WITH_TIMEOUT(sync->host(), u"mac.local"_s, Timeout);

        QQuickItem *button = nullptr;
        QTRY_VERIFY_WITH_TIMEOUT((button = visibleItem(u"syncFromMacButton"_s)), Timeout);
        const auto busyItems = button->findChildren<QQuickItem *>(u"macSyncBusy"_s);
        QCOMPARE(busyItems.size(), 1);
        auto *busy = busyItems.first();
        QVERIFY(button->isEnabled());
        QVERIFY(!busy->isVisible());
        QVERIFY(sync->lastSyncText().isEmpty());
        auto *action = m_window->findChild<QObject *>(u"syncFromMacAction"_s);
        QVERIFY(action);
        QVERIFY(action->property("tooltip").toString().contains(u"mac.local"_s));
        QVERIFY(action->property("tooltip").toString().contains(u"Not synced yet"_s));

        // Success.
        QSignalSpy finished(sync, &MacSync::finished);
        click(button);
        QTRY_VERIFY_WITH_TIMEOUT(sync->isRunning(), Timeout);
        QVERIFY(busy->isVisible());
        QVERIFY(button->isEnabled()); // a click stops it
        QVERIFY(!action->property("enabled").toBool());
        QVERIFY(action->property("tooltip").toString().contains(u"Click to stop"_s));
        shoot(u"sync-from-mac"_s);
        QTRY_COMPARE_WITH_TIMEOUT(finished.count(), 1, Timeout);
        const QString summary = u"Synced from mac.local: 312 contacts (3 updated, 1 removed), 3 pins"_s;
        QCOMPARE(finished.first().at(0).toBool(), true);
        QCOMPARE(finished.first().at(1).toString(), summary);
        QTRY_VERIFY_WITH_TIMEOUT(showsText(summary), Timeout); // the passive notification
        QVERIFY(!busy->isVisible());
        QVERIFY(button->isEnabled());
        QVERIFY(!sync->lastSyncText().isEmpty());
        QVERIFY(action->property("tooltip").toString().contains(u"Last synced "_s));
        auto readArgs = [&] {
            QFile f(bin.filePath(u"args"_s));
            return f.open(QIODevice::ReadOnly) ? QString::fromUtf8(f.readAll()).trimmed() : QString();
        };
        QCOMPARE(readArgs(), u"sync-mac --json"_s); // the daemon's mac_host: imsg finds it itself
        const QDateTime last = sync->lastSync();
        QVERIFY(QMetaObject::invokeMethod(m_window, "hidePassiveNotification", Q_ARG(QVariant, QVariant(0))));

        // Failure, with a host from Settings: the error is shown, the last sync kept.
        m_controller->settings()->setMacHost(u"me@other.local"_s);
        QCOMPARE(sync->host(), u"me@other.local"_s);
        qputenv("FAKE_IMSG_MODE", "fail");
        click(button);
        QTRY_VERIFY_WITH_TIMEOUT(sync->isRunning(), Timeout);
        QTRY_COMPARE_WITH_TIMEOUT(finished.count(), 2, Timeout);
        QCOMPARE(finished.at(1).at(0).toBool(), false);
        const QString error = u"Sync from Mac failed: ssh mac.local failed (exit 255)"_s;
        QCOMPARE(finished.at(1).at(1).toString(), error);
        QTRY_VERIFY_WITH_TIMEOUT(showsText(error), Timeout);
        QCOMPARE(readArgs(), u"sync-mac --json -- me@other.local"_s);
        QCOMPARE(sync->lastSync(), last);
        QVERIFY(button->isEnabled());

        QVERIFY(QMetaObject::invokeMethod(m_window, "hidePassiveNotification", Q_ARG(QVariant, QVariant(0))));

        // Clicking the busy button stops a sync.
        qputenv("FAKE_IMSG_MODE", "slow");
        click(button);
        QTRY_VERIFY_WITH_TIMEOUT(sync->isRunning() && busy->isVisible(), Timeout);
        QTest::qWait(300);
        click(button);
        QTRY_COMPARE_WITH_TIMEOUT(finished.count(), 3, Timeout);
        QCOMPARE(finished.at(2).at(1).toString(), u"Sync from Mac stopped."_s);
        QVERIFY(!sync->isRunning());
        QVERIFY(!busy->isVisible());
        QCOMPARE(sync->lastSync(), last);

        // Back to how the other tests expect it.
        QVERIFY(QMetaObject::invokeMethod(m_window, "hidePassiveNotification", Q_ARG(QVariant, QVariant(0))));
        qunsetenv("FAKE_IMSG_MODE");
        qputenv("PATH", oldPath);
        m_controller->settings()->setMacHost(QString());
        m_fake.control(u"SetMacHost"_s, {QString()});
        m_controller->daemon()->refreshStatus();
        QTRY_VERIFY_WITH_TIMEOUT(sync->host().isEmpty(), Timeout);
    }

    void needsLogin()
    {
        m_fake.stop();
        QTRY_COMPARE_WITH_TIMEOUT(m_controller->daemon()->connectionState(), u"unavailable"_s, Timeout);
        shoot(u"daemon-unavailable"_s);
        QVERIFY(m_fake.start({u"--state"_s, u"needs_login"_s, u"--empty"_s}));
        QTRY_COMPARE_WITH_TIMEOUT(m_controller->daemon()->connectionState(), u"needs_login"_s, Timeout);
        QTRY_COMPARE_WITH_TIMEOUT(m_controller->chatList()->rowCount(), 0, Timeout);
        invoke("popToList");
        shoot(u"needs-login"_s);
    }

    void noQmlWarnings()
    {
        QVERIFY2(g_qmlWarnings.isEmpty(), qPrintable(g_qmlWarnings.join(u'\n')));
    }

    void cleanupTestCase()
    {
        if (m_engine) {
            m_engine.reset();
        }
        m_controller.reset();
        m_fake.stop();
        qInfo().noquote() << "screenshots:" << m_shots.join(u", "_s);
    }
};

int main(int argc, char **argv)
{
    useTemporaryXdgDirs(); // settings, caches: nothing lands in ~/.config or ~/.qttest
    KIconTheme::initTheme();
    QApplication app(argc, argv);
    if (qEnvironmentVariableIsEmpty("QT_QUICK_CONTROLS_STYLE")) {
        QQuickStyle::setStyle(u"org.kde.desktop"_s);
    }
    QApplication::setApplicationName(u"kmessages-test"_s);
    TestScreens tc;
    QTEST_SET_MAIN_SOURCE_PATH
    return QTest::qExec(&tc, argc, argv);
}

#include "tst_screens.moc"
