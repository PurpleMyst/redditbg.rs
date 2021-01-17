use std::pin::Pin;
use std::task::Poll;

use eyre::{eyre, Result};
use futures::prelude::*;
use reqwest::Client;
use serde_json::Value;
use slog::{warn, Logger};

use crate::utils::ReportValue;
use crate::with_backoff;

pub struct Posts<'a> {
    logger: Logger,
    client: &'a Client,
    subreddits: &'a [&'a str],
    next_page_id: Option<String>,
    state: PostsState,
}

struct Page {
    next_page_id: Option<String>,
    posts: Vec<String>,
}

enum PostsState {
    NeedMore,
    Fetching(Pin<Box<dyn Future<Output = Result<Page>>>>),
    Fetched(Vec<String>),
    Exhausted,
}

impl<'a> Posts<'a> {
    pub fn new(logger: Logger, client: &'a Client, subreddits: &'a [&'a str]) -> Self {
        Self {
            logger,
            client,
            subreddits,
            next_page_id: None,
            state: PostsState::NeedMore,
        }
    }

    fn get_next_page(&mut self) -> impl Future<Output = Result<Page>> {
        // Spin up the request builder at the correct URL
        let url = format!(
            "https://reddit.com/r/{}/new.json",
            self.subreddits.join("+")
        );
        let mut req_builder = self.client.get(&url);

        // Make sure we're getting the freshest posts
        if let Some(after) = self.next_page_id.as_ref() {
            req_builder = req_builder.query(&[("after", after)]);
        }

        // *puts on sunglasses* Now it's time to enter the matrix
        async {
            // Here we make our retryable future that just sends out the
            // response and parses it as JSON. It's important that we parse the
            // response into JSON inside the retryable future because RequestBuilder::send()
            // does not actually consume the response
            let mut listing: Value = with_backoff!(move || {
                req_builder
                    .try_clone()
                    .unwrap()
                    .send()
                    .and_then(|resp| resp.json())
                    .map_err(eyre::Error::from)
            })?;

            let data = listing
                .get_mut("data")
                .ok_or_else(|| eyre!("Toplevel JSON did not have data"))?;

            let next_page_id = data
                .get("after")
                .and_then(|after| after.as_str())
                .map(ToOwned::to_owned);

            // Now let's navigate the tree that Reddit gives us to get what we want
            Ok(Page {
                next_page_id,
                posts: data
                    .get_mut("children")
                    .ok_or_else(|| eyre!("Toplevel data did not contain children"))?
                    .as_array()
                    .ok_or_else(|| eyre!("Toplevel children were not an array"))?
                    .iter()
                    .filter_map(|child| {
                        let data = child.get("data")?;

                        if !data.get("over_18")?.as_bool()? {
                            Some(data.get("url")?.as_str()?.to_owned())
                        } else {
                            // skip over NSFW wallpapers
                            None
                        }
                    })
                    .collect(),
            })
        }
    }
}

impl<'a> Stream for Posts<'a> {
    type Item = String;

    fn poll_next(
        mut self: Pin<&mut Self>,
        ctx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        // Simple state-machine loop
        loop {
            match self.state {
                // If we need more posts, let's spin up a future that gets them for us
                PostsState::NeedMore => {
                    self.state = PostsState::Fetching(self.get_next_page().boxed_local())
                }

                // If we're currently fetching posts, let's poll the future
                PostsState::Fetching(ref mut fut) => {
                    // We'll use the `ready!` macro which is kinda like `try!` for `Poll`
                    let posts = futures::ready!(fut.as_mut().poll(ctx));

                    match posts {
                        // If we've got posts, move on to the next state
                        Ok(Page {
                            next_page_id,
                            posts,
                        }) => {
                            self.next_page_id = next_page_id;
                            self.state = PostsState::Fetched(posts);
                        }

                        Err(error) => {
                            // We've already got backoff baked into `get_next_page`, we probably can't recover here
                            // It's best if we just stop giving out posts
                            warn!(self.logger, "error while fetching posts"; "error" => ReportValue(error));
                            self.state = PostsState::Exhausted;
                        }
                    }
                }

                // Now that we've got posts, just send each one out individually until we need more
                PostsState::Fetched(ref mut posts) => {
                    if let Some(post) = posts.pop() {
                        return Poll::Ready(Some(post));
                    } else if self.next_page_id.is_some() {
                        self.state = PostsState::NeedMore;
                    } else {
                        // If the previous page had no "after", it's probably best to mark ourselves as exhausted
                        // So that we can avoid entering a sort of "cycle"
                        warn!(self.logger, "missing next_page_id");
                        self.state = PostsState::Exhausted;
                    }
                }

                // If we've exhausted the posts (AKA hit an error), just return no more items
                PostsState::Exhausted => return Poll::Ready(None),
            }
        }
    }
}
