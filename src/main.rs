#![recursion_limit = "512"]
#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

use std::{convert::Infallible, time::Duration};

use directories::ProjectDirs;
use eyre::{bail, Result, WrapErr};
use futures::channel::mpsc::{unbounded, UnboundedReceiver};
use futures::prelude::*;
use reqwest::Client;
use slog::{debug, error, info, o, Logger};
use tokio::{fs, time::delay_for};

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

mod platform;

async fn find_new_background(logger: &Logger, client: &Client) -> Result<()> {
    let subreddits_txt = fs::read_to_string(DIRS.config_dir().join("subreddits.txt"))
        .await
        .wrap_err("Could not read subreddits.txt")?;

    let subreddits = subreddits_txt.trim().lines().collect::<Vec<&str>>();

    // Make a closure that tells fetches our images
    let mut already_fetched = false;
    let do_fetch = || async {
        // Get the list of images from reddit
        let posts = reddit::Posts::new(
            logger.new(o!("state" => "getting posts")),
            client,
            &subreddits,
        );
        info!(logger, "got posts");

        // Fetch them
        let fetched = fetcher::fetch(logger.new(o!("state" => "fetching")), client, posts).await?;
        info!(logger, "fetched"; "count" => fetched);

        Result::<(), eyre::Report>::Ok(())
    };

    let picked = {
        let logger = logger.new(o!("state" => "picking"));

        // Try to pick an image from the ones we've already fetched, so that we
        // don't make our user wait too long in the case that they don't have
        // internet access at the present moment
        match picker::pick(logger.clone()).await {
            // If that succeeds, just return it
            Ok(img) => img,

            Err(err) => {
                if let Some(picker::NoValidImage) = err.downcast_ref() {
                    debug!(logger, "found no valid image on first try");
                    // Otherwise, if we found no valid image, try to fetch them and pick again
                    do_fetch().await?;
                    already_fetched = true;
                    picker::pick(logger).await?
                } else {
                    // If we got any other error, bail and return it to the caller
                    bail!(err);
                }
            }
        }
    };

    debug!(logger, "resizing background");
    let (w, h) = utils::screen_size()?;
    let picked = picked.resize(w, h, image::imageops::FilterType::Lanczos3);

    // Save it to the filesystem so that we can set it
    let path = DIRS.cache_dir().join("background.png");
    debug!(logger, "saving background"; "path" => %path.display());
    picked.save(&path)?;

    // Set it as a background
    debug!(logger, "setting background");
    platform::set_background(&path)?;

    // If we didn't fetch while picking the image, do so after setting the background
    if !already_fetched {
        do_fetch().await?;
    }

    Ok(())
}

fn setup_dirs() -> Result<()> {
    use std::fs::create_dir_all;
    create_dir_all(DIRS.cache_dir())?;
    create_dir_all(DIRS.data_local_dir().join("images"))?;
    create_dir_all(DIRS.data_local_dir().join("logs"))?;
    create_dir_all(DIRS.config_dir())?;
    Ok(())
}

fn setup_logging() -> slog::Logger {
    use slog::Drain;

    let file = file_rotator::RotatingFile::new(
        env!("CARGO_PKG_NAME"),
        DIRS.data_local_dir().join("logs"),
        file_rotator::RotationPeriod::Interval(std::time::Duration::from_secs(60 * 60 * 24)),
        7,
    );

    let drain1 = slog_bunyan::with_name(env!("CARGO_PKG_NAME"), file)
        .build()
        .fuse();

    let drain2 = {
        let decorator = slog_term::TermDecorator::new().build();
        slog_term::CompactFormat::new(decorator).build().fuse()
    };

    let drain3 = platform::NotifyDrain {
        title: env!("CARGO_PKG_NAME").into(),
        icon: ICON_PATH.into(),
    }
    .filter(|record| {
        record.level().is_at_least(slog::Level::Error) || record.tag() == "notification"
    })
    .ignore_res();

    let drain = slog::Duplicate::new(drain1, drain2).fuse();
    let drain = slog::Duplicate::new(drain, drain3).fuse();

    let drain = slog_async::Async::new(drain).build().fuse();

    slog::Logger::root(drain, slog::o!())
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
        .wrap_err("Failed to create client")
}

enum Message {
    ChangeNow,
    CopyImage,
    Quit,
}

const ICON_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/icon.ico");

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
                error!(logger, "could not send message"; "error" => %err);
            }

            Ok(())
        })?;
    }

    {
        let tx = tx.clone();
        let logger = logger.clone();
        app.add_menu_item(
            "Copy background to clipboard",
            move |_app| -> Result<(), Infallible> {
                info!(logger, "sending message"; "message" => "copy image");

                if let Err(err) = tx.unbounded_send(Message::CopyImage) {
                    error!(logger, "could not send message"; "error" => %err);
                }

                Ok(())
            },
        )?;
    }

    {
        let logger = logger.clone();
        app.add_menu_item("Quit", move |app| -> Result<(), Infallible> {
            info!(logger, "sending message"; "message" => "quit");

            // at this point i'm praying this works
            if let Err(err) = app.shutdown() {
                error!(logger, "shutdown failed"; "error" => %err);
            }
            app.quit();

            if let Err(err) = tx.unbounded_send(Message::Quit) {
                error!(logger, "could not send message"; "error" => %err);
            }

            Ok(())
        })?;
    }

    // This should really support Path.. grumble grumble..
    app.set_icon_from_file(ICON_PATH)?;

    let handle = std::thread::Builder::new()
        .name("systray".to_owned())
        .spawn(move || app.wait_for_message().map_err(eyre::Error::from))?;

    Ok((utils::JoinOnDrop::new(logger.clone(), handle), rx))
}

#[tokio::main]
async fn main() -> Result<()> {
    setup_dirs()?;
    let logger = setup_logging();
    let (_guard, mut messages) = setup_systray(logger.new(o!("state" => "systray")))?;
    let client = setup_client()?;

    'mainloop: loop {
        info!(logger, "finding new background");

        match find_new_background(&logger, &client).await {
            Ok(()) => info!(logger, "set background successfully"),
            Err(err) => error!(logger, "error while finding new background"; "error" => %err),
        }

        loop {
            futures::select! {
                // If we get a message while waiting, let's act on it
                msg = messages.next() => match msg {
                    Some(Message::Quit) => {
                        info!(logger, "got quit message");
                        break 'mainloop;
                    },

                    Some(Message::ChangeNow) => {
                        info!(logger, "got change now message");
                        continue 'mainloop;
                    }

                    Some(Message::CopyImage) => {
                        match image::io::Reader::open(&DIRS.cache_dir().join("background.png"))
                            .map_err(eyre::Error::from)
                            .and_then(|reader| Ok(reader.with_guessed_format()?.decode()?))
                            .and_then(|img| platform::copy_image(img.into_rgba()))
                        {
                            Ok(()) => info!(logger, #"notification", "copied image"),

                            // TODO: Show tray notification
                            Err(error) => {
                                error!(logger, "copy image error"; "error" => %error);
                            }
                        }
                    }

                    None => {
                        error!(logger, "sys tray hung up");
                        break 'mainloop;
                    },
                },

                _ = delay_for(Duration::from_secs(60 * 60)).fuse() => { continue 'mainloop; },
            }
        }
    }

    Ok(())
}
