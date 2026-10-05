//! Configuration file (`config.toml`) and the shared layout file (`layout.toml`),
//! both in the MultiPC config folder (`%APPDATA%\MultiPC` on Windows).

use crate::protocol::{SharedLayout, DEFAULT_CONTROL_PORT, DEFAULT_PORT};
use crate::transport::Psk;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Name of this machine as other machines see it.
    pub name: String,
    /// Pairing key, the same on every machine of the group (64 hex characters).
    pub key: String,
    /// TCP port for peers and UDP port for discovery.
    pub port: u16,
    /// Port of the local control panel, http://127.0.0.1:<control_port>.
    pub control_port: u16,
    /// Addresses ("192.168.1.20" or "host:port") to connect to even when
    /// discovery broadcasts do not get through.
    pub peers: Vec<String>,
    /// Where received files go. Defaults to `Downloads\MultiPC`.
    pub download_dir: Option<PathBuf>,
    pub share_input: bool,
    pub share_clipboard: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: gethostname::gethostname().to_string_lossy().into_owned(),
            key: String::new(),
            port: DEFAULT_PORT,
            control_port: DEFAULT_CONTROL_PORT,
            peers: Vec::new(),
            download_dir: None,
            share_input: true,
            share_clipboard: true,
        }
    }
}

pub fn generate_key() -> String {
    let mut k = [0u8; 32];
    getrandom::fill(&mut k).expect("system random generator");
    hex::encode(k)
}

pub fn parse_key(key: &str) -> Result<Psk> {
    let cleaned: String = key.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
    let bytes = hex::decode(&cleaned).context("pairing key must be hex")?;
    let Ok(psk) = <Psk>::try_from(bytes.as_slice()) else {
        bail!("pairing key must be 64 hex characters");
    };
    Ok(psk)
}

/// Short public tag of the group, sent in discovery beacons so machines of other
/// groups on the same network ignore each other. Does not reveal the key.
pub fn group_id(psk: &Psk) -> [u8; 8] {
    let mut h = Sha256::new();
    h.update(b"multipc group id v1");
    h.update(psk);
    h.finalize()[..8].try_into().unwrap()
}

/// Key formatted in groups of 8 for reading aloud or copying.
pub fn pretty_key(key: &str) -> String {
    key.as_bytes().chunks(8).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join("-")
}

impl Config {
    pub fn dir() -> PathBuf {
        if let Some(d) = std::env::var_os("MULTIPC_CONFIG_DIR") {
            return PathBuf::from(d);
        }
        dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("MultiPC")
    }

    pub fn path() -> PathBuf {
        Self::dir().join("config.toml")
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn psk(&self) -> Result<Psk> {
        if self.key.is_empty() {
            bail!("no pairing key yet: run `multipc init` first");
        }
        parse_key(&self.key)
    }

    pub fn download_dir(&self) -> PathBuf {
        self.download_dir.clone().unwrap_or_else(|| dirs::download_dir().unwrap_or_else(|| PathBuf::from(".")).join("MultiPC"))
    }
}

pub fn load_layout(dir: &Path) -> SharedLayout {
    std::fs::read_to_string(dir.join("layout.toml")).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
}

pub fn save_layout(dir: &Path, layout: &SharedLayout) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("layout.toml"), toml::to_string_pretty(layout)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Placement;

    #[test]
    fn key_roundtrip() {
        let k = generate_key();
        assert_eq!(k.len(), 64);
        let psk = parse_key(&k).unwrap();
        assert_eq!(parse_key(&pretty_key(&k)).unwrap(), psk);
        assert!(parse_key("abcd").is_err());
        assert_ne!(group_id(&psk), group_id(&parse_key(&generate_key()).unwrap()));
    }

    #[test]
    fn config_and_layout_files() {
        let dir = std::env::temp_dir().join(format!("mpc-config-test-{}", std::process::id()));
        let cfg = Config { name: "pc1".into(), key: generate_key(), peers: vec!["10.0.0.2".into()], ..Default::default() };
        cfg.save_to(&dir.join("config.toml")).unwrap();
        let back = Config::load_from(&dir.join("config.toml")).unwrap();
        assert_eq!(back.name, "pc1");
        assert_eq!(back.peers, cfg.peers);
        assert_eq!(back.port, DEFAULT_PORT);

        // Missing fields fall back to defaults.
        std::fs::write(dir.join("partial.toml"), "name = \"x\"\n").unwrap();
        assert!(Config::load_from(&dir.join("partial.toml")).unwrap().share_input);

        let mut layout = SharedLayout { version: 5, author: "pc1".into(), ..Default::default() };
        layout.placements.insert("pc1".into(), Placement { x: 0, y: 0 });
        layout.placements.insert("pc 2".into(), Placement { x: 1920, y: -40 });
        save_layout(&dir, &layout).unwrap();
        assert_eq!(load_layout(&dir), layout);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
