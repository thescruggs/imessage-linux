-- imessaged message store, schema version 3: the paintable image extracted
-- from a background payload (see src/background.rs).

ALTER TABLE chat_backgrounds ADD COLUMN image_path TEXT;
