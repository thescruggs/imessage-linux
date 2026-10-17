// SPDX-License-Identifier: SSPL-1.0
//
// JSON parsing of every payload shape in docs/DBUS.md (fixtures in data/
// are copied from that document), plus handle normalization (mirrors the
// Rust tests in imessage-common) and the presentation helpers.
#include "fakedaemon.h"
#include "types.h"

#include <QFile>
#include <QTest>
#include <QTimeZone>

using namespace Qt::StringLiterals;
using namespace km;

class TestJson : public QObject
{
    Q_OBJECT

    static QString fixture(const char *name)
    {
        QFile f(QFINDTESTDATA(u"data/"_s + QString::fromLatin1(name)));
        if (!f.open(QIODevice::ReadOnly)) {
            qFatal("missing fixture %s", name);
        }
        return QString::fromUtf8(f.readAll());
    }

private Q_SLOTS:
    void status()
    {
        QString err;
        auto s = Json::parseStatus(fixture("status.json"), &err);
        QVERIFY2(s, qPrintable(err));
        QCOMPARE(s->version, u"0.1.0"_s);
        QCOMPARE(s->connectionState, u"connected"_s);
        QVERIFY(s->connectionDetail.isEmpty());
        QVERIFY(s->registered);
        QCOMPARE(s->handles, (QStringList{u"mailto:me@example.com"_s, u"mailto:me@icloud.com"_s}));
        QCOMPARE(s->sender, u"mailto:me@icloud.com"_s);
        QCOMPARE(s->otherHandles, (QStringList{u"tel:+12055550100"_s, u"mailto:me@work.example"_s}));
        QCOMPARE(s->registeredAt, 1700000000);
        QCOMPARE(s->expiresAt, 1702592000);
        QCOMPARE(s->unread, 3);
    }

    void statusNeedsLoginAndError()
    {
        auto s = Json::parseStatus(fixture("status_needs_login.json"));
        QVERIFY(s);
        QCOMPARE(s->connectionState, u"needs_login"_s);
        QVERIFY(!s->registered);
        QVERIFY(s->handles.isEmpty());
        QVERIFY(s->sender.isEmpty()); // null
        QVERIFY(s->otherHandles.isEmpty()); // missing (older daemon)
        QCOMPARE(s->registeredAt, 0);

        auto e = Json::parseStatus(fixture("status_error.json"));
        QVERIFY(e);
        QCOMPARE(e->connectionState, u"error"_s);
        QCOMPARE(e->connectionDetail, u"registration failing, retrying: timeout"_s);
        QCOMPARE(e->unread, -1); // null
    }

    void chat()
    {
        QString err;
        auto c = Json::parseChat(fixture("chat.json"), &err);
        QVERIFY2(c, qPrintable(err));
        QCOMPARE(c->id, u"group:5B0E2C1A-3F4D-4E21-9A55-0C7E1B2D3F40"_s);
        QCOMPARE(c->participants.size(), 2);
        QCOMPARE(c->participants.at(0).uri, u"mailto:bob@example.com"_s);
        QVERIFY(c->participants.at(1).isPhone());
        QCOMPARE(c->displayName, u"Team"_s);
        QVERIFY(c->isGroup);
        QCOMPARE(c->lastMessageAt, 1700000123456LL);
        QCOMPARE(c->unreadCount, 2u);
        QCOMPARE(c->title(), u"Team"_s);
        QVERIFY(c->backgroundId.isEmpty()); // no "background" key
    }

    void chatBackground()
    {
        const QString base = uR"({"id":"mailto:bob@example.com","participants":[],"is_group":false,"unread_count":0,"background":%1})"_s;
        auto c = Json::parseChat(base.arg(uR"({"id":"BG-1","path":"/s/bg/BG-1.heic","payload":"/s/bg/BG-1","version":1,"set_by":null,"set_at":0})"_s));
        QVERIFY(c);
        QCOMPARE(c->backgroundId, u"BG-1"_s);
        QCOMPARE(c->backgroundPath, u"/s/bg/BG-1.heic"_s);
        // Not downloaded yet / removed.
        c = Json::parseChat(base.arg(uR"({"id":"BG-1","path":null,"version":1})"_s));
        QVERIFY(c && c->backgroundId == u"BG-1"_s && c->backgroundPath.isEmpty());
        c = Json::parseChat(base.arg(u"null"_s));
        QVERIFY(c && c->backgroundId.isEmpty() && c->backgroundPath.isEmpty());
    }

    void chatTitles()
    {
        auto c = Json::parseChat(uR"({"id":"mailto:bob@example.com","participants":[{"uri":"mailto:bob@example.com","display":"Bob Smith"}],
                                      "display_name":null,"is_group":false,"last_message_at":null,"unread_count":0})"_s);
        QVERIFY(c);
        QCOMPARE(c->title(), u"Bob Smith"_s);
        QCOMPARE(c->lastMessageAt, 0);

        auto g = Json::parseChat(uR"({"id":"group-p:a,b","participants":[{"uri":"mailto:ann@x.org","display":"Ann Lee"},{"uri":"tel:+15551234567"}],
                                      "display_name":null,"is_group":true,"last_message_at":5,"unread_count":0})"_s);
        QVERIFY(g);
        QCOMPARE(g->title(), u"Ann & +1 (555) 123-4567"_s);

        QVERIFY(Json::parseChatList(u"[]"_s)->isEmpty());
        QCOMPARE(Json::parseChatList(u"["_s + fixture("chat.json") + u"]"_s)->size(), 1);
    }

    void message()
    {
        QString err;
        auto m = Json::parseMessage(fixture("message.json"), &err);
        QVERIFY2(m, qPrintable(err));
        QCOMPARE(m->id, u"0F1E2D3C-4B5A-6978-8796-A5B4C3D2E1F0"_s);
        QCOMPARE(m->chatId, u"mailto:bob@example.com"_s);
        QCOMPARE(m->sender.uri, u"mailto:bob@example.com"_s);
        QCOMPARE(m->sentAt, 1700000123456LL);
        QCOMPARE(m->text, u"look at this"_s);
        QCOMPARE(m->parts.size(), 4);
        QCOMPARE(m->parts.at(0).type, MessagePart::Type::Text);
        QCOMPARE(m->parts.at(1).type, MessagePart::Type::Attachment);
        QCOMPARE(m->parts.at(1).attachmentId, u"0F1E2D3C-4B5A-6978-8796-A5B4C3D2E1F0:1"_s);
        QCOMPARE(m->parts.at(1).size, 183422);
        QCOMPARE(m->parts.at(2).type, MessagePart::Type::Mention);
        QCOMPARE(m->parts.at(2).handle, u"mailto:carol@example.com"_s);
        QCOMPARE(m->parts.at(2).text, u"Carol"_s);
        QCOMPARE(m->parts.at(3).type, MessagePart::Type::Unsupported);
        QCOMPARE(m->parts.at(3).description, u"app message"_s);
        QVERIFY(m->subject.isNull());
        QVERIFY(m->replyTo.isNull());
        QVERIFY(!m->isFromMe);
        QCOMPARE(m->delivery, Delivery::Delivered);
        QVERIFY(!m->edited);
        QVERIFY(!m->unsent);
        QVERIFY(m->verified);
        QVERIFY(m->effect.isNull());

        QCOMPARE(m->reactions.size(), 2);
        QCOMPARE(m->reactions.at(0).kind, u"love"_s);
        QCOMPARE(m->reactions.at(0).glyph(), u"❤️"_s);
        QCOMPARE(m->reactions.at(0).methodKind(), u"love"_s);
        QVERIFY(m->reactions.at(1).isEmoji());
        QCOMPARE(m->reactions.at(1).emoji, u"🔥"_s);
        QCOMPARE(m->reactions.at(1).methodKind(), u"emoji:🔥"_s);
        QCOMPARE(m->reactions.at(1).sender.uri, u"mailto:carol@example.com"_s);

        const auto atts = m->attachments();
        QCOMPARE(atts.size(), 1);
        QVERIFY(atts.first().isImage());
        QCOMPARE(atts.first().filename, u"IMG_0001.jpeg"_s);
        QCOMPARE(m->unsupported(), QStringList{u"app message"_s});
        QCOMPARE(m->snippet(), u"look at this"_s);
    }

    void unsentMessageAndDefaults()
    {
        auto m = Json::parseMessage(fixture("message_unsent.json"));
        QVERIFY(m);
        QVERIFY(m->unsent);
        QVERIFY(m->text.isNull());
        QVERIFY(m->parts.isEmpty());
        QVERIFY(m->verified); // serde default when the field is missing
        QCOMPARE(m->replyTo, u"BB"_s);
        QCOMPARE(m->delivery, Delivery::Read);
        QCOMPARE(m->effect, u"com.apple.messages.effect.CKConfettiEffect"_s);
        QVERIFY(!m->snippet().isEmpty());
    }

    void page()
    {
        auto p = Json::parseMessagePage(fixture("page.json"));
        QVERIFY(p);
        QCOMPARE(p->messages.size(), 2);
        QCOMPARE(p->messages.at(0).id, u"M1"_s); // oldest first
        QVERIFY(!p->messages.at(0).verified);
        QCOMPARE(p->messages.at(1).delivery, Delivery::Failed);
        QVERIFY(p->messages.at(1).edited);
        QCOMPARE(p->messages.at(1).subject, u"Re"_s);
        QCOMPARE(p->messages.at(1).replyTo, u"M1"_s);
        QCOMPARE(p->nextCursor, u"1700000000000-42"_s);

        auto last = Json::parseMessagePage(fixture("page_last.json"));
        QVERIFY(last);
        QVERIFY(last->messages.isEmpty());
        QVERIFY(last->nextCursor.isEmpty());
    }

    void sentAndLookup()
    {
        auto s = Json::parseSentMessage(fixture("sent.json"));
        QVERIFY(s);
        QCOMPARE(s->id, u"6C1D0000-0000-0000-0000-000000000000"_s);
        QCOMPARE(s->chatId, u"group:5B0E2C1A-3F4D-4E21-9A55-0C7E1B2D3F40"_s);
        QCOMPARE(s->sentAt, 1700000123456LL);
        QCOMPARE(s->sender.uri, u"mailto:me@example.com"_s);

        auto l = Json::parseLookup(fixture("lookup.json"));
        QVERIFY(l);
        QCOMPARE(l->size(), 2);
        QCOMPARE(l->at(0).handle, u"mailto:bob@example.com"_s);
        QVERIFY(l->at(0).reachable);
        QVERIFY(!l->at(1).reachable);
    }

    void deliveryStrings()
    {
        for (const auto d : {Delivery::Pending, Delivery::Sent, Delivery::Delivered, Delivery::Read, Delivery::Failed}) {
            QCOMPARE(deliveryFromString(deliveryToString(d)), d);
        }
    }

    void malformed_data()
    {
        QTest::addColumn<QString>("json");
        QTest::newRow("garbage") << u"{nope"_s;
        QTest::newRow("array for object") << u"[]"_s;
        QTest::newRow("no id") << uR"({"participants":[],"is_group":false,"unread_count":0})"_s;
        QTest::newRow("bad handle") << uR"({"id":"x","participants":["bob"],"is_group":false,"unread_count":0})"_s;
    }
    void malformed()
    {
        QFETCH(QString, json);
        QString err;
        QVERIFY(!Json::parseChat(json, &err));
        QVERIFY(!err.isEmpty());
    }

    void malformedMessage()
    {
        QString err;
        QVERIFY(!Json::parseMessage(uR"({"id":"A","chat_id":"c","sender":{"uri":"x"},"parts":[{"type":"attachment"}]})"_s, &err));
        QVERIFY(!Json::parseMessage(uR"({"id":"A","chat_id":"c","sender":{"uri":"x"},"parts":[],"reactions":[{"kind":5,"sender":{"uri":"x"}}]})"_s, &err));
        QVERIFY(!Json::parseStatus(u"{}"_s, &err));
        QVERIFY(!Json::parseLookup(u"[{}]"_s, &err));
        // Unknown part types are tolerated (forward compatibility).
        auto m = Json::parseMessage(uR"({"id":"A","chat_id":"c","sender":{"uri":"x"},"parts":[{"type":"sticker"}],"reactions":[]})"_s, &err);
        QVERIFY(m);
        QCOMPARE(m->parts.first().type, MessagePart::Type::Unsupported);
    }

    void normalize_data()
    {
        QTest::addColumn<QString>("raw");
        QTest::addColumn<QString>("expected");
        QTest::newRow("email") << u"Foo@Example.COM"_s << u"mailto:foo@example.com"_s;
        QTest::newRow("mailto") << u"mailto:Foo@Example.com"_s << u"mailto:foo@example.com"_s;
        QTest::newRow("MAILTO") << u"MAILTO:a@b.c"_s << u"mailto:a@b.c"_s;
        QTest::newRow("phone") << u"+1 (555) 123-4567"_s << u"tel:+15551234567"_s;
        QTest::newRow("tel") << u"tel:+15551234567"_s << u"tel:+15551234567"_s;
        QTest::newRow("tel dots") << u"tel: 555.123"_s << u"tel:555123"_s;
        for (const auto &bad : {u""_s, u"  "_s, u"bob"_s, u"tel:"_s, u"tel:abc"_s, u"+"_s, u"@x.com"_s, u"a@"_s, u"a1"_s, u"mailto:"_s, u"mailto:bob"_s}) {
            QTest::newRow(qPrintable(u"bad '"_s + bad + u"'"_s)) << bad << QString();
        }
    }
    void normalize()
    {
        QFETCH(QString, raw);
        QFETCH(QString, expected);
        QCOMPARE(normalizeHandle(raw), expected);
    }

    void handles()
    {
        const Handle phone{u"tel:+15551234567"_s, {}};
        QCOMPARE(phone.address(), u"+15551234567"_s);
        QCOMPARE(phone.displayName(), u"+1 (555) 123-4567"_s);
        const Handle mail{u"mailto:x@y.z"_s, u"x@y.z"_s};
        QCOMPARE(mail.displayName(), u"x@y.z"_s); // display == address: not a real name
        const Handle named{u"mailto:x@y.z"_s, u"Xavier"_s};
        QCOMPARE(named.displayName(), u"Xavier"_s);
    }

    void initials()
    {
        QCOMPARE(initialsFor(u"Bob Smith"_s), u"BS"_s);
        QCOMPARE(initialsFor(u"bob@example.com"_s), u"B"_s);
        QCOMPARE(initialsFor(u"mary.jane@example.com"_s), u"MJ"_s);
        QCOMPARE(initialsFor(u"+1 (555) 123-4567"_s), QString());
        QCOMPARE(initialsFor(u"Weekend Trip Crew"_s), u"WT"_s);
        QCOMPARE(avatarColorFor(u"a"_s), avatarColorFor(u"a"_s));
    }

    void times()
    {
        const QDateTime now(QDate(2026, 10, 11), QTime(15, 0), QTimeZone::UTC);
        const auto at = [&](int daysAgo, int h) {
            return QDateTime(now.date().addDays(-daysAgo), QTime(h, 5), QTimeZone::UTC).toMSecsSinceEpoch();
        };
        QVERIFY(formatListTime(at(0, 9), now).contains(u"9"_s));
        QCOMPARE(formatListTime(at(1, 9), now), u"Yesterday"_s);
        QCOMPARE(formatListTime(at(3, 9), now), QLocale().dayName(QDate(2026, 10, 8).dayOfWeek()));
        QVERIFY(!formatListTime(at(30, 9), now).isEmpty());
        QCOMPARE(formatListTime(0, now), QString());
        QVERIFY(formatSeparatorTime(at(0, 9), now).startsWith(u"Today "_s));
        QVERIFY(formatSeparatorTime(at(1, 9), now).startsWith(u"Yesterday "_s));
        QVERIFY(formatSeparatorTime(at(40, 9), now).contains(u"2026"_s));
        QVERIFY(!formatSize(183422).isEmpty());
    }
};

int main(int argc, char **argv)
{
    useTemporaryXdgDirs();
    QCoreApplication app(argc, argv);
    TestJson tc;
    QTEST_SET_MAIN_SOURCE_PATH
    return QTest::qExec(&tc, argc, argv);
}
#include "tst_json.moc"
