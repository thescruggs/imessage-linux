-- imessaged message store, schema version 2: transcript backgrounds.

-- At most one background per chat (the newest one announced).
CREATE TABLE chat_backgrounds (
    chat_id          TEXT PRIMARY KEY REFERENCES chats(id) ON DELETE CASCADE,
    background_id    TEXT NOT NULL,     -- Apple's id (trabaid)
    payload_version  INTEGER NOT NULL,  -- trabapv
    seq              INTEGER NOT NULL,  -- sender's sequence number (tbid)
    size             INTEGER NOT NULL,
    locator          BLOB,              -- download locator, sealed with the state key
    local_path       TEXT,              -- downloaded payload, if any
    set_by           TEXT,
    set_at           INTEGER NOT NULL   -- `at` of the change
);

-- `at` of the newest removal per chat, so a late "set" older than it is ignored.
ALTER TABLE chats ADD COLUMN background_removed_at INTEGER;
