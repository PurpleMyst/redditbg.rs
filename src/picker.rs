use std::fs;

use anyhow::{Context, Result};
use image::DynamicImage;
use log::{info, warn};

use crate::utils::AlreadySet;
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

                // FIXME: remove erroneous images
                Err(err) => {
                    warn!("Failed to open {:?}: {}", path, err);
                    None
                }
            }
        })
        .context("Could not find a valid image")?;

    // Put it in the "already set" pile
    info!("Adding {:?} to the already set file", path);
    let mut already_set = AlreadySet::load().await?;
    already_set.insert_hash(
        path.file_stem()
            .context("Could not get path file stem")?
            .to_str()
            .context("Path was not valid UTF-8")?
            .to_string(),
    );
    already_set.store().await?;

    // Remove the original file
    info!("Removing {:?}", path);
    fs::remove_file(path)?;

    Ok(image)
}
