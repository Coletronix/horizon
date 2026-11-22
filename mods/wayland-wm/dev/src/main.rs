//! The game with the `wayland_wm` mod linked in, as the launcher's native build would link it, for
//! developing the mod against this checkout's engine. Run it as the game:
//!
//! ```sh
//! HORIZON_WM_OVERLAY=1 HORIZON_WM_SPAWN=foot \
//!   cargo run --profile fast -p wayland-wm-dev -- --resources .local/horizon-client/26.3 --world W
//! ```
//!
//! `HORIZON_WM_OVERLAY=1` draws every window on the HUD, `HORIZON_WM_WORLD=1` stands them up in
//! the world in front of the camera, and `HORIZON_WM_SPAWN` starts a program whose windows open in
//! the game.
#![forbid(unsafe_code)]

/// The mod as the launcher's build declares it: checked against its installed package.
static BUILT: &[mod_api::__private::native::BuiltMod] = &[mod_api::__private::native::BuiltMod {
    id: "wayland_wm",
    version: "0.1.0",
    side: mod_api::Side::Client,
}];

fn main() {
    horizon_guest_stack::prepare_main();
    horizon_mod_host::exit_if_compile_child();
    install_package();
    let startup = horizon_client::entry::prepare();
    horizon_mod_host::main_with(
        vec![mod_api::ModEntry::new(
            "wayland_wm",
            Box::new(wayland_wm::WaylandWm::default()),
        )],
        &[],
        BUILT,
        startup,
    );
}

/// The mod's package as the launcher exports it beside the game: its manifest and its source.
const PACKAGE: &[(&str, &str)] = &[
    ("horizon-mod.toml", include_str!("../../horizon-mod.toml")),
    ("Cargo.toml", include_str!("../../Cargo.toml")),
    ("src/lib.rs", include_str!("../../src/lib.rs")),
    ("src/session.rs", include_str!("../../src/session.rs")),
    ("src/overlay.rs", include_str!("../../src/overlay.rs")),
    ("src/panels.rs", include_str!("../../src/panels.rs")),
];

/// Put the mod's package where the game looks for the native mods it was built with, beside this
/// executable, as the launcher installs it.
fn install_package() {
    let installed = std::env::current_exe().map(|exe| {
        exe.with_file_name(horizon_mod_package::mod_list::NATIVE_DIR)
            .join("wayland_wm")
    });
    let result = installed.and_then(|dir| {
        for (path, text) in PACKAGE {
            let to = dir.join(path);
            std::fs::create_dir_all(to.parent().expect("a file in the package"))?;
            std::fs::write(to, text)?;
        }
        Ok(())
    });
    if let Err(error) = result {
        eprintln!("wayland-wm-dev: the mod's package couldn't be installed: {error}");
    }
}
