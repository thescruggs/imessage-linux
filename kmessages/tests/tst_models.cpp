// SPDX-License-Identifier: SSPL-1.0
//
// DaemonClient + models against tests/fake_daemon.py on a private bus.
#include "appsettings.h"
#include "chatlistmodel.h"
#include "contactresolver.h"
#include "daemonclient.h"
#include "fakedaemon.h"
#include "handlelookupmodel.h"
#include "macsync.h"
#include "messagelistmodel.h"
#include "messagesearchmodel.h"
#include "outgoingfiles.h"

#include <QAbstractItemModelTester>
#include <QDir>
#include <QFileInfo>
#include <QImage>
#include <QTemporaryDir>
#include <QSignalSpy>
#include <QTest>

using namespace Qt::StringLiterals;
using namespace km;

namespace
{
const QString Bob = u"mailto:bob@example.com"_s;
const QString Team = u"group:5B0E2C1A-3F4D-4E21-9A55-0C7E1B2D3F40"_s;
const QString Dave = u"tel:+15551234567"_s;
const QString Carol = u"mailto:carol@example.com"_s;
constexpr int Timeout = 5000;

QVariant roleOf(const QAbstractItemModel &m, int row, int role)
{
    return m.data(m.index(row, 0), role);
}

int rowWithText(const MessageListModel &m, const QString &text)
{
    for (int i = 0; i < m.rowCount(); ++i) {
        if (roleOf(m, i, MessageListModel::TextRole).toString() == text) {
            return i;
        }
    }
    return -1;
}

QVariantMap reactionWithGlyph(const MessageListModel &m, int row, const QString &glyph)
{
    const auto list = roleOf(m, row, MessageListModel::ReactionsRole).toList();
    for (const auto &r : list) {
        if (r.toMap().value(u"glyph"_s).toString() == glyph) {
            return r.toMap();
        }
    }
    return {};
}
}

class TestModels : public QObject
{
    Q_OBJECT

    FakeDaemon m_fake;

    std::unique_ptr<DaemonClient> connectedClient()
    {
        auto c = std::make_unique<DaemonClient>();
        if (!QTest::qWaitFor([&] {
                return c->isAvailable() && !c->ownHandles().isEmpty();
            }, Timeout)) {
            qWarning("client did not connect");
        }
        return c;
    }

    std::unique_ptr<MessageListModel> loadedMessages(DaemonClient *client, ChatListModel *chats, const QString &chatId, int pageSize = 50)
    {
        auto m = std::make_unique<MessageListModel>(client, chats);
        m->setPageSize(pageSize);
        m->setChatId(chatId);
        if (!QTest::qWaitFor([&] {
                return m->isLoaded();
            }, Timeout)) {
            qWarning("messages did not load");
        }
        return m;
    }

private Q_SLOTS:
    void initTestCase()
    {
        if (!FakeDaemon::onPrivateBus()) {
            QSKIP("needs the private test bus; run through ctest");
        }
    }

    void init()
    {
        QVERIFY(m_fake.start());
    }

    void cleanup()
    {
        m_fake.stop();
    }

    void clientStatus()
    {
        DaemonClient client;
        QSignalSpy resync(&client, &DaemonClient::resyncRequested);
        QTRY_VERIFY_WITH_TIMEOUT(client.isAvailable(), Timeout);
        QTRY_COMPARE_WITH_TIMEOUT(client.connectionState(), u"connected"_s, Timeout);
        QTRY_COMPARE_WITH_TIMEOUT(client.ownHandles(), QStringList{u"mailto:me@example.com"_s}, Timeout);
        QCOMPARE(client.version(), u"0.1.0-fake"_s);
        QVERIFY(client.isRegistered());
        QVERIFY(client.isOwnHandle(u"mailto:me@example.com"_s));
        QCOMPARE(client.ownSender(), u"mailto:me@example.com"_s); // registration.sender
        QCOMPARE(resync.count(), 1);

        // ConnectionStateChanged + PropertiesChanged
        m_fake.control(u"SetConnectionState"_s, {u"registering"_s});
        QTRY_COMPARE_WITH_TIMEOUT(client.connectionState(), u"registering"_s, Timeout);
        m_fake.control(u"SetConnectionState"_s, {u"error"_s});
        QTRY_COMPARE_WITH_TIMEOUT(client.connectionDetail(), u"registration failing, retrying: fake"_s, Timeout);

        QSignalSpy bg(&client, &DaemonClient::backgroundError);
        m_fake.control(u"EmitBackgroundError"_s, {u"3 engine event(s) were dropped; clients should resync"_s});
        QTRY_COMPARE_WITH_TIMEOUT(bg.count(), 1, Timeout);
        QTRY_COMPARE_WITH_TIMEOUT(resync.count(), 2, Timeout);
    }

    void chatListPopulates()
    {
        auto client = connectedClient();
        ChatListModel chats(client.get());
        QAbstractItemModelTester tester(&chats);
        QTRY_COMPARE_WITH_TIMEOUT(chats.rowCount(), 4, Timeout);
        QVERIFY(chats.isLoaded());
        // newest activity first
        QCOMPARE(roleOf(chats, 0, ChatListModel::ChatIdRole).toString(), Bob);
        QCOMPARE(roleOf(chats, 1, ChatListModel::ChatIdRole).toString(), Team);
        QCOMPARE(roleOf(chats, 2, ChatListModel::ChatIdRole).toString(), Dave);
        QCOMPARE(roleOf(chats, 0, ChatListModel::NameRole).toString(), u"Bob Smith"_s);
        QCOMPARE(roleOf(chats, 1, ChatListModel::NameRole).toString(), u"Weekend Trip"_s);
        QCOMPARE(roleOf(chats, 2, ChatListModel::NameRole).toString(), u"+1 (555) 123-4567"_s);
        QCOMPARE(roleOf(chats, 3, ChatListModel::NameRole).toString(), u"Eve Adams"_s);
        QCOMPARE(roleOf(chats, 0, ChatListModel::InitialsRole).toString(), u"BS"_s);
        QVERIFY(roleOf(chats, 1, ChatListModel::IsGroupRole).toBool());
        QCOMPARE(roleOf(chats, 1, ChatListModel::UnreadCountRole).toInt(), 2);
        QVERIFY(roleOf(chats, 1, ChatListModel::HasUnreadRole).toBool());
        QVERIFY(!roleOf(chats, 0, ChatListModel::HasUnreadRole).toBool());
        QCOMPARE(chats.totalUnread(), 3);
        QVERIFY(roleOf(chats, 0, ChatListModel::AvatarColorRole).value<QColor>().isValid());
        QVERIFY(!roleOf(chats, 0, ChatListModel::TimeRole).toString().isEmpty());
        // snippets come from ListMessages(limit 1)
        QTRY_COMPARE_WITH_TIMEOUT(roleOf(chats, 0, ChatListModel::SnippetRole).toString(), u"Here's the best one"_s, Timeout);
        QTRY_COMPARE_WITH_TIMEOUT(roleOf(chats, 1, ChatListModel::SnippetRole).toString(), u"@Dave don't forget the stove"_s, Timeout);
        QTRY_COMPARE_WITH_TIMEOUT(roleOf(chats, 3, ChatListModel::SnippetRole).toString(), u"Unsupported: app message"_s, Timeout);
        QCOMPARE(chats.titleFor(Team), u"Weekend Trip"_s);
        QCOMPARE(chats.nameForHandle(Bob), u"Bob Smith"_s);
        QCOMPARE(chats.get(Team).value(u"participants"_s).toStringList().size(), 3);

        ChatFilterModel filter(&chats);
        filter.setFilterText(u"eve"_s);
        QCOMPARE(filter.rowCount(), 1);
        filter.setFilterText(u"carol"_s); // participant address
        QCOMPARE(filter.rowCount(), 1);
        QCOMPARE(filter.chatIdAt(0), Team);
        filter.setFilterText(u"stove"_s); // snippet
        QCOMPARE(filter.rowCount(), 1);
        filter.setFilterText(QString());
        QCOMPARE(filter.rowCount(), 4);
    }

    void pinnedChatsFirst()
    {
        const QString eve = u"mailto:eve@example.org"_s;
        auto client = connectedClient();
        ChatListModel chats(client.get());
        QAbstractItemModelTester tester(&chats);
        QTRY_COMPARE_WITH_TIMEOUT(chats.rowCount(), 4, Timeout);
        const auto order = [&] {
            QStringList ids;
            for (int i = 0; i < chats.rowCount(); ++i) {
                ids << roleOf(chats, i, ChatListModel::ChatIdRole).toString();
            }
            return ids;
        };
        QCOMPARE(order(), (QStringList{Bob, Team, Dave, eve}));
        QVERIFY(!chats.hasPinned());
        QCOMPARE(roleOf(chats, 0, ChatListModel::SectionRole).toString(), QString()); // no sections without pins
        QVERIFY(!roleOf(chats, 0, ChatListModel::PinnedRole).toBool());
        QCOMPARE(roleOf(chats, 0, ChatListModel::PinOrderRole).toInt(), -1);

        // Pinning moves the row into the pinned block, new pins last.
        QSignalSpy hasPinned(&chats, &ChatListModel::hasPinnedChanged);
        client->setChatPinned(eve, true);
        QTRY_COMPARE_WITH_TIMEOUT(order(), (QStringList{eve, Bob, Team, Dave}), Timeout);
        QVERIFY(chats.hasPinned());
        QCOMPARE(hasPinned.count(), 1);
        client->setChatPinned(Dave, true);
        QTRY_COMPARE_WITH_TIMEOUT(order(), (QStringList{eve, Dave, Bob, Team}), Timeout);
        QVERIFY(roleOf(chats, 1, ChatListModel::PinnedRole).toBool());
        QCOMPARE(roleOf(chats, 0, ChatListModel::PinOrderRole).toInt(), 0);
        QCOMPARE(roleOf(chats, 1, ChatListModel::PinOrderRole).toInt(), 1);
        QCOMPARE(roleOf(chats, 1, ChatListModel::SectionRole).toString(), u"Pinned"_s);
        QCOMPARE(roleOf(chats, 2, ChatListModel::SectionRole).toString(), u"Conversations"_s);
        QCOMPARE(chats.get(Dave).value(u"pinned"_s).toBool(), true);

        // Moving a pin first renumbers the others (one ChatUpdated each).
        client->setChatPinned(Dave, true, 0);
        QTRY_COMPARE_WITH_TIMEOUT(order(), (QStringList{Dave, eve, Bob, Team}), Timeout);
        QCOMPARE(roleOf(chats, 1, ChatListModel::PinOrderRole).toInt(), 1);

        // New activity sorts the rest, never above or out of the pinned block.
        m_fake.control(u"InjectMessage"_s, {Team, Carol, u"pins stay put"_s});
        QTRY_COMPARE_WITH_TIMEOUT(order(), (QStringList{Dave, eve, Team, Bob}), Timeout);
        m_fake.control(u"InjectMessage"_s, {eve, eve, u"I am pinned"_s});
        QTRY_COMPARE_WITH_TIMEOUT(roleOf(chats, 1, ChatListModel::SnippetRole).toString(), u"I am pinned"_s, Timeout);
        QTest::qWait(200); // and the ChatUpdated that follows
        QCOMPARE(order(), (QStringList{Dave, eve, Team, Bob}));

        // ReorderPins
        client->reorderPins({eve});
        QTRY_COMPARE_WITH_TIMEOUT(order(), (QStringList{eve, Dave, Team, Bob}), Timeout);

        // A fresh ListChats sorts the same way.
        {
            ChatListModel fresh(client.get());
            QTRY_COMPARE_WITH_TIMEOUT(fresh.rowCount(), 4, Timeout);
            QCOMPARE(roleOf(fresh, 0, ChatListModel::ChatIdRole).toString(), eve);
            QCOMPARE(roleOf(fresh, 1, ChatListModel::ChatIdRole).toString(), Dave);
            QCOMPARE(roleOf(fresh, 2, ChatListModel::ChatIdRole).toString(), Team);
            QVERIFY(fresh.hasPinned());
        }

        // Unpinned, a chat goes back to its place by time (Eve has the newest message).
        client->setChatPinned(eve, false);
        QTRY_COMPARE_WITH_TIMEOUT(order(), (QStringList{Dave, eve, Team, Bob}), Timeout);
        QVERIFY(!roleOf(chats, 1, ChatListModel::PinnedRole).toBool());
        client->setChatPinned(Dave, false);
        QTRY_COMPARE_WITH_TIMEOUT(order(), (QStringList{eve, Team, Bob, Dave}), Timeout);
        QVERIFY(!chats.hasPinned());
        QCOMPARE(hasPinned.count(), 2);
        QCOMPARE(roleOf(chats, 3, ChatListModel::SectionRole).toString(), QString());

        // Failures are reported (unknown chat).
        QSignalSpy failed(client.get(), &DaemonClient::operationFailed);
        client->setChatPinned(u"mailto:nobody@example.com"_s, true);
        QTRY_COMPARE_WITH_TIMEOUT(failed.count(), 1, Timeout);
        QCOMPARE(failed.first().at(0).toString(), u"SetChatPinned"_s);
    }

    void macSyncSummary()
    {
        bool ok = false;
        const QByteArray full = R"({"host":"mac.local","dry_run":false,
            "contacts":{"dir":"/x","received":312,"photos":200,"written":3,"unchanged":308,"removed":1},
            "pins":{"mac":[{"pin":"+12055550100","chat_id":"tel:+12055550100","result":"pinned"},
                           {"pin":"bob@example.com","chat_id":"mailto:bob@example.com","result":"pinned"},
                           {"pin":"9F1C","chat_id":"group:9F1C","result":"not-in-daemon"}],
                    "order":[],"kept_local":[],"unpin":[]}})";
        QCOMPARE(MacSync::summarize(full, 0, QString(), &ok), u"Synced from mac.local: 312 contacts (4 updated), 2 pins (1 not found)"_s);
        QVERIFY(ok);
        QCOMPARE(MacSync::summarize(R"({"host":"m","contacts":{"received":1,"written":0,"removed":0},"pins":{"mac":[]}})", 0, QString(), &ok),
                 u"Synced from m: 1 contact, 0 pins"_s);
        QVERIFY(ok);
        // A part failed (exit 1): what worked and the error.
        QCOMPARE(MacSync::summarize(R"({"host":"m","contacts":{"error":"no Full Disk Access"},"pins":{"mac":[{"result":"pinned"}]}})", 1, QString(), &ok),
                 u"Sync from Mac incomplete: 1 pin; contacts: no Full Disk Access"_s);
        QVERIFY(!ok);
        // Nothing applied: {"host", "error"}.
        QCOMPARE(MacSync::summarize(R"({"host":null,"error":"no Mac host given"})", 2, QString(), &ok), u"Sync from Mac failed: no Mac host given"_s);
        QVERIFY(!ok);
        QCOMPARE(MacSync::summarize(R"({"host":"m","error":"daemon not running"})", MacSync::ExitNoDaemon, QString(), &ok),
                 u"Sync from Mac: imessaged is not running. Start it and try again."_s);
        QVERIFY(!ok);
        // Not JSON (an old imsg): the last stderr line.
        QCOMPARE(MacSync::summarize("", 2, u"warning\nimsg: error: unrecognized subcommand 'sync-mac'\n"_s, &ok),
                 u"Sync from Mac failed: unrecognized subcommand 'sync-mac'"_s);
        QCOMPARE(MacSync::summarize("", 1, QString(), &ok), u"Sync from Mac failed (imsg exited with code 1)."_s);
        QVERIFY(!ok);
    }

    void sharedProfilesAndGroupIcons()
    {
        ContactResolver contacts(false); // no address book: shared profiles only
        auto client = connectedClient();
        ChatListModel chats(client.get());
        QTRY_COMPARE_WITH_TIMEOUT(chats.rowCount(), 4, Timeout);
        const auto role = [&](const QString &chatId, int r) {
            return chats.data(chats.index(chats.indexOf(chatId)), r);
        };
        QCOMPARE(role(Dave, ChatListModel::NameRole).toString(), u"+1 (555) 123-4567"_s);
        auto messages = loadedMessages(client.get(), &chats, Team);

        // Dave shares a name and photo: the signal updates the resolver, every
        // model re-reads its rows.
        QSignalSpy updated(client.get(), &DaemonClient::handleProfileUpdated);
        QSignalSpy messageRows(messages.get(), &QAbstractItemModel::dataChanged);
        m_fake.control(u"SetProfile"_s, {Dave, u"Dee Ray"_s, u"Dee"_s, true});
        QTRY_COMPARE_WITH_TIMEOUT(updated.count(), 1, Timeout);
        QCOMPARE(updated.first().first().toString(), Dave);
        QCOMPARE(contacts.sharedNameFor(Dave), u"Dee Ray"_s);
        QCOMPARE(role(Dave, ChatListModel::NameRole).toString(), u"Dee Ray"_s);
        QCOMPARE(role(Dave, ChatListModel::InitialsRole).toString(), u"DR"_s);
        const QString photo = role(Dave, ChatListModel::AvatarSourceRole).toString();
        QVERIFY2(photo.startsWith(u"image://attachment/"_s), qPrintable(photo));
        QCOMPARE(photo, contacts.sharedPhotoSource(Dave));
        QVERIFY(messageRows.count() >= 1);
        QCOMPARE(role(Team, ChatListModel::ParticipantsRole).toStringList().at(2), u"Dee Ray"_s);
        QCOMPARE(role(Team, ChatListModel::ParticipantSharedNamesRole).toStringList(), (QStringList{QString(), QString(), u"Dee Ray"_s}));
        QCOMPARE(role(Team, ChatListModel::ParticipantSharedPhotosRole).toStringList().at(2), photo);
        int tent = -1;
        for (int i = 0; i < messages->rowCount(); ++i) {
            if (messages->data(messages->index(i), MessageListModel::TextRole).toString() == u"I have a tent for 4"_s) {
                tent = i;
            }
        }
        QVERIFY(tent >= 0);
        QCOMPARE(messages->data(messages->index(tent), MessageListModel::SenderNameRole).toString(), u"Dee Ray"_s);
        QCOMPARE(messages->data(messages->index(tent), MessageListModel::AvatarSourceRole).toString(), photo);
        // A shared name wins over the daemon's `display` (Bob Smith), like an
        // address-book name does.
        m_fake.control(u"SetProfile"_s, {Bob, u"Bobby"_s, QString(), false});
        QTRY_COMPARE_WITH_TIMEOUT(role(Bob, ChatListModel::NameRole).toString(), u"Bobby"_s, Timeout);
        QVERIFY(role(Bob, ChatListModel::AvatarSourceRole).toString().isEmpty());

        // Stopping clears it again.
        m_fake.control(u"SetProfile"_s, {Dave, QString(), QString(), false});
        QTRY_COMPARE_WITH_TIMEOUT(role(Dave, ChatListModel::NameRole).toString(), u"+1 (555) 123-4567"_s, Timeout);
        QVERIFY(contacts.sharedNameFor(Dave).isEmpty());
        QVERIFY(role(Dave, ChatListModel::AvatarSourceRole).toString().isEmpty());

        // Group photo: the group's avatar shows it.
        QVERIFY(role(Team, ChatListModel::AvatarSourceRole).toString().isEmpty());
        m_fake.control(u"SetGroupIcon"_s, {Team, true});
        QTRY_VERIFY_WITH_TIMEOUT(role(Team, ChatListModel::AvatarSourceRole).toString().startsWith(u"image://attachment/"_s), Timeout);
        QCOMPARE(role(Team, ChatListModel::IconSourceRole).toString(), role(Team, ChatListModel::AvatarSourceRole).toString());
        m_fake.control(u"SetGroupIcon"_s, {Team, false});
        QTRY_VERIFY_WITH_TIMEOUT(role(Team, ChatListModel::AvatarSourceRole).toString().isEmpty(), Timeout);
    }

    void messagePaging()
    {
        auto client = connectedClient();
        ChatListModel chats(client.get());
        QTRY_COMPARE_WITH_TIMEOUT(chats.rowCount(), 4, Timeout);
        auto msgs = loadedMessages(client.get(), &chats, Bob, 10);
        // (no QAbstractItemModelTester here: it calls fetchMore() itself)
        QCOMPARE(msgs->rowCount(), 10);
        QVERIFY(msgs->canLoadMore());
        QVERIFY(msgs->canFetchMore({}));
        QCOMPARE(roleOf(*msgs, 0, MessageListModel::TextRole).toString(), u"Here's the best one"_s);

        for (int expected : {20, 30, 31}) {
            msgs->fetchMore({});
            QTRY_COMPARE_WITH_TIMEOUT(msgs->rowCount(), expected, Timeout);
        }
        QTRY_VERIFY_WITH_TIMEOUT(!msgs->canLoadMore(), Timeout);
        QVERIFY(!msgs->canFetchMore({}));
        QCOMPARE(roleOf(*msgs, 30, MessageListModel::TextRole).toString(), u"Bob message 00"_s);
        for (int i = 1; i < msgs->rowCount(); ++i) {
            QVERIFY(roleOf(*msgs, i - 1, MessageListModel::TimestampRole).toLongLong() >= roleOf(*msgs, i, MessageListModel::TimestampRole).toLongLong());
        }
        // cursors were passed back
        const auto calls = m_fake.callsOf(u"ListMessages"_s);
        int paged = 0;
        for (const auto &c : calls) {
            if (c.at(1).toString() == Bob && !c.at(2).toString().isEmpty()) {
                ++paged;
            }
        }
        QCOMPARE(paged, 3);
        // the oldest row always starts with a separator
        QVERIFY(roleOf(*msgs, 30, MessageListModel::ShowSeparatorRole).toBool());
    }

    void groupingAndRoles()
    {
        auto client = connectedClient();
        ChatListModel chats(client.get());
        QTRY_COMPARE_WITH_TIMEOUT(chats.rowCount(), 4, Timeout);
        auto msgs = loadedMessages(client.get(), &chats, Bob);
        QCOMPARE(msgs->rowCount(), 31);

        // rows 0 and 1 are Bob, one minute apart: one group, tail on row 0
        QVERIFY(roleOf(*msgs, 0, MessageListModel::LastInGroupRole).toBool());
        QVERIFY(!roleOf(*msgs, 0, MessageListModel::FirstInGroupRole).toBool());
        QVERIFY(!roleOf(*msgs, 1, MessageListModel::LastInGroupRole).toBool());
        QVERIFY(roleOf(*msgs, 1, MessageListModel::FirstInGroupRole).toBool());
        QVERIFY(!msgs->isGroup());
        QVERIFY(!roleOf(*msgs, 0, MessageListModel::ShowSenderNameRole).toBool());

        // the newest own message carries the delivery label
        const int own = rowWithText(*msgs, u"Yes! They look amazing 😍 https://example.com/trip"_s);
        QCOMPARE(own, 2);
        QCOMPARE(roleOf(*msgs, own, MessageListModel::DeliveryLabelRole).toString(), u"Read"_s);
        QVERIFY(roleOf(*msgs, own, MessageListModel::RichTextRole).toString().contains(u"<a href=\"https://example.com/trip\">"_s));
        const int olderOwn = rowWithText(*msgs, u"My reply 25"_s);
        QVERIFY(olderOwn > own);
        QVERIFY(roleOf(*msgs, olderOwn, MessageListModel::DeliveryLabelRole).toString().isEmpty());

        // reactions + attachments on the newest message
        const auto love = reactionWithGlyph(*msgs, 0, u"❤️"_s);
        QCOMPARE(love.value(u"count"_s).toInt(), 1);
        QVERIFY(love.value(u"fromMe"_s).toBool());
        QCOMPARE(roleOf(*msgs, 0, MessageListModel::MyReactionRole).toString(), u"love"_s);
        const auto atts = roleOf(*msgs, 0, MessageListModel::AttachmentsRole).toList();
        QCOMPARE(atts.size(), 1);
        QVERIFY(atts.first().toMap().value(u"isImage"_s).toBool());
        QCOMPARE(atts.first().toMap().value(u"filename"_s).toString(), u"beach.png"_s);

        // group chat: sender names, mentions, attachments without images
        auto team = loadedMessages(client.get(), &chats, Team);
        QVERIFY(team->isGroup());
        const int stove = rowWithText(*team, u"@Dave don't forget the stove"_s);
        QCOMPARE(stove, 0);
        QVERIFY(roleOf(*team, stove, MessageListModel::RichTextRole).toString().contains(u"<b>@Dave</b>"_s));
        const int list = rowWithText(*team, u"Here's the packing list"_s);
        QVERIFY(roleOf(*team, list, MessageListModel::ShowSenderNameRole).toBool());
        QCOMPARE(roleOf(*team, list, MessageListModel::SenderNameRole).toString(), u"carol@example.com"_s);
        QVERIFY(!roleOf(*team, stove, MessageListModel::ShowSenderNameRole).toBool()); // same run
        QVERIFY(roleOf(*team, stove, MessageListModel::ShowAvatarRole).toBool());
        QVERIFY(roleOf(*team, rowWithText(*team, u"Leaving Friday at 6"_s), MessageListModel::EditedRole).toBool());

        // unsent + unverified
        auto dave = loadedMessages(client.get(), &chats, Dave);
        QCOMPARE(dave->rowCount(), 3);
        QVERIFY(roleOf(*dave, 1, MessageListModel::UnsentRole).toBool());
        QVERIFY(!roleOf(*dave, 0, MessageListModel::VerifiedRole).toBool());
        QVERIFY(roleOf(*dave, 1, MessageListModel::DeliveryLabelRole).toString().isEmpty());

        // failed delivery
        auto eve = loadedMessages(client.get(), &chats, u"mailto:eve@example.org"_s);
        QCOMPARE(roleOf(*eve, 0, MessageListModel::DeliveryLabelRole).toString(), u"Not Delivered"_s);
        QCOMPARE(roleOf(*eve, 0, MessageListModel::UnsupportedRole).toStringList(), QStringList{u"app message"_s});
    }

    void unverifiedRunsAreGroups()
    {
        auto client = connectedClient();
        auto msgs = loadedMessages(client.get(), nullptr, Bob);
        // Bob, all within a minute: verified, unverified ×2, verified.
        const QStringList texts{u"v1"_s, u"u1"_s, u"u2"_s, u"v2"_s};
        const QList<bool> verified{true, false, false, true};
        for (int i = 0; i < texts.size(); ++i) {
            m_fake.control(u"InjectMessageVerified"_s, {Bob, Bob, texts.at(i), verified.at(i)});
            QTRY_COMPARE_WITH_TIMEOUT(roleOf(*msgs, 0, MessageListModel::TextRole).toString(), texts.at(i), Timeout);
        }
        auto first = [&](const QString &t) {
            return roleOf(*msgs, rowWithText(*msgs, t), MessageListModel::FirstInGroupRole).toBool();
        };
        auto last = [&](const QString &t) {
            return roleOf(*msgs, rowWithText(*msgs, t), MessageListModel::LastInGroupRole).toBool();
        };
        QVERIFY(last(u"v1"_s)); // the unverified run starts a new group…
        QVERIFY(first(u"u1"_s)); // …whose first bubble carries the badge
        QVERIFY(!last(u"u1"_s));
        QVERIFY(!first(u"u2"_s));
        QVERIFY(last(u"u2"_s));
        QVERIFY(first(u"v2"_s));
    }

    void liveMessageReceived()
    {
        auto client = connectedClient();
        ChatListModel chats(client.get());
        QTRY_COMPARE_WITH_TIMEOUT(chats.rowCount(), 4, Timeout);
        auto msgs = loadedMessages(client.get(), &chats, Bob);
        QAbstractItemModelTester tester(msgs.get());
        QSignalSpy added(msgs.get(), &MessageListModel::newestMessageAdded);
        const int before = msgs->rowCount();

        m_fake.control(u"InjectMessage"_s, {Bob, Bob, u"hello live"_s});
        QTRY_COMPARE_WITH_TIMEOUT(msgs->rowCount(), before + 1, Timeout);
        QCOMPARE(roleOf(*msgs, 0, MessageListModel::TextRole).toString(), u"hello live"_s);
        QCOMPARE(added.count(), 1);
        QVERIFY(!added.first().first().toBool());
        // grouped with the previous Bob message: tail moved to the new row
        QVERIFY(roleOf(*msgs, 0, MessageListModel::LastInGroupRole).toBool());
        QVERIFY(!roleOf(*msgs, 1, MessageListModel::LastInGroupRole).toBool());
        QTRY_COMPARE_WITH_TIMEOUT(roleOf(chats, 0, ChatListModel::SnippetRole).toString(), u"hello live"_s, Timeout);

        // a message in another chat moves it to the top and bumps unread
        m_fake.control(u"InjectMessage"_s, {Team, Carol, u"see you there"_s});
        QTRY_COMPARE_WITH_TIMEOUT(roleOf(chats, 0, ChatListModel::ChatIdRole).toString(), Team, Timeout);
        QTRY_COMPARE_WITH_TIMEOUT(roleOf(chats, 0, ChatListModel::UnreadCountRole).toInt(), 3, Timeout);
        QCOMPARE(roleOf(chats, 0, ChatListModel::SnippetRole).toString(), u"see you there"_s);
        QCOMPARE(msgs->rowCount(), before + 1); // not this chat
        QCOMPARE(chats.totalUnread(), 5);
    }

    void reactionsUpdate()
    {
        auto client = connectedClient();
        ChatListModel chats(client.get());
        QTRY_COMPARE_WITH_TIMEOUT(chats.rowCount(), 4, Timeout);
        auto msgs = loadedMessages(client.get(), &chats, Team);
        const int row = rowWithText(*msgs, u"I have a tent for 4"_s);
        QVERIFY(row >= 0);
        const QString id = roleOf(*msgs, row, MessageListModel::MessageIdRole).toString();
        QCOMPARE(reactionWithGlyph(*msgs, row, u"👍"_s).value(u"count"_s).toInt(), 2);
        QVERIFY(reactionWithGlyph(*msgs, row, u"🔥"_s).value(u"fromMe"_s).toBool());
        QCOMPARE(roleOf(*msgs, row, MessageListModel::MyReactionRole).toString(), u"emoji:🔥"_s);

        // Carol switches from like to love (one reaction per sender)
        m_fake.control(u"InjectReaction"_s, {id, u"love"_s, Carol});
        QTRY_COMPARE_WITH_TIMEOUT(reactionWithGlyph(*msgs, row, u"❤️"_s).value(u"count"_s).toInt(), 1, Timeout);
        QCOMPARE(reactionWithGlyph(*msgs, row, u"👍"_s).value(u"count"_s).toInt(), 1);

        // toggling our own 🔥 removes it
        msgs->react(id, u"emoji:🔥"_s);
        QTRY_VERIFY_WITH_TIMEOUT(reactionWithGlyph(*msgs, row, u"🔥"_s).isEmpty(), Timeout);
        const auto reacts = m_fake.callsOf(u"React"_s);
        QCOMPARE(reacts.size(), 1);
        QCOMPARE(reacts.first().at(3).toString(), u"emoji:🔥"_s);
        QCOMPARE(reacts.first().at(4).toBool(), true);

        // and a new tapback is added
        msgs->react(id, u"laugh"_s);
        QTRY_COMPARE_WITH_TIMEOUT(roleOf(*msgs, row, MessageListModel::MyReactionRole).toString(), u"laugh"_s, Timeout);
        QVERIFY(reactionWithGlyph(*msgs, row, u"😂"_s).value(u"fromMe"_s).toBool());
    }

    void typingClears()
    {
        auto client = connectedClient();
        ChatListModel chats(client.get());
        QTRY_COMPARE_WITH_TIMEOUT(chats.rowCount(), 4, Timeout);
        auto msgs = loadedMessages(client.get(), &chats, Bob);
        QSignalSpy typing(msgs.get(), &MessageListModel::typingChanged);

        m_fake.control(u"InjectTyping"_s, {Bob, Bob, true});
        QTRY_VERIFY_WITH_TIMEOUT(msgs->someoneTyping(), Timeout);
        QVERIFY(msgs->typingText().contains(u"Bob Smith"_s));
        QTRY_VERIFY_WITH_TIMEOUT(roleOf(chats, chats.indexOf(Bob), ChatListModel::TypingRole).toBool(), Timeout);

        // explicit stop
        m_fake.control(u"InjectTyping"_s, {Bob, Bob, false});
        QTRY_VERIFY_WITH_TIMEOUT(!msgs->someoneTyping(), Timeout);
        QTRY_VERIFY_WITH_TIMEOUT(!roleOf(chats, chats.indexOf(Bob), ChatListModel::TypingRole).toBool(), Timeout);

        // a message from the typist clears it too
        m_fake.control(u"InjectTyping"_s, {Bob, Bob, true});
        QTRY_VERIFY_WITH_TIMEOUT(msgs->someoneTyping(), Timeout);
        m_fake.control(u"InjectMessage"_s, {Bob, Bob, u"done typing"_s});
        QTRY_VERIFY_WITH_TIMEOUT(!msgs->someoneTyping(), Timeout);
        QTRY_VERIFY_WITH_TIMEOUT(!roleOf(chats, chats.indexOf(Bob), ChatListModel::TypingRole).toBool(), Timeout);
        QCOMPARE(typing.count(), 4);

        // typing in another chat does not leak into this one
        m_fake.control(u"InjectTyping"_s, {Team, Carol, true});
        QTRY_VERIFY_WITH_TIMEOUT(roleOf(chats, chats.indexOf(Team), ChatListModel::TypingRole).toBool(), Timeout);
        QVERIFY(!msgs->someoneTyping());

        // our own typing goes out
        msgs->setTyping(true);
        QTRY_COMPARE_WITH_TIMEOUT(m_fake.callsOf(u"SetTyping"_s).size(), 1, Timeout);
    }

    void sendEditUnsend()
    {
        auto client = connectedClient();
        ChatListModel chats(client.get());
        QTRY_COMPARE_WITH_TIMEOUT(chats.rowCount(), 4, Timeout);
        auto msgs = loadedMessages(client.get(), &chats, Bob);
        QSignalSpy sent(client.get(), &DaemonClient::sendFinished);
        const int before = msgs->rowCount();

        msgs->sendText(u"from the test"_s);
        QTRY_COMPARE_WITH_TIMEOUT(msgs->rowCount(), before + 1, Timeout);
        QTRY_COMPARE_WITH_TIMEOUT(sent.count(), 1, Timeout);
        QVERIFY(sent.first().at(0).toBool());
        QCOMPARE(sent.first().at(1).toString(), Bob);
        const QString id = sent.first().at(2).toString();
        QCOMPARE(roleOf(*msgs, 0, MessageListModel::MessageIdRole).toString(), id);
        QVERIFY(roleOf(*msgs, 0, MessageListModel::IsFromMeRole).toBool());
        QCOMPARE(roleOf(*msgs, 0, MessageListModel::DeliveryLabelRole).toString(), u"Sent"_s);
        // the label moved off the previous own message
        QVERIFY(roleOf(*msgs, rowWithText(*msgs, u"Yes! They look amazing 😍 https://example.com/trip"_s), MessageListModel::DeliveryLabelRole).toString().isEmpty());

        m_fake.control(u"SetDelivery"_s, {id, u"read"_s});
        QTRY_COMPARE_WITH_TIMEOUT(roleOf(*msgs, 0, MessageListModel::DeliveryLabelRole).toString(), u"Read"_s, Timeout);

        // reply_to is passed through
        msgs->sendText(u"replying"_s, id);
        QTRY_COMPARE_WITH_TIMEOUT(roleOf(*msgs, 0, MessageListModel::ReplyToRole).toString(), id, Timeout);
        QCOMPARE(roleOf(*msgs, 0, MessageListModel::ReplyToTextRole).toString(), u"from the test"_s);
        const QString replyId = roleOf(*msgs, 0, MessageListModel::MessageIdRole).toString();

        msgs->edit(id, u"edited text"_s);
        const int row = msgs->rowOf(id);
        QTRY_VERIFY_WITH_TIMEOUT(roleOf(*msgs, row, MessageListModel::EditedRole).toBool(), Timeout);
        QCOMPARE(roleOf(*msgs, row, MessageListModel::TextRole).toString(), u"edited text"_s);
        // the quoting reply follows the edit
        QCOMPARE(roleOf(*msgs, msgs->rowOf(replyId), MessageListModel::ReplyToTextRole).toString(), u"edited text"_s);

        msgs->unsend(replyId);
        QTRY_VERIFY_WITH_TIMEOUT(roleOf(*msgs, 0, MessageListModel::UnsentRole).toBool(), Timeout);
        QVERIFY(!roleOf(*msgs, 0, MessageListModel::HasTextRole).toBool());
        // delivery label falls back to the newest own message still standing
        QCOMPARE(roleOf(*msgs, row, MessageListModel::DeliveryLabelRole).toString(), u"Read"_s);
        QTRY_COMPARE_WITH_TIMEOUT(roleOf(chats, chats.indexOf(Bob), ChatListModel::SnippetRole).toString(), u"This message was unsent"_s, Timeout);

        // a failing call surfaces as operationFailed
        QSignalSpy failed(client.get(), &DaemonClient::operationFailed);
        client->edit(Bob, u"NO-SUCH-ID"_s, u"x"_s);
        QTRY_COMPARE_WITH_TIMEOUT(failed.count(), 1, Timeout);
        QCOMPARE(failed.first().at(1).toString(), u"NotFound"_s);
    }

    void sendToNewRecipient()
    {
        auto client = connectedClient();
        ChatListModel chats(client.get());
        QTRY_COMPARE_WITH_TIMEOUT(chats.rowCount(), 4, Timeout);
        QSignalSpy sent(client.get(), &DaemonClient::sendFinished);
        client->sendText(QString(), {u"new@example.com"_s}, u"hi there"_s);
        QTRY_COMPARE_WITH_TIMEOUT(sent.count(), 1, Timeout);
        QVERIFY(sent.first().at(0).toBool());
        QCOMPARE(sent.first().at(1).toString(), u"mailto:new@example.com"_s);
        QTRY_COMPARE_WITH_TIMEOUT(chats.rowCount(), 5, Timeout);
        QCOMPARE(roleOf(chats, 0, ChatListModel::ChatIdRole).toString(), u"mailto:new@example.com"_s);
        QTRY_COMPARE_WITH_TIMEOUT(roleOf(chats, 0, ChatListModel::SnippetRole).toString(), u"hi there"_s, Timeout);
        const auto calls = m_fake.callsOf(u"SendText"_s);
        QCOMPARE(calls.first().at(2).toString(), u"[\"new@example.com\"]"_s);

        // group of two creates a group chat
        client->sendText(QString(), {u"a@example.com"_s, u"b@example.com"_s}, u"group hello"_s);
        QTRY_COMPARE_WITH_TIMEOUT(sent.count(), 2, Timeout);
        QVERIFY(sent.at(1).at(1).toString().startsWith(u"group:"_s));
        QTRY_COMPARE_WITH_TIMEOUT(chats.rowCount(), 6, Timeout);

        // send failure is reported
        client->sendText(QString(), {u"not a handle"_s}, u"x"_s);
        QTRY_COMPARE_WITH_TIMEOUT(sent.count(), 3, Timeout);
        QVERIFY(!sent.at(2).at(0).toBool());
        QVERIFY(!sent.at(2).at(3).toString().isEmpty());
    }

    void attachmentDownload()
    {
        auto client = connectedClient();
        auto msgs = loadedMessages(client.get(), nullptr, Bob);
        const QString attId = roleOf(*msgs, 0, MessageListModel::AttachmentsRole).toList().first().toMap().value(u"id"_s).toString();
        QSignalSpy ready(msgs.get(), &MessageListModel::attachmentReady);
        msgs->downloadAttachment(attId);
        QVERIFY(roleOf(*msgs, 0, MessageListModel::AttachmentsRole).toList().first().toMap().value(u"downloading"_s).toBool());
        msgs->downloadAttachment(attId); // coalesced
        QTRY_COMPARE_WITH_TIMEOUT(ready.count(), 1, Timeout);
        const auto att = roleOf(*msgs, 0, MessageListModel::AttachmentsRole).toList().first().toMap();
        QVERIFY(!att.value(u"downloading"_s).toBool());
        QVERIFY(QFileInfo::exists(att.value(u"localPath"_s).toString()));
        QVERIFY(att.value(u"url"_s).toUrl().isLocalFile());
        QCOMPARE(m_fake.callsOf(u"DownloadAttachment"_s).size(), 1);

        // unknown attachment: error recorded, no crash
        m_fake.control(u"ClearCalls"_s);
        DaemonClient &c = *client;
        QSignalSpy err(&c, &DaemonClient::operationFailed);
        bool failed = false;
        c.downloadAttachment(u"NOPE:1"_s, [](const QString &) {}, [&](const QString &name, const QString &) {
            failed = name == u"NotFound"_s;
        });
        QTRY_VERIFY_WITH_TIMEOUT(failed, Timeout);
    }

    void sendFileSameNames()
    {
        // Two files called the same, both in flight: each message gets its own.
        QTemporaryDir a;
        QTemporaryDir b;
        QVERIFY(a.isValid() && b.isValid());
        const QString first = a.filePath(u"notes.txt"_s);
        const QString second = b.filePath(u"notes.txt"_s);
        for (const auto &[path, data] : {std::pair{first, QByteArray("one")}, std::pair{second, QByteArray("second file")}}) {
            QFile f(path);
            QVERIFY(f.open(QIODevice::WriteOnly));
            f.write(data);
        }
        auto client = connectedClient();
        auto msgs = loadedMessages(client.get(), nullptr, Bob);
        QSignalSpy sent(client.get(), &DaemonClient::sendFinished);
        m_fake.control(u"SetSendFileDelay"_s, {500u}); // both messages arrive before either reply
        msgs->sendFile(QUrl::fromLocalFile(first));
        msgs->sendFile(QUrl::fromLocalFile(second));
        auto notesRows = [&] {
            int n = 0;
            for (int row = 0; row < msgs->rowCount(); ++row) {
                const auto atts = roleOf(*msgs, row, MessageListModel::AttachmentsRole).toList();
                n += !atts.isEmpty() && atts.first().toMap().value(u"filename"_s).toString() == u"notes.txt"_s;
            }
            return n;
        };
        QTRY_COMPARE_WITH_TIMEOUT(notesRows(), 2, Timeout);
        QCOMPARE(sent.count(), 0);
        QTRY_COMPARE_WITH_TIMEOUT(sent.count(), 2, Timeout);
        QSet<QString> paths;
        for (int row = 0; row < 2; ++row) {
            const auto att = roleOf(*msgs, row, MessageListModel::AttachmentsRole).toList().first().toMap();
            const QString path = att.value(u"localPath"_s).toString();
            QCOMPARE(QFileInfo(path).size(), att.value(u"size"_s).toLongLong());
            paths << path;
        }
        QCOMPARE(paths, (QSet<QString>{first, second}));
    }

    void sendFileStaging()
    {
        using namespace km::Outgoing;
        // Where the sandboxed daemon can (not) look.
        QVERIFY(!needsStaging(QDir::homePath() + u"/Pictures/a.png"_s));
        QVERIFY(needsStaging(u"/tmp/a.png"_s));
        QVERIFY(needsStaging(u"/run/user/1000/doc/1234/a.png"_s));
        QVERIFY(needsStaging(u"/usr/share/pixmaps/a.png"_s));
        QVERIFY(!needsStaging(stagingDir() + u"/x/a.png"_s));

        QTemporaryDir outside; // under /tmp: invisible to the daemon
        QVERIFY(outside.isValid());
        const QString original = outside.filePath(u"photo.png"_s);
        QImage img(64, 48, QImage::Format_RGB32);
        img.fill(Qt::darkCyan);
        QVERIFY(img.save(original));
        m_fake.control(u"SetUnreadablePrefixes"_s, {QStringList{outside.path()}});

        auto client = connectedClient();
        auto msgs = loadedMessages(client.get(), nullptr, Bob);
        QSignalSpy sent(client.get(), &DaemonClient::sendFinished);
        m_fake.control(u"ClearCalls"_s);
        msgs->sendFile(QUrl::fromLocalFile(original));
        QTRY_COMPARE_WITH_TIMEOUT(sent.count(), 1, Timeout);
        QVERIFY2(sent.first().at(0).toBool(), qPrintable(sent.first().at(3).toString()));
        const QString sentPath = m_fake.callsOf(u"SendFile"_s).first().at(3).toString();
        QVERIFY2(sentPath.startsWith(stagingDir() + u'/') && sentPath.endsWith(u"/photo.png"_s), qPrintable(sentPath));
        QVERIFY(!QFileInfo::exists(sentPath)); // the copy is gone once sent…
        QVERIFY(QFileInfo::exists(original)); // …the original is untouched
        QCOMPARE(QDir(stagingDir()).entryList(QDir::Dirs | QDir::NoDotAndDotDot), QStringList());

        // Own image: shown from the local file at once, no download.
        const QString id = sent.first().at(2).toString();
        QCOMPARE(roleOf(*msgs, 0, MessageListModel::MessageIdRole).toString(), id);
        const auto att = roleOf(*msgs, 0, MessageListModel::AttachmentsRole).toList().first().toMap();
        QCOMPARE(att.value(u"id"_s).toString(), id + u":0"_s);
        QCOMPARE(att.value(u"localPath"_s).toString(), original);
        QVERIFY(m_fake.callsOf(u"DownloadAttachment"_s).isEmpty());

        // A path the daemon cannot read anyway: a readable message, no leftovers.
        m_fake.control(u"SetUnreadablePrefixes"_s, {QStringList{stagingDir()}});
        msgs->sendFile(QUrl::fromLocalFile(original));
        QTRY_COMPARE_WITH_TIMEOUT(sent.count(), 2, Timeout);
        QVERIFY(!sent.at(1).at(0).toBool());
        QVERIFY2(sent.at(1).at(3).toString().startsWith(u"The messaging service cannot read that folder"_s), qPrintable(sent.at(1).at(3).toString()));
        QCOMPARE(QDir(stagingDir()).entryList(QDir::Dirs | QDir::NoDotAndDotDot), QStringList());

        // A file that does not exist fails before calling the daemon.
        m_fake.control(u"ClearCalls"_s);
        msgs->sendFile(QUrl::fromLocalFile(outside.filePath(u"missing.png"_s)));
        QTRY_COMPARE_WITH_TIMEOUT(sent.count(), 3, Timeout);
        QVERIFY(!sent.at(2).at(0).toBool());
        QVERIFY(m_fake.callsOf(u"SendFile"_s).isEmpty());

        // Clipboard images are saved there and sent without another copy.
        const QString pasted = saveImage(img, u"Pasted image"_s);
        QVERIFY(pasted.startsWith(stagingDir() + u'/'));
        QVERIFY(!needsStaging(pasted));
        QVERIFY(QImage(pasted).size() == img.size());
        purge(-1); // everything is older than "in a second"
        QVERIFY(!QFileInfo::exists(pasted));

        // A symlinked "staging dir" is skipped, its target left alone.
        QTemporaryDir target;
        QVERIFY(target.isValid());
        QFile keep(target.filePath(u"keep.txt"_s));
        QVERIFY(keep.open(QIODevice::WriteOnly));
        keep.write("precious");
        keep.close();
        QVERIFY(QDir().mkpath(stagingDir()));
        const QString link = stagingDir() + u"/planted"_s;
        QVERIFY(QFile::link(target.path(), link));
        purge(-1);
        QVERIFY(QFileInfo::exists(target.filePath(u"keep.txt"_s)));
        QVERIFY(QFileInfo(link).isSymLink());
        QFile::remove(link);
    }

    void attachmentDownloadTimeout()
    {
        auto client = connectedClient();
        auto msgs = loadedMessages(client.get(), nullptr, Bob);
        msgs->setDownloadRetryDelay(300);
        const QString attId = roleOf(*msgs, 0, MessageListModel::AttachmentsRole).toList().first().toMap().value(u"id"_s).toString();
        auto att = [&] {
            return roleOf(*msgs, 0, MessageListModel::AttachmentsRole).toList().first().toMap();
        };
        // One timeout: still "downloading", one retry, then the file.
        m_fake.control(u"FailDownloadsWithNoReply"_s, {1u});
        QSignalSpy ready(msgs.get(), &MessageListModel::attachmentReady);
        msgs->downloadAttachment(attId);
        QTRY_COMPARE_WITH_TIMEOUT(m_fake.callsOf(u"DownloadAttachment"_s).size(), 1, Timeout);
        QTest::qWait(100);
        QVERIFY(att().value(u"downloading"_s).toBool());
        QVERIFY(att().value(u"error"_s).toString().isEmpty());
        QTRY_COMPARE_WITH_TIMEOUT(ready.count(), 1, Timeout);
        QCOMPARE(m_fake.callsOf(u"DownloadAttachment"_s).size(), 2);

        // Two timeouts in a row: failed, with the error for the tooltip.
        auto other = loadedMessages(client.get(), nullptr, Team);
        other->setDownloadRetryDelay(300);
        const int row = rowWithText(*other, u"Here's the packing list"_s);
        const QString pdf = roleOf(*other, row, MessageListModel::AttachmentsRole).toList().first().toMap().value(u"id"_s).toString();
        m_fake.control(u"ClearCalls"_s);
        m_fake.control(u"FailDownloadsWithNoReply"_s, {2u});
        other->downloadAttachment(pdf);
        QTRY_VERIFY_WITH_TIMEOUT(!roleOf(*other, row, MessageListModel::AttachmentsRole).toList().first().toMap().value(u"error"_s).toString().isEmpty(), Timeout);
        QCOMPARE(m_fake.callsOf(u"DownloadAttachment"_s).size(), 2);
        QVERIFY(!roleOf(*other, row, MessageListModel::AttachmentsRole).toList().first().toMap().value(u"downloading"_s).toBool());
    }

    void handleLookup()
    {
        auto client = connectedClient();
        HandleLookupModel model(client.get());
        QAbstractItemModelTester tester(&model);
        QVERIFY(!model.canSend());

        QVERIFY(model.add(u"Bob@Example.com"_s));
        QVERIFY(model.isChecking());
        QTRY_COMPARE_WITH_TIMEOUT(roleOf(model, 0, HandleLookupModel::StateNameRole).toString(), u"reachable"_s, Timeout);
        QCOMPARE(roleOf(model, 0, HandleLookupModel::HandleRole).toString(), u"mailto:bob@example.com"_s);
        QVERIFY(model.canSend());
        QCOMPARE(model.handles(), QStringList{u"mailto:bob@example.com"_s});

        QVERIFY(!model.add(u"bob@example.com"_s)); // duplicate

        QVERIFY(model.add(u"android@example.com"_s));
        QTRY_COMPARE_WITH_TIMEOUT(roleOf(model, 1, HandleLookupModel::StateNameRole).toString(), u"unreachable"_s, Timeout);
        QVERIFY(!model.canSend());
        QVERIFY(model.problem().contains(u"android@example.com"_s));

        QVERIFY(model.add(u"bob"_s)); // invalid locally, never sent to the daemon
        QCOMPARE(roleOf(model, 2, HandleLookupModel::StateNameRole).toString(), u"invalid"_s);
        model.removeLast();
        model.remove(1);
        QVERIFY(model.canSend());

        QVERIFY(model.add(u"carol@example.com; +1 555 123 4567"_s));
        QCOMPARE(model.rowCount(), 3);
        QTRY_VERIFY_WITH_TIMEOUT(model.canSend(), Timeout);
        QCOMPARE(model.handles().last(), u"tel:+15551234567"_s);
        QCOMPARE(m_fake.callsOf(u"LookupHandles"_s).size(), 4);
        model.clear();
        QCOMPARE(model.rowCount(), 0);
    }

    void smsForwarding()
    {
        auto client = connectedClient();
        QCOMPARE(client->smsState(), u"off"_s);
        QVERIFY(!client->isSmsActive());
        QSignalSpy finished(client.get(), &DaemonClient::smsForwardingFinished);
        client->enableSmsForwarding(true);
        QTRY_COMPARE_WITH_TIMEOUT(finished.count(), 1, Timeout);
        QVERIFY(finished.first().at(0).toBool());
        QCOMPARE(client->smsState(), u"requested"_s);
        QCOMPARE(client->smsNumber(), u"tel:+15555550100"_s);
        QCOMPARE(m_fake.callsOf(u"EnableSmsForwarding"_s).size(), 1);

        // a green chat with a number that is not on iMessage
        const QString sam = m_fake.control(u"AddSmsChat"_s, {u"+1 555 555 0142"_s, u"Sam"_s}).arguments().value(0).toString();
        QCOMPARE(sam, u"tel:+15555550142"_s);
        ChatListModel chats(client.get());
        auto msgs = loadedMessages(client.get(), &chats, sam);
        QCOMPARE(msgs->rowCount(), 4);
        QVERIFY(roleOf(*msgs, 0, MessageListModel::IsSmsRole).toBool());
        QVERIFY(msgs->phoneRecipients() == QStringList{sam});
        QVERIFY(!msgs->smsMode()); // forwarding only requested
        QSignalSpy mode(msgs.get(), &MessageListModel::smsModeChanged);
        // the iPhone confirmed (signal only, like the daemon)
        m_fake.control(u"SetSms"_s, {u"active"_s, u"tel:+15555550100"_s});
        QTRY_VERIFY_WITH_TIMEOUT(client->isSmsActive(), Timeout);
        QTRY_VERIFY_WITH_TIMEOUT(msgs->smsMode(), Timeout);
        QVERIFY(mode.count() >= 1);
        // no tapbacks, no typing indicator in a text-message chat
        m_fake.control(u"ClearCalls"_s);
        msgs->react(roleOf(*msgs, 0, MessageListModel::MessageIdRole).toString(), u"love"_s);
        msgs->setTyping(true);
        QTest::qWait(200);
        QVERIFY(m_fake.callsOf(u"React"_s).isEmpty());
        QVERIFY(m_fake.callsOf(u"SetTyping"_s).isEmpty());
        // an email chat is never a text-message chat
        auto bob = loadedMessages(client.get(), &chats, Bob);
        QVERIFY(bob->phoneRecipients().isEmpty());
        QVERIFY(!bob->smsMode());
        QVERIFY(!roleOf(*bob, 0, MessageListModel::IsSmsRole).toBool());

        // new message: an unreachable number can be texted now
        HandleLookupModel lookup(client.get());
        QVERIFY(lookup.add(u"+1 555 555 0142"_s));
        QTRY_COMPARE_WITH_TIMEOUT(roleOf(lookup, 0, HandleLookupModel::StateNameRole).toString(), u"unreachable"_s, Timeout);
        QVERIFY(lookup.canSend());
        QVERIFY(lookup.smsMode());
        QVERIFY(lookup.problem().isEmpty());
        QVERIFY(lookup.add(u"android@example.com"_s)); // email: still not sendable
        QTRY_COMPARE_WITH_TIMEOUT(roleOf(lookup, 1, HandleLookupModel::StateNameRole).toString(), u"unreachable"_s, Timeout);
        QVERIFY(!lookup.canSend());
        lookup.removeLast();

        // sending in the green chat goes out as a text
        msgs->sendText(u"see you there"_s);
        QTRY_VERIFY_WITH_TIMEOUT(msgs->rowCount() == 5, Timeout);
        QVERIFY(roleOf(*msgs, 0, MessageListModel::IsSmsRole).toBool());

        client->enableSmsForwarding(false);
        QTRY_COMPARE_WITH_TIMEOUT(client->smsState(), u"off"_s, Timeout);
        QTRY_VERIFY_WITH_TIMEOUT(!msgs->smsMode(), Timeout);
        QVERIFY(!lookup.canSend());
    }

    void markRead()
    {
        auto client = connectedClient();
        ChatListModel chats(client.get());
        QTRY_COMPARE_WITH_TIMEOUT(chats.rowCount(), 4, Timeout);
        QSignalSpy unread(&chats, &ChatListModel::unreadChanged);
        QCOMPARE(chats.unreadFor(Team), 2);
        client->markRead(Team);
        QTRY_COMPARE_WITH_TIMEOUT(chats.unreadFor(Team), 0, Timeout);
        QCOMPARE(chats.totalUnread(), 1);
        QCOMPARE(unread.count(), 1);
        QCOMPARE(m_fake.callsOf(u"MarkRead"_s).first().at(2).toString(), QString());
    }

    void otherOwnHandles()
    {
        // "My other addresses" setting → IDS URIs
        AppSettings settings(KSharedConfig::openConfig(u"kmessages-otherhandlesrc"_s));
        QSignalSpy changed(&settings, &AppSettings::changed);
        settings.setOtherHandles(u" (555) 000-1111 , Me@iCloud.com, not a handle,"_s);
        QCOMPARE(changed.count(), 1);
        QCOMPARE(settings.otherHandleUris(), (QStringList{u"tel:5550001111"_s, u"mailto:me@icloud.com"_s}));

        auto client = connectedClient();
        QSignalSpy handlesChanged(client.get(), &DaemonClient::otherOwnHandlesChanged);
        client->setOtherOwnHandles(settings.otherHandleUris());
        QCOMPARE(handlesChanged.count(), 1);
        QVERIFY(client->isOtherOwnHandle(u"tel:+15550001111"_s)); // national number matches +1…
        QVERIFY(client->isOtherOwnHandle(u"mailto:me@icloud.com"_s));
        QVERIFY(!client->isOtherOwnHandle(u"tel:+15550001112"_s));
        QVERIFY(!client->isOwnHandle(u"tel:+15550001111"_s)); // not registered: no tapback ownership
        ChatListModel chats(client.get());
        QTRY_COMPARE_WITH_TIMEOUT(chats.rowCount(), 4, Timeout);
        // A message the user wrote on their iPhone counts as theirs in the chat list.
        m_fake.control(u"InjectMessage"_s, {Dave, u"tel:+15550001111"_s, u"sent from my phone"_s});
        QTRY_COMPARE_WITH_TIMEOUT(roleOf(chats, 0, ChatListModel::SnippetRole).toString(), u"sent from my phone"_s, Timeout);
        QVERIFY(roleOf(chats, 0, ChatListModel::LastMessageFromMeRole).toBool());
        m_fake.control(u"InjectMessage"_s, {Dave, Dave, u"got it"_s});
        QTRY_COMPARE_WITH_TIMEOUT(roleOf(chats, 0, ChatListModel::SnippetRole).toString(), u"got it"_s, Timeout);
        QVERIFY(!roleOf(chats, 0, ChatListModel::LastMessageFromMeRole).toBool());

        // The daemon's own list (registration.other_handles) counts too, and is
        // kept apart from the setting.
        QVERIFY(client->daemonOtherHandles().isEmpty());
        QVERIFY(!client->isOtherOwnHandle(u"tel:+447911123456"_s));
        m_fake.control(u"SetOtherHandles"_s, {QStringList{u"tel:+447911123456"_s}});
        QSignalSpy status(client.get(), &DaemonClient::statusChanged);
        client->refreshStatus();
        QTRY_COMPARE_WITH_TIMEOUT(client->daemonOtherHandles(), QStringList{u"tel:+447911123456"_s}, Timeout);
        QVERIFY(client->isOtherOwnHandle(u"tel:+447911123456"_s));
        QVERIFY(client->isOtherOwnHandle(u"tel:+15550001111"_s)); // the setting still counts
        QVERIFY(!client->isOtherOwnHandle(u"tel:+17911123456"_s)); // same last ten digits, other country
        QCOMPARE(client->otherOwnHandles(), (QStringList{u"tel:5550001111"_s, u"mailto:me@icloud.com"_s}));
        client->setOtherOwnHandles({});
        QVERIFY(client->isOtherOwnHandle(u"tel:+447911123456"_s));
        QVERIFY(!client->isOtherOwnHandle(u"mailto:me@icloud.com"_s));
    }

    void sameHandle_data()
    {
        QTest::addColumn<QString>("a");
        QTest::addColumn<QString>("b");
        QTest::addColumn<bool>("same");
        QTest::newRow("identical") << u"tel:+15550001111"_s << u"tel:+15550001111"_s << true;
        QTest::newRow("bare national vs +1") << u"tel:5550001111"_s << u"tel:+15550001111"_s << true;
        QTest::newRow("+1 vs bare national") << u"tel:+15550001111"_s << u"tel:5550001111"_s << true;
        QTest::newRow("UK vs US, same last ten") << u"tel:+447911123456"_s << u"tel:+17911123456"_s << false;
        QTest::newRow("bare UK with trunk 0") << u"tel:07911123456"_s << u"tel:+447911123456"_s << true;
        QTest::newRow("different numbers") << u"tel:+15550001111"_s << u"tel:+15550001112"_s << false;
        QTest::newRow("short codes") << u"tel:12345"_s << u"tel:+112345"_s << false;
        QTest::newRow("email case") << u"mailto:Me@iCloud.com"_s << u"mailto:me@icloud.com"_s << true;
        QTest::newRow("email vs phone") << u"mailto:5550001111"_s << u"tel:5550001111"_s << false;
    }

    void sameHandle()
    {
        QFETCH(QString, a);
        QFETCH(QString, b);
        QFETCH(bool, same);
        QCOMPARE(DaemonClient::sameHandle(a, b), same);
    }

    void search()
    {
        auto client = connectedClient();
        ChatListModel chats(client.get());
        QTRY_COMPARE_WITH_TIMEOUT(chats.rowCount(), 4, Timeout);
        MessageSearchModel search(client.get(), &chats);
        search.setQuery(u"tent"_s);
        QTRY_COMPARE_WITH_TIMEOUT(search.rowCount(), 2, Timeout);
        QCOMPARE(roleOf(search, 0, MessageSearchModel::ChatNameRole).toString(), u"Weekend Trip"_s);
        search.setQuery(u"t"_s); // too short: cleared without a call
        QCOMPARE(search.rowCount(), 0);
    }

    void daemonRestart()
    {
        auto client = connectedClient();
        ChatListModel chats(client.get());
        QTRY_COMPARE_WITH_TIMEOUT(chats.rowCount(), 4, Timeout);
        auto msgs = loadedMessages(client.get(), &chats, Bob);

        m_fake.stop();
        QTRY_VERIFY_WITH_TIMEOUT(!client->isAvailable(), Timeout);
        QCOMPARE(client->connectionState(), u"unavailable"_s);

        QVERIFY(m_fake.start({u"--state"_s, u"needs_login"_s, u"--empty"_s}));
        QTRY_VERIFY_WITH_TIMEOUT(client->isAvailable(), Timeout);
        QTRY_COMPARE_WITH_TIMEOUT(client->connectionState(), u"needs_login"_s, Timeout);
        QTRY_COMPARE_WITH_TIMEOUT(chats.rowCount(), 0, Timeout); // resynced
        QTRY_VERIFY_WITH_TIMEOUT(client->ownHandles().isEmpty(), Timeout);
    }

    void propertiesChangedOnly()
    {
        auto client = connectedClient();
        QVERIFY(client->isStatusKnown());
        // ConnectionState arriving only as a PropertiesChanged value …
        m_fake.control(u"SetStateViaProperties"_s, {u"registering"_s, false});
        QTRY_COMPARE_WITH_TIMEOUT(client->connectionState(), u"registering"_s, Timeout);
        // … or only as an invalidated property name.
        m_fake.control(u"SetStateViaProperties"_s, {u"connected"_s, true});
        QTRY_COMPARE_WITH_TIMEOUT(client->connectionState(), u"connected"_s, Timeout);
        QVERIFY(client->isRegistered());

        // status is unknown again while the daemon is gone
        m_fake.stop();
        QTRY_VERIFY_WITH_TIMEOUT(!client->isAvailable(), Timeout);
        QVERIFY(!client->isStatusKnown());
    }

    void ownHandlesUnchangedNoRefresh()
    {
        auto client = connectedClient();
        ChatListModel chats(client.get());
        QTRY_COMPARE_WITH_TIMEOUT(chats.rowCount(), 4, Timeout);
        auto msgs = loadedMessages(client.get(), &chats, Bob);
        QSignalSpy changed(msgs.get(), &QAbstractItemModel::dataChanged);
        QSignalSpy status(client.get(), &DaemonClient::statusChanged);
        client->refreshStatus();
        QTRY_COMPARE_WITH_TIMEOUT(status.count(), 1, Timeout);
        QTest::qWait(100);
        QCOMPARE(changed.count(), 0); // same handles: no row refresh
    }

    void refreshCoalescedAndSnippetsCached()
    {
        auto client = connectedClient();
        ChatListModel chats(client.get());
        QTRY_COMPARE_WITH_TIMEOUT(chats.rowCount(), 4, Timeout);
        QTRY_VERIFY_WITH_TIMEOUT(!roleOf(chats, 3, ChatListModel::SnippetRole).toString().isEmpty(), Timeout);
        QTest::qWait(300);

        // A resync with unchanged chats reloads the list but not the snippets.
        m_fake.control(u"ClearCalls"_s);
        m_fake.control(u"EmitBackgroundError"_s, {u"events dropped; clients should resync"_s});
        QTRY_COMPARE_WITH_TIMEOUT(m_fake.callsOf(u"ListChats"_s).size(), 1, Timeout);
        QTest::qWait(600);
        QCOMPARE(m_fake.callsOf(u"ListChats"_s).size(), 1);
        QCOMPARE(m_fake.callsOf(u"ListMessages"_s).size(), 0);
        QCOMPARE(roleOf(chats, 0, ChatListModel::SnippetRole).toString(), u"Here's the best one"_s);

        // A changed chat gets its snippet re-read.
        m_fake.control(u"InjectMessage"_s, {Team, Carol, u"new activity"_s});
        QTRY_COMPARE_WITH_TIMEOUT(roleOf(chats, 0, ChatListModel::ChatIdRole).toString(), Team, Timeout);

        // Daemon (re)appearance: resyncRequested + connectionStateChanged(connected)
        // coalesce into one ListChats.
        m_fake.stop();
        QTRY_VERIFY_WITH_TIMEOUT(!client->isAvailable(), Timeout);
        QVERIFY(m_fake.start());
        QTRY_COMPARE_WITH_TIMEOUT(client->connectionState(), u"connected"_s, Timeout);
        QTRY_COMPARE_WITH_TIMEOUT(m_fake.callsOf(u"ListChats"_s).size(), 1, Timeout);
        QTest::qWait(800);
        QCOMPARE(m_fake.callsOf(u"ListChats"_s).size(), 1);
    }

    void revisionTracksChanges()
    {
        auto client = connectedClient();
        ChatListModel chats(client.get());
        QTRY_COMPARE_WITH_TIMEOUT(chats.rowCount(), 4, Timeout);
        QSignalSpy revision(&chats, &ChatListModel::revisionChanged);
        const int before = chats.revision();
        client->markRead(Team); // ChatUpdated: no row count change
        QTRY_COMPARE_WITH_TIMEOUT(chats.unreadFor(Team), 0, Timeout);
        QVERIFY(chats.revision() > before);
        QVERIFY(revision.count() >= 1);
    }

    void failedSendAndEditReported()
    {
        auto client = connectedClient();
        ChatListModel chats(client.get());
        QTRY_COMPARE_WITH_TIMEOUT(chats.rowCount(), 4, Timeout);
        auto msgs = loadedMessages(client.get(), &chats, Bob);
        QSignalSpy sendFailed(msgs.get(), &MessageListModel::sendTextFailed);
        QSignalSpy editFailed(msgs.get(), &MessageListModel::editFailed);
        QSignalSpy sent(client.get(), &DaemonClient::sendFinished);

        const QString quoted = roleOf(*msgs, 1, MessageListModel::MessageIdRole).toString();
        msgs->sendText(u"__fail__"_s, quoted);
        QTRY_COMPARE_WITH_TIMEOUT(sendFailed.count(), 1, Timeout);
        QCOMPARE(sendFailed.first().at(0).toString(), Bob);
        QCOMPARE(sendFailed.first().at(1).toString(), u"__fail__"_s);
        QCOMPARE(sendFailed.first().at(2).toString(), quoted);
        QCOMPARE(sendFailed.first().at(3).toString(), u"fake send failure"_s);
        QCOMPARE(sent.count(), 1); // the app-wide signal still fires
        QVERIFY(!sent.first().at(0).toBool());

        // Bob's message is not ours: Edit fails
        const QString theirs = roleOf(*msgs, 0, MessageListModel::MessageIdRole).toString();
        msgs->edit(theirs, u"nope"_s);
        QTRY_COMPARE_WITH_TIMEOUT(editFailed.count(), 1, Timeout);
        QCOMPARE(editFailed.first().at(1).toString(), theirs);
        QCOMPARE(editFailed.first().at(2).toString(), u"nope"_s);

        // a successful send reports nothing
        msgs->sendText(u"fine"_s);
        QTRY_COMPARE_WITH_TIMEOUT(sent.count(), 2, Timeout);
        QCOMPARE(sendFailed.count(), 1);
    }

    void reactWaitsForStatus()
    {
        // Before the first GetStatus our handles are unknown: react() must not
        // guess "add" for a tapback that is already ours.
        m_fake.control(u"SetStatusDelay"_s, {1500u});
        DaemonClient client;
        MessageListModel msgs(&client);
        msgs.setChatId(Bob);
        QTRY_VERIFY_WITH_TIMEOUT(client.isAvailable(), Timeout);
        msgs.reload(); // setChatId ran before the daemon was seen
        QTRY_VERIFY_WITH_TIMEOUT(msgs.isLoaded() && msgs.rowCount() > 0, Timeout);
        QVERIFY(!client.isStatusKnown());
        QCOMPARE(roleOf(msgs, 0, MessageListModel::MyReactionRole).toString(), QString()); // unknown yet
        const QString id = roleOf(msgs, 0, MessageListModel::MessageIdRole).toString();
        msgs.react(id, u"love"_s);
        QTest::qWait(200);
        QCOMPARE(m_fake.callsOf(u"React"_s).size(), 0); // waits for the status
        QTRY_VERIFY_WITH_TIMEOUT(client.isStatusKnown(), Timeout);
        QTRY_COMPARE_WITH_TIMEOUT(m_fake.callsOf(u"React"_s).size(), 1, Timeout);
        QCOMPARE(m_fake.callsOf(u"React"_s).first().at(4).toBool(), true); // ours already: a removal
    }

    void linkifyPunctuation()
    {
        auto client = connectedClient();
        ChatListModel chats(client.get());
        QTRY_COMPARE_WITH_TIMEOUT(chats.rowCount(), 4, Timeout);
        auto msgs = loadedMessages(client.get(), &chats, Bob);
        m_fake.control(u"InjectMessage"_s,
                       {Bob, Bob, u"see https://en.wikipedia.org/wiki/Foo_(bar)), (https://x.org/y). \"https://q.org/z\"; https://a.org/b, ok"_s});
        QTRY_COMPARE_WITH_TIMEOUT(roleOf(*msgs, 0, MessageListModel::TextRole).toString().left(3), u"see"_s, Timeout);
        const QString html = roleOf(*msgs, 0, MessageListModel::RichTextRole).toString();
        QVERIFY2(html.contains(u"<a href=\"https://en.wikipedia.org/wiki/Foo_(bar)\">https://en.wikipedia.org/wiki/Foo_(bar)</a>), "_s), qPrintable(html));
        QVERIFY2(html.contains(u"(<a href=\"https://x.org/y\">https://x.org/y</a>). "_s), qPrintable(html));
        QVERIFY2(html.contains(u"&quot;<a href=\"https://q.org/z\">https://q.org/z</a>&quot;; "_s), qPrintable(html));
        QVERIFY2(html.contains(u"<a href=\"https://a.org/b\">https://a.org/b</a>, ok"_s), qPrintable(html));
    }

    void loginFlow()
    {
        m_fake.stop();
        QVERIFY(m_fake.start({u"--state"_s, u"needs_login"_s, u"--empty"_s}));
        DaemonClient client;
        QTRY_COMPARE_WITH_TIMEOUT(client.connectionState(), u"needs_login"_s, Timeout);
        QSignalSpy login(&client, &DaemonClient::loginFinished);
        QSignalSpy twoFa(&client, &DaemonClient::twoFactorFinished);
        QSignalSpy required(&client, &DaemonClient::twoFactorRequired);

        client.login(u"me@example.com"_s, u"wrong"_s);
        QTRY_COMPARE_WITH_TIMEOUT(login.count(), 1, Timeout);
        QVERIFY(login.at(0).at(0).toString().isEmpty());
        QCOMPARE(login.at(0).at(1).toString(), u"Login"_s);

        client.login(u"2fa@example.com"_s, u"secret"_s);
        QTRY_COMPARE_WITH_TIMEOUT(login.count(), 2, Timeout);
        QCOMPARE(login.at(1).at(0).toString(), u"two_factor_required"_s);
        QTRY_COMPARE_WITH_TIMEOUT(required.count(), 1, Timeout);
        QTRY_COMPARE_WITH_TIMEOUT(client.connectionState(), u"needs_two_factor"_s, Timeout);

        client.submitTwoFactor(u"000000"_s);
        QTRY_COMPARE_WITH_TIMEOUT(twoFa.count(), 1, Timeout);
        QCOMPARE(twoFa.at(0).at(1).toString(), u"BadTwoFactorCode"_s);

        client.submitTwoFactor(u"123456"_s);
        QTRY_COMPARE_WITH_TIMEOUT(twoFa.count(), 2, Timeout);
        QCOMPARE(twoFa.at(1).at(0).toString(), u"ok"_s);
        QTRY_COMPARE_WITH_TIMEOUT(client.connectionState(), u"connected"_s, Timeout);
        QTRY_VERIFY_WITH_TIMEOUT(client.isRegistered(), Timeout);
        // the password never shows up in the fake's call log
        const auto calls = m_fake.callsOf(u"Login"_s);
        for (const auto &c : calls) {
            QVERIFY(!QString::fromUtf8(QJsonDocument(c).toJson()).contains(u"secret"_s));
        }
    }
};

int main(int argc, char **argv)
{
    useTemporaryXdgDirs();
    QCoreApplication app(argc, argv);
    TestModels tc;
    QTEST_SET_MAIN_SOURCE_PATH
    return QTest::qExec(&tc, argc, argv);
}
#include "tst_models.moc"
