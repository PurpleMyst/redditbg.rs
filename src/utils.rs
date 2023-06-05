use std::collections::HashSet;

use std::{
    fmt::{Debug, Display},
    time::Duration,
};

use exponential_backoff::Backoff;
use eyre::Result;
use futures::Future;
use futures_retry::{ErrorHandler, FutureRetry, RetryPolicy};
use sha2::{Digest, Sha256};
use tokio::{
    fs,
    io::{self, AsyncBufReadExt, AsyncWriteExt},
};
use tracing::{debug, error, trace, warn};

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

pub struct PersistentSet {
    name: &'static str,
    contents: HashSet<String>,
}

impl PersistentSet {
    pub async fn load(name: &'static str) -> Result<Self> {
        // We use this in all subsequent logging calls rather than setting it as
        // a logger value because that introduces weird borrowck things
        let path = DIRS.data_local_dir().join(format!("{}.txt", name));

        trace!(path = %path.display(), "loading persistent set");
        let file = match fs::OpenOptions::new().read(true).open(&path).await {
            Ok(file) => file,
            Err(error) => {
                let error = eyre::Report::from(error);
                warn!(name, error = %LogError(&error), path = %path.display(), "failed to open persistent set");
                return Ok(Self {
                    name,
                    contents: HashSet::new(),
                });
            }
        };

        let mut reader = io::BufReader::new(file);
        let mut contents = HashSet::new();

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
        trace!(path = %path.display(), "loaded persistent set");
        Ok(Self { name, contents })
    }

    pub async fn store(self) -> Result<()> {
        let path = DIRS.data_local_dir().join(format!("{}.txt", self.name));
        trace!(path = %path.display(), "storing persistent set");
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
        hasher.update(url);
        let hash = hasher.finalize();
        format!("{:x}", hash)
    }

    pub fn insert(&mut self, value: String) -> bool {
        self.insert_hash(Self::hash(&value))
    }

    pub fn insert_hash(&mut self, hash: String) -> bool {
        self.contents.insert(hash)
    }

    pub fn contains(&self, url: &str) -> bool {
        self.contents.contains(&Self::hash(url))
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
                error!(error = %LogError(&error), "child thread returned error")
            }

            Err(error) => {
                let error: &dyn Display = if let Some(error) = error.downcast_ref::<String>() {
                    error
                } else if let Some(error) = error.downcast_ref::<&'static str>() {
                    error
                } else {
                    &"not of known type"
                };

                error!(%error, "child thread panic")
            }
        }
    }
}

pub(crate) struct LogError<T: Debug>(pub(crate) T);

impl<T: Debug> Display for LogError<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
}
