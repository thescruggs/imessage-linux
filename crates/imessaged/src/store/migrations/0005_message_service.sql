-- imessaged message store, schema version 5: how a message travelled.
-- 'imessage', or 'sms' for text messages (SMS / MMS / RCS) relayed by the
-- user's iPhone (Text Message Forwarding). Older rows are iMessage.
ALTER TABLE messages ADD COLUMN service TEXT NOT NULL DEFAULT 'imessage';
