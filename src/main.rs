#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use image::GenericImageView;
use log::{debug, error, info};
use noisy_float::prelude::*;
use reqwest::Client;

const SCREEN_ASPECT_RATIO: f64 = 1366. / 768.;

mod utils;
use utils::*;

mod reddit;
use reddit::*;

mod background;

async fn find_new_background(client: &Client) -> Result<()> {
    // Calculate url based on given subreddits
    // I think we could cache this but I'm not sure it matters
    let subreddits = include_str!("subreddits.txt")
        .trim()
        .lines()
        .collect::<Vec<&str>>()
        .join("+");
    let url = format!("https://reddit.com/r/{}/new.json", subreddits);

    // Get the images and find which one fits best on our screen
    let images = get_images(client, &url).await?;
    let image = images
        .into_iter()
        .min_by_key(|image| {
            (
                (aspect_ratio(image) - SCREEN_ASPECT_RATIO).abs(),
                std::cmp::Reverse(image.dimensions()),
            )
        })
        .context("Failed to find any images")?;

    // Save it to a path so that we can set it.
    // It's important to not put it in the home directory because people who do that are evil
    let mut path = dirs::cache_dir().context("could not get cache dir")?;
    path.push("redditbg_image.png");
    debug!("Saving image to {:?}", path);
    image.save(&path)?;

    // And then we'll delegate the actual image-setting to the background module
    info!("Setting background");
    background::set(&path)
}

fn setup_logging() -> Result<()> {
    use simplelog::*;

    let mut path = dirs::data_local_dir().unwrap();
    path.push("redditbg.log");

    let file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .context("Could not open log file")?;

    let config = ConfigBuilder::new()
        .add_filter_allow_str(module_path!())
        .set_time_format_str("%F %T")
        .set_thread_level(LevelFilter::Off)
        .build();

    let mut loggers: Vec<Box<dyn SharedLogger>> = Vec::with_capacity(2);
    loggers.push(WriteLogger::new(LevelFilter::Debug, config.clone(), file));
    if let Some(logger) = TermLogger::new(LevelFilter::Debug, config, TerminalMode::Mixed) {
        loggers.push(logger);
    }
    CombinedLogger::init(loggers)?;

    Ok(())
}

fn setup_client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .timeout(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .context("Failed to create client")
}

fn setup_systray() -> Result<()> {
    let mut app = systray::Application::new()?;

    app.set_tooltip("Reddit Background Setter")?;
    app.add_menu_item("Quit", |app| -> Result<(), std::convert::Infallible> {
        info!("Quit was clicked in System Tray");
        RUNNING.store(false, Ordering::Release);
        app.quit();
        Ok(())
    })?;

    let mut icon_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    icon_path.push("src");
    icon_path.push("icon.ico");
    // This should really support Path.. grumble grumble..
    app.set_icon_from_file(
        icon_path
            .to_str()
            .context("Icon path was not valid UTF-8")?,
    )?;

    std::thread::Builder::new()
        .name("systray".to_owned())
        .spawn(move || -> Result<()> {
            loop {
                app.wait_for_message()?;
            }
        })?;

    Ok(())
}

static RUNNING: AtomicBool = AtomicBool::new(true);

#[tokio::main]
async fn main() -> Result<()> {
    setup_logging()?;
    setup_systray()?;

    let client = setup_client()?;

    // Alright, so, if we get RUNNING = false while we're in the delay _technically_ the process is still open...
    // but no I/O should happen because after the delay the while condition will be checked
    // TODO: Maybe we could fix this by using a channel instead of an atomic and support even a few other messages.. Like "change now"
    while RUNNING.load(Ordering::Acquire) {
        info!("Fetching new posts...");

        match find_new_background(&client).await {
            Ok(()) => info!("Set background successfully"),
            Err(err) => error!("{:?}", err),
        }

        tokio::time::delay_for(Duration::from_secs(60 * 60)).await;
    }

    info!("Quitting");
    Ok(())
}
