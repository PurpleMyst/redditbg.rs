use async_recursion::async_recursion;
use bytes::Bytes;
use eyre::{ensure, format_err, Result, WrapErr};
use futures::stream;
use tracing::{debug, trace};

use super::Fetcher;

#[derive(serde::Deserialize)]
struct ImgurGallery {
    media: Vec<ImgurMedia>,
}

#[derive(serde::Deserialize, Debug)]
struct ImgurMedia {
    url: String,
}

impl<'client> Fetcher<'client> {
    #[tracing::instrument(skip(self, body))]
    #[async_recursion(?Send)]
    pub(super) async fn parse_imgur_gallery(&self, url: &str, body: Bytes) -> Result<()> {
        // Parse HTML and ensure there were no errors
        let html = scraper::Html::parse_document(std::str::from_utf8(&body).wrap_err("Body was not valid UTF-8.")?);
        ensure!(html.errors.is_empty(), "html.errors was not empty");

        // Extract a script tag containing the text "postDataJSON"
        let script = html
            .select(
                &scraper::Selector::parse("script")
                    .map_err(|_| format_err!("Could not parse `script` selector. In other news, 1 = 2."))?,
            )
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
        let data: String = serde_json::from_str(code).wrap_err("Could not parse postDataJSON as a String")?;
        let gallery: ImgurGallery =
            serde_json::from_str(&data).wrap_err("Could not parse inner postDataJSON as a gallery")?;
        trace!(?gallery.media, "parsed imgur gallery");

        // Make an iterator over the gallery's images
        let url_amount = gallery.media.len();
        let urls = gallery.media.into_iter().map(|media| media.url);

        // Fetch as many as we need
        let touched = self.fetch_multiple(stream::iter(urls)).await?;

        // If we've touched all the images in the gallery, we've exhausted it and can therefore consider it "invalid"
        if touched >= url_amount {
            debug!(%url, "exhausted imgur gallery");
            if let Some(ref tx) = self.invalid_tx {
                let _ = tx.send(url.to_string());
            }
        }

        Ok(())
    }
}
