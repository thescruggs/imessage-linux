//! Turning user arguments into daemon arguments: chat ids, handles,
//! reaction kinds, MIME types. The matching logic is pure (unit-tested);
//! [`resolve_chat`] / [`resolve_target`] add the daemon lookups.

use imessage_common::model::{Chat, Handle};

use crate::client::{CliError, Client, Result, Target, exit};
use crate::format::chat_name;

/// What a chat argument looks like before asking the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatArg {
    /// `group:…` / `group-p:…`: used verbatim.
    GroupId(String),
    /// An email address / phone number (1:1 chat id = its normalized URI).
    Handle(String),
    /// Anything else: matched against chat names and id prefixes.
    Name(String),
}

pub fn classify(arg: &str) -> ChatArg {
    let a = arg.trim();
    if a.starts_with("group:") || a.starts_with("group-p:") {
        ChatArg::GroupId(a.to_string())
    } else if let Some(uri) = Handle::try_normalize(a) {
        ChatArg::Handle(uri)
    } else {
        ChatArg::Name(a.to_string())
    }
}

/// Chats whose name equals `query` (case-insensitive); if none, chats whose
/// id or name starts with it, or whose group GUID does.
pub fn match_chats<'a>(query: &str, chats: &'a [Chat]) -> Vec<&'a Chat> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return vec![];
    }
    let exact: Vec<&Chat> = chats.iter().filter(|c| chat_name(c).to_lowercase() == q || c.id.0.to_lowercase() == q).collect();
    if !exact.is_empty() {
        return exact;
    }
    chats
        .iter()
        .filter(|c| {
            let id = c.id.0.to_lowercase();
            id.starts_with(&q) || c.id.group_guid().is_some_and(|g| g.to_lowercase().starts_with(&q)) || chat_name(c).to_lowercase().starts_with(&q)
        })
        .collect()
}

fn one_of(query: &str, found: Vec<&Chat>) -> Result<String> {
    match found.as_slice() {
        [] => Err(CliError::new(exit::NOT_FOUND, format!("no conversation matches {query:?} (see `imsg chats`)"))),
        [c] => Ok(c.id.0.clone()),
        many => {
            let list: Vec<String> = many.iter().take(10).map(|c| format!("  {}  {}", c.id, chat_name(c))).collect();
            Err(CliError::usage(format!("{query:?} matches {} conversations; use the id:\n{}", many.len(), list.join("\n"))))
        }
    }
}

/// Resolve a chat argument to an existing chat id.
pub async fn resolve_chat(client: &Client, arg: &str) -> Result<String> {
    match classify(arg) {
        ChatArg::GroupId(id) => match client.get_chat(&id).await? {
            Some(_) => Ok(id),
            None => {
                let chats = client.all_chats().await?;
                one_of(arg, match_chats(&id, &chats))
            }
        },
        ChatArg::Handle(uri) => match client.get_chat(&uri).await? {
            Some(_) => Ok(uri),
            None => Err(CliError::new(exit::NOT_FOUND, format!("no conversation with {} yet", Handle::new(&uri).address()))),
        },
        ChatArg::Name(name) => {
            let chats = client.all_chats().await?;
            one_of(&name, match_chats(&name, &chats))
        }
    }
}

/// Resolve a chat argument for an action that also works on a 1:1 chat
/// that has no stored history yet (typing): handles are used as-is.
pub async fn resolve_chat_or_handle(client: &Client, arg: &str) -> Result<String> {
    match classify(arg) {
        ChatArg::Handle(uri) => Ok(uri),
        _ => resolve_chat(client, arg).await,
    }
}

/// Split a send recipient argument (`a@x.com,+1555…` or a chat).
pub fn split_recipients(arg: &str) -> Vec<String> {
    arg.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// Recipients for `send` / `send-file`: one chat (id or name), or one or
/// more handles (several = a group, new or existing).
pub async fn resolve_target(client: &Client, arg: &str) -> Result<Target> {
    let parts = split_recipients(arg);
    match parts.as_slice() {
        [] => Err(CliError::usage("no recipient given")),
        [one] => match classify(one) {
            ChatArg::Handle(_) => Ok(Target::Participants(vec![one.clone()])),
            _ => Ok(Target::Chat(resolve_chat(client, one).await?)),
        },
        many => {
            let bad: Vec<&String> = many.iter().filter(|h| Handle::try_normalize(h).is_none()).collect();
            if !bad.is_empty() {
                return Err(CliError::usage(format!(
                    "not an email address or phone number: {}",
                    bad.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                )));
            }
            Ok(Target::Participants(many.to_vec()))
        }
    }
}

/// Validate a reaction kind and return what `React` expects. Accepts the
/// six tapbacks, a few aliases and any emoji.
pub fn reaction_kind(arg: &str) -> Result<String> {
    let a = arg.trim();
    let lower = a.to_ascii_lowercase();
    let k = match lower.as_str() {
        "love" | "heart" => "love",
        "like" | "thumbsup" | "+1" => "like",
        "dislike" | "thumbsdown" | "-1" => "dislike",
        "laugh" | "haha" | "lol" => "laugh",
        "emphasize" | "emphasis" | "!!" => "emphasize",
        "question" | "?" => "question",
        _ => {
            if let Some(e) = a.strip_prefix("emoji:") {
                if !e.is_empty() {
                    return Ok(a.to_string());
                }
            } else if !a.is_empty() && !a.is_ascii() {
                return Ok(a.to_string());
            }
            return Err(CliError::usage(format!(
                "unknown reaction {arg:?}: use love, like, dislike, laugh, emphasize, question or an emoji"
            )));
        }
    };
    Ok(k.to_string())
}

/// MIME type from a file extension (`""` = let the daemon default).
pub fn guess_mime(path: &std::path::Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "webp" => "image/webp",
        "tif" | "tiff" => "image/tiff",
        "mov" => "video/quicktime",
        "mp4" | "m4v" => "video/mp4",
        "m4a" => "audio/mp4",
        "caf" => "audio/x-caf",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "vcf" => "text/vcard",
        "zip" => "application/zip",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use imessage_common::model::ChatId;

    fn chat(id: ChatId, name: Option<&str>, parts: &[&str]) -> Chat {
        Chat {
            is_group: id.is_group(),
            id,
            participants: parts.iter().map(|p| Handle::new(p)).collect(),
            display_name: name.map(str::to_string),
            last_message_at: None,
            unread_count: 0,
            background: None,
            icon_path: None,
        }
    }

    #[test]
    fn classify_args() {
        assert_eq!(classify("group:ABC"), ChatArg::GroupId("group:ABC".into()));
        assert_eq!(classify("group-p:mailto:a@x,mailto:b@x"), ChatArg::GroupId("group-p:mailto:a@x,mailto:b@x".into()));
        assert_eq!(classify("Alice@Example.com"), ChatArg::Handle("mailto:alice@example.com".into()));
        assert_eq!(classify("mailto:alice@example.com"), ChatArg::Handle("mailto:alice@example.com".into()));
        assert_eq!(classify("+1 (555) 123-4567"), ChatArg::Handle("tel:+15551234567".into()));
        assert_eq!(classify("Team"), ChatArg::Name("Team".into()));
    }

    #[test]
    fn chat_matching() {
        let chats = vec![
            chat(ChatId::group("5B0E2C1A-0000"), Some("Team"), &["a@x.com", "b@x.com"]),
            chat(ChatId::group("5B0F0000-0000"), Some("Team Leads"), &["c@x.com", "d@x.com"]),
            chat(ChatId::direct("bob@example.com"), None, &["bob@example.com"]),
        ];
        let ids = |q: &str| match_chats(q, &chats).iter().map(|c| c.id.0.clone()).collect::<Vec<_>>();
        assert_eq!(ids("team"), vec!["group:5B0E2C1A-0000"], "exact name beats prefix");
        assert_eq!(ids("Team L"), vec!["group:5B0F0000-0000"]);
        assert_eq!(ids("5b0e"), vec!["group:5B0E2C1A-0000"], "GUID prefix");
        assert_eq!(ids("group:5B0").len(), 2, "ambiguous id prefix");
        assert_eq!(ids("bob"), vec!["mailto:bob@example.com"], "name prefix (participant)");
        assert!(ids("nobody").is_empty());
        assert!(ids("  ").is_empty());
        assert_eq!(one_of("x", vec![]).unwrap_err().code, exit::NOT_FOUND);
        let amb = one_of("group:5B0", match_chats("group:5B0", &chats)).unwrap_err();
        assert_eq!(amb.code, exit::USAGE);
        assert!(amb.message.contains("Team Leads"));
    }

    #[test]
    fn recipients() {
        assert_eq!(split_recipients("a@x.com, b@x.com,"), vec!["a@x.com", "b@x.com"]);
        assert!(split_recipients(" , ").is_empty());
    }

    #[test]
    fn reaction_kinds() {
        for (i, o) in [("love", "love"), ("Heart", "love"), ("like", "like"), ("dislike", "dislike"), ("laugh", "laugh"), ("!!", "emphasize"), ("question", "question"), ("?", "question")] {
            assert_eq!(reaction_kind(i).unwrap(), o, "{i}");
        }
        assert_eq!(reaction_kind("🔥").unwrap(), "🔥");
        assert_eq!(reaction_kind("emoji:🔥").unwrap(), "emoji:🔥");
        for bad in ["", "wow", "emoji:", "lov"] {
            assert_eq!(reaction_kind(bad).unwrap_err().code, exit::USAGE, "{bad:?}");
        }
    }

    #[test]
    fn mime_guess() {
        assert_eq!(guess_mime(std::path::Path::new("/x/IMG_1.JPG")), "image/jpeg");
        assert_eq!(guess_mime(std::path::Path::new("/x/a.heic")), "image/heic");
        assert_eq!(guess_mime(std::path::Path::new("/x/noext")), "");
    }
}
