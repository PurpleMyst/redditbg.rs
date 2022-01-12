use std::collections::HashMap;

use async_recursion::async_recursion;
use bytes::Bytes;
use eyre::{ensure, format_err, Result, WrapErr};
use futures::stream;
use serde::Deserialize;
use tracing::{debug, trace};

use super::Fetcher;

#[derive(Deserialize)]
struct RedditGallery {
    posts: Posts,
}

#[derive(Deserialize)]
struct Posts {
    models: HashMap<String, Model>,
}

#[derive(Deserialize)]
struct Model {
    media: Media,
}

#[derive(Deserialize)]
struct Media {
    #[serde(rename(deserialize = "mediaMetadata"))]
    media_metadata: HashMap<String, MediaMetadata>,
}

#[derive(Deserialize)]
struct MediaMetadata {
    s: S,
}

#[derive(Deserialize)]
struct S {
    u: String,
}

impl<'client> Fetcher<'client> {
    #[tracing::instrument(skip(self, body))]
    #[async_recursion(?Send)]
    pub(super) async fn parse_reddit_gallery(&self, url: &str, body: Bytes) -> Result<()> {
        // Parse HTML and ensure there were no errors
        let html = scraper::Html::parse_document(
            std::str::from_utf8(&body).wrap_err("Body was not valid UTF-8.")?,
        );
        ensure!(html.errors.is_empty(), "html.errors was not empty");

        // Extract a script tag whose code starts with "window.___r"
        // Before, this script tag had the id "data" but it seems they've removed that. In any
        // case, this code is more robust.
        let text = html
            .select(&scraper::Selector::parse("script").unwrap())
            .find_map(|script| {
                let text = script.text().collect::<String>();
                text.trim().starts_with("window.___r").then(|| text)
            })
            .ok_or_else(|| format_err!("Could not find a valid script tag."))?;

        // That script that will be of the format `window.___r = {...}`. We're
        // interested in just the "..." bit, so extract that.
        let start = text
            .find('{')
            .ok_or_else(|| format_err!("Could not find starting quote"))?;
        let end = text
            .rfind('}')
            .ok_or_else(|| format_err!("Could not find ending quote"))?;
        let code = &text[start..end + 1];

        // Dig out the URLs from the extremely nested structure.
        let gallery: RedditGallery = serde_json::from_str(code)?;
        let gallery = gallery
            .posts
            .models
            .into_iter()
            .flat_map(|(_, model)| {
                model
                    .media
                    .media_metadata
                    .into_iter()
                    .map(|(_, metadata)| metadata.s.u)
            })
            .collect::<Vec<String>>();
        trace!(?gallery, "parsed reddit gallery");

        // Count how many we've got.
        let contained = gallery.len();

        // Fetch as many as we need.
        let touched = self.fetch_multiple(stream::iter(gallery)).await?;

        // If we've touched all the images in the gallery, we've exhausted it and can
        // therefore consider it "invalid".
        if touched >= contained {
            debug!(%url, "exhausted reddit gallery");
            if let Some(ref tx) = self.invalid_tx {
                let _ = tx.send(url.to_string());
            }
        }

        Ok(())
    }
}
