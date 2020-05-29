use std::collections::HashSet;

use anyhow::Result;
use futures_retry::{ErrorHandler, RetryPolicy};
use log::warn;
use sha2::{Digest, Sha256};
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

/// Calculate the aspect ratio of a given image
pub struct AlreadySet(HashSet<String>);

// XXX: would this be better as an "already downloaded" instead of "already set"?
impl AlreadySet {
    pub async fn load() -> Result<Self> {
        let path = DIRS.data_local_dir().join("already_set.txt");

        let file = match fs::OpenOptions::new().read(true).open(path).await {
            Ok(file) => file,
            Err(err) => {
                warn!("Could not open already_set.txt: {:?}", err);
                return Ok(Self(HashSet::new()));
            }
        };

        let mut reader = io::BufReader::new(file);
        let mut result = HashSet::new();

        loop {
            let mut line = String::new();
            let read = reader.read_line(&mut line).await?;
            if read == 0 {
                break;
            }
            result.insert(line);
        }
        Ok(Self(result))
    }

    pub async fn store(self) -> Result<()> {
        let path = DIRS.data_local_dir().join("already_set.txt");
        let contents = self.0.into_iter().collect::<Vec<_>>().join("\n");
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

    pub fn insert_hash(&mut self, hash: String) -> bool {
        self.0.insert(hash)
    }

    pub fn contains(&self, url: &str) -> bool {
        let mut hasher = Sha256::new();
        hasher.input(url);
        let hash = hasher.result();
        self.0.contains(&format!("{:x}", hash))
    }
}

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

        let retry_future = ::futures_retry::FutureRetry::new(
            || $expr,
            $crate::utils::BackoffPolicy(backoff.iter()),
        );
        match retry_future.await {
            Ok((value, _)) => Ok(value),
            Err((err, _)) => Err(err),
        }
    }};
}
