use std::{fs, path::PathBuf};

use eyre::Result;
use image::DynamicImage;
use tracing::{debug, info, trace, trace_span, warn};
use tracing_unwrap::*;

use crate::{utils::LogError, DIRS};

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

    let hasher = image_hasher::HasherConfig::new().to_hasher();
    let conn = rusqlite::Connection::open(DIRS.data_local_dir().join("db.sqlite3"))?;
    conn.execute_batch(include_str!("picker.sql"))?;

    let (path, image) = fs::read_dir(DIRS.data_local_dir().join("images"))?
        .find_map(|entry| -> Option<(PathBuf, DynamicImage)> {
            let _span = trace_span!("picking", ?entry).entered();

            // Validate if this entry is actually an image and if so return it and its loaded image
            let entry = entry.ok()?;
            let path = entry.path();

            let reader = image::io::Reader::open(&path).ok()?;
            let maybe_image = reader.with_guessed_format().ok()?.decode();

            match maybe_image {
                Ok(image) => {
                    let image_hash = hasher.hash_image(&image);
                    let already_applied = conn
                        .query_row(
                            "SELECT COUNT(*) FROM AppliedImages WHERE image_hash = ?",
                            [image_hash.as_bytes()],
                            |row| Ok(row.get::<_, usize>(0)? != 0),
                        )
                        .expect_or_log("should be able to count matching image hashes");
                    if already_applied {
                        debug!("skipping image that's already been applied");
                        fs::remove_file(path).expect_or_log("should be able to remove image");
                        return None;
                    }

                    conn.execute(
                        "INSERT INTO AppliedImages(image_hash) VALUES (?)",
                        [image_hash.as_bytes()],
                    )
                    .expect_or_log("should be able to insert new image hashes");

                    info!("picked next background!");
                    Some((path, image))
                }

                Err(error) => {
                    debug!(error = %LogError(&error), "could not parse image");
                    if let Err(error) = std::fs::remove_file(&path) {
                        let error = eyre::Report::from(error);
                        warn!(error = %LogError(&error), "error while removing");
                    }
                    None
                }
            }
        })
        .ok_or(NoValidImage)?;

    // Remove the original file
    trace!(path = %path.display(), "removing original file");
    fs::remove_file(path)?;

    Ok(image)
}
