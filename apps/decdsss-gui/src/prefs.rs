// made by CasualArclamp
//! Remembered settings: a small `key=value` file in the user's config folder.
//! (No dependency: the values are a few short strings and numbers.)

use std::collections::BTreeMap;
use std::path::PathBuf;

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).or_else(|| {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    })?;
    Some(base.join("DecDSSS").join("prefs.txt"))
}

/// Settings read from the file (empty if there is none).
#[derive(Default, Clone, Debug)]
pub struct Prefs {
    map: BTreeMap<String, String>,
}

impl Prefs {
    pub fn load() -> Prefs {
        let mut map = BTreeMap::new();
        if let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) {
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    map.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
        }
        Prefs { map }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.map.get(key).map(String::as_str)
    }

    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        self.map.insert(key.to_string(), value.into());
    }

    /// Writes the file; a failure is not worth interrupting anyone for.
    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text: String = self.map.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
        let _ = std::fs::write(p, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_get() {
        let mut p = Prefs::default();
        p.set("call", "VK2ABC");
        p.set("dir", "C:\\x y\\z");
        assert_eq!(p.get("call"), Some("VK2ABC"));
        assert_eq!(p.get("dir"), Some("C:\\x y\\z"));
        assert_eq!(p.get("none"), None);
    }
}
