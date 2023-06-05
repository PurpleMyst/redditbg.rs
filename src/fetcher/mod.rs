use std::{
    ffi::OsStr,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

use async_recursion::async_recursion;
use base64::prelude::*;
use bytes::Bytes;
use eyre::{ensure, format_err, Result, WrapErr};
use futures::prelude::*;
use image::ImageFormat;
use reqwest::Client;
use tokio::fs;
use tokio_stream::wrappers::ReadDirStream;
use tracing::{trace, warn};

use crate::{
    platform,
    utils::{with_backoff, LogError, PersistentSet},
    DIRS,
};

// This value is kinda arbitrary but there are 25 potential images in one reddit page
const MAX_CACHED: usize = 25;

// The accepted difference between the screen's aspect ratio and a potential image's aspect ratio
const ASPECT_RATIO_EPSILON: f64 = 0.01;

/// Append a generated filename for an url to the given path buffer
fn make_filename(url: &str, image_format: ImageFormat) -> PathBuf {
    let mut s = BASE64_URL_SAFE_NO_PAD.encode(url.as_bytes());
    s.push('.');
    s.push_str(image_format.extensions_str().get(0).unwrap_or(&"dat"));
    DIRS.data_local_dir().join("images").join(s)
}

/// Count how many images we've got cached.
async fn count_downloaded() -> Result<usize> {
    let path = DIRS.data_local_dir().join("images");
    Ok(ReadDirStream::new(fs::read_dir(path).await?)
        .fold(0usize, |acc, _| future::ready(acc + 1))
        .await)
}

struct Fetcher<'client> {
    downloaded: PersistentSet,
    invalid: PersistentSet,
    gotten: AtomicUsize,
    need: usize,
    client: &'client Client,
}

mod imgur;
mod reddit_gallery;

impl<'client> Fetcher<'client> {
    async fn new(client: &'client Client) -> Result<Fetcher<'client>> {
        let downloaded = PersistentSet::new("downloaded").await?;
        let invalid = PersistentSet::new("invalid").await?;
        let need = MAX_CACHED.saturating_sub(count_downloaded().await?);
        Ok(Self {
            downloaded,
            invalid,
            need,
            gotten: AtomicUsize::new(0),
            client,
        })
    }

    #[tracing::instrument(skip(self, body))]
    async fn parse_direct_image(&self, url: &str, body: Bytes) -> Result<()> {
        // Try to guess the format of the given body.
        // This will fail if the body isn't an image, returning early.
        let image_format = image::guess_format(&body)?;
        trace!(format = ?image_format, "got image");

        // Ensure the aspect ratio of the image is similiar to the one of the screen.
        let img = image::load_from_memory_with_format(&body, image_format)?;
        let (iw, ih) = (img.width(), img.height());
        let (sw, sh) = platform::screen_size()?;
        ensure!(
            (iw as f64 / ih as f64 - sw as f64 / sh as f64).abs() <= ASPECT_RATIO_EPSILON,
            "Aspect ratio not within epsilon of {} ({}:{} instead of {}:{})",
            ASPECT_RATIO_EPSILON,
            iw,
            ih,
            sw,
            sh
        );

        // Let's calculate the path the image should be saved to.
        let path = make_filename(url, image_format);

        // Now we'll write the image to a temporary file, that will then be
        // persisted once it's all written. The use of `spawn_blocking` means that once
        // we start writing an image, we will write an image. As per the tokio docs:
        // "Closures spawned using spawn_blocking cannot be cancelled. When you shut
        // down the executor, it will wait indefinitely for all blocking operations to
        // finish."
        tokio::task::spawn_blocking({
            move || -> Result<()> {
                use std::io::prelude::*;
                let mut file = tempfile::NamedTempFile::new()?;
                trace!(path = %file.path().display(), "created temporary file");
                file.write_all(&body).wrap_err("writing body")?;
                trace!("flushing temporary file");
                file.flush().wrap_err("flushing")?;
                trace!(path = %path.display(), "persisting temporary file");
                file.persist(path).wrap_err("persisting")?;
                Ok(())
            }
        })
        .await??;

        // If we get here, we've succesffully persisted an image to disk and
        // we can add it to the `gotten` count.
        self.gotten.fetch_add(1, Ordering::AcqRel);

        Ok(())
    }

    /// Download one image into its place
    #[tracing::instrument(skip(self))]
    #[async_recursion(?Send)]
    async fn fetch_one(&self, url: String) -> Result<()> {
        // We create a closure as a pseudo-try block.
        let result = (|| async {
            // Fetch the url's body
            let body: Bytes = with_backoff(|| {
                self.client
                    .get(&url)
                    .header("Accept", "image/*")
                    .send()
                    .and_then(|response| response.bytes())
            })
            .await
            .wrap_err_with(|| format!("Failed to fetch {:?}", url))?;
            trace!(size = body.len(), "got body");

            // Try to parse it as a direct image.
            match self.parse_direct_image(&url, body.clone()).await {
                Ok(()) => return Ok(()),
                Err(error) => {
                    trace!(error = %LogError(&error), "failed direct image check");
                }
            }

            // Try to parse it as an imgur gallery.
            match self.parse_imgur_gallery(&url, body.clone()).await {
                Ok(..) => return Ok(()),
                Err(error) => {
                    trace!(error = %LogError(&error), "failed imgur gallery check");
                }
            }

            // Try to parse it as a reddit gallery.
            match self.parse_reddit_gallery(&url, body.clone()).await {
                Ok(..) => return Ok(()),
                Err(error) => {
                    trace!(error = %LogError(&error), "failed reddit gallery check");
                }
            }

            // If we get here, we've no idea what this URL is.
            Err(format_err!("Unable to parse as anything known"))
        })()
        .await;

        // Having collected the result, if we got an error log it and mark this URL as invalid.
        if let Err(ref error) = result {
            warn!(%url, error = %LogError(&error), "failed fetching");
            self.invalid.insert(url).await?;
        }

        result
    }

    #[tracing::instrument(skip_all)]
    #[async_recursion(?Send)]
    async fn fetch_multiple<Urls>(&self, urls: Urls) -> Result<usize>
    where
        Urls: Stream<Item = String> + Unpin,
    {
        // Iterate over the given URLs, counting how many we "touch".
        let mut touched = 0;
        {
            let mut futures = std::pin::pin!(urls
                .inspect(|_| touched += 1)
                // Skip over URLs we've already examined
                .filter(|url| {
                    let url = url.to_owned();
                    async move {
                        let downloaded = self.downloaded.contains(url.clone()).await.unwrap();
                        let invalid = self.invalid.contains(url.clone()).await.unwrap();
                        trace!(%url, downloaded, invalid, "url status");
                        !(downloaded || invalid)
                    }
                })
                // Start fetching the specfic URLs themselves
                .map(|url| self.fetch_one(url))
                // Instead of polling in order, take a block of 25 and poll them all at once
                .buffer_unordered(25));

            // Iterate over the futures as they complete and stop once we've gotten enough.
            while let Some(res) = futures.next().await {
                let gotten = self.gotten.load(Ordering::Acquire);
                trace!(gotten, success = res.is_ok(), "future completed");
                if gotten >= self.need {
                    break;
                }
            }
        }
        Ok(touched)
    }

    #[tracing::instrument(skip_all)]
    async fn fetch_toplevel<Urls>(self, urls: Urls) -> Result<()>
    where
        Urls: Stream<Item = String> + Unpin,
    {
        // Offload actual fetching to `fetch_multiple`.
        self.fetch_multiple(urls).await?;

        // Add that which we've downloaded to our database
        let mut dir = tokio::fs::read_dir(DIRS.data_local_dir().join("images")).await?;
        while let Some(entry) = dir.next_entry().await? {
            if let Some(url) = entry
                .path()
                .file_stem()
                .and_then(OsStr::to_str)
                .and_then(|s| Some(BASE64_URL_SAFE_NO_PAD.decode(s.as_bytes()).ok()?))
                .and_then(|buf| String::from_utf8(buf).ok())
            {
                self.downloaded.insert(url).await?;
            }
        }

        Ok(())
    }
}

#[tracing::instrument(skip_all)]
pub async fn fetch<Urls>(client: &Client, urls: Urls) -> Result<()>
where
    Urls: Stream<Item = String> + Unpin,
{
    Fetcher::new(client).await?.fetch_toplevel(urls).await
}
