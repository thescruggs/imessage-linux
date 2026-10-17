#!/usr/bin/env python3
# SPDX-License-Identifier: SSPL-1.0
"""Fake imessaged for kmessages tests.

Serves ``org.imessage.Daemon1`` on the session bus exactly as documented in
docs/DBUS.md, backed by an in-memory store with canned chats and messages,
plus a test-only control interface ``org.imessage.FakeControl`` on the same
object to inject events (incoming messages, typing, reactions, delivery
updates, connection state) and to inspect the calls the client made.

Run it on a private bus, e.g.::

    dbus-run-session --config-file=kmessages/tests/session.conf -- \
        python3 kmessages/tests/fake_daemon.py --state connected

Login behaviour (for the login dialog): password ``wrong`` fails with
``Error.Login``; an Apple ID containing ``2fa`` asks for a code; code
``000000`` is rejected with ``Error.BadTwoFactorCode``; anything else is ok.
LookupHandles: addresses containing ``android`` are unreachable.
SendText: the text ``__fail__`` fails with ``Error.Transient``.
"""

import argparse
import json
import os
import re
import struct
import sys
import tempfile
import time
import uuid
import zlib

import dbus
import dbus.mainloop.glib
import dbus.service
from gi.repository import GLib

BUS_NAME = "org.imessage.Daemon1"
PATH = "/org/imessage/Daemon1"
IFACE = "org.imessage.Daemon1"
CTRL = "org.imessage.FakeControl"
PROPS = "org.freedesktop.DBus.Properties"
ME = "mailto:me@example.com"
VERSION = "0.1.0-fake"


class Error(dbus.DBusException):
    def __init__(self, name, message):
        super().__init__(message, name=f"{IFACE}.Error.{name}")


def normalize(raw):
    """Port of imessage_common::model::Handle::try_normalize."""
    raw = raw.strip()
    lower = raw.lower()
    forced_email = False
    if lower.startswith("mailto:"):
        body, forced_email = lower[7:].strip(), True
    elif lower.startswith("tel:"):
        body = raw[4:].strip()
    else:
        body = raw
    if forced_email or "@" in body:
        email = body.lower()
        if "@" not in email:
            return None
        local, domain = email.split("@", 1)
        if not local or not domain or "@" in domain or re.search(r"\s", email):
            return None
        return "mailto:" + email
    if not all(c.isdigit() or c in "+ -.()" for c in body):
        return None
    digits = "".join(c for c in body if c.isdigit() or c == "+")
    if not any(c.isdigit() for c in digits):
        return None
    return "tel:" + digits


def png_bytes(width=320, height=200):
    """A small gradient PNG, generated without any imaging library."""
    rows = []
    for y in range(height):
        row = bytearray([0])
        for x in range(width):
            row += bytes([(x * 255) // width, (y * 255) // height, 200])
        rows.append(bytes(row))
    raw = b"".join(rows)

    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))


def guid():
    return str(uuid.uuid4()).upper()


class Store:
    def __init__(self, now_ms, with_data=True):
        self.chats = {}      # id -> chat dict
        self.messages = {}   # chat id -> [message dict], oldest first
        self.seq = 0
        if with_data:
            self._seed(now_ms)

    def _msg(self, chat_id, sender, sent_at, text=None, from_me=False, delivery="delivered",
             parts=None, reactions=None, verified=True, mid=None, reply_to=None, edited=False):
        if parts is None:
            parts = [{"type": "text", "text": text}] if text is not None else []
        self.seq += 1
        m = {
            "id": mid or guid(),
            "chat_id": chat_id,
            "sender": {"uri": ME if from_me else sender},
            "sent_at": sent_at,
            "text": text,
            "parts": parts,
            "subject": None,
            "reply_to": reply_to,
            "is_from_me": from_me,
            "delivery": delivery,
            "edited": edited,
            "unsent": False,
            "reactions": reactions or [],
            "effect": None,
            "verified": verified,
            "_seq": self.seq,
        }
        self.messages.setdefault(chat_id, []).append(m)
        return m

    def _seed(self, now):
        minute = 60_000
        hour = 60 * minute
        day = 24 * hour

        bob = "mailto:bob@example.com"
        self.chats[bob] = {"id": bob, "participants": [{"uri": bob, "display": "Bob Smith"}],
                           "display_name": None, "is_group": False, "last_message_at": None, "unread_count": 0}
        # 30 messages spread over three days for paging tests.
        start = now - 2 * day - 3 * hour
        for i in range(30):
            from_me = i % 3 == 1
            t = start + i * 97 * minute if i < 27 else now - (30 - i) * 2 * minute
            text = f"Bob message {i:02d}" if not from_me else f"My reply {i:02d}"
            if i == 27:
                text = "Did you see the photos from the trip?"
            if i == 28:
                text = "Yes! They look amazing 😍 https://example.com/trip"
            self._msg(bob, bob, t, text, from_me=from_me, delivery="read" if from_me else "delivered")
        last = self._msg(bob, bob, now - 60_000, "Here's the best one", parts=[
            {"type": "text", "text": "Here's the best one"},
            {"type": "attachment", "attachment_id": "", "mime": "image/png", "filename": "beach.png", "size": 48213},
        ])
        last["parts"][1]["attachment_id"] = last["id"] + ":1"
        last["reactions"] = [{"kind": "love", "sender": {"uri": ME}, "added": True}]

        team = "group:5B0E2C1A-3F4D-4E21-9A55-0C7E1B2D3F40"
        carol = "mailto:carol@example.com"
        dave = "tel:+15551234567"
        self.chats[team] = {"id": team, "participants": [{"uri": bob, "display": "Bob Smith"}, {"uri": carol},
                                                          {"uri": dave}],
                            "display_name": "Weekend Trip", "is_group": True, "last_message_at": None,
                            "unread_count": 0}
        t0 = now - 5 * hour
        self._msg(team, carol, t0, "Who's bringing the tent?")
        self._msg(team, carol, t0 + minute, "I can bring snacks")
        self._msg(team, dave, t0 + 2 * minute, "I have a tent for 4", reactions=[
            {"kind": "like", "sender": {"uri": carol}, "added": True},
            {"kind": "like", "sender": {"uri": bob}, "added": True},
            {"kind": {"emoji": "🔥"}, "sender": {"uri": ME}, "added": True}])
        self._msg(team, None, t0 + 3 * minute, "Perfect, I'll book the site", from_me=True, delivery="delivered")
        self._msg(team, None, t0 + 4 * minute, "Leaving Friday at 6", from_me=True, delivery="delivered",
                  edited=True)
        self._msg(team, carol, now - 20 * minute, "Here's the packing list", parts=[
            {"type": "text", "text": "Here's the packing list"},
            {"type": "attachment", "attachment_id": "", "mime": "application/pdf", "filename": "packing-list.pdf",
             "size": 120334}])
        pl = self.messages[team][-1]
        pl["parts"][1]["attachment_id"] = pl["id"] + ":1"
        self._msg(team, carol, now - 19 * minute, "@Dave don't forget the stove", parts=[
            {"type": "mention", "handle": dave, "text": "@Dave"},
            {"type": "text", "text": " don't forget the stove"}])
        self.chats[team]["unread_count"] = 2

        self.chats[dave] = {"id": dave, "participants": [{"uri": dave}], "display_name": None, "is_group": False,
                            "last_message_at": None, "unread_count": 1}
        self._msg(dave, dave, now - 3 * day, "Call me when you land", verified=False)
        um = self._msg(dave, None, now - 3 * day + hour, None, from_me=True, delivery="delivered")
        um["unsent"] = True
        self._msg(dave, dave, now - 2 * day, "Sure thing", verified=False)

        eve = "mailto:eve@example.org"
        self.chats[eve] = {"id": eve, "participants": [{"uri": eve, "display": "Eve Adams"}], "display_name": None,
                           "is_group": False, "last_message_at": None, "unread_count": 0}
        self._msg(eve, None, now - 9 * day, "Happy birthday! 🎉", from_me=True, delivery="read")
        self._msg(eve, eve, now - 9 * day + 30 * minute, "Thank you!!")
        self._msg(eve, None, now - 9 * day + 31 * minute, "", from_me=True, delivery="failed",
                  parts=[{"type": "unsupported", "description": "app message"}])
        self.messages[eve][-1]["text"] = None

        for cid in self.chats:
            self.touch(cid)

    def touch(self, cid):
        msgs = self.messages.get(cid, [])
        self.chats[cid]["last_message_at"] = max((m["sent_at"] for m in msgs), default=None)

    def unread_total(self):
        return sum(c["unread_count"] for c in self.chats.values())

    def find(self, message_id):
        for msgs in self.messages.values():
            for m in msgs:
                if m["id"] == message_id:
                    return m
        return None


def public(m):
    return {k: v for k, v in m.items() if not k.startswith("_")}


def dumps(v):
    return json.dumps(v, ensure_ascii=False)


class FakeDaemon(dbus.service.Object):
    def __init__(self, bus, args):
        super().__init__(bus, PATH)
        self.args = args
        self.state = args.state
        self.store = Store(int(time.time() * 1000), with_data=not args.empty)
        self.calls = []
        self.status_delay_ms = 0
        self.unreadable = []
        self.other_handles = []  # SetOtherHandles: registration.other_handles
        self.send_file_delay_ms = 0
        self.download_no_reply = 0  # FailDownloadsWithNoReply  # SetUnreadablePrefixes: paths the "sandboxed" daemon cannot see
        self.pending_2fa = False
        self.profiles = {}  # SetProfile: handle -> HandleProfile dict
        self.tmp = tempfile.mkdtemp(prefix="kmessages-fake-")

    # -- helpers ----------------------------------------------------------
    def log(self, method, *args):
        self.calls.append([method] + [a if isinstance(a, (bool, int)) else str(a) for a in args])

    def require_login(self):
        if self.state in ("needs_login", "needs_two_factor"):
            raise Error("NotLoggedIn", "not logged in")

    def set_state(self, state):
        if state == self.state:
            return
        self.state = state
        self.ConnectionStateChanged(state)
        self.PropertiesChanged(IFACE, {"ConnectionState": dbus.String(state),
                                       "Registered": dbus.Boolean(state == "connected")}, [])

    def status(self):
        conn = {"state": self.state}
        if self.state == "error":
            conn["detail"] = "registration failing, retrying: fake"
        registered = self.state == "connected"
        return {"version": VERSION, "connection": conn, "registered": registered,
                "registration": {"handles": [{"uri": ME}] if registered else [],
                                 "registered_at": 1700000000 if registered else None,
                                 "expires_at": 1702592000 if registered else None,
                                 "sender": {"uri": ME} if registered else None,
                                 "other_handles": list(self.other_handles)},
                "unread": self.store.unread_total(),
                "profiles": {"state": "ready"}}

    def chat(self, cid):
        c = self.store.chats.get(cid)
        if c is None:
            raise Error("NotFound", f"not found: chat {cid}")
        return c

    def resolve_target(self, chat_id, participants_json):
        if chat_id:
            return self.chat(chat_id)["id"]
        try:
            parts = json.loads(participants_json)
        except ValueError as e:
            raise Error("InvalidInput", f"bad participants json: {e}")
        handles = []
        for p in parts:
            n = normalize(p)
            if n is None:
                raise Error("InvalidHandle", f"invalid handle: {p}")
            handles.append(n)
        if not handles:
            raise Error("InvalidInput", "no participants")
        if len(handles) == 1:
            cid = handles[0]
            if cid not in self.store.chats:
                self.store.chats[cid] = {"id": cid, "participants": [{"uri": cid}], "display_name": None,
                                         "is_group": False, "last_message_at": None, "unread_count": 0}
            return cid
        for c in self.store.chats.values():
            if c["is_group"] and sorted(p["uri"] for p in c["participants"]) == sorted(handles):
                return c["id"]
        cid = "group:" + guid()
        self.store.chats[cid] = {"id": cid, "participants": [{"uri": h} for h in handles], "display_name": None,
                                 "is_group": True, "last_message_at": None, "unread_count": 0}
        return cid

    def announce_new(self, m):
        cid = m["chat_id"]
        self.store.touch(cid)
        self.MessageReceived(cid, dumps(public(m)))
        self.ChatUpdated(dumps(self.store.chats[cid]))

    # -- org.imessage.Daemon1 methods ---------------------------------------
    @dbus.service.method(IFACE, in_signature="ss", out_signature="s")
    def Login(self, apple_id, password):
        self.log("Login", apple_id)
        if password == "wrong":
            raise Error("Login", "Your Apple ID or password was incorrect.")
        if "2fa" in apple_id:
            self.pending_2fa = True
            self.set_state("needs_two_factor")
            GLib.idle_add(lambda: self.TwoFactorRequired() and False)
            return "two_factor_required"
        self.set_state("connected")
        return "ok"

    @dbus.service.method(IFACE, in_signature="s", out_signature="s")
    def SubmitTwoFactor(self, code):
        self.log("SubmitTwoFactor")
        if not self.pending_2fa:
            raise Error("NoPendingTwoFactor", "no login is waiting for a code")
        if code == "000000":
            raise Error("BadTwoFactorCode", "Incorrect verification code.")
        self.pending_2fa = False
        self.set_state("connected")
        return "ok"

    @dbus.service.method(IFACE, in_signature="", out_signature="")
    def Logout(self):
        self.log("Logout")
        self.set_state("needs_login")

    @dbus.service.method(IFACE, in_signature="", out_signature="s",
                         async_callbacks=("reply", "error"))
    def GetStatus(self, reply, error):
        if self.status_delay_ms <= 0:
            reply(dumps(self.status()))
            return
        # SetStatusDelay: answer later (other calls are served meanwhile).
        GLib.timeout_add(self.status_delay_ms, lambda: reply(dumps(self.status())) or False)

    @dbus.service.method(IFACE, in_signature="uu", out_signature="s")
    def ListChats(self, limit, offset):
        self.log("ListChats", int(limit), int(offset))
        limit = 50 if limit == 0 else min(int(limit), 500)
        chats = sorted(self.store.chats.values(), key=lambda c: c["last_message_at"] or 0, reverse=True)
        return dumps(chats[offset:offset + limit])

    @dbus.service.method(IFACE, in_signature="s", out_signature="s")
    def GetChat(self, chat_id):
        return dumps(self.chat(chat_id))

    @dbus.service.method(IFACE, in_signature="ssu", out_signature="s")
    def ListMessages(self, chat_id, before_cursor, limit):
        self.log("ListMessages", chat_id, before_cursor, int(limit))
        self.chat(chat_id)
        limit = 50 if limit == 0 else min(int(limit), 500)
        msgs = sorted(self.store.messages.get(chat_id, []), key=lambda m: (m["sent_at"], m["_seq"]))
        if before_cursor:
            try:
                t, s = (int(x) for x in before_cursor.split("-"))
            except ValueError:
                raise Error("InvalidInput", f"bad cursor {before_cursor}")
            msgs = [m for m in msgs if (m["sent_at"], m["_seq"]) < (t, s)]
        page = msgs[-limit:]
        more = len(msgs) > len(page)
        cursor = f"{page[0]['sent_at']}-{page[0]['_seq']}" if page and more else None
        return dumps({"messages": [public(m) for m in page], "next_cursor": cursor})

    @dbus.service.method(IFACE, in_signature="su", out_signature="s")
    def Search(self, query, limit):
        self.log("Search", query)
        q = query.lower()
        hits = [m for msgs in self.store.messages.values() for m in msgs
                if not m["unsent"] and q and q in (m["text"] or "").lower()]
        hits.sort(key=lambda m: m["sent_at"], reverse=True)
        return dumps([public(m) for m in hits[: (limit or 50)]])

    @dbus.service.method(IFACE, in_signature="ssss", out_signature="s")
    def SendText(self, chat_id, participants_json, text, reply_to):
        self.log("SendText", chat_id, participants_json, text, reply_to)
        self.require_login()
        if text == "__fail__":
            raise Error("Transient", "fake send failure")
        cid = self.resolve_target(chat_id, participants_json)
        m = self.store._msg(cid, None, int(time.time() * 1000), text, from_me=True, delivery="sent",
                            reply_to=reply_to or None)
        self.announce_new(m)
        return dumps({"id": m["id"], "chat_id": cid, "sent_at": m["sent_at"], "sender": {"uri": ME}})

    @dbus.service.method(IFACE, in_signature="ssss", out_signature="s",
                         async_callbacks=("reply", "error"))
    def SendFile(self, chat_id, participants_json, path, mime, reply, error):
        try:
            result = self.send_file(chat_id, participants_json, path, mime)
        except dbus.DBusException as e:
            error(e)
            return
        if self.send_file_delay_ms <= 0:
            reply(result)
            return
        # SetSendFileDelay: MessageReceived now, the reply later.
        GLib.timeout_add(self.send_file_delay_ms, lambda: reply(result) or False)

    def send_file(self, chat_id, participants_json, path, mime):
        self.log("SendFile", chat_id, participants_json, path, mime)
        self.require_login()
        if any(path.startswith(p) for p in self.unreadable):
            # What the real daemon says behind PrivateTmp / ProtectSystem.
            raise Error("InvalidInput", f"{path}: No such file or directory (os error 2)")
        if not os.path.isabs(path) or not os.path.isfile(path):
            raise Error("InvalidInput", f"not a regular file: {path}")
        cid = self.resolve_target(chat_id, participants_json)
        m = self.store._msg(cid, None, int(time.time() * 1000), None, from_me=True, delivery="sent", parts=[])
        m["parts"] = [{"type": "attachment", "attachment_id": m["id"] + ":0", "mime": mime or "application/octet-stream",
                       "filename": os.path.basename(path), "size": os.path.getsize(path)}]
        m["_local"] = path
        self.announce_new(m)
        return dumps({"id": m["id"], "chat_id": cid, "sent_at": m["sent_at"], "sender": {"uri": ME}})

    @dbus.service.method(IFACE, in_signature="sssb", out_signature="")
    def React(self, chat_id, message_id, kind, remove):
        self.log("React", chat_id, message_id, kind, bool(remove))
        m = self.store.find(message_id)
        if m is None or m["chat_id"] != chat_id:
            raise Error("NotFound", f"not found: message {message_id}")
        classic = ("love", "like", "dislike", "laugh", "emphasize", "question")
        if kind in classic:
            k = kind
        elif kind.startswith("emoji:") and len(kind) > 6:
            k = {"emoji": kind[6:]}
        elif kind and not kind.isascii():
            k = {"emoji": kind}
        else:
            raise Error("InvalidInput", f"bad reaction kind {kind}")
        m["reactions"] = [r for r in m["reactions"] if r["sender"]["uri"] != ME]
        if not remove:
            m["reactions"].append({"kind": k, "sender": {"uri": ME}, "added": True})
        self.MessageUpdated(chat_id, dumps(public(m)))

    @dbus.service.method(IFACE, in_signature="sss", out_signature="")
    def Edit(self, chat_id, message_id, text):
        self.log("Edit", chat_id, message_id, text)
        m = self.store.find(message_id)
        if m is None:
            raise Error("NotFound", f"not found: message {message_id}")
        if not m["is_from_me"]:
            raise Error("InvalidInput", "can only edit own messages")
        m["text"] = text
        m["parts"] = [p for p in m["parts"] if p["type"] not in ("text", "mention")] + [{"type": "text", "text": text}]
        m["edited"] = True
        self.MessageUpdated(chat_id, dumps(public(m)))

    @dbus.service.method(IFACE, in_signature="ss", out_signature="")
    def Unsend(self, chat_id, message_id):
        self.log("Unsend", chat_id, message_id)
        m = self.store.find(message_id)
        if m is None:
            raise Error("NotFound", f"not found: message {message_id}")
        if not m["is_from_me"]:
            raise Error("InvalidInput", "can only unsend own messages")
        m.update(text=None, parts=[], reactions=[], unsent=True)
        self.MessageUpdated(chat_id, dumps(public(m)))
        self.ChatUpdated(dumps(self.store.chats[chat_id]))

    @dbus.service.method(IFACE, in_signature="sb", out_signature="")
    def SetTyping(self, chat_id, typing):
        self.log("SetTyping", chat_id, bool(typing))

    @dbus.service.method(IFACE, in_signature="ss", out_signature="")
    def MarkRead(self, chat_id, up_to):
        self.log("MarkRead", chat_id, up_to)
        c = self.chat(chat_id)
        if up_to:
            # Incoming messages up to and including `up_to` become read.
            incoming = [m for m in self.store.messages.get(chat_id, []) if not m["is_from_me"]]
            ids = [m["id"] for m in incoming]
            if up_to not in ids:
                raise Error("NotFound", up_to)
            c["unread_count"] = min(c["unread_count"], len(ids) - ids.index(up_to) - 1)
        elif c["unread_count"]:
            c["unread_count"] = 0
        self.ChatUpdated(dumps(c))

    @dbus.service.method(IFACE, in_signature="s", out_signature="s")
    def LookupHandles(self, handles_json):
        self.log("LookupHandles", handles_json)
        try:
            handles = json.loads(handles_json)
        except ValueError as e:
            raise Error("InvalidInput", str(e))
        out = []
        for h in handles:
            n = normalize(h)
            if n is None:
                raise Error("InvalidHandle", f"invalid handle: {h}")
            out.append({"handle": n, "reachable": "android" not in n})
        return dumps(out)

    @dbus.service.method(IFACE, in_signature="s", out_signature="s")
    def DownloadAttachment(self, attachment_id):
        self.log("DownloadAttachment", attachment_id)
        if self.download_no_reply > 0:
            self.download_no_reply -= 1
            raise dbus.DBusException("did not receive a reply (fake)", name="org.freedesktop.DBus.Error.NoReply")
        mid, _, _ = attachment_id.rpartition(":")
        m = self.store.find(mid)
        part = None
        if m is not None:
            part = next((p for p in m["parts"] if p.get("attachment_id") == attachment_id), None)
        if part is None:
            raise Error("NotFound", f"not found: attachment {attachment_id}")
        if m.get("_local"):
            return m["_local"]
        d = os.path.join(self.tmp, attachment_id.replace(":", "_"))
        os.makedirs(d, exist_ok=True)
        path = os.path.join(d, part["filename"])
        if not os.path.exists(path):
            with open(path, "wb") as f:
                f.write(png_bytes() if part["mime"].startswith("image/") else b"%PDF-1.4\n% fake\n")
        return path

    @dbus.service.method(IFACE, in_signature="s", out_signature="s")
    def GetHandleProfile(self, handle):
        self.log("GetHandleProfile", handle)
        n = normalize(handle)
        if n is None:
            raise Error("InvalidHandle", f"invalid handle: {handle}")
        return dumps(self.profiles.get(n))

    @dbus.service.method(IFACE, in_signature="", out_signature="s")
    def Version(self):
        return VERSION

    # -- properties -----------------------------------------------------------
    def props(self):
        return {"ConnectionState": dbus.String(self.state), "Registered": dbus.Boolean(self.state == "connected")}

    @dbus.service.method(PROPS, in_signature="ss", out_signature="v")
    def Get(self, interface, name):
        p = self.props()
        if interface != IFACE or name not in p:
            raise dbus.DBusException("no such property", name="org.freedesktop.DBus.Error.UnknownProperty")
        return p[name]

    @dbus.service.method(PROPS, in_signature="s", out_signature="a{sv}")
    def GetAll(self, interface):
        return self.props() if interface == IFACE else {}

    @dbus.service.method(PROPS, in_signature="ssv", out_signature="")
    def Set(self, interface, name, value):
        raise dbus.DBusException("read-only", name="org.freedesktop.DBus.Error.PropertyReadOnly")

    @dbus.service.signal(PROPS, signature="sa{sv}as")
    def PropertiesChanged(self, interface, changed, invalidated):
        pass

    # -- signals ----------------------------------------------------------------
    @dbus.service.signal(IFACE, signature="ss")
    def MessageReceived(self, chat_id, message_json):
        pass

    @dbus.service.signal(IFACE, signature="ss")
    def MessageUpdated(self, chat_id, message_json):
        pass

    @dbus.service.signal(IFACE, signature="s")
    def ChatUpdated(self, chat_json):
        pass

    @dbus.service.signal(IFACE, signature="ssb")
    def Typing(self, chat_id, handle, typing):
        pass

    @dbus.service.signal(IFACE, signature="s")
    def ConnectionStateChanged(self, state):
        pass

    @dbus.service.signal(IFACE, signature="")
    def TwoFactorRequired(self):
        pass

    @dbus.service.signal(IFACE, signature="ss")
    def HandleProfileUpdated(self, handle, profile_json):
        pass

    @dbus.service.signal(IFACE, signature="s")
    def BackgroundError(self, message):
        pass

    # -- org.imessage.FakeControl (tests only) ------------------------------------
    @dbus.service.method(CTRL, in_signature="sss", out_signature="s")
    def InjectMessage(self, chat_id, sender, text):
        """Incoming message from `sender` (a URI); returns its id."""
        self.chat(chat_id)
        m = self.store._msg(chat_id, sender, int(time.time() * 1000), text)
        self.store.chats[chat_id]["unread_count"] += 1
        self.announce_new(m)
        return m["id"]

    @dbus.service.method(CTRL, in_signature="sssb", out_signature="s")
    def InjectMessageVerified(self, chat_id, sender, text, verified):
        """InjectMessage with an explicit `verified` flag."""
        self.chat(chat_id)
        m = self.store._msg(chat_id, sender, int(time.time() * 1000), text, verified=bool(verified))
        self.store.chats[chat_id]["unread_count"] += 1
        self.announce_new(m)
        return m["id"]

    @dbus.service.method(CTRL, in_signature="sb", out_signature="")
    def SetBackground(self, chat_id, on):
        """Give the chat a transcript background (a gradient PNG) or remove it."""
        c = self.chat(chat_id)
        if on:
            d = os.path.join(self.tmp, "backgrounds")
            os.makedirs(d, exist_ok=True)
            bid = guid()
            path = os.path.join(d, bid + ".png")
            with open(path, "wb") as f:
                f.write(png_bytes(360, 640))
            c["background"] = {"id": bid, "path": path, "payload": path, "version": 1,
                               "set_by": "mailto:bob@example.com", "set_at": int(time.time() * 1000)}
        else:
            c["background"] = None
        self.ChatUpdated(dumps(c))

    @dbus.service.method(CTRL, in_signature="sssb", out_signature="")
    def SetProfile(self, handle, name, first, photo):
        """`handle` shares `name` (and a photo, a generated PNG) like the real
        daemon after a fetch: HandleProfileUpdated, then ChatUpdated for every
        chat with that participant. An empty name and no photo = stopped."""
        profile = None
        if name or first or photo:
            profile = {"name": name or None, "first": first or None, "last": None, "photo": None,
                       "updated_at": int(time.time() * 1000)}
            if photo:
                d = os.path.join(self.tmp, "profiles")
                os.makedirs(d, exist_ok=True)
                path = os.path.join(d, guid() + ".png")
                with open(path, "wb") as f:
                    f.write(png_bytes(96, 96))
                profile["photo"] = path
            self.profiles[handle] = profile
        else:
            self.profiles.pop(handle, None)
        for c in self.store.chats.values():
            for p in c["participants"]:
                if p["uri"] == handle:
                    if profile:
                        p["profile"] = profile
                    else:
                        p.pop("profile", None)
        self.HandleProfileUpdated(handle, dumps(profile))
        for c in self.store.chats.values():
            if any(p["uri"] == handle for p in c["participants"]):
                self.ChatUpdated(dumps(c))

    @dbus.service.method(CTRL, in_signature="sb", out_signature="")
    def SetGroupIcon(self, chat_id, on):
        """Give the group a photo (a generated PNG) or remove it."""
        c = self.chat(chat_id)
        if on:
            d = os.path.join(self.tmp, "chat_icons")
            os.makedirs(d, exist_ok=True)
            path = os.path.join(d, guid() + ".png")
            with open(path, "wb") as f:
                f.write(png_bytes(128, 128))
            c["icon_path"] = path
        else:
            c.pop("icon_path", None)
        self.ChatUpdated(dumps(c))

    @dbus.service.method(CTRL, in_signature="ssb", out_signature="")
    def InjectTyping(self, chat_id, handle, typing):
        self.Typing(chat_id, handle, typing)

    @dbus.service.method(CTRL, in_signature="sss", out_signature="")
    def InjectReaction(self, message_id, kind, sender):
        m = self.store.find(message_id)
        if m is None:
            raise Error("NotFound", message_id)
        k = {"emoji": kind[6:]} if kind.startswith("emoji:") else kind
        m["reactions"] = [r for r in m["reactions"] if r["sender"]["uri"] != sender]
        m["reactions"].append({"kind": k, "sender": {"uri": sender}, "added": True})
        self.MessageUpdated(m["chat_id"], dumps(public(m)))

    @dbus.service.method(CTRL, in_signature="ss", out_signature="")
    def SetDelivery(self, message_id, delivery):
        m = self.store.find(message_id)
        if m is None:
            raise Error("NotFound", message_id)
        m["delivery"] = delivery
        self.MessageUpdated(m["chat_id"], dumps(public(m)))

    @dbus.service.method(CTRL, in_signature="s", out_signature="")
    def SetConnectionState(self, state):
        self.set_state(state)

    @dbus.service.method(CTRL, in_signature="u", out_signature="")
    def SetStatusDelay(self, ms):
        self.status_delay_ms = int(ms)

    @dbus.service.method(CTRL, in_signature="u", out_signature="")
    def FailDownloadsWithNoReply(self, n):
        """The next `n` DownloadAttachment calls fail as if they had timed out."""
        self.download_no_reply = int(n)

    @dbus.service.method(CTRL, in_signature="u", out_signature="")
    def SetSendFileDelay(self, ms):
        self.send_file_delay_ms = int(ms)

    @dbus.service.method(CTRL, in_signature="as", out_signature="")
    def SetOtherHandles(self, uris):
        """`registration.other_handles` in GetStatus (no signal: clients refresh)."""
        self.other_handles = [str(u) for u in uris]

    @dbus.service.method(CTRL, in_signature="as", out_signature="")
    def SetUnreadablePrefixes(self, prefixes):
        """SendFile fails like the sandboxed daemon for paths under these."""
        self.unreadable = [str(p) for p in prefixes]

    @dbus.service.method(CTRL, in_signature="sb", out_signature="")
    def SetStateViaProperties(self, state, invalidate):
        """Change the state announcing it only through PropertiesChanged:
        with the new value, or (invalidate) as invalidated property names."""
        self.state = state
        if invalidate:
            self.PropertiesChanged(IFACE, dbus.Dictionary({}, signature="sv"), ["ConnectionState", "Registered"])
        else:
            self.PropertiesChanged(IFACE, {"ConnectionState": dbus.String(state),
                                           "Registered": dbus.Boolean(state == "connected")}, [])

    @dbus.service.method(CTRL, in_signature="s", out_signature="")
    def EmitBackgroundError(self, message):
        self.BackgroundError(message)

    @dbus.service.method(CTRL, in_signature="", out_signature="s")
    def Calls(self):
        return dumps(self.calls)

    @dbus.service.method(CTRL, in_signature="", out_signature="")
    def ClearCalls(self):
        self.calls = []

    @dbus.service.method(CTRL, in_signature="", out_signature="")
    def Quit(self):
        GLib.idle_add(loop.quit)


loop = None


def main():
    global loop
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--state", default="connected",
                    choices=["disconnected", "connecting", "connected", "needs_login", "needs_two_factor",
                             "registering", "error"])
    ap.add_argument("--empty", action="store_true", help="start with an empty store")
    args = ap.parse_args()
    dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
    bus = dbus.SessionBus()
    daemon = FakeDaemon(bus, args)
    # Claim the name only after the object is exported (like the real daemon).
    name = dbus.service.BusName(BUS_NAME, bus, do_not_queue=True)
    loop = GLib.MainLoop()
    try:
        from gi.repository import GLibUnix
        signal_add = GLibUnix.signal_add
    except ImportError:
        signal_add = GLib.unix_signal_add
    for sig in (2, 15):
        signal_add(GLib.PRIORITY_DEFAULT, sig, lambda: loop.quit() or False)
    print("fake imessaged ready", flush=True)
    loop.run()
    del name, daemon


if __name__ == "__main__":
    sys.exit(main())
