//! Windows as items.
//!
//! - **Picking a window up** (holding attack on its bezel) puts it away on this client and asks
//!   the server for a window item, named after the window. The item carries which window it is.
//! - **Using a window item** puts that window back in the world in front of the player, and the
//!   item is used up. An item of a window that no longer exists starts its program again.
//! - **Destroying a window item** closes the window, as its close button does: the program may
//!   ask to save first. Despawning, lava, fire, cactus, the void, an explosion: the server tracks
//!   window items lying in the world, and when one leaves it without going into a player's
//!   inventory or unloading with its chunk, it was destroyed. Hoppers don't pick window items
//!   up, so they never vanish into one.
//!
//! The client and the server talk over one payload channel, `wm_world:window`. On a server
//! without this mod, picking a window up only puts it away (the recall key brings it back).
use mod_api::{
    Compound, IPayloadContext, IPayloadHandler, ItemStack, PayloadBytes, PayloadHandleError,
    PayloadId, ServerHost, Tag,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use wayland_wm::WindowId;

pub const WINDOW_ITEM: &str = "wm_world:window";
const CHANNEL: &str = "wm_world:window";
/// The custom data keys a window item carries.
const KEY_WINDOW: &str = "wm_world.window";
const KEY_SESSION: &str = "wm_world.session";
const KEY_OWNER: &str = "wm_world.owner";
const KEY_APP: &str = "wm_world.app";

/// The window item's key, resolved at registration.
static KEY: std::sync::OnceLock<mod_api::ItemKey> = std::sync::OnceLock::new();

pub fn set_key(key: mod_api::ItemKey) {
    let _ = KEY.set(key);
}

fn window_key() -> Option<mod_api::ItemKey> {
    KEY.get().copied()
}

pub fn channel() -> PayloadId {
    PayloadId(CHANNEL.into())
}

/// Which window an item is: the session run and the window's id in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WindowKey {
    pub session: u64,
    pub window: u64,
}

/// What crosses the channel.
#[derive(Debug, Serialize, Deserialize)]
pub enum Message {
    /// Client to server: the player picked this window up; give them its item.
    PickedUp {
        key: WindowKey,
        title: String,
        app_id: String,
    },
    /// Server to client: this window's item was destroyed; close the window.
    Close { key: WindowKey },
}

impl Message {
    pub fn bytes(&self) -> PayloadBytes {
        PayloadBytes(serde_json::to_vec(self).unwrap_or_default())
    }
    fn parse(bytes: &PayloadBytes) -> Result<Self, PayloadHandleError> {
        serde_json::from_slice(&bytes.0)
            .map_err(|e| PayloadHandleError::Failed(format!("wm_world: {e}")))
    }
}

/// The window a stack is, if it is a window item.
pub fn key_of(stack: &ItemStack) -> Option<(WindowKey, String)> {
    if stack.item() != window_key()? {
        return None;
    }
    let data = stack
        .components()?
        .as_nbt()
        .get(&"minecraft:custom_data".into())?;
    let Tag::Compound(data) = data else {
        return None;
    };
    let long = |key: &str| match data.get(&key.into()) {
        Some(Tag::Long(v)) => Some(*v as u64),
        _ => None,
    };
    let app = match data.get(&KEY_APP.into()) {
        Some(Tag::String(s)) => String::from_utf16_lossy(&s.0),
        _ => String::new(),
    };
    Some((
        WindowKey {
            session: long(KEY_SESSION)?,
            window: long(KEY_WINDOW)?,
        },
        app,
    ))
}

/// The owner a window item was given to.
fn owner_of(stack: &ItemStack) -> Option<u128> {
    let data = stack
        .components()?
        .as_nbt()
        .get(&"minecraft:custom_data".into())?;
    let Tag::Compound(data) = data else {
        return None;
    };
    match data.get(&KEY_OWNER.into()) {
        Some(Tag::String(s)) => String::from_utf16_lossy(&s.0).parse().ok(),
        _ => None,
    }
}

/// A window item for `key`, named after the window.
fn window_stack(key: WindowKey, title: &str, app_id: &str, owner: u128) -> Option<ItemStack> {
    let mut stack = ItemStack::new(window_key()?, 1)?;
    let mut data = Compound::new();
    data.insert(KEY_SESSION.into(), Tag::Long(key.session as i64));
    data.insert(KEY_WINDOW.into(), Tag::Long(key.window as i64));
    data.insert(
        KEY_OWNER.into(),
        Tag::String(owner.to_string().as_str().into()),
    );
    data.insert(KEY_APP.into(), Tag::String(app_id.into()));
    let mut patch = Compound::new();
    patch.insert("minecraft:custom_data".into(), Tag::Compound(data));
    let name = if title.trim().is_empty() {
        app_id
    } else {
        title
    };
    if !name.is_empty() {
        patch.insert("minecraft:custom_name".into(), Tag::String(name.into()));
    }
    stack.set_components(patch).ok()?;
    Some(stack)
}

/// The item declarations: the window item looks like a painting, a framed picture of the window.
pub fn declare(items: &mut Vec<mod_api::Item>) {
    let mut window = mod_api::Item::new(WINDOW_ITEM, "minecraft:paper");
    window.max_stack_size = Some(1);
    window.components.push((
        "minecraft:item_model".into(),
        Some(Tag::String("minecraft:painting".into())),
    ));
    items.push(window);
}

// --- Server side ---

/// A window item lying in the world.
#[derive(Clone, Debug)]
struct Lying {
    key: WindowKey,
    owner: Option<u128>,
    dimension: String,
    pos: [f64; 3],
}

/// What the server knows of window items in the world.
#[derive(Default)]
pub struct Server {
    /// By entity id.
    lying: BTreeMap<u32, Lying>,
    /// Ones that just left the world: decided on the next tick.
    left: Vec<Lying>,
}

static SERVER: Mutex<Option<Server>> = Mutex::new(None);

fn with_server<R>(f: impl FnOnce(&mut Server) -> R) -> R {
    let mut server = SERVER.lock().unwrap_or_else(|p| p.into_inner());
    f(server.get_or_insert_with(Server::default))
}

/// An entity joined a level: if it is a window item, keep an eye on it.
pub fn joined(host: &mut dyn ServerHost, dimension: &str, entity: u32, kind: &str, pos: [f64; 3]) {
    if kind != "minecraft:item" {
        return;
    }
    let Some(stack) = host
        .entity(entity)
        .and_then(|e| host.item_entity(e))
        .and_then(|i| i.stack(&*host))
    else {
        return;
    };
    let Some((key, _)) = key_of(&stack) else {
        return;
    };
    let lying = Lying {
        key,
        owner: owner_of(&stack),
        dimension: dimension.to_string(),
        pos,
    };
    with_server(|s| {
        s.left.retain(|l| l.key != key);
        s.lying.insert(entity, lying);
    });
}

/// An entity left a level: decided next tick, once a pickup has landed in an inventory.
pub fn left(entity: u32) {
    with_server(|s| {
        if let Some(lying) = s.lying.remove(&entity) {
            s.left.push(lying);
        }
    });
}

/// A window item despawned: its window closes.
pub fn expired(host: &mut dyn ServerHost, entity: u32, stack: &ItemStack) {
    let Some((key, _)) = key_of(stack) else {
        return;
    };
    with_server(|s| {
        s.lying.remove(&entity);
    });
    close(host, key, owner_of(stack));
}

/// Each server tick: follow window items, and close the windows of the ones destroyed.
pub fn tick(host: &mut dyn ServerHost) {
    let (tracked, left) = with_server(|s| {
        (
            s.lying.keys().copied().collect::<Vec<_>>(),
            std::mem::take(&mut s.left),
        )
    });
    // Where each lies now, for the chunk check when it leaves.
    for id in tracked {
        if let Some(pos) = host.entity(id).and_then(|e| e.position(&*host)) {
            with_server(|s| {
                if let Some(l) = s.lying.get_mut(&id) {
                    l.pos = pos;
                }
            });
        }
    }
    if left.is_empty() {
        return;
    }
    let players = host.players().server_players();
    for lying in left {
        // Picked up by a player: it is in their inventory now.
        let held = players.iter().any(|p| {
            p.inventory(&*host).is_some_and(|inventory| {
                inventory
                    .iter()
                    .any(|stack| key_of(stack).is_some_and(|(k, _)| k == lying.key))
            })
        });
        if held {
            continue;
        }
        // Unloaded with its chunk: it comes back when the chunk does.
        let chunk = [
            (lying.pos[0].floor() as i32).div_euclid(16),
            (lying.pos[2].floor() as i32).div_euclid(16),
        ];
        let loaded = host
            .level(&lying.dimension)
            .and_then(|level| host.chunk(level, chunk))
            .is_some_and(|chunk| chunk.loaded(&*host));
        if !loaded {
            continue;
        }
        close(host, lying.key, lying.owner);
    }
}

/// Tell the window's owner to close it.
fn close(host: &mut dyn ServerHost, key: WindowKey, owner: Option<u128>) {
    let Some(player) = owner.and_then(|o| host.player(o)) else {
        return;
    };
    log::info!("wm_world: window item {key:?} destroyed: closing its window");
    let _ = player.send_payload(host, &channel(), Message::Close { key }.bytes());
}

/// The server's end of the channel: give the player the item of the window they picked up.
pub struct ServerHandler;

impl IPayloadHandler for ServerHandler {
    fn handle(
        &self,
        payload: PayloadBytes,
        context: &mut dyn IPayloadContext,
    ) -> Result<(), PayloadHandleError> {
        let Message::PickedUp { key, title, app_id } = Message::parse(&payload)? else {
            return Ok(());
        };
        let player = context
            .player()
            .map_err(|e| PayloadHandleError::Failed(format!("{e:?}")))?;
        let host = context
            .server_host()
            .map_err(|e| PayloadHandleError::Failed(format!("{e:?}")))?;
        let owner = player.profile(&*host).map_or(0, |p| p.uuid);
        if let Some(stack) = window_stack(key, &title, &app_id, owner)
            && let Err(error) = player.give_stack(host, stack)
        {
            log::warn!("wm_world: the window item couldn't be given: {error:?}");
        }
        Ok(())
    }
}

// --- Client side ---

/// Closes the client asks for, from the payload thread to the game's.
pub static CLOSES: Mutex<Vec<WindowKey>> = Mutex::new(Vec::new());

/// The client's end of the channel: a window item was destroyed.
pub struct ClientHandler;

impl IPayloadHandler for ClientHandler {
    fn handle(
        &self,
        payload: PayloadBytes,
        _context: &mut dyn IPayloadContext,
    ) -> Result<(), PayloadHandleError> {
        if let Message::Close { key } = Message::parse(&payload)? {
            CLOSES.lock().unwrap_or_else(|p| p.into_inner()).push(key);
        }
        Ok(())
    }
}

/// The handlers, for registration.
pub fn handlers() -> (Arc<dyn IPayloadHandler>, Arc<dyn IPayloadHandler>) {
    (Arc::new(ServerHandler), Arc::new(ClientHandler))
}

/// The window an item names, on this client's session.
pub fn local_window(key: WindowKey, session: u64) -> Option<WindowId> {
    (key.session == session).then_some(WindowId(key.window))
}
