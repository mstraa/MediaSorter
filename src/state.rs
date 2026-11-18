use std::collections::HashMap;
use std::path::PathBuf;

use crate::config::AppConfig;
use crate::db::Database;
use crate::dedupe::DedupeService;
use crate::jobs::JobManager;
use crate::providers::MetadataProviders;

pub const MEDIA_TYPES: &[&str] = &["tv", "anime", "film", "music"];
pub const SOURCE_STATUSES: &[&str] = &[
    "none", "imported", "failed", "skipped", "preview", "conflict",
];
pub const PICKER_ROOTS: &[&str] = &["/mnt", "/media", "/srv", "/opt", "/var/lib", "/"];
/// Allowed import actions. The `imports.action` CHECK constraint in `db.rs`
/// (schema + migration) must mirror this list.
pub const IMPORT_ACTIONS: &[&str] = &["hardlink", "copy", "move", "test"];
/// Allowed conflict policies. Mirrored by the `imports.conflict_policy` CHECK
/// constraint in `db.rs`.
pub const CONFLICT_POLICIES: &[&str] = &["skip", "replace", "index", "fail"];

#[derive(Clone)]
pub struct AppState {
    #[allow(dead_code)]
    pub config: AppConfig,
    pub db: Database,
    pub providers: MetadataProviders,
    pub jobs: JobManager,
    pub dedupe: DedupeService,
}

impl AppState {
    pub fn output_roots(&self) -> HashMap<String, PathBuf> {
        let mut roots = HashMap::new();
        for &media_type in MEDIA_TYPES {
            let key = format!("{media_type}_output_root");
            let value = self.db.get_setting(&key, "");
            if !value.is_empty() {
                roots.insert(media_type.to_string(), PathBuf::from(value));
            }
        }
        roots
    }

    pub fn output_root_for(&self, media_type: &str) -> Option<PathBuf> {
        let value = self
            .db
            .get_setting(&format!("{media_type}_output_root"), "");
        if value.is_empty() {
            None
        } else {
            Some(PathBuf::from(value))
        }
    }

    pub fn copy_rate_limit_mbps(&self) -> Option<f64> {
        let value = self.db.get_setting("copy_rate_limit_mbps", "15");
        match value.parse::<f64>() {
            Ok(limit) if limit > 0.0 => Some(limit),
            Ok(_) => None,
            Err(_) => Some(15.0),
        }
    }
}

pub fn is_valid_media_type(media_type: &str) -> bool {
    MEDIA_TYPES.contains(&media_type)
}

pub fn is_valid_action(action: &str) -> bool {
    IMPORT_ACTIONS.contains(&action)
}

pub fn is_valid_conflict_policy(policy: &str) -> bool {
    CONFLICT_POLICIES.contains(&policy)
}
