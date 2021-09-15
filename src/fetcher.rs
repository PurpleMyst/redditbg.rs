use std::path::PathBuf;

use eyre::{bail, ensure, Result, WrapErr};
use futures::prelude::*;
use image::{GenericImageView, ImageFormat};
use reqwest::Client;
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio_stream::wrappers::ReadDirStream;
use tracing::{debug, trace, warn};

use crate::platform;
use crate::utils::PersistentSet;
use crate::DIRS;

// This value is kinda arbitrary but there are 25 potential images in one reddit page
const MAX_CACHED: usize = 25;

/// Append a generated filename for an url to the given path buffer
fn make_filename(url: &str, image_format: ImageFormat) -> PathBuf {
    let mut path = DIRS.data_local_dir().join("images");
    let mut hasher = Sha256::new();
    hasher.update(url);
    let hash = hasher.finalize();
    path.push(format!(
        "{:x}.{}",
        hash,
        image_format.extensions_str().get(0).unwrap_or(&"dat")
    ));
    path
}

async fn download_count() -> Result<usize> {
    let path = DIRS.data_local_dir().join("images");
    Ok(ReadDirStream::new(fs::read_dir(path).await?)
        .fold(0usize, |acc, _| future::ready(acc + 1))
        .await)
}

/// Download one image into its place
#[tracing::instrument(skip(client))]
async fn fetch_one(client: &Client, url: String) -> Result<(), (String, eyre::Report)> {
    let result = (|| async {
        // Fetch the image's body
        let body: bytes::Bytes = with_backoff!(|| client
            .get(&url)
            .header("Accept", "image/*")
            .send()
            .and_then(|response| response.bytes()))
        .wrap_err_with(|| format!("Failed to fetch {:?}", url))?;
        trace!(size = body.len(), "got body");

        // Verify that it looks like an image
        let image_format = match image::guess_format(&body) {
            Ok(image_format) => {
                trace!(format = ?image_format, "got image");
                image_format
            }

            Err(err) => {
                trace!("not image");
                bail!(err);
            }
        };

        // Check the aspect ratio of our image
        let img = image::load_from_memory_with_format(&body, image_format)?;
        let (iw, ih) = (img.width(), img.height());
        let (sw, sh) = platform::screen_size()?;
        ensure!(
            (iw as f64 / ih as f64 - sw as f64 / sh as f64).abs() <= 0.01,
            "Aspect ratio not within two decimal places ({}:{} instead of {}:{})",
            iw,
            ih,
            sw,
            sh
        );

        // Let's calculate the path we want
        let path = make_filename(&url, image_format);

        // Now we'll write it to a temporary file that will then be *atomically* persisted once it's all written
        // The use of `spawn_blocking` means that once we start writing an image we *will* write an image
        // As per the tokio docs:
        // "Closures spawned using spawn_blocking cannot be cancelled.
        //  When you shut down the executor, it will wait indefinitely for all blocking operations to finish."
        // XXX: ^ We may be able to fix this if we use spawn_blocking just to
        //      return a `(File, Path)` back to async-land
        tokio::task::spawn_blocking({
            move || -> Result<()> {
                use std::io::prelude::*;
                let mut file = tempfile::NamedTempFile::new()?;
                trace!(path = ?file.path(), "created temporary file");
                file.write_all(&body).wrap_err("writing body")?;
                trace!("flushing temporary file");
                file.flush().wrap_err("flushing")?;
                trace!(path = %path.display(), "persisting temporary file");
                file.persist(path).wrap_err("persisting")?;
                Ok(())
            }
        })
        .await??;

        debug!("fetched successfully");

        Ok(())
    })();

    match result.await {
        Ok(()) => Ok(()),
        Err(err) => Err((url, err)),
    }
}

#[tracing::instrument(skip(client, urls))]
pub async fn fetch<Urls>(client: &Client, urls: Urls) -> Result<usize>
where
    Urls: Stream<Item = String>,
{
    let mut downloaded = PersistentSet::load("downloaded").await?;
    let cached_count = download_count().await?;

    let mut invalid = PersistentSet::load("invalid").await?;
    let mut new_invalid = Vec::new();

    // Fetch the needed images
    let fetched = urls
        // Skip over images we've already set
        .filter(|url| future::ready(!downloaded.contains(url) && !invalid.contains(url)))
        // Start downloading them, not necessarily in order
        .map(|url| {
            fetch_one(client, url).map_err(|(url, error)| {
                warn!(%url, %error, "failed fetching");
                url
            })
        })
        // Instead of polling in order, take a block of 25 and poll them all at once
        .buffer_unordered(25)
        // As they come in, filter out the ones that failed
        // Consider only the ones that succeeded for the max download calculation
        .filter_map(|r| {
            future::ready(match r {
                Ok(()) => Some(()),
                Err(url) => {
                    new_invalid.push(url);
                    None
                }
            })
        })
        .take(MAX_CACHED.saturating_sub(cached_count))
        // Count them out and return it
        .fold(0, |acc, ()| future::ready(acc + 1))
        .await;

    // Now let's update `downloaded` with what we've got in `images/`
    let dir = DIRS.data_local_dir().join("images");
    ReadDirStream::new(tokio::fs::read_dir(dir).await?)
        .try_for_each(|entry| {
            if let Some(stem) = entry.path().file_stem().and_then(std::ffi::OsStr::to_str) {
                downloaded.insert_hash(stem.to_owned());
            }

            future::ready(Ok(()))
        })
        .await?;

    downloaded.store().await?;

    new_invalid.into_iter().for_each(|url| {
        invalid.insert(url);
    });
    invalid.store().await?;

    Ok(fetched)
}
