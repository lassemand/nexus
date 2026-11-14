//! The exchange calendars of simulated trades, loaded from signal's database and
//! reloaded hourly.

use std::future::Future;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use model::session::Sessions;
use sqlx::PgPool;
use tracing::{info, warn};

/// How often the calendars are read again.
pub const RELOAD_EVERY: Duration = Duration::from_secs(3600);

/// The current calendars, cheap to read and replaced as a whole on reload. A
/// failed reload keeps the previous ones; before the first load there are none,
/// so every trade is `calendar_unknown`: never guessed.
#[derive(Debug, Clone, Default)]
pub struct SessionsCache(Arc<RwLock<Arc<Sessions>>>);

impl SessionsCache {
    /// A cache holding `sessions`.
    pub fn new(sessions: Sessions) -> SessionsCache {
        SessionsCache(Arc::new(RwLock::new(Arc::new(sessions))))
    }

    /// The calendars as they are now.
    pub fn get(&self) -> Arc<Sessions> {
        Arc::clone(
            &self
                .0
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    /// Replaces the calendars.
    pub fn set(&self, sessions: Sessions) {
        *self
            .0
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(sessions);
    }

    /// Reads the calendars from signal's database. Keeps the current ones when
    /// the read fails.
    pub async fn reload(&self, signal: &PgPool) {
        match Sessions::load(signal).await {
            Ok(sessions) => {
                if sessions.len() != self.get().len() {
                    info!(exchanges = sessions.len(), "exchange calendars loaded");
                }
                self.set(sessions);
            }
            Err(error) => {
                warn!(%error, "loading exchange calendars failed, using the last load");
            }
        }
    }

    /// Reloads every [`RELOAD_EVERY`] until `shutdown` completes. The first load
    /// is the caller's, at startup.
    pub async fn run_reload(&self, signal: PgPool, shutdown: impl Future<Output = ()>) {
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => return,
                () = tokio::time::sleep(RELOAD_EVERY) => self.reload(&signal).await,
            }
        }
    }
}
