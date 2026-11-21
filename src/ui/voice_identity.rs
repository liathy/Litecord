//! Server nicknames and avatars of the people in a call.
//!
//! Someone already in the call at login arrives as a bare user id, and a name known from
//! the friends list hides their nickname. Each person is looked up once per server.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Semaphore;

use crate::http::DiscordHttpClient;

const MAX_KNOWN: usize = 2048;
const RETRY_AFTER: Duration = Duration::from_secs(60);

static KNOWN: Mutex<Option<HashSet<(String, u64)>>> = Mutex::new(None);
/// A full call asks for a few people at a time.
static REQUESTS: Semaphore = Semaphore::const_new(3);

fn first_mark(guild: &str, user_id: u64) -> bool {
    let mut guard = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    let known = guard.get_or_insert_with(HashSet::new);
    if known.len() >= MAX_KNOWN {
        known.clear();
    }
    known.insert((guild.to_owned(), user_id))
}

fn forget(guild: &str, user_id: u64) {
    let mut guard = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(known) = guard.as_mut() {
        known.remove(&(guild.to_owned(), user_id));
    }
}

/// The name shown in a server, and the account's avatar hash.
fn member_identity(member: &Value) -> (Option<&str>, &str) {
    let user = &member["user"];
    let not_empty = |value: &'_ Value| value.as_str().is_some_and(|text| !text.is_empty());
    let name = [&member["nick"], &user["global_name"], &user["username"]]
        .into_iter()
        .find(|value| not_empty(value))
        .and_then(Value::as_str);
    (name, user["avatar"].as_str().unwrap_or(""))
}

/// Loads this person's nickname and avatar for the server, once.
pub fn ensure(http: Option<DiscordHttpClient>, guild: &str, user_id: u64) {
    let Some(http) = http else { return };
    if guild.is_empty() || guild == "@me" || user_id == 0 || !first_mark(guild, user_id) {
        return;
    }
    let guild = guild.to_owned();
    tokio::spawn(async move {
        let Ok(_slot) = REQUESTS.acquire().await else { return };
        match http.get_guild_member(&guild, &user_id.to_string()).await {
            Ok(member) => {
                let (name, avatar) = member_identity(&member);
                if let Some(name) = name {
                    crate::gateway::register_user_name(user_id, name.to_owned());
                }
                crate::gateway::register_user_avatar(user_id, avatar);
            }
            Err(error) => {
                log::debug!("Membro {} do servidor {} indisponível: {}", user_id, guild, error);
                drop(_slot);
                tokio::time::sleep(RETRY_AFTER).await;
                forget(&guild, user_id);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_server_nickname_comes_first() {
        let member = json!({ "nick": "Nick", "user": { "global_name": "Global", "username": "user", "avatar": "abc" } });
        assert_eq!(member_identity(&member), (Some("Nick"), "abc"));
    }

    #[test]
    fn without_a_nickname_the_display_name_is_used() {
        let member = json!({ "nick": null, "user": { "global_name": "Global", "username": "user" } });
        assert_eq!(member_identity(&member), (Some("Global"), ""));
    }

    #[test]
    fn empty_names_fall_through_to_the_username() {
        let member = json!({ "nick": "", "user": { "global_name": "", "username": "user" } });
        assert_eq!(member_identity(&member).0, Some("user"));
    }

    #[test]
    fn each_person_is_looked_up_once_per_server() {
        assert!(first_mark("guild-test", 77));
        assert!(!first_mark("guild-test", 77));
        assert!(first_mark("other-guild-test", 77));
        forget("guild-test", 77);
        assert!(first_mark("guild-test", 77));
    }
}
