use std::{
    ffi::OsStr,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

use async_recursion::async_recursion;
use base64::prelude::*;
use bytes::Bytes;
use eyre::{bail, Result, WrapErr};
use futures::prelude::*;
use image::{imageops::FilterType::Lanczos3, ImageFormat};
use reqwest::Client;
use tokio::fs;
use tokio_stream::wrappers::ReadDirStream;
use tracing::{debug, trace, trace_span};

use crate::{
    platform,
    utils::{with_backoff, PersistentSet},
    DIRS,
};

// This value is kinda arbitrary but there are 25 potential images in one reddit page
const MAX_CACHED: usize = 25;

// The accepted difference between the screen's aspect ratio and a potential image's aspect ratio
const ASPECT_RATIO_EPSILON: f64 = 0.01;

// Which format to utilize for storing the images in the directory.
const STORAGE_FORMAT: ImageFormat = ImageFormat::Png;

#[derive(thiserror::Error, Debug)]
#[error("Aspect ratio not within epsilon ({iw}:{ih} instead of {sw}:{sh})")]
struct InvalidAspectRatio {
    iw: u32,
    ih: u32,
    sw: u32,
    sh: u32,
}

/// Append a generated filename for an url to the given path buffer
fn make_filename(url: &str, image_format: ImageFormat) -> PathBuf {
    let mut s = BASE64_URL_SAFE_NO_PAD.encode(url.as_bytes());
    s.push('.');
    s.push_str(image_format.extensions_str().first().unwrap_or(&"dat"));
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
    async fn parse_raw_image(&self, url: &str, body: Bytes) -> Result<()> {
        // Try to guess the format from the body, returning early if it isn't an image.
        let original_format = image::guess_format(&body)?;
        trace!(?original_format, "detected as image");

        // Load the image and ensure the aspect ratio of the image is similiar to the one of the screen.
        let img = image::load_from_memory_with_format(&body, original_format)?;
        let (iw, ih) = (img.width(), img.height());
        let (sw, sh) = platform::screen_size()?;
        if (f64::from(iw) / f64::from(ih) - f64::from(sw) / f64::from(sh)).abs() > ASPECT_RATIO_EPSILON {
            bail!(InvalidAspectRatio { iw, ih, sw, sh });
        }

        // Now let's spawn a blocking task that resizes our image and persists it to a temporary
        // file. We do this in a separate task due to two advantages it has:
        // 1) the runtime isn't blocked on the CPU-heavy task of resizing the image;
        // 2) blocking tasks can not be canceled so we won't get half-written images.
        let dst = make_filename(url, STORAGE_FORMAT);
        tokio::task::spawn_blocking({
            move || -> Result<()> {
                use std::io::prelude::*;
                let _span = trace_span!("writing fetched image", dst = %dst.display()).entered();
                let mut file = tempfile::NamedTempFile::new()?;
                trace!(tmp_path = %file.path().display(), "created temporary file");
                img.resize(sw, sh, Lanczos3)
                    .write_to(&mut file, STORAGE_FORMAT)
                    .wrap_err("failed to write image")?;
                trace!("flushing temporary file");
                file.flush().wrap_err("failed to flush")?;
                trace!("persisting temporary file");
                file.persist(dst).wrap_err("failed to persist")?;
                Ok(())
            }
        })
        .await??;

        // If we get here, we've successfully persisted an image to disk and we can add it to the `gotten` count.
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
                    .and_then(reqwest::Response::bytes)
            })
            .await
            .wrap_err_with(|| format!("Failed to fetch {url:?}"))?;
            trace!(size = body.len(), "got body");

            // Try to parse it as a raw image.
            match self.parse_raw_image(&url, body.clone()).await {
                Ok(()) => return Ok(()),
                Err(error) => {
                    if let Some(InvalidAspectRatio { .. }) = error.downcast_ref() {
                        trace!(%error, "failed direct image check due to aspect ratio, bailing");
                        return Err(error);
                    }

                    trace!(?error, "failed direct image check, continuing on");
                }
            }

            // Try to parse it as an imgur gallery.
            match self.parse_imgur_gallery(&url, body.clone()).await {
                Ok(..) => return Ok(()),
                Err(error) => {
                    trace!(?error, "failed imgur gallery check");
                }
            }

            // Try to parse it as a reddit gallery.
            match self.parse_reddit_gallery(&url, body.clone()).await {
                Ok(..) => return Ok(()),
                Err(error) => {
                    trace!(?error, "failed reddit gallery check");
                }
            }

            // If we get here, we've no idea what this URL is.
            bail!("Unable to parse as anything known");
        })()
        .await;

        // Having collected the result, if we got an error log it and mark this URL as invalid.
        if let Err(ref error) = result {
            debug!(%url, ?error, "failed fetching");
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
                    let url = url.clone();
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
        // If we don't need anything, bail!
        if self.need == 0 {
            return Ok(());
        }

        // Offload actual fetching to `fetch_multiple`.
        self.fetch_multiple(urls).await?;

        // Add that which we've downloaded to our database
        let mut dir = tokio::fs::read_dir(DIRS.data_local_dir().join("images")).await?;
        while let Some(entry) = dir.next_entry().await? {
            if let Some(url) = entry
                .path()
                .file_stem()
                .and_then(OsStr::to_str)
                .and_then(|s| BASE64_URL_SAFE_NO_PAD.decode(s.as_bytes()).ok())
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
