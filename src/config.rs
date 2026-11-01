//! キーは利用者だけが読める設定ファイルに置く。CIでは `NODD_KEY` を使う。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize, Deserialize)]
pub struct Profile {
    pub api: String,
    pub key: String,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

pub fn path() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or("Cannot find the config directory. Set HOME")?;
    Ok(base.join("nodd").join("config.json"))
}

pub fn load(path: &Path) -> Result<Config, String> {
    match fs::read(path) {
        Ok(raw) => serde_json::from_slice(&raw).map_err(|e| format!("{}: {e}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// 途中で止まっても壊れないよう、別名で書いてから置き換える。
pub fn save(path: &Path, config: &Config) -> Result<(), String> {
    let describe = |e: std::io::Error| format!("{}: {e}", path.display());
    let dir = path.parent().ok_or("Invalid config path")?;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(describe)?;
    let temporary = path.with_extension(format!("tmp{}", std::process::id()));
    let _ = fs::remove_file(&temporary);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(describe)?;
    let body = serde_json::to_vec_pretty(config).map_err(|e| e.to_string())?;
    file.write_all(&body).map_err(describe)?;
    file.sync_all().map_err(describe)?;
    fs::rename(&temporary, path).map_err(describe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn saved_file_is_private_and_reloads() {
        let dir = std::env::temp_dir().join(format!("nod-config-{}", ulid::Ulid::new()));
        let path = dir.join("nodd").join("config.json");
        let mut config = Config::default();
        config.profiles.insert("default".into(), Profile { api: "https://a".into(), key: "k".into() });
        save(&path, &config).unwrap();
        save(&path, &config).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(load(&path).unwrap().profiles["default"].key, "k");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_file_is_an_empty_config() {
        assert!(load(Path::new("/nonexistent/nod.json")).unwrap().profiles.is_empty());
    }
}
