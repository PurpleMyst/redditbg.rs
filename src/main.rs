#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

use std::collections::HashSet;
use std::convert::Infallible;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::prelude::*;
use image::GenericImageView;
use log::{debug, error, info, warn};
use noisy_float::prelude::*;
use reqwest::Client;

use futures::channel::mpsc::{unbounded, UnboundedReceiver};
use tokio::time::delay_for;

const SCREEN_ASPECT_RATIO: f64 = 1366. / 768.;

mod utils;
use utils::*;

mod reddit;
use reddit::*;

mod background;

async fn find_new_background(client: &Client, already_set: &mut HashSet<String>) -> Result<()> {
    // Calculate url based on given subreddits
    // I think we could cache this but I'm not sure it matters
    let subreddits = include_str!("subreddits.txt")
        .trim()
        .lines()
        .collect::<Vec<&str>>()
        .join("+");
    let url = format!("https://reddit.com/r/{}/new.json", subreddits);

    // Get the images and find which one fits best on our screen
    let images = get_images(client, &url, &already_set).await?;
    let (url, image) = images
        .into_iter()
        .min_by_key(|(_, image)| {
            (
                (aspect_ratio(image) - SCREEN_ASPECT_RATIO).abs(),
                std::cmp::Reverse(image.dimensions()),
            )
        })
        .context("Failed to find any images")?;
    already_set.insert(url);

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

    let mut path = dirs::data_local_dir().context("Could not get local data directory")?;
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

enum Message {
    ChangeNow,
    Quit,
}

// TODO: fork systray so we can make it actually work with async
fn setup_systray() -> Result<UnboundedReceiver<Message>> {
    let mut app = systray::Application::new()?;

    let (tx, rx) = unbounded();

    app.set_tooltip("Reddit Background Setter")?;

    {
        let tx = tx.clone();
        app.add_menu_item("Change now", move |_app| -> Result<(), Infallible> {
            info!("Sending Change Now message");

            if let Err(err) = tx.unbounded_send(Message::ChangeNow) {
                warn!("Error while sending change now message: {:?}", err);
            }

            Ok(())
        })?;
    }

    app.add_menu_item("Quit", move |app| -> Result<(), Infallible> {
        info!("Quit was clicked in System Tray");

        // XXX: for whatever erason i have to mouse over the icon to get it to disappear? weird
        app.quit();
        if let Err(err) = tx.unbounded_send(Message::Quit) {
            warn!("Error while sending quit message: {:?}", err);
        }

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

    // XXX: this thread remains alive even after we quit the app, probably leading to the ghost icon problem.
    //      what can we do to fix it?
    std::thread::Builder::new()
        .name("systray".to_owned())
        .spawn(move || -> Result<()> {
            loop {
                app.wait_for_message()?;
            }
        })?;

    Ok(rx)
}

#[tokio::main]
async fn main() -> Result<()> {
    setup_logging()?;
    let mut messages = setup_systray()?;
    let client = setup_client()?;

    let mut already_set = HashSet::new();

    loop {
        info!("Fetching new posts...");

        match find_new_background(&client, &mut already_set).await {
            Ok(()) => info!("Set background successfully"),
            Err(err) => error!("{:?}", err),
        }

        futures::select! {
            // If we get a message while waiting, let's act on it
            msg = messages.next() => match msg {
                Some(Message::Quit) => {
                    info!("Got Quit message, see ya!");
                    return Ok(());
                },

                Some(Message::ChangeNow) => {}

                None => {
                    warn!("sys tray hung up! exiting");
                    return Ok(());
                },
            },

            _ = delay_for(Duration::from_secs(60 * 60)).fuse() => {
                // If we get here, we didn't get woken up by a message, so it's assumed roundabout one hour passed
                // So let's reset the "already set" cache to avoid what is practically a memory leak
                already_set.clear();

                // Then let's just fall into the next iteration of the loop
            },
        }
    }
}
