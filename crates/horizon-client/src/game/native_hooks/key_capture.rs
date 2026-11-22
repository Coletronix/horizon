//! `key_capture`: a key, before the game handles it.
use super::*;

impl Game<'_> {
    /// A key from the window or the control socket, before the game handles it in any way: before
    /// its key mappings, a screen, the chat, the pause menu, and `input_key`. Cancel it and the
    /// game never sees it.
    pub(in crate::game) fn native_key_capture(
        &mut self,
        key: &str,
        raw_code: u32,
        action: mod_api::KeyAction,
        modifiers: u32,
        canceled: &mut bool,
    ) {
        self.fire_key_capture(key, raw_code, action, modifiers, canceled);
    }
}
