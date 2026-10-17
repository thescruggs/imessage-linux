-- imessaged message store, schema version 1.
-- Timestamps are ms since the Unix epoch (imessage_common::model).

CREATE TABLE handles (
    uri      TEXT PRIMARY KEY,          -- normalized IDS URI (mailto:/tel:)
    display  TEXT
);

CREATE TABLE chats (
    id               TEXT PRIMARY KEY,  -- model::ChatId
    display_name     TEXT,
    is_group         INTEGER NOT NULL DEFAULT 0,
    last_message_at  INTEGER,
    unread_count     INTEGER NOT NULL DEFAULT 0,  -- denormalized; see Store::refresh_unread
    name_at          INTEGER,           -- `at` of the rename that set display_name
    participants_at  INTEGER            -- `at` of the membership change that set participants
);

-- Everyone in the chat except the local user.
CREATE TABLE chat_participants (
    chat_id  TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    handle   TEXT NOT NULL REFERENCES handles(uri),
    position INTEGER NOT NULL,
    PRIMARY KEY (chat_id, handle)
);

CREATE TABLE messages (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,  -- insertion order; cursor tie-breaker
    id          TEXT NOT NULL UNIQUE,               -- model::MessageId
    chat_id     TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    sender      TEXT NOT NULL,
    sent_at     INTEGER NOT NULL,
    text        TEXT,
    parts       TEXT NOT NULL,                      -- JSON array of model::MessagePart
    subject     TEXT,
    reply_to    TEXT,
    is_from_me  INTEGER NOT NULL,
    delivery    TEXT NOT NULL,                      -- model::DeliveryState (snake_case)
    error       TEXT,                               -- send failure reason
    edited      INTEGER NOT NULL DEFAULT 0,
    edited_at   INTEGER,
    unsent      INTEGER NOT NULL DEFAULT 0,
    verified    INTEGER NOT NULL DEFAULT 1,
    effect      TEXT,
    is_read     INTEGER NOT NULL DEFAULT 0          -- local read state (incoming messages)
);
CREATE INDEX messages_chat_order ON messages(chat_id, sent_at, seq);
CREATE INDEX messages_unread ON messages(chat_id, is_read) WHERE is_read = 0 AND is_from_me = 0;

CREATE TABLE attachments (
    id          TEXT PRIMARY KEY,                   -- model::AttachmentId (<message id>:<part>)
    message_id  TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    mime        TEXT NOT NULL,
    filename    TEXT NOT NULL,
    size        INTEGER NOT NULL,
    local_path  TEXT,
    locator     BLOB                                -- download locator sealed with the state key (state::seal_bytes)
);
CREATE INDEX attachments_message ON attachments(message_id);

-- One tapback per sender per message (a new one replaces the old).
-- Removals are kept as added = 0 tombstones so a late, older "add" cannot
-- resurrect a removed reaction. No FK: reactions may arrive before the
-- message they target.
CREATE TABLE reactions (
    message_id  TEXT NOT NULL,
    sender      TEXT NOT NULL,
    kind        TEXT NOT NULL,                      -- JSON of model::ReactionKind
    added       INTEGER NOT NULL,
    at          INTEGER NOT NULL,
    PRIMARY KEY (message_id, sender)
);
