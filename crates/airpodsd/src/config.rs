//! จัดเก็บ daemon config แบบ atomic ภายใต้ XDG config directory

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use airpods_ipc::{
    DEFAULT_GAIN_DB, DEFAULT_LIMITER_DB, MAX_GAIN_DB, MAX_LIMITER_DB, MIN_GAIN_DB, MIN_LIMITER_DB,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub selected_device: String,
    pub gain_db: f64,
    pub limiter_db: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            selected_device: String::new(),
            gain_db: DEFAULT_GAIN_DB,
            limiter_db: DEFAULT_LIMITER_DB,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn from_xdg() -> io::Result<Self> {
        let base = dirs::config_dir().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "XDG config directory is unavailable")
        })?;
        Ok(Self {
            path: base.join("airpods-linux").join("config.toml"),
        })
    }

    pub fn load(&self) -> anyhow::Result<Config> {
        match fs::read_to_string(&self.path) {
            Ok(contents) => {
                let config: Config = toml::from_str(&contents)?;
                config.validate()?;
                Ok(config)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save(&self, config: &Config) -> anyhow::Result<()> {
        let parent = self.path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "config path has no parent")
        })?;
        fs::create_dir_all(parent)?;

        let temporary = temporary_path(&self.path);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let encoded = toml::to_string_pretty(config)?;
        if let Err(error) = (|| -> io::Result<()> {
            file.write_all(encoded.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            sync_directory(parent)?;
            Ok(())
        })() {
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }
        Ok(())
    }
}

impl Config {
    fn validate(&self) -> anyhow::Result<()> {
        if !self.gain_db.is_finite() || !(MIN_GAIN_DB..=MAX_GAIN_DB).contains(&self.gain_db) {
            anyhow::bail!("configured gain is outside the supported range");
        }
        if !self.limiter_db.is_finite()
            || !(MIN_LIMITER_DB..=MAX_LIMITER_DB).contains(&self.limiter_db)
        {
            anyhow::bail!("configured limiter is outside the supported range");
        }
        Ok(())
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}
