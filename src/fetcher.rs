use anyhow::{Context, Result};
use futures::prelude::*;
use log::{debug, info, warn};
use reqwest::Client;
use sha2::{Digest, Sha256};
use tokio::{fs, prelude::*};

use crate::utils::AlreadySet;
use crate::DIRS;

const MAX_DOWNLOADED: usize = 25;

/// Append a generated filename for an url to the given path buffer
fn make_filename(path: &mut std::path::PathBuf, url: &str) {
    let mut hasher = Sha256::new();
    hasher.input(url);
    let hash = hasher.result();
    // As far as I can tell the image crate has no way to get the extension for a given format
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
async fn fetch_one(client: &Client, url: String) -> Result<()> {
    // Create the file for the image with a generated filename
    let mut path = DIRS.data_local_dir().join("images");
    make_filename(&mut path, &url);
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .await
        .with_context(|| format!("Failed to open file {:?} for {:?}", path, url))?;

    // Fetch the image's body
    let body: bytes::Bytes = with_backoff!(client
        .get(&url)
        .send()
        .and_then(|response| response.bytes()))
    .with_context(|| format!("Failed to fetch {:?}", url))?;
    info!("Fetched the body of {:?}", url);

    // Verify that it looks like an image
    match ::image::guess_format(&body) {
        Ok(fmt) => info!("{:?} is an image of format {:?}", url, fmt),
        Err(..) => debug!("{:?} was not an image", url),
    }

    // Write it to the filesystem
    info!("Saving {:?} to {:?}", url, path);
    file.write_all(&body).await?;
    file.sync_all().await?;

    Ok(())
}

/// Fetch the images in the given urls
///
/// This skips over images which have been already set as a background and it also
/// tries to only download `MAX_DOWNLOADED` images.
pub async fn fetch<Urls>(client: &Client, urls: Urls) -> Result<usize>
where
    Urls: IntoIterator<Item = String>,
{
    let already_set = AlreadySet::load().await?;
    let downloaded_count = download_count().await?;

    Ok(urls
        .into_iter()
        // Skip over images we've already set
        .filter(|url| !already_set.contains(url))
        // Start downloading them, not necessarily in order
        .map(|url| fetch_one(client, url))
        .collect::<stream::FuturesOrdered<_>>()
        // As they come in, filter out the ones that failed
        .filter_map(|result| {
            future::ready(match result {
                Ok(()) => Some(()),
                Err(err) => {
                    warn!("{:?}", err);
                    None
                }
            })
        })
        // Consider only the ones that succeeded for the max download calculation
        .take(MAX_DOWNLOADED.saturating_sub(downloaded_count))
        // Count them out and return it
        .fold(0, |acc, ()| future::ready(acc + 1))
        .await)
}
