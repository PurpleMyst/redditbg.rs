use std::fs;

use eyre::Result;
use image::DynamicImage;
use tracing::{debug, info, trace, warn};

use crate::DIRS;

#[derive(Debug)]
pub struct NoValidImage;

impl std::fmt::Display for NoValidImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad("no valid image")
    }
}

impl std::error::Error for NoValidImage {}

#[tracing::instrument]
pub fn pick() -> Result<DynamicImage> {
    // Get all downloaded images
    let (path, image) = fs::read_dir(DIRS.data_local_dir().join("images"))?
        .find_map(|entry| {
            // Validate if this entry is actually an image and if so return it and its loaded image
            let entry = entry.ok()?;
            let path = entry.path();

            let maybe_image = image::io::Reader::open(&path)
                .map_err(eyre::Error::from)
                .and_then(|reader| Ok(reader.with_guessed_format()?.decode()?));

            match maybe_image {
                Ok(image) => Some((path, image)),

                Err(error) => {
                    debug!(%error, "could not parse image");
                    trace!("removing image");
                    if let Err(error) = std::fs::remove_file(&path) {
                        let error = eyre::Report::from(error);
                        warn!(%error, "error while removing");
                    }
                    None
                }
            }
        })
        .ok_or(NoValidImage)?;
    info!(path = %path.display(), "picked next background");

    // Remove the original file
    trace!(path = %path.display(), "removing original file");
    fs::remove_file(path)?;

    Ok(image)
}
