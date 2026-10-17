// SPDX-License-Identifier: SSPL-1.0
//
// ContactResolver: handle normalization, names and photos from KPeople
// (fixture vCards in a temporary XDG_DATA_HOME), the image provider, and
// the models picking contact names up (fake daemon on a private bus).
#include "chatlistmodel.h"
#include "contactresolver.h"
#include "contactsuggestionmodel.h"
#include "daemonclient.h"
#include "fakedaemon.h"
#include "messagelistmodel.h"

#include <QDir>
#include <QFile>
#include <QGuiApplication>
#include <QJsonArray>
#include <QJsonDocument>
#include <QSignalSpy>
#include <QStandardPaths>
#include <QTemporaryDir>
#include <QTest>
#include <QUrl>

using namespace Qt::StringLiterals;
using namespace km;

namespace
{
constexpr int Timeout = 10000;

QImage solid(QColor c, int size = 8)
{
    QImage img(size, size, QImage::Format_RGB32);
    img.fill(c);
    return img;
}

/// The fake daemon's people (tests/fake_daemon.py), as address-book entries.
QList<ContactResolver::Person> fakeDaemonPeople()
{
    return {
        {u"1"_s, u"Robert Smith"_s, u"Robert"_s, {u"BOB@example.com"_s}, {}, solid(Qt::red)},
        {u"2"_s, u"Carol Jones"_s, {}, {u"carol@example.com"_s}, {}, {}},
        {u"3"_s, u"Dave Miller"_s, u"Dave"_s, {}, {u"(555) 123-4567"_s}, solid(Qt::blue)},
    };
}
}

class TestContacts : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void keys()
    {
        QCOMPARE(ContactResolver::keysFor(u"mailto:Bob@Example.com"_s), QStringList{u"e:bob@example.com"_s});
        QCOMPARE(ContactResolver::keysFor(u" Bob@Example.com "_s), QStringList{u"e:bob@example.com"_s});
        QCOMPARE(ContactResolver::keysFor(u"tel:+12055550100"_s), (QStringList{u"p:12055550100"_s, u"t:2055550100"_s}));
        QCOMPARE(ContactResolver::keysFor(u"(205) 555-0100"_s), (QStringList{u"p:2055550100"_s, u"t:2055550100"_s}));
        QCOMPARE(ContactResolver::keysFor(u"tel:12345"_s), QStringList{u"p:12345"_s});
        // Other country codes: all digits only (no last-ten key to collide with +1 …).
        QCOMPARE(ContactResolver::keysFor(u"tel:+447911123456"_s), QStringList{u"p:447911123456"_s});
        QCOMPARE(ContactResolver::keysFor(u"+1 791-112-3456"_s), (QStringList{u"p:17911123456"_s, u"t:7911123456"_s}));
        QVERIFY(ContactResolver::keysFor(u"no address"_s).isEmpty());

        QCOMPARE(ContactResolver::uriForAddress(u"(205) 555-0100"_s), u"tel:+12055550100"_s);
        QCOMPARE(ContactResolver::uriForAddress(u"1-205-555-0100"_s), u"tel:+12055550100"_s);
        QCOMPARE(ContactResolver::uriForAddress(u"+44 20 7946 0000"_s), u"tel:+442079460000"_s);
        QCOMPARE(ContactResolver::uriForAddress(u"Bob@Example.com"_s), u"mailto:bob@example.com"_s);
        QVERIFY(ContactResolver::uriForAddress(u"not a number"_s).isEmpty());
    }

    void phoneCountryCodes()
    {
        // Same last ten digits, different country: not each other.
        ContactResolver r(false);
        r.setPeople({{u"9"_s, u"Uma UK"_s, {}, {}, {u"+44 7911 123456"_s}, {}}, {u"8"_s, u"Nat National"_s, {}, {}, {u"(205) 555-0100"_s}, {}}});
        QCOMPARE(r.nameFor(u"tel:+447911123456"_s), u"Uma UK"_s);
        QVERIFY(r.nameFor(u"tel:+17911123456"_s).isEmpty());
        QCOMPARE(r.nameFor(u"tel:+12055550100"_s), u"Nat National"_s); // +1 still matches a national number
    }

    void lookupAndGroupNames()
    {
        ContactResolver r(false);
        QCOMPARE(ContactResolver::instance(), &r);
        // Before the contacts load: handles, as before.
        QCOMPARE((Handle{u"tel:+15551234567"_s, {}}.displayName()), u"+1 (555) 123-4567"_s);

        QSignalSpy changed(&r, &ContactResolver::contactsChanged);
        r.setPeople(fakeDaemonPeople());
        QCOMPARE(changed.count(), 1);
        QCOMPARE(r.count(), 3);

        // email: case-insensitive; phone: +1 URI against a national number
        QCOMPARE(r.nameFor(u"mailto:bob@example.com"_s), u"Robert Smith"_s);
        QCOMPARE(r.nameFor(u"tel:+15551234567"_s), u"Dave Miller"_s);
        QCOMPARE(r.nameFor(u"tel:5551234567"_s), u"Dave Miller"_s);
        QVERIFY(r.nameFor(u"mailto:nobody@example.com"_s).isEmpty());
        QVERIFY(r.nameFor(u"tel:+15559999999"_s).isEmpty());

        // Handle names prefer the contact over the daemon's display name.
        QCOMPARE((Handle{u"mailto:bob@example.com"_s, u"Bob S."_s}.displayName()), u"Robert Smith"_s);
        QCOMPARE((Handle{u"mailto:eve@example.org"_s, u"Eve Adams"_s}.displayName()), u"Eve Adams"_s);

        // Unnamed groups: first names, like Messages.app.
        Chat group;
        group.id = u"group:x"_s;
        group.isGroup = true;
        group.participants = {{u"mailto:bob@example.com"_s, {}}, {u"tel:+15551234567"_s, {}}, {u"mailto:carol@example.com"_s, {}}};
        QCOMPARE(group.title(), u"Robert, Dave & Carol"_s);
        group.participants.append({u"tel:+15550000000"_s, {}});
        QCOMPARE(group.title(), u"Robert, Dave & 2 more"_s);
        group.participants = {{u"mailto:carol@example.com"_s, {}}, {u"mailto:zed@example.com"_s, {}}};
        QCOMPARE(group.title(), u"Carol & zed@example.com"_s);
        group.displayName = u"Weekend Trip"_s;
        QCOMPARE(group.title(), u"Weekend Trip"_s);

        // Photos: avatar URL only for handles with one.
        QVERIFY(r.avatarSource(u"mailto:carol@example.com"_s).isEmpty());
        QVERIFY(r.avatarSource(u"mailto:nobody@example.com"_s).isEmpty());
        const QString src = r.avatarSource(u"tel:+15551234567"_s);
        QVERIFY(src.startsWith(u"image://contact/"_s));

        // The provider gets what QML passes: the URL minus "image://contact/".
        ContactImageProvider provider(r.shared());
        QSize size;
        const QImage img = provider.requestImage(src.mid(16), &size, QSize());
        QCOMPARE(size, QSize(8, 8));
        QCOMPARE(img.pixelColor(0, 0), QColor(Qt::blue));
        QVERIFY(provider.requestImage(u"mailto%3Anobody%40example.com"_s, &size, QSize()).isNull());
        QVERIFY(size.isEmpty());

        // Search by name and by number; one row per address, IDS URIs.
        auto hits = r.search(u"rob"_s);
        QCOMPARE(hits.size(), 1);
        QCOMPARE(hits.first().uri, u"mailto:bob@example.com"_s);
        hits = r.search(u"555 123"_s);
        QCOMPARE(hits.size(), 1);
        QCOMPARE(hits.first().uri, u"tel:+15551234567"_s);
        QCOMPARE(hits.first().name, u"Dave Miller"_s);
        QVERIFY(r.search(u"zzz"_s).isEmpty());
    }

    void kpeopleVCards()
    {
        // KPeopleVCard reads $XDG_DATA_HOME/kpeoplevcard (useTemporaryXdgDirs() in main()).
        const QString dir = QStandardPaths::writableLocation(QStandardPaths::GenericDataLocation) + u"/kpeoplevcard"_s;
        QVERIFY(QDir().mkpath(dir));
        const auto fixtures = QDir(QFINDTESTDATA("data/contacts")).entryInfoList({u"*.vcf"_s}, QDir::Files);
        QCOMPARE(fixtures.size(), 4);
        for (const auto &f : fixtures) {
            QFile::remove(dir + u'/' + f.fileName());
            QVERIFY(QFile::copy(f.filePath(), dir + u'/' + f.fileName()));
        }

        ContactResolver r(true);
        QVERIFY(r.nameFor(u"mailto:alice@example.com"_s).isEmpty()); // loads asynchronously
#if KMESSAGES_WITH_KPEOPLE
        QTRY_COMPARE_WITH_TIMEOUT(r.count(), 4, Timeout);
        QCOMPARE(r.nameFor(u"mailto:alice@example.com"_s), u"Alice Anderson"_s);
        QCOMPARE(r.nameFor(u"tel:+15550101001"_s), u"Alice Anderson"_s); // "+1 (555) 010-1001"
        QCOMPARE(r.nameFor(u"mailto:bob.smith@example.com"_s), u"Bob Smith"_s); // "Bob.Smith@Example.COM"
        QCOMPARE(r.nameFor(u"tel:+15550102002"_s), u"Bob Smith"_s); // "(555) 010-2002": last 10 digits
        QCOMPARE(r.nameFor(u"tel:+15550103003"_s), u"Carol Jones"_s); // "15550103003"
        QCOMPARE(r.nameFor(u"mailto:support@acme.example"_s), u"Acme Rockets"_s);

        Chat group;
        group.id = u"group:y"_s;
        group.isGroup = true;
        group.participants = {{u"mailto:alice@example.com"_s, {}}, {u"tel:+15550102002"_s, {}}, {u"mailto:carol@example.com"_s, {}}};
        QCOMPARE(group.title(), u"Alice, Bob & Carol"_s);

        // Alice's embedded PHOTO, served through the provider.
        const QString src = r.avatarSource(u"tel:+15550101001"_s);
        QVERIFY(!src.isEmpty());
        QVERIFY(r.avatarSource(u"mailto:carol@example.com"_s).isEmpty());
        ContactImageProvider provider(r.shared());
        QSize size;
        const QImage img = provider.requestImage(QUrl(src).toString(QUrl::RemoveScheme | QUrl::RemoveAuthority).mid(1), &size, QSize(4, 4));
        QCOMPARE(size, QSize(4, 4));
        QCOMPARE(img.pixelColor(1, 1), QColor(200, 30, 60));

        // Edits on disk are picked up (KDirWatch → model change → debounced re-index).
        QSignalSpy changed(&r, &ContactResolver::contactsChanged);
        QFile bob(dir + u"/bob.vcf"_s);
        QVERIFY(bob.open(QIODevice::ReadOnly));
        QByteArray card = bob.readAll();
        bob.close();
        card.replace("FN:Bob Smith", "FN:Bobby Smith");
        QVERIFY(bob.open(QIODevice::WriteOnly | QIODevice::Truncate));
        bob.write(card);
        bob.close();
        QTRY_COMPARE_WITH_TIMEOUT(r.nameFor(u"tel:+15550102002"_s), u"Bobby Smith"_s, Timeout);
#else
        QSKIP("built without KPeople");
#endif
    }

    void sharedProfiles()
    {
        ContactResolver r(false);
        r.setPeople(fakeDaemonPeople());
        QSignalSpy changed(&r, &ContactResolver::contactsChanged);
        QTemporaryDir dir;
        const QString png = dir.filePath(u"p.png"_s);
        QVERIFY(solid(Qt::green, 300).save(png));

        // A handle the address book does not know: the shared name and photo.
        const QString zed = u"mailto:zed@example.com"_s;
        r.setSharedProfile(zed, SharedProfile{u"Zed Zero"_s, u"Zed"_s, {}, png, 42});
        QCOMPARE(changed.count(), 1);
        r.setSharedProfile(zed, SharedProfile{u"Zed Zero"_s, u"Zed"_s, {}, png, 42});
        QCOMPARE(changed.count(), 1); // unchanged: no signal
        QCOMPARE((Handle{zed, {}}.displayName()), u"Zed Zero"_s);
        QCOMPARE((Handle{zed, {}}.shortName()), u"Zed"_s);
        QVERIFY(r.nameFor(zed).isEmpty()); // address-book names only
        QCOMPARE(r.sharedNameFor(zed), u"Zed Zero"_s);
        const QString src = r.avatarSource(zed);
        QCOMPARE(src, r.sharedPhotoSource(zed));
        QVERIFY2(src.startsWith(u"image://attachment/"_s) && src.endsWith(u"?u=42"_s), qPrintable(src));
        QCOMPARE(r.photoFor(zed).size(), QSize(256, 256)); // scaled like address-book photos

        // The address book wins for the name, and its photo for the avatar;
        // without one there, the shared photo is used.
        r.setSharedProfiles({Handle{u"mailto:bob@example.com"_s, {}, SharedProfile{u"Bobby"_s, {}, {}, png, 1}},
                             Handle{u"mailto:carol@example.com"_s, {}, SharedProfile{u"Caz"_s, {}, {}, png, 1}}});
        QCOMPARE(changed.count(), 2);
        QCOMPARE((Handle{u"mailto:bob@example.com"_s, {}}.displayName()), u"Robert Smith"_s);
        QVERIFY(r.avatarSource(u"mailto:bob@example.com"_s).startsWith(u"image://contact/"_s));
        QCOMPARE((Handle{u"mailto:carol@example.com"_s, {}}.displayName()), u"Carol Jones"_s);
        QVERIFY(r.avatarSource(u"mailto:carol@example.com"_s).startsWith(u"image://attachment/"_s));
        QVERIFY(r.lookup(u"mailto:carol@example.com"_s)->hasPhoto);

        // A chat payload without `profile` for a participant clears theirs.
        r.setSharedProfiles({Handle{zed, {}}});
        QCOMPARE(changed.count(), 3);
        QCOMPARE((Handle{zed, {}}.displayName()), u"zed@example.com"_s);
        QVERIFY(r.avatarSource(zed).isEmpty());
        // A shared photo without a name names nobody.
        r.setSharedProfile(zed, SharedProfile{{}, {}, {}, png, 2});
        QVERIFY(!r.lookup(zed));
        QVERIFY(!r.avatarSource(zed).isEmpty());
    }

    void modelsUseContacts()
    {
        if (!FakeDaemon::onPrivateBus()) {
            QSKIP("needs the private test bus; run through ctest");
        }
        FakeDaemon fake;
        QVERIFY(fake.start());
        ContactResolver contacts(false);
        DaemonClient client;
        QTRY_VERIFY_WITH_TIMEOUT(client.isAvailable() && !client.ownHandles().isEmpty(), Timeout);
        ChatListModel chats(&client);
        QTRY_VERIFY_WITH_TIMEOUT(chats.isLoaded(), Timeout);
        const QString dave = u"tel:+15551234567"_s;
        const QString bob = u"mailto:bob@example.com"_s;
        const QString team = u"group:5B0E2C1A-3F4D-4E21-9A55-0C7E1B2D3F40"_s;
        const auto role = [&](const QString &chatId, int r) {
            return chats.data(chats.index(chats.indexOf(chatId)), r);
        };
        QCOMPARE(role(dave, ChatListModel::NameRole).toString(), u"+1 (555) 123-4567"_s);
        QCOMPARE(role(bob, ChatListModel::NameRole).toString(), u"Bob Smith"_s); // daemon's display name
        QVERIFY(role(dave, ChatListModel::AvatarSourceRole).toString().isEmpty());

        MessageListModel messages(&client, &chats);
        messages.setChatId(team);
        QTRY_VERIFY_WITH_TIMEOUT(messages.isLoaded(), Timeout);

        // The contacts arrive after the chats: rows update in place.
        QSignalSpy chatRows(&chats, &QAbstractItemModel::dataChanged);
        QSignalSpy messageRows(&messages, &QAbstractItemModel::dataChanged);
        contacts.setPeople(fakeDaemonPeople());
        QVERIFY(chatRows.count() >= 1);
        QVERIFY(messageRows.count() >= 1);

        QCOMPARE(role(dave, ChatListModel::NameRole).toString(), u"Dave Miller"_s);
        QCOMPARE(role(dave, ChatListModel::InitialsRole).toString(), u"DM"_s);
        QVERIFY(role(dave, ChatListModel::AvatarSourceRole).toString().startsWith(u"image://contact/"_s));
        QCOMPARE(role(bob, ChatListModel::NameRole).toString(), u"Robert Smith"_s);
        QVERIFY(role(team, ChatListModel::AvatarSourceRole).toString().isEmpty());
        QCOMPARE(role(team, ChatListModel::ParticipantsRole).toStringList(), (QStringList{u"Robert Smith"_s, u"Carol Jones"_s, u"Dave Miller"_s}));
        const auto avatars = role(team, ChatListModel::ParticipantAvatarsRole).toStringList();
        QCOMPARE(avatars.size(), 3);
        QVERIFY(!avatars.at(0).isEmpty() && avatars.at(1).isEmpty() && !avatars.at(2).isEmpty());
        QCOMPARE(chats.titleFor(dave), u"Dave Miller"_s);

        // Group transcript: sender names, avatars, reaction tooltips.
        int tent = -1;
        for (int i = 0; i < messages.rowCount(); ++i) {
            if (messages.data(messages.index(i), MessageListModel::TextRole).toString() == u"I have a tent for 4"_s) {
                tent = i;
            }
        }
        QVERIFY(tent >= 0);
        QCOMPARE(messages.data(messages.index(tent), MessageListModel::SenderNameRole).toString(), u"Dave Miller"_s);
        QVERIFY(messages.data(messages.index(tent), MessageListModel::AvatarSourceRole).toString().startsWith(u"image://contact/"_s));
        const auto reactions = messages.data(messages.index(tent), MessageListModel::ReactionsRole).toList();
        bool sawLike = false;
        for (const auto &v : reactions) {
            const auto m = v.toMap();
            if (m.value(u"kind"_s).toString() == u"like"_s) {
                sawLike = true;
                QCOMPARE(m.value(u"senders"_s).toString(), u"Carol Jones, Robert Smith"_s);
            }
        }
        QVERIFY(sawLike);

        // Recipient suggestions search the address book by name.
        ContactSuggestionModel suggestions(&client, &contacts);
        suggestions.setFilterText(u"dave"_s);
        QCOMPARE(suggestions.rowCount(), 1);
        const auto idx = suggestions.index(0);
        QCOMPARE(idx.data(ContactSuggestionModel::NameRole).toString(), u"Dave Miller"_s);
        QCOMPARE(idx.data(ContactSuggestionModel::UriRole).toString(), dave);
        QCOMPARE(idx.data(ContactSuggestionModel::AddressRole).toString(), u"(555) 123-4567"_s);
        // …and are checked with LookupHandles.
        QTRY_COMPARE_WITH_TIMEOUT(idx.data(ContactSuggestionModel::ReachabilityRole).toString(), u"reachable"_s, Timeout);
        QVERIFY(!fake.callsOf(u"LookupHandles"_s).isEmpty());
        suggestions.setFilterText(QString());
        QCOMPARE(suggestions.rowCount(), 0);
        fake.stop();
    }

    void suggestionLookupBursts()
    {
        if (!FakeDaemon::onPrivateBus()) {
            QSKIP("needs the private test bus; run through ctest");
        }
        FakeDaemon fake;
        QVERIFY(fake.start());
        ContactResolver contacts(false);
        QList<ContactResolver::Person> people;
        for (int i = 0; i < 12; ++i) {
            people.append({QString::number(i), u"Pat Person %1"_s.arg(i, 2, 10, u'0'), {}, {u"pat%1@example.com"_s.arg(i)}, {}, {}});
        }
        contacts.setPeople(people);
        DaemonClient client;
        QTRY_VERIFY_WITH_TIMEOUT(client.isAvailable() && !client.ownHandles().isEmpty(), Timeout);
        ContactSuggestionModel suggestions(&client, &contacts);
        const auto handlesPerCall = [&] {
            QList<int> sizes;
            for (const auto &c : fake.callsOf(u"LookupHandles"_s)) {
                sizes << int(QJsonDocument::fromJson(c.at(1).toString().toUtf8()).array().size());
            }
            return sizes;
        };
        const auto allChecked = [&] {
            for (int i = 0; i < suggestions.rowCount(); ++i) {
                if (suggestions.index(i).data(ContactSuggestionModel::ReachabilityRole).toString() != u"reachable"_s) {
                    return false;
                }
            }
            return suggestions.rowCount() > 0;
        };
        fake.control(u"ClearCalls"_s);
        suggestions.setFilterText(u"pat"_s);
        QCOMPARE(suggestions.rowCount(), 12);
        QTRY_VERIFY_WITH_TIMEOUT(allChecked(), Timeout);
        QCOMPARE(handlesPerCall(), (QList<int>{5, 5, 2})); // bursts of at most five, one after another

        // Typing on and back: everything is fresh, no new calls.
        suggestions.setFilterText(u"pat person 0"_s);
        suggestions.setFilterText(u"pat"_s);
        QTest::qWait(800);
        QCOMPARE(handlesPerCall().size(), 3);
        QVERIFY(allChecked());

        // Stale results are looked up again (still shown meanwhile).
        suggestions.setCacheTtlMs(0);
        suggestions.setFilterText(u"pat person 1"_s);
        QCOMPARE(suggestions.rowCount(), 3); // 01, 10, 11
        QTRY_COMPARE_WITH_TIMEOUT(handlesPerCall().size(), 4, Timeout);
        QCOMPARE(handlesPerCall().last(), 3);
        fake.stop();
    }
};

int main(int argc, char **argv)
{
    useTemporaryXdgDirs();
    QGuiApplication app(argc, argv);
    TestContacts tc;
    QTEST_SET_MAIN_SOURCE_PATH
    return QTest::qExec(&tc, argc, argv);
}
#include "tst_contacts.moc"
