use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_recursion::async_recursion;
use bytes::Bytes;
use eyre::{ensure, format_err, Result, WrapErr};
use futures::prelude::*;
use image::{GenericImageView, ImageFormat};
use reqwest::Client;
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_stream::wrappers::ReadDirStream;
use tracing::{debug, trace, warn};

use crate::platform;
use crate::utils::{with_backoff, PersistentSet};
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

#[derive(serde::Deserialize)]
struct ImgurGallery {
    media: Vec<Media>,
}

#[derive(serde::Deserialize)]
struct Media {
    url: String,
}

struct Fetcher<'client> {
    downloaded: PersistentSet,
    invalid: PersistentSet,
    gotten: AtomicUsize,
    need: usize,
    client: &'client Client,

    invalid_tx: Option<UnboundedSender<String>>,
    invalid_rx: UnboundedReceiver<String>,
}

impl<'client> Fetcher<'client> {
    #[tracing::instrument(skip_all)]
    async fn try_as_direct_image(&self, url: &str, body: Bytes) -> Result<()> {
        let image_format = image::guess_format(&body)?;
        trace!(format = ?image_format, "got image");

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
        let path = make_filename(url, image_format);

        // Now we'll write it to a temporary file that will then be *atomically* persisted once it's all written
        // The use of `spawn_blocking` means that once we start writing an image we *will* write an image
        // As per the tokio docs:
        // "Closures spawned using spawn_blocking cannot be cancelled.
        //  When you shut down the executor, it will wait indefinitely for all blocking operations to finish."
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

        self.gotten.fetch_add(1, Ordering::SeqCst);

        Ok(())
    }

    #[tracing::instrument(skip_all)]
    #[async_recursion(?Send)]
    async fn try_as_imgur_gallery(&self, url: &str, body: Bytes) -> Result<()> {
        // Parse HTML and ensure there were no errors
        let html = scraper::Html::parse_document(
            std::str::from_utf8(&body).wrap_err("Body was not valid UTF-8.")?,
        );
        ensure!(html.errors.is_empty(), "html.errors was not empty");

        // Extract a script tag containing the text "postDataJSON"
        let script = html
            .select(&scraper::Selector::parse("script").map_err(|_| {
                format_err!("Could not parse `script` selector. In other news, 1 = 2.")
            })?)
            .find(|tag| tag.text().any(|text| text.contains("postDataJSON")))
            .ok_or_else(|| format_err!("Could not find postDataJSON in body."))?;
        let text = script.text().collect::<String>();

        // That script that will be of the format `window.postDataJSON = "..."`. We're
        // interested in just the "..." bit, so extract that.
        let start = text
            .find(&['\'', '"'][..])
            .ok_or_else(|| format_err!("Could not find starting quote"))?;
        let end = text
            .rfind(&['\'', '"'][..])
            .ok_or_else(|| format_err!("Could not find ending quote"))?;
        let code = &text[start..end + 1];

        // Parse the javascript string as a String and then parse its contents as a gallery
        let data: String =
            serde_json::from_str(code).wrap_err("Could not parse postDataJSON as a String")?;
        let gallery: ImgurGallery = serde_json::from_str(&data)
            .wrap_err("Could not parse inner postDataJSON as a gallery")?;

        // Make an iterator over the gallery's images
        let url_amount = gallery.media.len();
        let urls = gallery.media.into_iter().map(|media| media.url);

        // Fetch as many as we need
        let touched = self.fetch_multiple(stream::iter(urls)).await?;

        // If we've touched all the images in the gallery, we've exhausted it and can therefore consider it "invalid"
        if touched >= url_amount {
            debug!("exhausted gallery");
            if let Some(ref tx) = self.invalid_tx {
                let _ = tx.send(url.to_string());
            }
        }

        Ok(())
    }

    /// Download one image into its place
    #[tracing::instrument(skip(self))]
    #[async_recursion(?Send)]
    async fn fetch_one(&self, url: String) -> Result<()> {
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

            // Try to save it as a direct image. If it succeeds, we've saved one image.
            match self.try_as_direct_image(&url, body.clone()).await {
                Ok(()) => return Ok(()),
                Err(error) => {
                    trace!(%error, "failed direct image check");
                }
            }

            // Try to save it as an imgur gallery.
            match self.try_as_imgur_gallery(&url, body.clone()).await {
                Ok(..) => return Ok(()),
                Err(error) => {
                    trace!(%error, "failed imgur gallery check");
                }
            }

            // If we get here, we've no idea what this URL is.
            Err(format_err!("Unable to parse as anything known"))
        })()
        .await;

        if let Err(ref error) = result {
            warn!(%url, %error, "failed fetching");
            if let Some(ref tx) = self.invalid_tx {
                let _ = tx.send(url);
            }
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
        let mut futures = urls
            .inspect(|_| touched += 1)
            // Skip over URLs we've already examined
            .filter(|url| {
                future::ready(!self.downloaded.contains(url) && !self.invalid.contains(url))
            })
            // Start fetching the specfic URLs themselves
            .map(|url| self.fetch_one(url))
            // Instead of polling in order, take a block of 25 and poll them all at once
            .buffer_unordered(25);

        // Iterate over the futures as they complete and stop once we've gotten enough.
        while let Some(res) = futures.next().await {
            let gotten = self.gotten.load(Ordering::Acquire);
            trace!(gotten, success = res.is_ok(), "future completed");
            if gotten >= self.need {
                break;
            }
        }

        // Drop `futures` to ensure `touched` isn't mutably borrowed anymore, so we can return it.
        drop(futures);
        Ok(touched)
    }

    #[tracing::instrument(skip_all)]
    async fn fetch_toplevel<Urls>(mut self, urls: Urls) -> Result<()>
    where
        Urls: Stream<Item = String> + Unpin,
    {
        // Offload actual fetching to `fetch_multiple`.
        self.fetch_multiple(urls).await?;

        // Read the images directory and store new images in `downloaded`.
        debug!("storing new downloaded");
        let mut dir = tokio::fs::read_dir(DIRS.data_local_dir().join("images")).await?;
        while let Some(entry) = dir.next_entry().await? {
            if let Some(stem) = entry.path().file_stem().and_then(std::ffi::OsStr::to_str) {
                self.downloaded.insert_hash(stem.to_owned());
            }
        }

        // Close the "new invalid" channel and receive all that can be received, saving it to `invalid`.
        debug!("storing new invalids");
        self.invalid_tx.take();
        while let Some(url) = self.invalid_rx.recv().await {
            trace!(%url, "new invalid");
            self.invalid.insert(url);
        }

        self.downloaded.store().await?;
        self.invalid.store().await?;

        Ok(())
    }
}

#[tracing::instrument(skip_all)]
pub async fn fetch<Urls>(client: &Client, urls: Urls) -> Result<()>
where
    Urls: Stream<Item = String> + Unpin,
{
    let downloaded = PersistentSet::load("downloaded").await?;
    let invalid = PersistentSet::load("invalid").await?;
    let need = MAX_CACHED.saturating_sub(download_count().await?);
    let (invalid_tx, invalid_rx) = unbounded_channel();
    let fetcher = Fetcher {
        downloaded,
        invalid,
        need,
        gotten: AtomicUsize::new(0),
        client,
        invalid_tx: Some(invalid_tx),
        invalid_rx,
    };
    fetcher.fetch_toplevel(urls).await
}
