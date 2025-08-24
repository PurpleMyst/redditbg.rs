#![recursion_limit = "512"]
#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

use std::{
    convert::Infallible,
    fs,
    sync::mpsc::{sync_channel, Receiver, RecvTimeoutError},
    time::Duration,
};

use directories::ProjectDirs;
use eyre::{bail, Result, WrapErr};
use reqwest::Client;
use tokio::runtime::Runtime;
use tracing::{debug, error, info, trace, Level};

static DIRS: once_cell::sync::Lazy<ProjectDirs> = once_cell::sync::Lazy::new(|| {
    ProjectDirs::from("it", "PurpleMyst", env!("CARGO_PKG_NAME")).expect("could not create ProjectDirs")
});

mod utils;

mod reddit;

mod fetcher;

mod picker;

mod platform;

#[tracing::instrument(skip_all)]
fn find_new_background(runtime: &mut Runtime, client: &Client) -> Result<()> {
    let subreddits_txt =
        fs::read_to_string(DIRS.config_dir().join("subreddits.txt")).wrap_err("Could not read subreddits.txt")?;

    let subreddits = subreddits_txt.trim().lines().collect::<Vec<&str>>();
    info!(?subreddits, "using subreddits");

    // Make a closure that tells fetches our images
    let mut already_fetched = false;
    let do_fetch = || -> Result<()> {
        runtime.block_on(async {
            // Create a stream of URLs from Reddit
            let posts = reddit::Posts::new(client, &subreddits);

            // Fetch them
            fetcher::fetch(client, posts).await
        })
    };

    // Try to pick an image from the ones we've already fetched, so that we don't make
    // our user wait too long in the case that they don't have internet access at the
    // present moment.
    let picked = match picker::pick() {
        // If that succeeds, just return it
        Ok(img) => img,

        Err(err) => {
            if let Some(picker::NoValidImage) = err.downcast_ref() {
                // Otherwise, if we found no valid image, try to fetch them and pick again
                debug!("found no valid image on first try");
                do_fetch()?;
                already_fetched = true;
                picker::pick()?
            } else {
                // If we got any other error, bail and return it to the caller
                bail!(err);
            }
        }
    };

    // Save it to the filesystem so that we can set it
    let path = DIRS.cache_dir().join("background.png");
    trace!(path = %path.display(), "saving background");
    picked.save(&path)?;

    // Set it as a background
    trace!("setting background");
    platform::set_background(&path)?;

    // If we didn't fetch while picking the image, do so after setting the background
    if !already_fetched {
        do_fetch()?;
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

fn setup_tracing() {
    use tracing_subscriber::prelude::*;

    let file = std::sync::Mutex::new(file_rotator::RotatingFile::new(
        env!("CARGO_PKG_NAME"),
        DIRS.data_local_dir().join("logs"),
        file_rotator::RotationPeriod::Interval(std::time::Duration::from_secs(60 * 60 * 24)),
        std::num::NonZeroUsize::new(128).unwrap(),
        file_rotator::Compression::Zstd { level: 0 },
    ));

    let notifier = platform::Notifier {
        title: env!("CARGO_PKG_NAME").into(),
        icon: ICON_PATH.into(),
    }
    .with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
        metadata.is_event() && (*metadata.level() == Level::ERROR || metadata.target().ends_with("notification"))
    }));

    let filter = tracing_subscriber::filter::Targets::new()
        .with_default(tracing::Level::INFO)
        .with_target("redditbg", tracing::level_filters::STATIC_MAX_LEVEL);

    let fmt = tracing_subscriber::fmt::layer().event_format(tracing_subscriber::fmt::format().pretty());

    let bunyan = tracing_bunyan_formatter::JsonStorageLayer.and_then(
        tracing_bunyan_formatter::BunyanFormattingLayer::new(env!("CARGO_PKG_NAME").into(), file),
    );

    tracing_subscriber::registry()
        .with(fmt.and_then(bunyan).with_filter(filter))
        .with(notifier)
        .init();
}

fn setup_client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")))
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

fn setup_systray() -> Result<(utils::JoinOnDrop, Receiver<Message>)> {
    let mut app = systray::Application::new()?;

    let (tx, rx) = sync_channel(10);

    app.set_tooltip("Reddit Background Setter")?;

    {
        let tx = tx.clone();
        app.add_menu_item("Change now", move |_app| -> Result<(), Infallible> {
            info!(payload = "change now", "sending message");

            if let Err(error) = tx.send(Message::ChangeNow) {
                let error = eyre::Report::from(error);
                error!(?error, "could not send message");
            }

            Ok(())
        })?;
    }

    {
        let tx = tx.clone();
        app.add_menu_item("Copy background to clipboard", move |_app| -> Result<(), Infallible> {
            info!(payload = "copy image", "sending message");

            if let Err(error) = tx.send(Message::CopyImage) {
                let error = eyre::Report::from(error);
                error!(?error, "could not send message");
            }

            Ok(())
        })?;
    }

    app.add_menu_item("Quit", move |app| -> Result<(), Infallible> {
        info!(payload = "quit", "sending message");

        // at this point i'm praying this works
        if let Err(error) = app.shutdown() {
            let error = eyre::Report::from(error);
            error!(?error, "shutdown failed");
        }
        app.quit();

        if let Err(error) = tx.send(Message::Quit) {
            let error = eyre::Report::from(error);
            error!(?error, "could not send message");
        }

        Ok(())
    })?;

    // This should really support Path.. grumble grumble..
    app.set_icon_from_file(ICON_PATH)?;

    let handle = std::thread::Builder::new()
        .name("systray".to_owned())
        .spawn(move || app.wait_for_message().map_err(eyre::Error::from))?;

    Ok((utils::JoinOnDrop::new(handle), rx))
}

fn main() -> Result<()> {
    setup_dirs()?;
    setup_tracing();
    let (_guard, messages) = setup_systray()?;
    let client = setup_client()?;

    let mut runtime = Runtime::new()?;

    'mainloop: loop {
        match find_new_background(&mut runtime, &client) {
            Ok(()) => info!("set background successfully"),
            Err(error) => {
                error!(?error, "error while finding new background");
            }
        }

        loop {
            match messages.recv_timeout(Duration::from_secs(60 * 60)) {
                Ok(Message::Quit) => {
                    info!("got quit message");
                    break 'mainloop;
                }

                Ok(Message::ChangeNow) => {
                    info!("got change now message");
                    continue 'mainloop;
                }

                Ok(Message::CopyImage) => {
                    match image::ImageReader::open(&DIRS.cache_dir().join("background.png"))
                        .map_err(eyre::Error::from)
                        .and_then(|reader| platform::copy_image(&reader.with_guessed_format()?.decode()?))
                    {
                        Ok(()) => info!(target: "notification", "copied image"),

                        Err(error) => {
                            error!(?error, "copy image error");
                        }
                    }
                }

                Err(RecvTimeoutError::Disconnected) => {
                    error!("sys tray hung up");
                    break 'mainloop;
                }

                Err(RecvTimeoutError::Timeout) => {
                    continue 'mainloop;
                }
            }
        }
    }

    Ok(())
}
