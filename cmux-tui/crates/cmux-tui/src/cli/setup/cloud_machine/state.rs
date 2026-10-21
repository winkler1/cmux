//! What `cmux setup cloud-machine` keeps between runs:
//! `<state root>/setup/cloud-machine.json`, beside the daemon's `sessions`.

use std::path::PathBuf;

use serde_json::{Value, json};

use super::super::Refusal;

/// The machine setup made, or what it asked for and has not seen become
/// one: the create's idempotency key (kept before the create goes out),
/// the creation the app answered, and every machine there was before.
#[derive(Default)]
pub(super) struct Saved {
    pub(super) machine_id: Option<String>,
    pub(super) creation_id: Option<String>,
    pub(super) key: Option<String>,
    pub(super) known: Vec<String>,
}

impl Saved {
    fn path() -> Option<PathBuf> {
        let sessions = cmux_tui_core::platform::workspace_state_dir()?;
        let root = sessions.parent().unwrap_or(sessions.as_path());
        Some(root.join("setup").join("cloud-machine.json"))
    }

    pub(super) fn load() -> Self {
        let text = Self::path().and_then(|path| std::fs::read_to_string(path).ok());
        let value = text.and_then(|text| serde_json::from_str::<Value>(&text).ok());
        let Some(value) = value else { return Self::default() };
        let text =
            |key: &str| value[key].as_str().filter(|text| !text.is_empty()).map(str::to_owned);
        Self {
            machine_id: text("machine_id"),
            creation_id: text("creation_id"),
            key: text("key"),
            known: value["known"]
                .as_array()
                .map(|ids| ids.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default(),
        }
    }

    /// Writes the state whole (a temporary file renamed over it), or
    /// removes it when there is nothing to keep.
    pub(super) fn save(&self) -> Result<(), Refusal> {
        let failed = |error: String| {
            Refusal::Failed(format!("cmux: setup cloud-machine: cannot keep its state: {error}"))
        };
        let path = Self::path().ok_or_else(|| failed("no state folder (HOME is unset)".into()))?;
        if self.machine_id.is_none() && self.creation_id.is_none() && self.key.is_none() {
            return match std::fs::remove_file(&path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    Err(failed(error.to_string()))
                }
                _ => Ok(()),
            };
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| failed(error.to_string()))?;
        }
        let value = json!({
            "machine_id": self.machine_id,
            "creation_id": self.creation_id,
            "key": self.key,
            "known": self.known,
        });
        let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&temporary, format!("{value}\n"))
            .and_then(|()| std::fs::rename(&temporary, &path))
            .map_err(|error| {
                let _ = std::fs::remove_file(&temporary);
                failed(error.to_string())
            })
    }
}
