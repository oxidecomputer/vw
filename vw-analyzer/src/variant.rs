// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! The editor's `variant` setting — which `[[workspace.variants]]`
//! entry a session analyzes.
//!
//! Set through LSP `initializationOptions` and updated through
//! `workspace/didChangeConfiguration`, both carrying
//! `{ "variant": "<name>" }`. In Helix that's
//!
//! ```toml
//! [language-server.vw-analyzer]
//! command = "vw"
//! args = ["analyzer"]
//! config = { variant = "metro" }
//! ```
//!
//! One setting, shared by every backend, resolved per workspace by
//! [`vw_lib::editor_variant`] (setting → `VW_ACTIVE_VARIANT` →
//! default variant).

use std::collections::HashSet;
use std::sync::{Mutex, RwLock};

use camino::{Utf8Path, Utf8PathBuf};
use tower_lsp::lsp_types::MessageType;
use tower_lsp::Client;
use tracing::warn;

#[derive(Default)]
pub struct VariantSetting {
    /// Where to surface a setting that names no declared variant.
    /// `None` in tests, which only log.
    client: Option<Client>,
    requested: RwLock<Option<String>>,
    /// Warnings already shown, so re-rendering a workspace's config
    /// on every file event doesn't repeat the popup.
    warned: Mutex<HashSet<(Utf8PathBuf, String)>>,
}

impl VariantSetting {
    pub fn new(client: Client) -> Self {
        Self {
            client: Some(client),
            ..Self::default()
        }
    }

    /// Replace the requested variant. Returns whether it changed.
    pub fn set(&self, requested: Option<String>) -> bool {
        let mut cur = self.requested.write().unwrap();
        if *cur == requested {
            return false;
        }
        *cur = requested;
        // A new setting deserves its own warnings.
        self.warned.lock().unwrap().clear();
        true
    }

    /// The variant to analyze `workspace_dir` as. Warns the user,
    /// once per workspace and message, when the setting names a
    /// variant that workspace doesn't declare.
    pub fn for_workspace(&self, workspace_dir: &Utf8Path) -> Option<String> {
        let requested = self.requested.read().unwrap().clone();
        let choice =
            vw_lib::editor_variant(workspace_dir, requested.as_deref());
        if let Some(message) = choice.warning {
            let key = (workspace_dir.to_path_buf(), message.clone());
            if self.warned.lock().unwrap().insert(key) {
                warn!("{message}");
                self.show_warning(message);
            }
        }
        choice.variant
    }

    fn show_warning(&self, message: String) {
        let Some(client) = self.client.clone() else {
            return;
        };
        // Called from sync code paths (config re-renders) as well
        // as async ones; the message is fire-and-forget either way.
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                client.show_message(MessageType::WARNING, message).await;
            });
        }
    }
}

/// The `variant` field of an `initializationOptions` or
/// `didChangeConfiguration` settings payload. Missing, null, or
/// empty → no request (use the default variant).
pub fn variant_from_settings(settings: &serde_json::Value) -> Option<String> {
    settings
        .get("variant")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn settings_payload_parsing() {
        let v = |j| variant_from_settings(&j);
        assert_eq!(v(json!({"variant": "metro"})).as_deref(), Some("metro"));
        assert_eq!(v(json!({"variant": " metro "})).as_deref(), Some("metro"));
        assert_eq!(v(json!({"variant": ""})), None);
        assert_eq!(v(json!({"variant": null})), None);
        assert_eq!(v(json!({})), None);
        assert_eq!(v(json!(null)), None);
    }

    #[test]
    fn set_reports_changes() {
        let s = VariantSetting::default();
        assert!(!s.set(None));
        assert!(s.set(Some("metro".into())));
        assert!(!s.set(Some("metro".into())));
        assert!(s.set(None));
    }
}
