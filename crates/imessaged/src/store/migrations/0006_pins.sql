-- imessaged message store, schema version 6: pinned conversations.
-- Local to this computer (Messages' own pins sync through CloudKit, which
-- we do not); `imsg sync-mac` copies the Mac's. NULL = not pinned.
ALTER TABLE chats ADD COLUMN pinned_at INTEGER;   -- ms when it was pinned
ALTER TABLE chats ADD COLUMN pin_order INTEGER;   -- position among the pins, 0 = first
