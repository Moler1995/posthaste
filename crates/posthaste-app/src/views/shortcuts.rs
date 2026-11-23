//! Shortcuts whose key combination and display text come from one definition.
//!
//! A binding and the hint that advertises it must never disagree, so both read
//! the same [`Shortcut`] constant: the shell registers `keystroke`, and an
//! empty state renders `keys`. The action name is a catalogue key, so it
//! follows the interface language while the key combination does not.

use crate::i18n::Key;

/// One registered shortcut: what gpui binds, what the hint shows, and the
/// action name a user reads.
#[derive(Clone, Copy)]
pub(crate) struct Shortcut {
    /// gpui keystroke syntax, handed to `KeyBinding::new`.
    pub(crate) keystroke: &'static str,
    /// Text shown in a hint.
    pub(crate) keys: &'static str,
    pub(crate) action: Key,
}

/// Open a blank request tab.
pub(crate) const NEW_TAB: Shortcut = Shortcut {
    keystroke: "secondary-t",
    keys: "Ctrl+T",
    action: Key::ActionNewRequest,
};

/// Send the active request.
pub(crate) const SEND: Shortcut = Shortcut {
    keystroke: "secondary-enter",
    keys: "Ctrl+Enter",
    action: Key::ActionSend,
};

/// Send the composed WebSocket message. It shares the request's send chord:
/// the same key sends whatever is in front of the user, without teaching a
/// second combination for the WebSocket pane.
pub(crate) const WS_SEND: Shortcut = Shortcut {
    keystroke: "secondary-enter",
    keys: "Ctrl+Enter",
    action: Key::WsSendMessage,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_registers_the_advertised_shortcuts() {
        let shell = crate::views::key_bindings();
        assert!(
            shell
                .iter()
                .any(|binding| binding.action().partial_eq(&crate::views::NewTab)),
            "the new-tab binding must exist"
        );

        let request_pane = crate::views::request_pane::key_bindings();
        assert!(
            request_pane.iter().any(|binding| binding
                .action()
                .partial_eq(&crate::views::request_pane::SendRequest)),
            "the send binding must exist"
        );

        let composer = crate::views::websocket_panel::key_bindings();
        assert!(
            composer.iter().any(|binding| binding
                .action()
                .partial_eq(&crate::views::websocket_panel::SendWsMessage)),
            "the composed-message binding must exist"
        );
    }

    #[test]
    fn every_shortcut_has_a_parseable_keystroke_and_a_hint() {
        for shortcut in [&NEW_TAB, &SEND, &WS_SEND] {
            assert!(
                gpui::Keystroke::parse(shortcut.keystroke).is_ok(),
                "{} must be a valid keystroke",
                shortcut.keystroke
            );
            assert!(!shortcut.keys.is_empty());
            assert!(!crate::i18n::t(shortcut.action).is_empty());
        }
    }
}
