use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;

use crate::with_backoff;

// TODO: keep scrolling through the pages
pub async fn get_posts(client: &Client, url: &str) -> Result<Vec<String>> {
    async fn doit(client: &Client, url: &str) -> Result<Value> {
        Ok(client.get(url).send().await?.json().await?)
    }

    let mut listing = with_backoff!(doit(client, url))?;

    Ok(listing
        .get_mut("data")
        .context("Toplevel was not a listing")?
        .get_mut("children")
        .context("Toplevel data did not contain children")?
        .as_array()
        .context("Toplevel children were not an array")?
        .iter()
        .filter_map(|child| Some(child.get("data")?.get("url")?.as_str()?.to_owned()))
        .collect())
}
