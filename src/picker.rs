use std::fs;

use eyre::{eyre, Result};
use image::DynamicImage;
use slog::{debug, info, o, warn, Logger};

use crate::DIRS;

// FIXME: use tokio::fs
pub async fn pick(logger: Logger) -> Result<DynamicImage> {
    // Get all downloaded images
    let (path, image) = fs::read_dir(DIRS.data_local_dir().join("images"))?
        .find_map(|entry| {
            // Validate if this entry is actually an image and if so return it and its loaded image
            let entry = entry.ok()?;
            let path = entry.path();

            let logger = logger.new(o!("path" => path.to_string_lossy().into_owned()));

            let maybe_image = image::io::Reader::open(&path)
                .map_err(eyre::Error::from)
                .and_then(|reader| Ok(reader.with_guessed_format()?.decode()?));

            match maybe_image {
                Ok(image) => Some((path, image)),

                Err(err) => {
                    warn!(logger, "could not parse image"; "error" => %err);
                    debug!(logger, "removing image");
                    if let Err(err) = std::fs::remove_file(&path) {
                        warn!(logger, "error while removing"; "error" => %err);
                    }
                    None
                }
            }
        })
        .ok_or_else(|| eyre!("Could not find a valid image"))?;
    // Remove the original file
    info!(logger, "removing original file"; "path" => ?path);
    fs::remove_file(path)?;

    Ok(image)
}
