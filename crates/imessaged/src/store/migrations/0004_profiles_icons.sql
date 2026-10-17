-- imessaged message store, schema version 4: contacts' shared names and
-- photos ("Share Name and Photo") and group photos.

-- One row per handle that shared a profile with us. The CloudKit record
-- key names the record, the decryption key opens it (sealed with the state
-- key like attachment locators). A contact who stops sharing keeps the row
-- with record_key / name / photo cleared (shared_at orders the changes).
CREATE TABLE handle_profiles (
    handle          TEXT PRIMARY KEY,
    record_key      TEXT,               -- CloudKit record name; NULL = not shared (any more)
    decryption_key  BLOB,               -- sealed
    poster          BLOB,               -- sealed contact-poster tags (plist), if announced; not fetched
    name            TEXT,               -- display name (dn)
    first           TEXT,
    last            TEXT,
    photo_path      TEXT,               -- downloaded photo under profiles/
    shared_at       INTEGER NOT NULL,   -- `at` of the newest share / unshare
    fetched_at      INTEGER,            -- last successful fetch of record_key (ms)
    fetched_key     TEXT,               -- record_key that fetch was for
    last_seen_at    INTEGER NOT NULL    -- last message that carried the share
);

-- The group photo: MMCS reference (sealed) of the newest change, its file.
ALTER TABLE chats ADD COLUMN icon_id TEXT;
ALTER TABLE chats ADD COLUMN icon_locator BLOB;
ALTER TABLE chats ADD COLUMN icon_size INTEGER;
ALTER TABLE chats ADD COLUMN icon_path TEXT;
ALTER TABLE chats ADD COLUMN icon_at INTEGER;
