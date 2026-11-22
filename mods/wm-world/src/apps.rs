//! App items: one per program installed on the desktop (its `.desktop` entry), listed in the
//! creative inventory's Tools & Utilities. Using one starts the program, as a spawn egg makes a
//! mob: its window opens in front of the player. A dispenser fired with one starts it too, the
//! window opening in front of the dispenser, facing out. The item isn't used up.
use mod_api::{Compound, ItemStack, Tag};
use std::path::PathBuf;
use std::sync::OnceLock;

pub const APP_ITEM: &str = "wm_world:app";
const KEY_EXEC: &str = "wm_world.exec";
const KEY_DESKTOP: &str = "wm_world.desktop";

static KEY: OnceLock<mod_api::ItemKey> = OnceLock::new();

pub fn set_key(key: mod_api::ItemKey) {
    let _ = KEY.set(key);
}

/// The item declaration: a book of the program, like a knowledge book.
pub fn declare(items: &mut Vec<mod_api::Item>) {
    let mut app = mod_api::Item::new(APP_ITEM, "minecraft:paper");
    app.max_stack_size = Some(1);
    app.components.push((
        "minecraft:item_model".into(),
        Some(Tag::String("minecraft:knowledge_book".into())),
    ));
    items.push(app);
}

/// A program from a desktop entry.
#[derive(Clone, Debug, PartialEq)]
pub struct App {
    /// The desktop entry's id (`org.gnome.Nautilus`).
    pub id: String,
    pub name: String,
    /// The command line, its field codes (`%U`...) taken out.
    pub exec: String,
}

/// The command an app item starts, if the stack is one.
pub fn exec_of(stack: &ItemStack) -> Option<String> {
    if stack.item() != *KEY.get()? {
        return None;
    }
    let Tag::Compound(data) = stack
        .components()?
        .as_nbt()
        .get(&"minecraft:custom_data".into())?
    else {
        return None;
    };
    match data.get(&KEY_EXEC.into()) {
        Some(Tag::String(s)) => Some(String::from_utf16_lossy(&s.0)),
        _ => None,
    }
}

/// The item for `app`, named after it.
pub fn stack(app: &App) -> Option<ItemStack> {
    let mut stack = ItemStack::new(*KEY.get()?, 1)?;
    let mut data = Compound::new();
    data.insert(KEY_EXEC.into(), Tag::String(app.exec.as_str().into()));
    data.insert(KEY_DESKTOP.into(), Tag::String(app.id.as_str().into()));
    let mut patch = Compound::new();
    patch.insert("minecraft:custom_data".into(), Tag::Compound(data));
    patch.insert(
        "minecraft:custom_name".into(),
        Tag::String(app.name.as_str().into()),
    );
    stack.set_components(patch).ok()?;
    Some(stack)
}

/// The desktop's programs, read once: the applications folders of `$XDG_DATA_HOME` and
/// `$XDG_DATA_DIRS`, an entry in an earlier folder hiding one of the same id in a later one.
pub fn installed() -> &'static [App] {
    static APPS: OnceLock<Vec<App>> = OnceLock::new();
    APPS.get_or_init(|| {
        let home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
        let dirs = std::env::var("XDG_DATA_DIRS")
            .ok()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        let mut roots: Vec<PathBuf> = home.into_iter().collect();
        roots.extend(dirs.split(':').map(PathBuf::from));
        let mut apps: Vec<App> = Vec::new();
        for root in roots {
            let mut files = Vec::new();
            collect(
                &root.join("applications"),
                &root.join("applications"),
                &mut files,
            );
            for (id, path) in files {
                if apps.iter().any(|a| a.id == id) {
                    continue;
                }
                if let Ok(text) = std::fs::read_to_string(&path)
                    && let Some(app) = parse(&id, &text)
                {
                    apps.push(app);
                }
            }
        }
        apps.sort_by_key(|a| a.name.to_lowercase());
        apps
    })
}

/// The `.desktop` files under `dir`, with their ids (`kde/foo.desktop` is `kde-foo`).
fn collect(base: &PathBuf, dir: &PathBuf, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(base, &path, out);
        } else if path.extension().is_some_and(|e| e == "desktop")
            && let Ok(relative) = path.strip_prefix(base)
        {
            let id = relative
                .with_extension("")
                .to_string_lossy()
                .replace('/', "-");
            out.push((id, path));
        }
    }
}

/// A desktop entry a person would launch: an application, shown, with a command.
pub fn parse(id: &str, text: &str) -> Option<App> {
    let mut in_entry = false;
    let (mut name, mut exec, mut kind) = (None, None, None);
    let (mut hidden, mut terminal) = (false, false);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Name" => name = Some(value.trim().to_string()),
            "Exec" => exec = Some(value.trim().to_string()),
            "Type" => kind = Some(value.trim().to_string()),
            "NoDisplay" | "Hidden" => hidden |= value.trim() == "true",
            "Terminal" => terminal = value.trim() == "true",
            _ => {}
        }
    }
    if hidden || terminal || kind.as_deref() != Some("Application") {
        return None;
    }
    let exec = strip_field_codes(&exec?);
    (!exec.is_empty()).then(|| App {
        id: id.to_string(),
        name: name.unwrap_or_else(|| id.to_string()),
        exec,
    })
}

/// A desktop entry's `Exec` without its field codes (`%U`, `%f`...), `%%` kept as `%`.
fn strip_field_codes(exec: &str) -> String {
    let mut out = String::new();
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            if let Some('%') = chars.next() {
                out.push('%');
            }
        } else {
            out.push(c);
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Where a window opens in front of a dispenser facing `facing` at `pos`: its centre, and the
/// view (yaw, pitch) that sees it straight on.
pub fn in_front_of(pos: [i32; 3], facing: mod_api::Direction) -> ([f64; 3], f32, f32) {
    use mod_api::Direction::*;
    let (dir, yaw, pitch): ([f64; 3], f32, f32) = match facing {
        North => ([0.0, 0.0, -1.0], 0.0, 0.0),
        South => ([0.0, 0.0, 1.0], 180.0, 0.0),
        West => ([-1.0, 0.0, 0.0], -90.0, 0.0),
        East => ([1.0, 0.0, 0.0], 90.0, 0.0),
        Up => ([0.0, 1.0, 0.0], 0.0, 90.0),
        Down => ([0.0, -1.0, 0.0], 0.0, -90.0),
    };
    let centre = [
        f64::from(pos[0]) + 0.5 + dir[0] * 0.6,
        f64::from(pos[1]) + 0.5 + dir[1] * 0.6,
        f64::from(pos[2]) + 0.5 + dir[2] * 0.6,
    ];
    (centre, yaw, pitch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entries_become_apps() {
        let entry = "[Desktop Entry]\nType=Application\nName=Files\nExec=nautilus --new-window %U\n\
                     [Desktop Action new]\nExec=nautilus --other\n";
        let app = parse("org.gnome.Nautilus", entry).unwrap();
        assert_eq!(app.name, "Files");
        assert_eq!(app.exec, "nautilus --new-window");
        // Hidden, terminal-only and non-application entries aren't apps.
        assert!(
            parse(
                "x",
                "[Desktop Entry]\nType=Application\nName=X\nExec=x\nNoDisplay=true\n"
            )
            .is_none()
        );
        assert!(
            parse(
                "x",
                "[Desktop Entry]\nType=Application\nName=X\nExec=htop\nTerminal=true\n"
            )
            .is_none()
        );
        assert!(parse("x", "[Desktop Entry]\nType=Link\nName=X\nURL=http://x\n").is_none());
        assert_eq!(strip_field_codes("echo 100%% %f"), "echo 100%");
    }
}
