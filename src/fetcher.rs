use eyre::{Result, WrapErr};
use futures::prelude::*;
use reqwest::Client;
use sha2::{Digest, Sha256};
use slog::{debug, info, o, warn, Logger};
use tokio::fs;

use crate::utils::PersistentSet;
use crate::DIRS;

// This value is kinda arbitrary but there are 25 potential images in one reddit page
const MAX_CACHED: usize = 25;

/// Append a generated filename for an url to the given path buffer
fn make_filename(path: &mut std::path::PathBuf, url: &str) {
    let mut hasher = Sha256::new();
    hasher.input(url);
    let hash = hasher.result();
    // As far as I can tell the image crate has no way to get the extension for a given format
    // TODO: ^ that's a lie thanks to my github issue
    path.push(format!("{:x}.dat", hash));
}

async fn download_count() -> Result<usize> {
    let path = DIRS.data_local_dir().join("images");
    Ok(fs::read_dir(path)
        .await?
        .fold(0, |acc, _| future::ready(acc + 1))
        .await)
}

/// Download one image into its place
// TODO: we can probably extract more images
async fn fetch_one(logger: Logger, client: &Client, url: String) -> Result<()> {
    // Fetch the image's body
    let body: bytes::Bytes = with_backoff!(|| client
        .get(&url)
        .header("Accept", "image/*")
        .send()
        .and_then(|response| response.bytes()))
    .wrap_err_with(|| format!("Failed to fetch {:?}", url))?;
    info!(logger, "got body"; "size" => body.len());

    // Verify that it looks like an image
    match ::image::guess_format(&body) {
        Ok(fmt) => info!(logger, "got image"; "format" => ?fmt),
        Err(err) => {
            info!(logger, "not image");
            return Err(err.into());
        }
    }

    // Let's calculate the path we want
    let mut path = DIRS.data_local_dir().join("images");
    make_filename(&mut path, &url);

    // Now we'll write it to a temporary file that will then be *atomically* persisted once it's all written
    // The use of `spawn_blocking` means that once we start writing an image we *will* write an image
    // As per the tokio docs:
    // "Closures spawned using spawn_blocking cannot be cancelled.
    //  When you shut down the executor, it will wait indefinitely for all blocking operations to finish."
    // XXX: ^ We may be able to fix this if we use spawn_blocking just to
    //      return a `(File, Path)` back to async-land
    tokio::task::spawn_blocking(move || -> Result<()> {
        use std::io::prelude::*;
        let mut file = tempfile::NamedTempFile::new()?;
        debug!(logger, "created temporary file"; "path" => ?file.path());
        file.write_all(&body).wrap_err("writing body")?;
        debug!(logger, "flushing temporary file");
        file.flush().wrap_err("flushing")?;
        debug!(logger, "persisting temporary file"; "path" => ?path);
        file.persist(path).wrap_err("persisting")?;
        Ok(())
    })
    .await??;

    Ok(())
}

pub async fn fetch<Urls>(logger: Logger, client: &Client, urls: Urls) -> Result<usize>
where
    Urls: Stream<Item = String>,
{
    let mut downloaded = PersistentSet::load(logger.clone(), "downloaded").await?;
    let cached_count = download_count().await?;

    // Fetch the needed images
    let fetched = urls
        // Skip over images we've already set
        .filter(|url| future::ready(!downloaded.contains(url)))
        // Start downloading them, not necessarily in order
        .map(|url| {
            let logger = logger.new(o!("url" => url.clone()));
            fetch_one(logger.clone(), client, url)
                .map_err(move |err| warn!(logger, "failed fetching"; "error" => ?err))
        })
        // Instead of polling in order, take a block of 25 and poll them all at once
        .buffer_unordered(25)
        // As they come in, filter out the ones that failed
        .filter_map(|result| future::ready(result.ok()))
        // Consider only the ones that succeeded for the max download calculation
        .take(MAX_CACHED.saturating_sub(cached_count))
        // Count them out and return it
        .fold(0, |acc, ()| future::ready(acc + 1))
        .await;

    // Now let's update `downloaded` with what we've got in `images/`
    let dir = DIRS.data_local_dir().join("images");
    tokio::fs::read_dir(dir)
        .await?
        .try_for_each(|entry| {
            if let Some(stem) = entry.path().file_stem().and_then(std::ffi::OsStr::to_str) {
                downloaded.insert_hash(stem.to_owned());
            }

            future::ready(Ok(()))
        })
        .await?;
    downloaded.store().await?;

    Ok(fetched)
}
