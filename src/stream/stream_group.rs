use futures_core::Stream;
use slab::Slab;
use std::fmt::{self, Debug};
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::task::Poll;

use crate::utils::iter_pin_mut_slab;

/// A growable group of streams which act as a single unit.
///
/// In order go mutate the group during iteration, the stream should be
/// combined with a mechanism such as
/// [`lend_mut`](https://docs.rs/async-iterator/latest/async_iterator/trait.Iterator.html#method.lend_mut).
/// This is not yet provided by the `futures-concurrency` crate.
///
/// # Example
///
/// ```rust
/// use futures_concurrency::stream::StreamGroup;
/// use futures_lite::{stream, StreamExt};
///
/// # futures_lite::future::block_on(async {
/// let mut group = StreamGroup::new();
/// group.insert(stream::once(2));
/// group.insert(stream::once(4));
///
/// let mut out = 0;
/// while let Some(num) = group.next().await {
///     out += num;
/// }
/// assert_eq!(out, 6);
/// # });
/// ```
#[must_use = "`StreamGroup` does nothing if not iterated over"]
#[derive(Default)]
#[pin_project::pin_project]
pub struct StreamGroup<S> {
    #[pin]
    streams: Slab<S>,
}

impl<T: Debug> Debug for StreamGroup<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamGroup")
            .field("slab", &"[..]")
            .finish()
    }
}

impl<S> StreamGroup<S> {
    /// Create a new instance of `StreamGroup`.
    ///
    /// # Example
    ///
    /// ```rust
    /// use futures_concurrency::stream::StreamGroup;
    ///
    /// let group = StreamGroup::new();
    /// # let group: StreamGroup<usize> = group;
    /// ```
    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    /// Create a new instance of `StreamGroup` with a given capacity.
    ///
    /// # Example
    ///
    /// ```rust
    /// use futures_concurrency::stream::StreamGroup;
    ///
    /// let group = StreamGroup::with_capacity(2);
    /// # let group: StreamGroup<usize> = group;
    /// ```
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            streams: Slab::with_capacity(capacity),
        }
    }

    /// Return the number of futures currently active in the group.
    ///
    /// # Example
    ///
    /// ```rust
    /// use futures_concurrency::stream::StreamGroup;
    /// use futures_lite::stream;
    ///
    /// let mut group = StreamGroup::with_capacity(2);
    /// assert_eq!(group.len(), 0);
    /// group.insert(stream::once(12));
    /// assert_eq!(group.len(), 1);
    /// ```
    pub fn len(&self) -> usize {
        self.streams.len()
    }

    /// Returns true if there are no futures currently active in the group.
    ///
    /// # Example
    ///
    /// ```rust
    /// use futures_concurrency::stream::StreamGroup;
    /// use futures_lite::stream;
    ///
    /// let mut group = StreamGroup::with_capacity(2);
    /// assert!(group.is_empty());
    /// group.insert(stream::once(12));
    /// assert!(!group.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.streams.is_empty()
    }

    /// Insert a new future into the group.
    ///
    /// # Example
    ///
    /// ```rust
    /// use futures_concurrency::stream::StreamGroup;
    /// use futures_lite::stream;
    ///
    /// let mut group = StreamGroup::with_capacity(2);
    /// group.insert(stream::once(12));
    /// ```
    pub fn insert(&mut self, stream: S) -> Key
    where
        S: Stream,
    {
        let index = self.streams.insert(stream);
        Key(index)
    }

    /// Removes a stream from the group. Returns whether the value was present in
    /// the group.
    ///
    /// # Example
    ///
    /// ```
    /// use futures_lite::stream;
    /// use futures_concurrency::stream::StreamGroup;
    ///
    /// # futures_lite::future::block_on(async {
    /// let mut group = StreamGroup::new();
    /// let key = group.insert(stream::once(4));
    /// assert_eq!(group.len(), 1);
    /// group.remove(key);
    /// assert_eq!(group.len(), 0);
    /// # })
    /// ```
    pub fn remove(&mut self, key: Key) -> bool {
        self.streams.try_remove(key.0).is_some()
    }

    /// Returns `true` if the `StreamGroup` contains a value for the specified key.
    ///
    /// # Example
    ///
    /// ```
    /// use futures_lite::stream;
    /// use futures_concurrency::stream::StreamGroup;
    ///
    /// # futures_lite::future::block_on(async {
    /// let mut group = StreamGroup::new();
    /// let key = group.insert(stream::once(4));
    /// assert!(group.contains_key(key));
    /// group.remove(key);
    /// assert!(!group.contains_key(key));
    /// # })
    /// ```
    pub fn contains_key(&mut self, key: Key) -> bool {
        self.streams.contains(key.0)
    }
}

impl<S: Stream> StreamGroup<S> {
    /// Create a stream which also yields the key of each item.
    ///
    /// # Example
    ///
    /// ```rust
    /// use futures_concurrency::stream::StreamGroup;
    /// use futures_lite::{stream, StreamExt};
    ///
    /// # futures_lite::future::block_on(async {
    /// let mut group = StreamGroup::new();
    /// group.insert(stream::once(2));
    /// group.insert(stream::once(4));
    ///
    /// let mut out = 0;
    /// let mut group = group.keyed();
    /// while let Some((_key, num)) = group.next().await {
    ///     out += num;
    /// }
    /// assert_eq!(out, 6);
    /// # });
    /// ```
    pub fn keyed(self) -> Keyed<S> {
        Keyed { group: self }
    }
}

impl<S: Stream> StreamGroup<S> {
    fn poll_next_inner(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<(Key, <S as Stream>::Item)>> {
        let mut this = self.project();

        // Short-circuit if we have no streams to iterate over
        if this.streams.is_empty() {
            return Poll::Ready(None);
        }

        // NOTE(yosh): lol, this is so bad
        // we should replace this with a proper `Wakergroup`
        let mut remove_idx = vec![];
        let mut ret = Poll::Pending;
        let total = this.streams.len();
        let mut empty = 0;

        for (index, stream) in iter_pin_mut_slab(this.streams.as_mut()) {
            match stream.poll_next(cx) {
                Poll::Ready(Some(item)) => {
                    ret = Poll::Ready(Some((Key(index), item)));
                    break;
                }
                Poll::Ready(None) => {
                    // The stream ended, remove the item.
                    remove_idx.push(index);
                    empty += 1;
                    continue;
                }
                Poll::Pending => continue,
            };
        }

        // Remove all indexes we just flagged as ready for removal
        for idx in remove_idx {
            // SAFETY: we're accessing the internal `streams` store
            // only to drop the stream.
            let streams = unsafe { this.streams.as_mut().get_unchecked_mut() };
            streams.remove(idx);
        }

        // If all streams turned up with `Poll::Ready(None)` our
        // stream should return that
        if empty == total {
            ret = Poll::Ready(None);
        }

        ret
    }
}

impl<S: Stream> Stream for StreamGroup<S> {
    type Item = <S as Stream>::Item;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        // let this = self.project();
        match self.poll_next_inner(cx) {
            Poll::Ready(Some((_key, item))) => Poll::Ready(Some(item)),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// A key used to index into the `StreamGroup` type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(usize);

/// Iterate over items in the stream group with their associated keys.
#[derive(Debug)]
#[pin_project::pin_project]
pub struct Keyed<S: Stream> {
    #[pin]
    group: StreamGroup<S>,
}

impl<S: Stream> Deref for Keyed<S> {
    type Target = StreamGroup<S>;

    fn deref(&self) -> &Self::Target {
        &self.group
    }
}

impl<S: Stream> DerefMut for Keyed<S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.group
    }
}

impl<S: Stream> Stream for Keyed<S> {
    type Item = (Key, <S as Stream>::Item);

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        this.group.as_mut().poll_next_inner(cx)
    }
}

#[cfg(test)]
mod test {
    use super::StreamGroup;
    use futures_lite::{prelude::*, stream};

    #[test]
    fn smoke() {
        futures_lite::future::block_on(async {
            let mut group = StreamGroup::new();
            group.insert(stream::once(2));
            group.insert(stream::once(4));

            let mut out = 0;
            while let Some(num) = group.next().await {
                out += num;
            }
            assert_eq!(out, 6);
            assert_eq!(group.len(), 0);
            assert!(group.is_empty());
        });
    }
}
