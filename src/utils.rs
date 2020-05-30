use std::collections::HashSet;
use std::convert::TryFrom;

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
pub struct PersistentSet(HashSet<String>);

impl PersistentSet {
    pub async fn load(name: &str) -> Result<Self> {
        let path = DIRS.data_local_dir().join(format!("{}.txt", name));

        let file = match fs::OpenOptions::new().read(true).open(path).await {
            Ok(file) => file,
            Err(err) => {
                warn!("Could not open {}.txt: {:?}", name, err);
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
            if line.ends_with('\n') {
                // Remove the final newline if it is present
                line.pop();
            }
            result.insert(line);
        }
        Ok(Self(result))
    }

    pub async fn store(self, name: &str) -> Result<()> {
        let path = DIRS.data_local_dir().join(format!("{}.txt", name));
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

    fn hash(url: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.input(url);
        let hash = hasher.result();
        format!("{:x}", hash)
    }

    pub fn insert_hash(&mut self, hash: String) -> bool {
        self.0.insert(hash)
    }

    pub fn contains(&self, url: &str) -> bool {
        self.0.contains(&Self::hash(url))
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
    anyhow::ensure!(width != 0, "GetSystemMetrics's returned width was zero");
    anyhow::ensure!(height != 0, "GetSystemMetrics's returned height was zero");

    Ok((u32::try_from(width)?, u32::try_from(height)?))
}
