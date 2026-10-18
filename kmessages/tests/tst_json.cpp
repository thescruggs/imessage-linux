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
        QCOMPARE(s->profilesState, u"ready"_s);
        QVERIFY(s->profilesDetail.isEmpty());
    }

    void smsStatusAndService()
    {
        auto s = Json::parseStatus(fixture("status.json"));
        QVERIFY(s);
        QVERIFY(s->smsState.isEmpty()); // older daemon: no `sms`
        auto a = Json::parseStatus(
            uR"({"version":"1","connection":{"state":"connected"},"registered":true,"registration":{"handles":[]},"unread":0,
                 "sms":{"state":"active","number":"tel:+15555550100","since":1700000000000}})"_s);
        QVERIFY(a);
        QCOMPARE(a->smsState, u"active"_s);
        QCOMPARE(a->smsNumber, u"tel:+15555550100"_s);
        QCOMPARE(a->smsSince, 1700000000000LL);
        Status st;
        QVERIFY(Json::parseSmsForwarding(uR"({"state":"error","detail":"no iPhone"})"_s, &st));
        QCOMPARE(st.smsState, u"error"_s);
        QCOMPARE(st.smsDetail, u"no iPhone"_s);
        QVERIFY(Json::parseSmsForwarding(u"{}"_s, &st));
        QCOMPARE(st.smsState, u"off"_s);
        QVERIFY(!Json::parseSmsForwarding(u"nope"_s, &st));

        auto m = Json::parseMessage(fixture("message.json"));
        QVERIFY(m);
        QCOMPARE(m->service, u"imessage"_s); // missing = iMessage
        QVERIFY(!m->isSms());
        auto t = Json::parseMessage(uR"({"id":"A","chat_id":"tel:+15555550142","sender":{"uri":"tel:+15555550142"},"sent_at":1,
                                        "text":"hi","parts":[{"type":"text","text":"hi"}],"is_from_me":false,"delivery":"delivered",
                                        "edited":false,"unsent":false,"reactions":[],"service":"sms"})"_s);
        QVERIFY(t);
        QVERIFY(t->isSms());
        auto sent = Json::parseSentMessage(uR"({"id":"A","chat_id":"tel:+15555550142","sent_at":1,"service":"sms"})"_s);
        QVERIFY(sent);
        QCOMPARE(sent->service, u"sms"_s);
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
        QVERIFY(e->profilesState.isEmpty()); // older daemon
    }

    void sharedProfiles()
    {
        // Participants carry `profile`, groups `icon_path`.
        QString err;
        auto c = Json::parseChat(uR"({"id":"group:G","is_group":true,"unread_count":0,"icon_path":"/s/chat_icons/g/I.jpg",
            "participants":[{"uri":"mailto:bob@example.com","profile":{"name":"Bobby","first":"Bob","last":null,"photo":"/s/profiles/b/R.jpg","updated_at":1700000000000}},
                            {"uri":"tel:+15551234567","profile":{"first":"Dee","last":"Ray"}},
                            {"uri":"mailto:carol@example.com","profile":{}}]})"_s, &err);
        QVERIFY2(c, qPrintable(err));
        QCOMPARE(c->iconPath, u"/s/chat_icons/g/I.jpg"_s);
        const auto &bob = c->participants.at(0).profile;
        QVERIFY(bob);
        QCOMPARE(bob->name, u"Bobby"_s);
        QCOMPARE(bob->first, u"Bob"_s);
        QVERIFY(bob->last.isEmpty());
        QCOMPARE(bob->photo, u"/s/profiles/b/R.jpg"_s);
        QCOMPARE(bob->updatedAt, 1700000000000LL);
        QCOMPARE(bob->displayName(), u"Bobby"_s);
        QCOMPARE(c->participants.at(1).profile->displayName(), u"Dee Ray"_s);
        QVERIFY(!c->participants.at(2).profile); // nothing to show
        QVERIFY(Json::parseChat(fixture("chat.json"))->iconPath.isEmpty());

        // GetHandleProfile / HandleProfileUpdated payloads
        std::optional<SharedProfile> p;
        QVERIFY(Json::parseHandleProfile(u"null"_s, &p));
        QVERIFY(!p);
        QVERIFY(Json::parseHandleProfile(uR"({"name":"Ann","photo":null,"updated_at":5})"_s, &p));
        QVERIFY(p && p->name == u"Ann"_s && p->photo.isEmpty() && p->updatedAt == 5);
        QVERIFY(!Json::parseHandleProfile(u"[1]"_s, &p, &err));
        QVERIFY(!Json::parseHandleProfile(u"{"_s, &p, &err));
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

    void chatPins()
    {
        // `pinned` / `pin_order` (older daemons: missing = not pinned).
        auto old = Json::parseChat(fixture("chat.json"));
        QVERIFY(old && !old->pinned && old->pinOrder == -1);
        const QString base = uR"({"id":"mailto:bob@example.com","participants":[],"is_group":false,"unread_count":0,%1})"_s;
        auto c = Json::parseChat(base.arg(uR"("pinned":true,"pin_order":2)"_s));
        QVERIFY(c && c->pinned && c->pinOrder == 2);
        c = Json::parseChat(base.arg(uR"("pinned":false,"pin_order":null)"_s));
        QVERIFY(c && !c->pinned && c->pinOrder == -1);
        c = Json::parseChat(base.arg(uR"("pinned":true,"pin_order":null)"_s)); // defensive: first
        QVERIFY(c && c->pinned && c->pinOrder == 0);

        // GetStatus.mac_host
        QVERIFY(Json::parseStatus(fixture("status.json"))->macHost.isEmpty());
        auto s = Json::parseStatus(uR"({"version":"1","connection":{"state":"connected"},"registered":true,"registration":{"handles":[]},"unread":0,"mac_host":"me@mac.local"})"_s);
        QVERIFY(s);
        QCOMPARE(s->macHost, u"me@mac.local"_s);
        s = Json::parseStatus(uR"({"version":"1","connection":{"state":"connected"},"registered":true,"registration":{"handles":[]},"unread":0,"mac_host":null})"_s);
        QVERIFY(s && s->macHost.isEmpty());
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

    void linkParts()
    {
        // A message that is only a URL, with the full preview.
        const QString full = fixture("message_link.json");
        QString err;
        auto m = Json::parseMessage(full, &err);
        QVERIFY2(m, qPrintable(err));
        QCOMPARE(m->parts.at(1).type, MessagePart::Type::Link);
        const auto l = m->link();
        QVERIFY(l);
        QCOMPARE(l->url, u"https://www.example.com/a"_s);
        QCOMPARE(l->title, u"A page"_s);
        QCOMPARE(l->summary, u"About a"_s);
        QCOMPARE(l->siteName, u"Example"_s);
        QCOMPARE(l->imageAttachmentId, u"L1:link-image"_s);
        QCOMPARE(l->iconAttachmentId, u"L1:link-icon"_s);
        QVERIFY(m->isOnlyLink());
        QCOMPARE(m->snippet(), u"A page"_s);
        // the preview's image is the card's, not an attachment of its own
        const auto atts = m->attachments();
        QCOMPARE(atts.size(), 1);
        QCOMPARE(atts.first().id, u"L1:2"_s);

        // Text around the URL, null fields, missing fields.
        const QString sparse = fixture("message_link_sparse.json");
        m = Json::parseMessage(sparse, &err);
        QVERIFY2(m, qPrintable(err));
        const auto l2 = m->link();
        QVERIFY(l2);
        QVERIFY(l2->title.isEmpty() && l2->summary.isEmpty() && l2->siteName.isEmpty());
        QVERIFY(l2->imageAttachmentId.isEmpty() && l2->iconAttachmentId.isEmpty());
        QVERIFY(!m->isOnlyLink());
        QCOMPARE(m->snippet(), u"look https://x.example/b"_s);

        // Only a URL, no title: the snippet is the URL.
        m = Json::parseMessage(QString(sparse).replace(u"\"text\":\"look https://x.example/b\""_s, u"\"text\":\"https://x.example/b\""_s), &err);
        QVERIFY2(m, qPrintable(err));
        QVERIFY(m->isOnlyLink());
        QCOMPARE(m->snippet(), u"https://x.example/b"_s);

        // Redirected: the text is the URL as typed, the card has the final
        // page (and original_url); a bare http(s) token counts without it.
        const QString redirected = QString(sparse)
                                       .replace(u"\"text\":\"look https://x.example/b\""_s, u"\"text\":\"https://t.example/abc \""_s)
                                       .replace(u"\"url\":\"https://x.example/b\""_s, u"\"url\":\"https://www.example.com/final\",\"original_url\":\"https://t.example/abc\""_s);
        m = Json::parseMessage(redirected, &err);
        QVERIFY2(m, qPrintable(err));
        QCOMPARE(m->link()->originalUrl, u"https://t.example/abc"_s);
        QVERIFY(m->isOnlyLink());
        m = Json::parseMessage(QString(redirected).replace(u",\"original_url\":\"https://t.example/abc\""_s, QString()), &err);
        QVERIFY2(m, qPrintable(err));
        QVERIFY(m->link()->originalUrl.isEmpty());
        QVERIFY(m->isOnlyLink());
        // two tokens, or not a URL: the text stays
        m = Json::parseMessage(QString(redirected).replace(u"https://t.example/abc \""_s, u"https://t.example/abc and more\""_s), &err);
        QVERIFY2(m, qPrintable(err));
        QVERIFY(!m->isOnlyLink());
        m = Json::parseMessage(QString(redirected).replace(u"\"text\":\"https://t.example/abc \""_s, u"\"text\":\"ftp://t.example/abc\""_s), &err);
        QVERIFY2(m, qPrintable(err));
        QVERIFY(!m->isOnlyLink());

        // A link part without a URL degrades to an unsupported part.
        m = Json::parseMessage(QString(sparse).replace(u"\"url\":\"https://x.example/b\","_s, QString()), &err);
        QVERIFY2(m, qPrintable(err));
        QVERIFY(!m->link());
        QCOMPARE(m->unsupported(), QStringList{u"link"_s});
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
