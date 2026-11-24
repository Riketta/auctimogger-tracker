//! Hot reload of the manual recipe file. The core's `CraftService` holds the
//! live book behind a lock and never touches the filesystem; this module is
//! the driving-side counterpart that watches the configured path and swaps
//! the book in place when the file changes — so editing `recipes.toml` takes
//! effect on the next `/crafts` view instead of the next restart.

use std::{
    path::PathBuf,
    sync::Mutex,
    time::SystemTime,
};

use amtracker_core::services::craft::CraftService;

use crate::config_file;

/// What one [`RecipeReloader::check`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reload {
    /// The file is unchanged since the last check; nothing happened.
    Unchanged,
    /// The changed file parsed and validated: `n` recipes are now live.
    Reloaded(usize),
    /// The changed file could not be read, parsed or validated: the previous
    /// book stays live and the view shows the reason.
    Failed(String),
}

/// The file's identity as the filesystem reports it: modification time plus
/// size. Any editor save moves at least one of the two; an unchanged pair
/// means no read at all.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

/// Watches the configured recipe file and applies changes to the crafting
/// service. Checks are serialized: two concurrent `/crafts` requests see one
/// reload, not two.
pub struct RecipeReloader {
    path: PathBuf,
    stamp: Mutex<Option<Stamp>>,
}

impl RecipeReloader {
    /// Watches `path` (resolved exactly like the boot loader does: relative
    /// paths against the working directory).
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), stamp: Mutex::new(None) }
    }

    /// The watched path, rendered for log lines.
    #[must_use]
    pub fn path(&self) -> std::path::Display<'_> {
        self.path.display()
    }

    /// Applies the recipe file to `crafts` when it changed since the last
    /// check. A file that vanished or cannot be read is a failure (the book
    /// stays live and the view says so) — an editor's atomic save is brief
    /// enough that the next view re-reads and succeeds.
    pub fn check(&self, crafts: &CraftService) -> Reload {
        let mut stamp = self.stamp.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let meta = match std::fs::metadata(&self.path) {
            Ok(meta) => meta,
            Err(error) => {
                let message = format!("{}: {error}", self.path.display());
                crafts.reload(Err(message.clone()));
                // Forget the stamp: when the file returns, the next check
                // reads it however its timestamps compare.
                *stamp = None;
                return Reload::Failed(message);
            }
        };
        let current = Stamp { modified: meta.modified().ok(), len: meta.len() };
        if *stamp == Some(current) {
            return Reload::Unchanged;
        }
        *stamp = Some(current);
        match std::fs::read_to_string(&self.path)
            .map_err(|e| format!("{}: {e}", self.path.display()))
            .and_then(|text| {
                config_file::load_recipe_book(&text).map_err(|error| error.to_string())
            }) {
            Ok(recipes) => {
                let count = recipes.len();
                crafts.reload(Ok(recipes));
                Reload::Reloaded(count)
            }
            Err(error) => {
                crafts.reload(Err(error.clone()));
                Reload::Failed(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // Test construction is allowed to unwrap/expect: fixture-driven inputs.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;

    use amtracker_core::{
        config::CraftsConfig,
        services::{
            craft::CraftService,
            metrics::MetricsService,
            opportunity::OpportunityService,
        },
    };

    use super::*;

    fn service() -> Arc<CraftService> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let temp = std::env::temp_dir().join(format!(
            "amtracker-reload-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&temp).unwrap();
        let clock: Arc<dyn amtracker_core::ports::ClockPort> =
            Arc::new(amtracker_infra::clock::SystemClock);
        let store = Arc::new(
            amtracker_infra::store::SqliteStore::open(
                &temp.join("db.sqlite3"),
                &temp.join("raw"),
                clock.clone(),
                amtracker_infra::store::AutoVacuum::Off,
            )
            .unwrap(),
        );
        let metrics = Arc::new(MetricsService::new(
            store.clone(),
            store.clone(),
            store.clone(),
            clock.clone(),
        ));
        let opportunities = Arc::new(OpportunityService::new(
            store.clone(),
            store.clone(),
            store.clone(),
            store.clone(),
            clock,
            &amtracker_core::config::TrackerConfig {
                server: amtracker_core::config::ServerConfig {
                    bind: "127.0.0.1:0".into(),
                    token_env: None,
                    allow_open: true,
                    feed_publish_secs: 300,
                },
                storage: amtracker_core::config::StorageConfig {
                    database: String::new(),
                    raw_listing_days: 7,
                    sight_retention_days: 0,
                    item_hour_retention_days: 45,
                    archive_dir: String::new(),
                    feed_dir: String::new(),
                    backup_dir: None,
                    backup_secs: 0,
                    backup_keep: 7,
                    vacuum: amtracker_core::config::VacuumPolicy::Off,
                },
                metrics: amtracker_core::config::MetricsConfig { spans: vec![] },
                opportunities: amtracker_core::config::OpportunitiesConfig {
                    min_samples: 1,
                    weights: amtracker_core::config::ScoreWeights {
                        discount: 1.0,
                        sale: 1.0,
                        churn: 1.0,
                        stability: 1.0,
                    },
                },
                alerts: amtracker_core::config::AlertsConfig {
                    enabled: false,
                    cooldown_secs: 0,
                    delivery: "log".into(),
                    webhook_env: None,
                    token_env: None,
                    chat_id_env: None,
                    max_data_age_secs: 0,
                },
                crafts: CraftsConfig::default(),
                catalog: amtracker_core::config::CatalogConfig::default(),
                targets: vec![],
            },
        ));
        Arc::new(CraftService::new(opportunities, metrics, &CraftsConfig::default()))
    }

    fn write(path: &std::path::Path, body: &str) {
        std::fs::write(path, body).unwrap();
    }

    /// The 1-copper sentinel of the unchanged branch: a size-preserving save
    /// keeps the test honest about the stamp, so give the file a second to
    /// tick its mtime between writes where the content does not change size.
    fn touch_later(path: &std::path::Path, body: &str) {
        write(path, body);
        // Filesystem mtime granularity can be coarser than a nanosecond;
        // bump the mtime explicitly so a same-length rewrite is seen.
        let later = SystemTime::now() + std::time::Duration::from_secs(2);
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(later).unwrap();
    }

    /// A changed file replaces the live book; an unchanged one is not read.
    #[test]
    fn a_changed_recipe_file_reloads_and_an_unchanged_one_does_not() {
        let dir = std::env::temp_dir().join(format!("amtracker-reload-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("recipes.toml");
        write(&path, "[[recipe]]\nitem = 1\nreagents = { 2 = 1 }\n");
        let crafts = service();
        let reloader = RecipeReloader::new(&path);

        assert_eq!(reloader.check(&crafts), Reload::Reloaded(1));
        assert_eq!(crafts.recipe_count(), 1);
        assert_eq!(crafts.reload_error(), None);

        assert_eq!(reloader.check(&crafts), Reload::Unchanged, "no read without a change");
        touch_later(&path, "[[recipe]]\nitem = 1\nreagents = { 2 = 1 }\n[[recipe]]\nitem = 3\nreagents = { 2 = 2 }\n");
        assert_eq!(reloader.check(&crafts), Reload::Reloaded(2));
        assert_eq!(crafts.recipe_count(), 2, "the live book grew without a restart");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A broken save keeps the previous book live and reports why; fixing the
    /// file clears the error.
    #[test]
    fn a_broken_recipe_file_keeps_the_previous_book_and_names_the_fault() {
        let dir = std::env::temp_dir().join(format!("amtracker-reload-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("recipes.toml");
        write(&path, "[[recipe]]\nitem = 1\nreagents = { 2 = 1 }\n");
        let crafts = service();
        let reloader = RecipeReloader::new(&path);
        assert_eq!(reloader.check(&crafts), Reload::Reloaded(1));

        touch_later(&path, "[[recipe]]\nitem = 1\nreagents = { x = 1 }\n");
        let outcome = reloader.check(&crafts);
        assert!(matches!(outcome, Reload::Failed(_)), "{outcome:?}");
        assert_eq!(crafts.recipe_count(), 1, "the previous book stays live");
        assert!(
            crafts.reload_error().unwrap().contains("not an item id"),
            "{:?}",
            crafts.reload_error()
        );

        // A structural fault (a looping book) is refused the same way.
        touch_later(&path, "[[recipe]]\nitem = 1\nreagents = { 2 = 1 }\n[[recipe]]\nitem = 2\nreagents = { 1 = 1 }\n");
        assert!(matches!(reloader.check(&crafts), Reload::Failed(_)));
        assert!(crafts.reload_error().unwrap().contains("loops"));

        touch_later(&path, "[[recipe]]\nitem = 5\nreagents = { 2 = 3 }\n");
        assert_eq!(reloader.check(&crafts), Reload::Reloaded(1));
        assert_eq!(crafts.reload_error(), None, "a good save clears the error");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A missing file is a failure, not an empty book.
    #[test]
    fn a_missing_recipe_file_is_a_failure_not_an_empty_book() {
        let dir =
            std::env::temp_dir().join(format!("amtracker-reload-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("recipes.toml");
        write(&path, "[[recipe]]\nitem = 1\nreagents = { 2 = 1 }\n");
        let crafts = service();
        let reloader = RecipeReloader::new(&path);
        assert_eq!(reloader.check(&crafts), Reload::Reloaded(1));

        std::fs::remove_file(&path).unwrap();
        assert!(matches!(reloader.check(&crafts), Reload::Failed(_)));
        assert_eq!(crafts.recipe_count(), 1, "the book survives the missing file");

        write(&path, "[[recipe]]\nitem = 9\nreagents = { 2 = 1 }\n");
        assert_eq!(reloader.check(&crafts), Reload::Reloaded(1), "a returning file is read again");
        std::fs::remove_dir_all(&dir).ok();
    }
}
