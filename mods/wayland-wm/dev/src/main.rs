//! The game with the `wayland_wm` mod linked in, as the launcher's native build would link it, for
//! developing the mod against this checkout's engine. Run it as the game:
//!
//! ```sh
//! HORIZON_WM_OVERLAY=1 HORIZON_WM_SPAWN=foot \
//!   cargo run --profile fast -p wayland-wm-dev -- --resources .local/horizon-client/26.3 --world W
//! ```
//!
//! `HORIZON_WM_OVERLAY=1` draws every window on the HUD; `HORIZON_WM_SPAWN` starts a program
//! whose windows open in the game.
#![forbid(unsafe_code)]

fn main() {
    horizon_guest_stack::prepare_main();
    horizon_mod_host::exit_if_compile_child();
    let startup = horizon_client::entry::prepare();
    horizon_mod_host::main_with(
        vec![mod_api::ModEntry::new(
            "wayland_wm",
            Box::new(wayland_wm::WaylandWm::default()),
        )],
        &[],
        &[],
        startup,
    );
}
