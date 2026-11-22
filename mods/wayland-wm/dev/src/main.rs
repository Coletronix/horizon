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
static BUILT: &[mod_api::__private::native::BuiltMod] = &[
    mod_api::__private::native::BuiltMod {
        id: "wayland_wm",
        version: "0.1.0",
        side: mod_api::Side::Client,
    },
    mod_api::__private::native::BuiltMod {
        id: "wm_world",
        version: "0.1.0",
        side: mod_api::Side::Client,
    },
];

fn main() {
    horizon_guest_stack::prepare_main();
    horizon_mod_host::exit_if_compile_child();
    install_package();
    let startup = horizon_client::entry::prepare();
    horizon_mod_host::main_with(
        vec![
            mod_api::ModEntry::new("wayland_wm", Box::new(wayland_wm::WaylandWm::default())),
            mod_api::ModEntry::new("wm_world", Box::new(wm_world::Hooked)),
        ],
        &[],
        BUILT,
        startup,
    );
}

/// The mods' packages as the launcher exports them beside the game: manifests, sources, assets.
const PACKAGES: &[(&str, &[(&str, &str)])] = &[
    (
        "wayland_wm",
        &[
            ("horizon-mod.toml", include_str!("../../horizon-mod.toml")),
            ("Cargo.toml", include_str!("../../Cargo.toml")),
            ("src/lib.rs", include_str!("../../src/lib.rs")),
            ("src/session.rs", include_str!("../../src/session.rs")),
            ("src/overlay.rs", include_str!("../../src/overlay.rs")),
            ("src/panels.rs", include_str!("../../src/panels.rs")),
        ],
    ),
    (
        "wm_world",
        &[
            (
                "horizon-mod.toml",
                include_str!("../../../wm-world/horizon-mod.toml"),
            ),
            ("Cargo.toml", include_str!("../../../wm-world/Cargo.toml")),
            ("src/lib.rs", include_str!("../../../wm-world/src/lib.rs")),
            (
                "src/space.rs",
                include_str!("../../../wm-world/src/space.rs"),
            ),
            ("src/draw.rs", include_str!("../../../wm-world/src/draw.rs")),
            (
                "src/store.rs",
                include_str!("../../../wm-world/src/store.rs"),
            ),
            (
                "assets/wm_world/lang/en_us.json",
                include_str!("../../../wm-world/assets/wm_world/lang/en_us.json"),
            ),
        ],
    ),
];

/// Put the mod's package where the game looks for the native mods it was built with, beside this
/// executable, as the launcher installs it.
fn install_package() {
    let native = std::env::current_exe()
        .map(|exe| exe.with_file_name(horizon_mod_package::mod_list::NATIVE_DIR));
    let result = native.and_then(|native| {
        for (id, files) in PACKAGES {
            for (path, text) in *files {
                let to = native.join(id).join(path);
                std::fs::create_dir_all(to.parent().expect("a file in the package"))?;
                std::fs::write(to, text)?;
            }
        }
        Ok(())
    });
    if let Err(error) = result {
        eprintln!("wayland-wm-dev: the mods' packages couldn't be installed: {error}");
    }
}
