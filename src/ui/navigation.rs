//! Back and forward between channels, like a browser, with the buttons and mouse 4/5.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use slint::{ComponentHandle, Model};

use crate::AppWindow;

const MAX_ENTRIES: usize = 100;
/// How long a move to another server may take to show the channel: 40 × 100 ms.
const OPEN_ATTEMPTS: u32 = 40;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Place {
    guild: String,
    channel: String,
}

#[derive(Default)]
struct History {
    entries: Vec<Place>,
    index: usize,
}

impl History {
    fn visit(&mut self, place: Place) {
        if self.entries.get(self.index) == Some(&place) {
            return;
        }
        if !self.entries.is_empty() {
            self.entries.truncate(self.index + 1);
        }
        self.entries.push(place);
        if self.entries.len() > MAX_ENTRIES {
            self.entries.remove(0);
        }
        self.index = self.entries.len() - 1;
    }

    fn can_go_back(&self) -> bool {
        self.index > 0
    }

    fn can_go_forward(&self) -> bool {
        self.index + 1 < self.entries.len()
    }

    fn step(&mut self, forward: bool) -> Option<Place> {
        if forward && self.can_go_forward() {
            self.index += 1;
        } else if !forward && self.can_go_back() {
            self.index -= 1;
        } else {
            return None;
        }
        self.entries.get(self.index).cloned()
    }
}

thread_local! {
    static HISTORY: RefCell<History> = RefCell::new(History::default());
    /// While a move crosses servers, the channels opened on the way are not history.
    static MOVING: Cell<bool> = const { Cell::new(false) };
    static MOVE_ID: Cell<u64> = const { Cell::new(0) };
}

fn publish(ui: &AppWindow) {
    let (back, forward) = HISTORY.with(|h| {
        let history = h.borrow();
        (history.can_go_back(), history.can_go_forward())
    });
    ui.set_can_go_back(back);
    ui.set_can_go_forward(forward);
}

fn observe(ui: &AppWindow) {
    if MOVING.with(Cell::get) {
        return;
    }
    let (guild, channel) = (ui.get_active_guild_id().to_string(), ui.get_active_channel_id().to_string());
    if guild.is_empty() || channel.is_empty() {
        return;
    }
    HISTORY.with(|h| h.borrow_mut().visit(Place { guild, channel }));
    publish(ui);
}

fn go(ui: &AppWindow, forward: bool) {
    let Some(place) = HISTORY.with(|h| h.borrow_mut().step(forward)) else { return };
    publish(ui);
    ui.set_is_voice_focused(false);
    if ui.get_active_guild_id() == place.guild.as_str() {
        ui.invoke_select_channel(place.channel.as_str().into());
        return;
    }
    MOVING.with(|moving| moving.set(true));
    let id = MOVE_ID.with(|value| {
        value.set(value.get() + 1);
        value.get()
    });
    crate::ui::data_loader::remember_last_channel(&place.guild, &place.channel);
    ui.invoke_select_guild(place.guild.as_str().into());
    finish_move(ui.as_weak(), place, id, OPEN_ATTEMPTS);
}

/// The other server's list loads in the background; the channel opens once it is there.
fn finish_move(app: slint::Weak<AppWindow>, place: Place, id: u64, attempts: u32) {
    if MOVE_ID.with(Cell::get) != id {
        return;
    }
    let Some(ui) = app.upgrade() else { return };
    let arrived = ui.get_active_guild_id() == place.guild.as_str()
        && ui.get_channels().iter().any(|channel| channel.id == place.channel.as_str());
    if arrived {
        if ui.get_active_channel_id() != place.channel.as_str() {
            ui.invoke_select_channel(place.channel.as_str().into());
        }
        MOVING.with(|moving| moving.set(false));
        return;
    }
    if attempts == 0 {
        MOVING.with(|moving| moving.set(false));
        return;
    }
    slint::Timer::single_shot(Duration::from_millis(100), move || finish_move(app, place, id, attempts - 1));
}

/// Mouse 4 and 5, from the window's event hook.
pub fn mouse_button(app: &slint::Weak<AppWindow>, forward: bool) {
    let app = app.clone();
    slint::Timer::single_shot(Duration::ZERO, move || {
        if let Some(ui) = app.upgrade() {
            go(&ui, forward);
        }
    });
}

pub fn install(ui: &AppWindow) {
    let app = ui.as_weak();
    ui.on_navigated(move || {
        if let Some(ui) = app.upgrade() {
            observe(&ui);
        }
    });
    let app = ui.as_weak();
    ui.on_navigate(move |forward| {
        if let Some(ui) = app.upgrade() {
            go(&ui, forward);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(channel: &str) -> Place {
        Place { guild: "1".into(), channel: channel.into() }
    }

    #[test]
    fn back_and_forward_walk_the_visited_channels() {
        let mut history = History::default();
        for channel in ["a", "b", "c"] {
            history.visit(place(channel));
        }
        assert_eq!(history.step(false), Some(place("b")));
        assert_eq!(history.step(false), Some(place("a")));
        assert_eq!(history.step(false), None);
        assert_eq!(history.step(true), Some(place("b")));
        assert!(history.can_go_forward());
    }

    #[test]
    fn a_new_visit_after_going_back_replaces_the_forward_path() {
        let mut history = History::default();
        for channel in ["a", "b", "c"] {
            history.visit(place(channel));
        }
        history.step(false);
        history.visit(place("d"));
        assert!(!history.can_go_forward());
        assert_eq!(history.step(false), Some(place("b")));
    }

    #[test]
    fn the_same_channel_is_not_recorded_twice() {
        let mut history = History::default();
        history.visit(place("a"));
        history.visit(place("a"));
        assert!(!history.can_go_back());
    }

    #[test]
    fn history_keeps_a_limited_number_of_places() {
        let mut history = History::default();
        for index in 0..(MAX_ENTRIES + 20) {
            history.visit(place(&index.to_string()));
        }
        assert_eq!(history.entries.len(), MAX_ENTRIES);
        assert_eq!(history.index, MAX_ENTRIES - 1);
    }
}
