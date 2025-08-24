use std::fs;

use eyre::{bail, Result, WrapErr};
use image::DynamicImage;
use tracing::{debug, info, trace_span, warn};

use crate::DIRS;

#[derive(thiserror::Error, Debug)]
#[error("No valid image")]
pub struct NoValidImage;

#[tracing::instrument]
pub fn pick() -> Result<DynamicImage> {
    // Create our hasher and our database connection
    let hasher = image_hasher::HasherConfig::new().to_hasher();
    let db = rusqlite::Connection::open(DIRS.data_local_dir().join("db.sqlite3"))?;
    db.execute_batch(include_str!("picker.sql"))?;

    // For every file in the images/ directory...
    for entry in DIRS.data_local_dir().join("images").read_dir()? {
        // Create a span and pick out the path (which is what we actually care about).
        let _span = trace_span!("picking", ?entry).entered();
        let path = entry?.path();

        // Try to read this path as an image
        let maybe_image = (image::ImageReader::open(&path).wrap_err("failed to open path"))
            .and_then(|r| r.with_guessed_format().wrap_err("failed to guess format"))
            .and_then(|r| r.decode().wrap_err("failed to decode"));

        match maybe_image {
            Ok(image) => {
                // If this actually is an image, make sure we haven't already applied anything with the same image hash.
                let image_hash = hasher.hash_image(&image);
                let already_applied = db.query_row(
                    "SELECT COUNT(*) FROM AppliedImages WHERE image_hash = ?",
                    [image_hash.as_bytes()],
                    |row| Ok(row.get::<_, usize>(0)? != 0),
                )?;
                if already_applied {
                    debug!("skipping image that's already been applied");
                    fs::remove_file(path)?;
                    continue;
                }

                // If we haven't, add the image hash to the database, remove the original file and return our image.
                db.execute(
                    "INSERT INTO AppliedImages(image_hash) VALUES (?)",
                    [image_hash.as_bytes()],
                )?;
                info!(?image_hash, "picked next background!");
                fs::remove_file(path)?;

                return Ok(image);
            }

            Err(error) => {
                // If we failed to read this as an image, send it to the shadow realm.
                debug!(?error, "could not parse image");
                fs::remove_file(&path)?;
            }
        }
    }

    bail!(NoValidImage);
}
