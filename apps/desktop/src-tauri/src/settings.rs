//! User settings, persisted as `settings.json` in the app config dir. This
//! module owns the file: nothing else reads or writes it.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::{fs, io};

pub const FILE: &str = "settings.json";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TalkMode {
    #[default]
    PushToTalk,
    VoiceActivity,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub display_name: String,
    /// Device ids from `audio::devices`; `None` follows the system default.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub talk_mode: TalkMode,
    /// Push-to-talk input, named as in `ptt` (e.g. "F8", "Mouse4").
    pub ptt_key: String,
    /// Update channel; unset until the updater's first run decides it.
    pub beta_updates: Option<bool>,
    /// Link to the server we were in, without invite token, rejoined on the
    /// next start (e.g. after an update restart). Cleared by leaving.
    pub last_server: Option<String>,
    /// Per-user volume in percent, by identity key (hex); see [`Self::volume`].
    pub volumes: BTreeMap<String, u8>,
    /// Keys written by other versions, kept as they are.
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            display_name: String::new(),
            input_device: None,
            output_device: None,
            talk_mode: TalkMode::default(),
            ptt_key: "F8".into(),
            beta_updates: None,
            last_server: None,
            volumes: BTreeMap::new(),
            other: Map::new(),
        }
    }
}

/// Volume of a user nobody adjusted, percent.
pub const DEFAULT_VOLUME: u8 = 100;
pub const MAX_VOLUME: u8 = 200;

impl Settings {
    /// The volume chosen for the user with this identity key, percent.
    pub fn volume(&self, key: &[u8; 32]) -> u8 {
        self.volumes
            .get(&hex(key))
            .copied()
            .unwrap_or(DEFAULT_VOLUME)
    }

    /// Clamped to [`MAX_VOLUME`]; the default is not stored.
    pub fn set_volume(&mut self, key: &[u8; 32], percent: u8) {
        match percent.min(MAX_VOLUME) {
            DEFAULT_VOLUME => self.volumes.remove(&hex(key)),
            percent => self.volumes.insert(hex(key), percent),
        };
    }
}

fn hex(key: &[u8; 32]) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

/// The settings in memory, saved on every change.
pub struct Store {
    /// `None` when there is nowhere to save, or the file on disk could not
    /// be parsed: it is never overwritten then, so nothing the user wrote
    /// is lost.
    path: Option<PathBuf>,
    settings: Mutex<Settings>,
    /// Held while writing, so saves land in order without holding
    /// `settings` during disk I/O.
    saving: Mutex<()>,
}

impl Store {
    /// Loads `path`; a missing file means defaults.
    pub fn open(path: Option<PathBuf>) -> Self {
        let (path, settings) = match path.as_deref().map(load) {
            Some(Ok(settings)) => (path, settings),
            Some(Err(e)) => {
                eprintln!("{e}; settings will not be saved");
                (None, Settings::default())
            }
            None => (None, Settings::default()),
        };
        Self {
            path,
            settings: Mutex::new(settings),
            saving: Mutex::new(()),
        }
    }

    pub fn get(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    /// Changes the settings and saves them if anything changed. Save errors
    /// are logged: settings still apply for this run.
    pub fn update(&self, change: impl FnOnce(&mut Settings)) -> Settings {
        let _saving = self.saving.lock().unwrap();
        let (changed, after) = {
            let mut settings = self.settings.lock().unwrap();
            let before = settings.clone();
            change(&mut settings);
            (*settings != before, settings.clone())
        };
        if changed
            && let Some(path) = &self.path
            && let Err(e) = save(path, &after)
        {
            eprintln!("cannot save {}: {e}", path.display());
        }
        after
    }

    /// Whether to follow the beta update channel. The first call stores
    /// `default`, so a beta user stays on beta after updating to a stable
    /// release.
    pub fn beta_updates(&self, default: bool) -> bool {
        self.update(|s| {
            s.beta_updates.get_or_insert(default);
        })
        .beta_updates
        .unwrap_or(default)
    }
}

fn load(path: &Path) -> Result<Settings, String> {
    match fs::read_to_string(path) {
        Ok(text) => {
            serde_json::from_str(&text).map_err(|e| format!("cannot parse {}: {e}", path.display()))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// Writes a temporary file and renames it over `path`, so a crash leaves
/// either the old or the new settings, never a torn file.
fn save(path: &Path, settings: &Settings) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut file = fs::File::create(&tmp)?;
    io::Write::write_all(
        &mut file,
        serde_json::to_string_pretty(settings)?.as_bytes(),
    )?;
    file.sync_all()?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn temp_path(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kuulu-settings-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join(FILE)
    }

    fn saved(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn beta_default_persists_on_first_run() {
        let path = temp_path("first");
        assert!(Store::open(Some(path.clone())).beta_updates(true));
        // A later stable build keeps following beta.
        assert!(Store::open(Some(path)).beta_updates(false));
    }

    #[test]
    fn round_trips_and_keeps_unknown_keys() {
        let path = temp_path("other");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"x":1,"beta_updates":false}"#).unwrap();
        let store = Store::open(Some(path.clone()));
        assert!(!store.beta_updates(true));
        store.update(|s| {
            s.display_name = "Saara".into();
            s.talk_mode = TalkMode::VoiceActivity;
        });
        let json = saved(&path);
        assert_eq!(json["x"], 1);
        assert_eq!(json["talk_mode"], "voice_activity");
        let again = Store::open(Some(path.clone())).get();
        assert_eq!(again.display_name, "Saara");
        assert_eq!(again.talk_mode, TalkMode::VoiceActivity);
        assert_eq!(again.ptt_key, "F8");
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn never_overwrites_unparseable_file() {
        let path = temp_path("broken");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{oops").unwrap();
        let store = Store::open(Some(path.clone()));
        assert!(store.beta_updates(true));
        store.update(|s| s.display_name = "x".into());
        assert_eq!(store.get().display_name, "x");
        assert_eq!(fs::read_to_string(&path).unwrap(), "{oops");
    }

    #[test]
    fn volumes_by_identity_key() {
        let mut s = Settings::default();
        let (a, b) = ([1; 32], [2; 32]);
        assert_eq!(s.volume(&a), DEFAULT_VOLUME);
        s.set_volume(&a, 85);
        s.set_volume(&b, 250);
        assert_eq!((s.volume(&a), s.volume(&b)), (85, MAX_VOLUME));
        s.set_volume(&a, DEFAULT_VOLUME);
        assert_eq!(s.volumes.len(), 1);
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["volumes"][hex(&b)], MAX_VOLUME);
    }
}
