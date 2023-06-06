use std::{
    fmt::{Debug, Display},
    time::Duration,
};

use exponential_backoff::Backoff;
use eyre::Result;
use futures::Future;
use futures_retry::{ErrorHandler, FutureRetry, RetryPolicy};
use rusqlite::{params, OptionalExtension};
use tokio::sync::OnceCell;
use tracing::{debug, error, trace};

use crate::DIRS;

pub struct BackoffPolicy<'a>(pub exponential_backoff::Iter<'a>);

impl<E> ErrorHandler<E> for BackoffPolicy<'_> {
    type OutError = E;

    fn handle(&mut self, _attempt: usize, err: E) -> RetryPolicy<Self::OutError> {
        match self.0.next() {
            Some(duration) => RetryPolicy::WaitRetry(duration),
            None => RetryPolicy::ForwardError(err),
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
pub fn report_ie(ie: deadpool_sqlite::InteractError) -> eyre::Report {
    eyre::format_err!("Interact error: {ie:?}")
}

static DB_POOL: OnceCell<deadpool_sqlite::Pool> = OnceCell::const_new();

#[derive(Clone, Copy, Debug)]
pub struct PersistentSet {
    name: &'static str,
}

impl PersistentSet {
    pub async fn new(name: &'static str) -> Result<Self> {
        let _pool = DB_POOL
            .get_or_try_init(|| async {
                let cfg = deadpool_sqlite::Config::new(DIRS.data_local_dir().join("db.sqlite3"));
                let pool = cfg.builder(deadpool_sqlite::Runtime::Tokio1)?.build()?;
                pool.get()
                    .await?
                    .interact(|conn| conn.execute_batch(include_str!("persistent_set.sql")))
                    .await
                    .map_err(report_ie)??;
                Ok::<_, eyre::Report>(pool)
            })
            .await?;
        Ok(Self { name })
    }

    pub async fn insert(&self, url: String) -> Result<()> {
        trace!(?self, ?url, "inserting into persistent set");
        let name = self.name; // so that the closure is able to Copy the static str into it
        let conn = DB_POOL.get().unwrap().get().await?;
        conn.interact(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO PersistentSets(name, url) VALUES (?, ?)",
                params![name, url],
            )
        })
        .await
        .map_err(report_ie)??;
        Ok(())
    }

    pub async fn contains(&self, url: String) -> Result<bool> {
        trace!(?self, ?url, "checking persistent set");
        let name = self.name;
        let conn = DB_POOL.get().unwrap().get().await?;
        Ok(conn
            .interact(move |conn| {
                conn.query_row(
                    "SELECT rowid FROM PersistentSets WHERE name = ? AND url = ?",
                    params![name, url],
                    |_| Ok(()),
                )
                .optional()
                .map(|o| o.is_some())
            })
            .await
            .map_err(report_ie)??)
    }
}

pub(crate) async fn with_backoff<T, E, F, Factory>(factory: Factory) -> Result<T, E>
where
    F: Future<Output = Result<T, E>>,
    Factory: std::marker::Unpin + FnMut() -> F,
{
    let mut backoff = Backoff::new(10, Duration::from_secs(1), Duration::from_secs(15));
    backoff.set_jitter(0.3);
    backoff.set_factor(2);

    let retry = FutureRetry::new(factory, BackoffPolicy(backoff.iter()));
    match retry.await {
        Ok((value, _)) => Ok(value),
        Err((error, _)) => Err(error),
    }
}

pub struct JoinOnDrop {
    handle: Option<std::thread::JoinHandle<Result<()>>>,
}

impl JoinOnDrop {
    pub fn new(handle: std::thread::JoinHandle<Result<()>>) -> Self {
        Self { handle: Some(handle) }
    }
}

impl Drop for JoinOnDrop {
    fn drop(&mut self) {
        match self.handle.take().unwrap().join() {
            Ok(Ok(())) => debug!("child thread joined"),

            Ok(Err(error)) => {
                error!(?error, "child thread returned error");
            }

            Err(error) => {
                let error: &dyn Display = if let Some(error) = error.downcast_ref::<String>() {
                    error
                } else if let Some(error) = error.downcast_ref::<&'static str>() {
                    error
                } else {
                    &"not of known type"
                };

                error!(%error, "child thread panic");
            }
        }
    }
}
