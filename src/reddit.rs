use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use futures::prelude::*;
use futures_retry::FutureRetry;
use image::DynamicImage;
use log::{debug, info, warn};
use reqwest::Client;
use serde_json::Value;

use super::utils::BackoffPolicy;

const MAX_IMAGES: usize = 10;

async fn get_posts(client: &Client, url: &str) -> Result<Vec<String>> {
    async fn doit(client: &Client, url: &str) -> Result<Value> {
        Ok(client.get(url).send().await?.json().await?)
    }

    let backoff = exponential_backoff::Backoff::new(10)
        .timeout_range(Duration::from_secs(1), Duration::from_secs(15))
        .jitter(0.3)
        .factor(2);

    info!("Getting posts for {:?}", url);
    let mut listing =
        match FutureRetry::new(|| doit(client, url), BackoffPolicy(backoff.iter())).await {
            Ok((listing, _)) => listing,
            Err((err, _)) => Err(err)?,
        };

    Ok(listing
        .get_mut("data")
        .context("Toplevel was not a listing")?
        .get_mut("children")
        .context("Toplevel data did not contain children")?
        .as_array()
        .context("Toplevel children were not an array")?
        .into_iter()
        .filter_map(|child| Some(child.get("data")?.get("url")?.as_str()?.to_owned()))
        .collect())
}

async fn get_post_image(client: &Client, url: String) -> Result<DynamicImage> {
    async fn doit(client: &Client, url: &str) -> Result<Bytes> {
        Ok(client.get(url).send().await?.bytes().await?)
    }

    let backoff = exponential_backoff::Backoff::new(10)
        .timeout_range(Duration::from_secs(1), Duration::from_secs(15))
        .jitter(0.3)
        .factor(2);

    debug!("Getting image {:?}", url);
    let bytes = match FutureRetry::new(|| doit(client, &url), BackoffPolicy(backoff.iter())).await {
        Ok((listing, _)) => listing,
        Err((err, _)) => Err(err).with_context(|| format!("Failed to fetch image {:?}", url))?,
    };
    info!("Got image {:?} of size {}", url, bytes.len());

    let image = image::load_from_memory(&bytes)
        .with_context(|| format!("Failed to parse image {:?}", url))?;

    Ok(image)
}

pub async fn get_images(client: &Client, url: &str) -> Result<Vec<DynamicImage>> {
    Ok(get_posts(client, url)
        .await?
        .into_iter()
        .map(|url| get_post_image(client, url))
        .collect::<stream::FuturesUnordered<_>>()
        .filter_map(|image| {
            future::ready(match image {
                Ok(image) => Some(image),
                Err(err) => {
                    warn!("{:?}", err);
                    None
                }
            })
        })
        .take(MAX_IMAGES) // only count successful images towards the limit
        .collect()
        .await)
}
