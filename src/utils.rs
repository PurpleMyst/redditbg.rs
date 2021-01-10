use std::collections::HashSet;
use std::convert::TryFrom;

use eyre::Result;
use futures_retry::{ErrorHandler, RetryPolicy};
use sha2::{Digest, Sha256};
use slog::{debug, error, warn, Logger};
use tokio::{fs, io, prelude::*};

use crate::DIRS;

pub struct BackoffPolicy<'a>(pub exponential_backoff::Iter<'a>);

impl<E> ErrorHandler<E> for BackoffPolicy<'_> {
    type OutError = E;

    fn handle(&mut self, _attempt: usize, err: E) -> RetryPolicy<Self::OutError> {
        match self.0.next() {
            Some(Some(duration)) => RetryPolicy::WaitRetry(duration),
            Some(None) | None => RetryPolicy::ForwardError(err),
        }
    }
}

pub struct PersistentSet {
    logger: Logger,
    name: &'static str,
    contents: HashSet<String>,
}

impl PersistentSet {
    pub async fn load(logger: Logger, name: &'static str) -> Result<Self> {
        // We use this in all subsequent logging calls rather than setting it as
        // a logger value because that introduces weird borrowck things
        let path = DIRS.data_local_dir().join(format!("{}.txt", name));

        debug!(logger, "loading persistent set"; "path" => %path.display());
        let file = match fs::OpenOptions::new().read(true).open(&path).await {
            Ok(file) => file,
            Err(err) => {
                warn!(logger, "failed to open persistent set"; "name" => name, "error" => %err, "path" => %path.display());
                return Ok(Self {
                    logger,
                    name,
                    contents: HashSet::new(),
                });
            }
        };

        let mut reader = io::BufReader::new(file);
        let mut contents = HashSet::new();

        // TODO: refactor this to use BufRead::lines
        loop {
            let mut line = String::new();
            let read = reader.read_line(&mut line).await?;
            if read == 0 {
                break;
            }
            if line.ends_with('\n') {
                // Remove the final newline if it is present
                line.pop();
            }
            contents.insert(line);
        }
        debug!(logger, "loaded persistent set"; "path" => %path.display());
        Ok(Self {
            logger,
            name,
            contents,
        })
    }

    pub async fn store(self) -> Result<()> {
        let path = DIRS.data_local_dir().join(format!("{}.txt", self.name));
        debug!(self.logger, "storing persistent set"; "path" => %path.display());
        let contents = self.contents.into_iter().collect::<Vec<_>>().join("\n");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .await?;
        file.write_all(contents.as_bytes()).await?;
        file.sync_all().await?;
        Ok(())
    }

    fn hash(url: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.input(url);
        let hash = hasher.result();
        format!("{:x}", hash)
    }

    pub fn insert_hash(&mut self, hash: String) -> bool {
        self.contents.insert(hash)
    }

    pub fn contains(&self, url: &str) -> bool {
        self.contents.contains(&Self::hash(url))
    }
}

// XXX: this shouldn't be a macro
#[macro_export]
macro_rules! with_backoff {
    ($expr:expr) => {{
        let backoff = ::exponential_backoff::Backoff::new(10)
            .timeout_range(
                ::std::time::Duration::from_secs(1),
                ::std::time::Duration::from_secs(15),
            )
            .jitter(0.3)
            .factor(2);

        let retry_future =
            ::futures_retry::FutureRetry::new($expr, $crate::utils::BackoffPolicy(backoff.iter()));
        match retry_future.await {
            Ok((value, _)) => Ok(value),
            Err((err, _)) => Err(err),
        }
    }};
}

#[cfg(windows)]
pub fn screen_size() -> Result<(u32, u32)> {
    use winapi::um::winuser::{GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN};

    let (width, height) = unsafe {
        (
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };

    // try_winapi! is useless here as GetSystemMetrics does not use GetLastError
    eyre::ensure!(width != 0, "GetSystemMetrics's returned width was zero");
    eyre::ensure!(height != 0, "GetSystemMetrics's returned height was zero");

    Ok((u32::try_from(width)?, u32::try_from(height)?))
}

pub struct JoinOnDrop {
    logger: Logger,
    handle: Option<std::thread::JoinHandle<Result<()>>>,
}

impl JoinOnDrop {
    pub fn new(logger: Logger, handle: std::thread::JoinHandle<Result<()>>) -> Self {
        Self {
            logger,
            handle: Some(handle),
        }
    }
}

impl Drop for JoinOnDrop {
    fn drop(&mut self) {
        match self.handle.take().unwrap().join() {
            Ok(Ok(())) => debug!(self.logger, "child thread joined"),

            Ok(Err(err)) => error!(self.logger, "child thread returned error"; "error" => %err),

            Err(err) => {
                let err: &dyn std::fmt::Display = if let Some(err) = err.downcast_ref::<String>() {
                    err
                } else if let Some(err) = err.downcast_ref::<&'static str>() {
                    err
                } else {
                    &"not of known type"
                };

                error!(self.logger, "child thread panic"; "error" => %err)
            }
        }
    }
}
