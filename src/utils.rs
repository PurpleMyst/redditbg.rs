use futures_retry::{ErrorHandler, RetryPolicy};
use image::GenericImageView;
use noisy_float::prelude::*;

pub struct BackoffPolicy<'a>(pub exponential_backoff::Iter<'a>);

impl<E> ErrorHandler<E> for BackoffPolicy<'_> {
    type OutError = E;

    fn handle(&mut self, _attempt: usize, err: E) -> RetryPolicy<Self::OutError> {
        match self.0.next() {
            Some(Some(duration)) => RetryPolicy::WaitRetry(duration),
            Some(None) | None => RetryPolicy::ForwardError(err),
        }
    }
}

pub fn aspect_ratio<Image: GenericImageView>(image: &Image) -> R64 {
    let (w, h) = image.dimensions();
    r64(w as f64 / h as f64)
}
