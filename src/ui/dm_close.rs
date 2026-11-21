//! Closing a direct message: hides the conversation until a new message reopens it.
//!
//! Discord uses the same request to close a DM and to leave a group DM, so only
//! one-to-one conversations can be closed here.

use std::collections::HashSet;
use std::sync::Mutex;

const DIRECT_MESSAGE: u64 = 1;

static CLOSED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Whether closing this channel type only hides it (a group DM would be left for good).
pub fn is_closable(channel_type: u64) -> bool {
    channel_type == DIRECT_MESSAGE
}

pub fn mark_closed(channel_id: &str) {
    CLOSED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashSet::new)
        .insert(channel_id.to_owned());
}

/// Forgets a closed DM. True when it was closed, so its row has to come back.
pub fn take_closed(channel_id: &str) -> bool {
    CLOSED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .is_some_and(|closed| closed.remove(channel_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_to_one_conversations_can_be_closed() {
        assert!(is_closable(1));
        assert!(!is_closable(3));
        assert!(!is_closable(0));
    }

    #[test]
    fn a_closed_dm_reopens_once() {
        mark_closed("dm-reopen-test");
        assert!(take_closed("dm-reopen-test"));
        assert!(!take_closed("dm-reopen-test"));
    }
}
