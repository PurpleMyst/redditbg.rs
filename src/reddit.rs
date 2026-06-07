use std::{io::Cursor, pin::Pin, task::Poll};

use atom_syndication::Feed;
use eyre::{Result, WrapErr};
use futures::prelude::*;
use reqwest::Client;
use tracing::{trace, warn};

use crate::utils::with_backoff;

pub struct Posts<'a> {
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

fn extract_post_url(content: &str) -> Option<String> {
    let html = scraper::Html::parse_fragment(content);
    let selector = scraper::Selector::parse("a").unwrap();

    html.select(&selector).find_map(|link| {
        if link.text().collect::<String>().trim() == "[link]" {
            link.value().attr("href").map(ToOwned::to_owned)
        } else {
            None
        }
    })
}

fn parse_feed(body: &[u8]) -> Result<Page> {
    let feed = Feed::read_from(Cursor::new(body)).wrap_err("failed to parse Reddit Atom feed")?;
    let next_page_id = feed.entries().last().map(|entry| entry.id().to_owned());
    let posts = feed
        .entries()
        .iter()
        .filter_map(|entry| {
            entry
                .content()
                .and_then(|content| content.value())
                .and_then(extract_post_url)
        })
        .collect();

    Ok(Page { next_page_id, posts })
}

impl<'a> Posts<'a> {
    pub fn new(client: &'a Client, subreddits: &'a [&'a str]) -> Self {
        Self {
            client,
            subreddits,
            next_page_id: None,
            state: PostsState::NeedMore,
        }
    }

    #[tracing::instrument(skip(self))]
    fn get_next_page(&mut self) -> impl Future<Output = Result<Page>> {
        // Spin up the request builder at the correct URL
        let url = format!("https://www.reddit.com/r/{}/new.rss", self.subreddits.join("+"));
        let mut req_builder = self.client.get(&url);

        // Make sure we're getting the freshest posts
        if let Some(after) = self.next_page_id.as_ref() {
            req_builder = req_builder.query(&[("after", after)]);
        }
        trace!(
            url = url.as_str(),
            next_page_id = ?self.next_page_id,
            "posts request"
        );

        // *puts on sunglasses* Now it's time to enter the matrix
        async move {
            // Here we make our retryable future that sends out the request, checks the status,
            // and collects the Atom body inside the retryable future.
            let body = with_backoff(move || {
                let req_builder = req_builder.try_clone().unwrap();
                async move { Ok::<_, reqwest::Error>(req_builder.send().await?.error_for_status()?.bytes().await?) }
            })
            .await
            .map_err(eyre::Error::from)?;

            parse_feed(&body)
        }
    }
}

impl<'a> Stream for Posts<'a> {
    type Item = String;

    fn poll_next(mut self: Pin<&mut Self>, ctx: &mut std::task::Context<'_>) -> Poll<Option<Self::Item>> {
        // Simple state-machine loop
        loop {
            match self.state {
                // If we need more posts, let's spin up a future that gets them for us
                PostsState::NeedMore => self.state = PostsState::Fetching(self.get_next_page().boxed_local()),

                // If we're currently fetching posts, let's poll the future
                PostsState::Fetching(ref mut fut) => {
                    // We'll use the `ready!` macro which is kinda like `try!` for `Poll`
                    let posts = futures::ready!(fut.as_mut().poll(ctx));

                    match posts {
                        // If we've got posts, move on to the next state
                        Ok(Page { next_page_id, posts }) => {
                            self.next_page_id = next_page_id;
                            self.state = PostsState::Fetched(posts);
                        }

                        Err(error) => {
                            // We've already got backoff baked into `get_next_page`, we probably can't recover here
                            // It's best if we just stop giving out posts
                            warn!(?error, "error while fetching posts");
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
                        warn!("missing next_page_id");
                        self.state = PostsState::Exhausted;
                    }
                }

                // If we've exhausted the posts (AKA hit an error), just return no more items
                PostsState::Exhausted => return Poll::Ready(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_post_url_finds_link_anchor() {
        let content = r#"
            <table><tr><td>
                <span><a href="https://i.redd.it/example.png">[link]</a></span>
                <span><a href="https://reddit.com/comments/example">[comments]</a></span>
            </td></tr></table>
        "#;

        assert_eq!(
            extract_post_url(content).as_deref(),
            Some("https://i.redd.it/example.png")
        );
    }

    #[test]
    fn parse_feed_extracts_post_urls_and_next_page_id() {
        let body = br#"<?xml version="1.0" encoding="UTF-8"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <title>newest submissions : multi</title>
                <updated>2026-06-07T15:18:45+00:00</updated>
                <id>/r/wallpaper/new.rss</id>
                <entry>
                    <title>Direct image</title>
                    <id>t3_first</id>
                    <updated>2026-06-07T15:11:43+00:00</updated>
                    <content type="html">
                        &lt;span&gt;&lt;a href=&quot;https://i.redd.it/direct.png&quot;&gt;[link]&lt;/a&gt;&lt;/span&gt;
                        &lt;a href=&quot;https://reddit.com/c/1&quot;&gt;[comments]&lt;/a&gt;
                    </content>
                </entry>
                <entry>
                    <title>Gallery</title>
                    <id>t3_second</id>
                    <updated>2026-06-07T15:10:43+00:00</updated>
                    <content type="html">
                        &lt;a href=&quot;https://www.reddit.com/gallery/a&quot;&gt;[link]&lt;/a&gt;
                    </content>
                </entry>
            </feed>"#;

        let page = parse_feed(body).unwrap();

        assert_eq!(page.next_page_id.as_deref(), Some("t3_second"));
        assert_eq!(
            page.posts,
            vec![
                "https://i.redd.it/direct.png".to_owned(),
                "https://www.reddit.com/gallery/a".to_owned(),
            ]
        );
    }

    #[test]
    fn parse_feed_handles_empty_feed() {
        let body = br#"<?xml version="1.0" encoding="UTF-8"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <title>newest submissions : multi</title>
                <updated>2026-06-07T15:18:45+00:00</updated>
                <id>/r/wallpaper/new.rss</id>
            </feed>"#;

        let page = parse_feed(body).unwrap();

        assert!(page.next_page_id.is_none());
        assert!(page.posts.is_empty());
    }
}
