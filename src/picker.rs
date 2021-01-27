use std::fs;

use eyre::Result;
use image::DynamicImage;
use slog::{debug, info, o, trace, warn, Logger};

use crate::utils::ReportValue;
use crate::DIRS;

#[derive(Debug)]
pub struct NoValidImage;

impl std::fmt::Display for NoValidImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad("no valid image")
    }
}

impl std::error::Error for NoValidImage {}

pub fn pick(logger: Logger) -> Result<DynamicImage> {
    // Get all downloaded images
    let (path, image) = fs::read_dir(DIRS.data_local_dir().join("images"))?
        .find_map(|entry| {
            // Validate if this entry is actually an image and if so return it and its loaded image
            let entry = entry.ok()?;
            let path = entry.path();

            let logger = logger.new(o!("path" => path.display().to_string()));

            let maybe_image = image::io::Reader::open(&path)
                .map_err(eyre::Error::from)
                .and_then(|reader| Ok(reader.with_guessed_format()?.decode()?));

            match maybe_image {
                Ok(image) => Some((path, image)),

                Err(error) => {
                    warn!(logger, "could not parse image"; "error" => ReportValue(error));
                    debug!(logger, "removing image");
                    if let Err(error) = std::fs::remove_file(&path) {
                        warn!(logger, "error while removing"; "error" => ReportValue(error.into()));
                    }
                    None
                }
            }
        })
        .ok_or(NoValidImage)?;
    info!(logger, "picked next background"; "path" => %path.display());

    // Remove the original file
    trace!(logger, "removing original file"; "path" => %path.display());
    fs::remove_file(path)?;

    Ok(image)
}
