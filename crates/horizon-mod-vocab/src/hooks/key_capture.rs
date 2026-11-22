//! The `key_capture` hook: declarations the mod API generator reads (see `hooks/mod.rs`).

pub trait Hooks {
    /// A key from the window or the control socket, before the game handles it in any way: before
    /// its key mappings, a screen, the chat, the pause menu, and `input_key`. Cancel it and the
    /// game never sees it: a mod that gives the keyboard to something else (a desktop window in
    /// the world) takes every key, Escape included. `key` is vanilla's name for the physical key
    /// (`key.keyboard.a`), `raw_code` the platform's code for it (on Linux the kernel's input
    /// code, `KEY_A` is 30), `action` what it did, and the modifier bits the screen hooks' GLFW
    /// encoding. A listener that cancels a press should cancel its release too, and leave alone
    /// the release of a key it didn't take, or the game keeps that key held. Beyond NeoForge,
    /// whose `InputEvent.Key` can't be cancelled.
    #[hook(
        side = "client",
        contract = "cancel",
        out = "canceled",
        neoforge = "InputEvent.Key"
    )]
    fn key_capture();
}
