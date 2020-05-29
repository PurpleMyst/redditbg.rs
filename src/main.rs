#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

// XXX:
// Most of the here is marked as `async`.. it's really not.
// Things like `background::set` are definitely not async, so..
// maybe we could only asynchronously when fetching the images?

// FIXME: only download images which are "close enough" to our aspect ratio

use std::convert::Infallible;
use std::time::Duration;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use futures::prelude::*;
use log::{debug, error, info, warn};
use reqwest::Client;

use futures::channel::mpsc::{unbounded, UnboundedReceiver};
use tokio::time::delay_for;

lazy_static::lazy_static! {
    static ref DIRS: ProjectDirs = ProjectDirs::from(
        "it",
        "PurpleMyst",
        env!("CARGO_PKG_NAME")
    ).expect("could not get project dirs");

    // TODO: calculate this on the fly so that we can change subreddits.txt
    static ref URL: String = {
        let subreddits = include_str!("subreddits.txt")
            .trim()
            .lines()
            .collect::<Vec<&str>>()
            .join("+");
        format!("https://reddit.com/r/{}/new.json", subreddits)
    };
}

#[macro_use]
mod utils;

mod reddit;

mod fetcher;

mod picker;

mod background;

async fn find_new_background(client: &Client) -> Result<()> {
    // Get the list of images from reddit
    let urls = reddit::get_posts(client, &URL).await?;
    info!("Got {:?} urls", urls.len());

    // Fetch them and save them to the filesystem
    debug!("Starting to fetch images ...");
    let fetched = fetcher::fetch(client, urls).await?;
    info!("Fetched {} new images", fetched);

    // Choose one
    debug!("Picking one...");
    let picked = picker::pick().await?;

    // Save it to the filesystem so that we can set it
    let path = DIRS.cache_dir().join("background.png");
    debug!("Saving {:?} ...", path);
    picked.save(&path)?;

    // Set it as a background
    debug!("Setting background ...");
    background::set(&path)?;

    Ok(())
}

fn setup_dirs() -> Result<()> {
    use std::fs::create_dir_all;
    create_dir_all(DIRS.cache_dir())?;
    create_dir_all(DIRS.data_local_dir().join("images"))?;
    Ok(())
}

fn setup_logging() -> Result<()> {
    use simplelog::*;

    let path = DIRS.data_local_dir().join("redditbg.log");

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

        // XXX: for whatever reason i have to mouse over the icon to get it to disappear? weird
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
    setup_dirs()?;
    setup_logging()?;
    let mut messages = setup_systray()?;
    let client = setup_client()?;

    loop {
        info!("Fetching new posts...");

        match find_new_background(&client).await {
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

            _ = delay_for(Duration::from_secs(60 * 60)).fuse() => { /* next iter! */ },
        }
    }
}
