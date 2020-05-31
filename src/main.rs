#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

use std::convert::Infallible;
use std::time::Duration;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use futures::prelude::*;
use reqwest::Client;
use slog::{debug, error, info, o, Logger};

use futures::channel::mpsc::{unbounded, UnboundedReceiver};
use tokio::fs;
use tokio::time::delay_for;

lazy_static::lazy_static! {
    static ref DIRS: ProjectDirs = ProjectDirs::from(
        "it",
        "PurpleMyst",
        env!("CARGO_PKG_NAME")
    ).expect("could not get project dirs");
}

#[macro_use]
mod utils;

mod reddit;

mod fetcher;

mod picker;

mod background;

async fn find_new_background(logger: &Logger, client: &Client) -> Result<()> {
    let subreddits_txt = fs::read_to_string(DIRS.config_dir().join("subreddits.txt"))
        .await
        .context("Could not read subreddits.txt")?;

    let subreddits = subreddits_txt.trim().lines().collect::<Vec<&str>>();

    // Get the list of images from reddit
    let posts = reddit::Posts::new(
        logger.new(o!("state" => "getting posts")),
        client,
        &subreddits,
    );
    info!(logger, "got posts");

    // Fetch them and save them to the filesystem
    let fetched = fetcher::fetch(logger.new(o!("state" => "fetching")), client, posts).await?;
    info!(logger, "fetched"; "count" => fetched);

    // Choose one
    let picked = picker::pick(logger.new(o!("state" => "picking"))).await?;

    debug!(logger, "resizing background");
    let (w, h) = utils::screen_size()?;
    let picked = picked.resize(w, h, image::imageops::FilterType::Lanczos3);

    // Save it to the filesystem so that we can set it
    let path = DIRS.cache_dir().join("background.png");
    debug!(logger, "saving background"; "path" => ?path);
    picked.save(&path)?;

    // Set it as a background
    debug!(logger, "setting background");
    background::set(&path)?;

    Ok(())
}

fn setup_dirs() -> Result<()> {
    use std::fs::create_dir_all;
    create_dir_all(DIRS.cache_dir())?;
    create_dir_all(DIRS.data_local_dir().join("images"))?;
    create_dir_all(DIRS.config_dir())?;
    Ok(())
}

fn setup_logging() -> Result<slog::Logger> {
    use slog::Drain;

    let path = DIRS.data_local_dir().join("redditbg.log.jsonl");

    let file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .context("Could not open log file")?;

    let drain1 = slog_bunyan::with_name(env!("CARGO_PKG_NAME"), file)
        .build()
        .fuse();

    let drain2 = {
        let decorator = slog_term::TermDecorator::new().build();
        slog_term::CompactFormat::new(decorator).build().fuse()
    };

    let drain = slog_async::Async::new(slog::Duplicate::new(drain1, drain2).fuse())
        .build()
        .fuse();

    Ok(slog::Logger::root(drain, slog::o!()))
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
fn setup_systray(logger: Logger) -> Result<(utils::JoinOnDrop, UnboundedReceiver<Message>)> {
    let mut app = systray::Application::new()?;

    let (tx, rx) = unbounded();

    app.set_tooltip("Reddit Background Setter")?;

    {
        let tx = tx.clone();
        let logger = logger.clone();
        app.add_menu_item("Change now", move |_app| -> Result<(), Infallible> {
            info!(logger, "sending message"; "message" => "change now");

            if let Err(err) = tx.unbounded_send(Message::ChangeNow) {
                error!(logger, "could not send message"; "error" => ?err);
            }

            Ok(())
        })?;
    }

    {
        let logger = logger.clone();
        app.add_menu_item("Quit", move |app| -> Result<(), Infallible> {
            info!(logger, "sending message"; "message" => "quit");

            // So I've kinda read through the source code of `systray` and
            // it seems to me that this is enough to get it to exit out of `wait_for_message`,
            // causing the thread calling that to exit and app to get dropped and therefore
            // `shutdown` is called. Hope that works.
            app.quit();

            if let Err(err) = tx.unbounded_send(Message::Quit) {
                error!(logger, "could not send message"; "error" => ?err);
            }

            Ok(())
        })?;
    }

    let mut icon_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    icon_path.push("src");
    icon_path.push("icon.ico");
    // This should really support Path.. grumble grumble..
    app.set_icon_from_file(
        icon_path
            .to_str()
            .context("Icon path was not valid UTF-8")?,
    )?;

    let handle = std::thread::Builder::new()
        .name("systray".to_owned())
        .spawn(move || app.wait_for_message().map_err(anyhow::Error::from))?;

    Ok((utils::JoinOnDrop::new(logger.clone(), handle), rx))
}

#[tokio::main]
async fn main() -> Result<()> {
    setup_dirs()?;
    let logger = setup_logging()?;
    let (_guard, mut messages) = setup_systray(logger.new(o!("state" => "systray")))?;
    let client = setup_client()?;

    loop {
        info!(logger, "finding new background");

        match find_new_background(&logger, &client).await {
            Ok(()) => info!(logger, "set background successfully"),
            Err(err) => error!(logger, "error while finding new background"; "error" => ?err),
        }

        futures::select! {
            // If we get a message while waiting, let's act on it
            msg = messages.next() => match msg {
                Some(Message::Quit) => {
                    info!(logger, "got quit message");
                    break;
                },

                Some(Message::ChangeNow) => info!(logger, "got change now message"),

                None => {
                    error!(logger, "sys tray hung up");
                    break;
                },
            },

            _ = delay_for(Duration::from_secs(60 * 60)).fuse() => { /* next iter! */ },
        }
    }

    Ok(())
}
