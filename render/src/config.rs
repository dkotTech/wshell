//! Renderer configuration (`--config render.toml`); every field has a default,
//! see `render.example.toml`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub api: Api,
    pub shell: Shell,
    pub screen: Screen,
    pub keys: Keys,
    pub tui: Tui,
}

/// Control API: renders, frames, input.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Api {
    /// Loopback by default: the API drives apps on behalf of the user.
    pub listen: SocketAddr,
    /// Access token; empty = a random one per start, printed to stderr.
    pub token: String,
}

impl Default for Api {
    fn default() -> Self {
        Api { listen: SocketAddr::from(([127, 0, 0, 1], 8090)), token: String::new() }
    }
}

/// The home screen: a launcher plugin of wshell, opened at start and on App switch.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Shell {
    /// App id of the launcher plugin; empty: no home screen (only `--url` renders).
    pub launcher: String,
    /// shelld's extra control socket for the Renderer (`[control.sockets.render]`);
    /// empty = `$XDG_RUNTIME_DIR/wshell/render.sock`.
    pub socket: Option<PathBuf>,
}

impl Default for Shell {
    fn default() -> Self {
        Shell { launcher: "org.wshell.launcher".into(), socket: None }
    }
}

impl Shell {
    pub fn launcher(&self) -> crate::shell::Launcher {
        let socket = match &self.socket {
            Some(p) => p.clone(),
            None => shell_core::config::extra_socket(&shell_core::config::default_runtime_dir(), "render"),
        };
        crate::shell::Launcher { socket, id: (!self.launcher.is_empty()).then(|| self.launcher.clone()) }
    }
}

/// Default size and frame rate of a render: the device screen.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Screen {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Default for Screen {
    fn default() -> Self {
        Screen { width: 256, height: 144, fps: 30 }
    }
}

/// Keys use `KeyboardEvent.key` names ("ArrowUp", "Enter", "GoBack", "AppSwitch", …),
/// whatever the source: API, WebSocket, and later terminal and device buttons.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Keys {
    /// Renames applied to every key event before anything else: `GoBack = "Escape"`.
    pub map: BTreeMap<String, String>,
    /// Keys the Renderer handles itself; the page never sees them.
    pub actions: BTreeMap<String, Action>,
}

impl Default for Keys {
    fn default() -> Self {
        Keys {
            map: BTreeMap::from([("GoBack".into(), "Escape".into())]),
            actions: BTreeMap::from([("AppSwitch".into(), Action::Launcher)]),
        }
    }
}

/// `render-rs tui`: the render in a terminal (kitty graphics protocol).
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Tui {
    /// Terminal key → key name sent to the render (then `[keys]` applies):
    /// the terminal has no App switcher or soft keys.
    pub keys: BTreeMap<String, String>,
}

impl Default for Tui {
    fn default() -> Self {
        Tui { keys: BTreeMap::from([("F12".into(), "AppSwitch".into())]) }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// Back to wshell's launcher.
    Launcher,
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Config, String> {
        let Some(path) = path else { return Ok(Config::default()) };
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_matches_defaults() {
        let example: Config = toml::from_str(include_str!("../render.example.toml")).unwrap();
        let default = Config::default();
        assert_eq!(example.api.listen, default.api.listen);
        assert_eq!((example.screen.width, example.screen.height), (256, 144));
        assert_eq!(example.keys.map, default.keys.map);
        assert!(matches!(example.keys.actions.get("AppSwitch"), Some(Action::Launcher)));
        assert_eq!(example.tui.keys, default.tui.keys);
    }
}
