use std::collections::HashSet;

use std::sync::{Arc, Mutex};

use eyre::Result;
use futures_retry::{ErrorHandler, RetryPolicy};
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt};
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
                warn!(name, %error, path = %path.display(), "failed to open persistent set");
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

// XXX: this shouldn't be a macro
#[macro_export]
macro_rules! with_backoff {
    ($expr:expr) => {{
        use ::exponential_backoff::Backoff;
        use ::futures_retry::FutureRetry;
        use ::std::time::Duration;

        let mut backoff = Backoff::new(10, Duration::from_secs(1), Duration::from_secs(15));
        backoff.set_jitter(0.3);
        backoff.set_factor(2);

        let retry_future = FutureRetry::new($expr, $crate::utils::BackoffPolicy(backoff.iter()));
        match retry_future.await {
            Ok((value, _)) => Ok(value),
            Err((err, _)) => Err(err),
        }
    }};
}

pub struct JoinOnDrop {
    handle: Option<std::thread::JoinHandle<Result<()>>>,
}

impl JoinOnDrop {
    pub fn new(handle: std::thread::JoinHandle<Result<()>>) -> Self {
        Self {
            handle: Some(handle),
        }
    }
}

impl Drop for JoinOnDrop {
    fn drop(&mut self) {
        match self.handle.take().unwrap().join() {
            Ok(Ok(())) => debug!("child thread joined"),

            Ok(Err(error)) => {
                error!(%error, "child thread returned error")
            }

            Err(error) => {
                let error: &dyn std::fmt::Display =
                    if let Some(error) = error.downcast_ref::<String>() {
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

#[derive(Debug)]
pub(crate) struct MutexWriter<T>(Arc<Mutex<T>>);

impl<T> MutexWriter<T> {
    pub(crate) fn new(x: T) -> Self {
        Self(Arc::new(Mutex::new(x)))
    }
}

impl<T> Clone for MutexWriter<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<W: std::io::Write> std::io::Write for MutexWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}
