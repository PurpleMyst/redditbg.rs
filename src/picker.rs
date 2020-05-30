use std::fs;

use anyhow::{Context, Result};
use image::DynamicImage;
use log::{debug, info, warn};

use crate::DIRS;

// FIXME: use tokio::fs
pub async fn pick() -> Result<DynamicImage> {
    // Get all downloaded images
    let (path, image) = fs::read_dir(DIRS.data_local_dir().join("images"))?
        .find_map(|entry| {
            // Validate if this entry is actually an image and if so return it and its loaded image
            let entry = entry.ok()?;
            let path = entry.path();

            let maybe_image = image::io::Reader::open(&path)
                .map_err(anyhow::Error::from)
                .and_then(|reader| Ok(reader.with_guessed_format()?.decode()?));

            match maybe_image {
                Ok(image) => Some((path, image)),

                Err(err) => {
                    warn!("Error while parsing as an image {:?}: {:?}", path, err);
                    debug!("Removing {:?}", path);
                    if let Err(err) = std::fs::remove_file(&path) {
                        warn!("Error while removing {:?}: {:?}", path, err);
                    }
                    None
                }
            }
        })
        .context("Could not find a valid image")?;
    // Remove the original file
    info!("Removing {:?}", path);
    fs::remove_file(path)?;

    Ok(image)
}
