use std::collections::VecDeque;

/// A bounded FIFO that evicts its oldest element when full and counts how often
/// that happened.
///
/// This replaces relying on the kernel's autotuned `SO_SNDBUF` for implicit
/// buffering. That approach was uncontrollable, unobservable, and its failure
/// mode was dropping the client connection.
///
/// Total capacity is `max_len` queued items plus at most one in-flight item
/// (see `in_flight` below); the evictable queue alone is bounded to `max_len`.
pub struct Backlog<T> {
    /// A partially-written item, held outside `items` so that `push`'s eviction
    /// can never discard it.
    ///
    /// Dropping a half-written packet would leave the peer with a torn frame,
    /// and because packets are fixed size and parsed by byte offset, every
    /// subsequent boundary would be shifted permanently. An ordinary
    /// whole-packet eviction is merely a gap; this would be corruption.
    in_flight: Option<T>,
    items: VecDeque<T>,
    max_len: usize,
    overflow_count: u64,
}

impl<T> Backlog<T> {
    /// # Panics
    /// Panics if `max_len == 0`, which is a configuration or programming error;
    /// this project's convention is to fail loudly on those.
    pub fn new(max_len: usize) -> Self {
        assert!(max_len > 0, "max_len must be > 0");
        Backlog {
            in_flight: None,
            items: VecDeque::with_capacity(max_len),
            max_len,
            overflow_count: 0,
        }
    }

    /// Appends to the tail, evicting the **oldest** element when full.
    ///
    /// Never touches `in_flight`: eviction here only ever reaches whole,
    /// unsent packets in `items`, never the partially-written remainder held
    /// by `restore_front`.
    pub fn push(&mut self, item: T) {
        if self.items.len() == self.max_len {
            self.items.pop_front();
            self.overflow_count += 1;
        }
        self.items.push_back(item);
    }

    /// Returns a partially-consumed item to the head, for a caller that wrote only
    /// part of it to a socket. Held outside the evictable queue, so unlike `push`
    /// this can never be dropped by a later overflow.
    ///
    /// # Panics
    /// Panics if an in-flight item is already held. Callers must pop before
    /// restoring; holding two would mean a lost frame.
    pub fn restore_front(&mut self, item: T) {
        assert!(
            self.in_flight.is_none(),
            "an in-flight item is already held"
        );
        self.in_flight = Some(item);
    }

    pub fn front(&self) -> Option<&T> {
        self.in_flight.as_ref().or_else(|| self.items.front())
    }

    pub fn pop_front(&mut self) -> Option<T> {
        self.in_flight.take().or_else(|| self.items.pop_front())
    }

    pub fn len(&self) -> usize {
        self.items.len() + self.in_flight.is_some() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.in_flight.is_none() && self.items.is_empty()
    }

    /// Total elements dropped because the bound was hit. Should stay at 0 in
    /// normal operation; anything else is an alarm signal.
    pub fn overflow_count(&self) -> u64 {
        self.overflow_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pushes_within_bound_are_kept_in_order() {
        let mut b = Backlog::new(3);
        b.push(1);
        b.push(2);
        assert_eq!(b.len(), 2);
        assert_eq!(b.front(), Some(&1));
        assert_eq!(b.pop_front(), Some(1));
        assert_eq!(b.pop_front(), Some(2));
        assert_eq!(b.pop_front(), None);
        assert!(b.is_empty());
    }

    #[test]
    fn overflow_evicts_oldest_and_counts() {
        let mut b = Backlog::new(2);
        b.push(1);
        b.push(2);
        b.push(3);
        assert_eq!(b.len(), 2, "must stay bounded");
        assert_eq!(b.overflow_count(), 1);
        assert_eq!(b.pop_front(), Some(2), "oldest should have been evicted");
        assert_eq!(b.pop_front(), Some(3));
    }

    #[test]
    fn overflow_count_accumulates() {
        let mut b = Backlog::new(1);
        for i in 0..5 {
            b.push(i);
        }
        assert_eq!(b.overflow_count(), 4);
        assert_eq!(b.front(), Some(&4));
    }

    /// A partial socket write needs the remainder returned to the head.
    #[test]
    fn restore_front_returns_item_to_head_without_counting_overflow() {
        let mut b = Backlog::new(3);
        b.push(2);
        b.push(3);
        b.restore_front(1);
        assert_eq!(b.overflow_count(), 0);
        assert_eq!(b.pop_front(), Some(1));
    }

    /// The in-flight slot sits outside `max_len`, so restoring at capacity must
    /// not evict anything -- the whole point is that this item is unevictable.
    #[test]
    fn restore_front_at_capacity_does_not_evict() {
        let mut b = Backlog::new(2);
        b.push(2);
        b.push(3);
        b.restore_front(1);
        assert_eq!(b.len(), 3);
        assert_eq!(b.overflow_count(), 0);
        assert_eq!(b.pop_front(), Some(1), "restored head must survive");
        assert_eq!(b.pop_front(), Some(2));
        assert_eq!(b.pop_front(), Some(3));
    }

    /// Regression: an in-flight partial remainder must survive a later overflow.
    /// If eviction could drop it, the peer would receive a torn frame and, because
    /// packets are fixed size and parsed by offset, every subsequent boundary would
    /// shift permanently.
    #[test]
    fn overflow_cannot_discard_the_in_flight_remainder() {
        let mut b: Backlog<&str> = Backlog::new(2);
        b.push("A");
        b.push("B");
        let a = b.pop_front().unwrap();
        assert_eq!(a, "A");
        b.restore_front("A-remainder");

        // The queue still holds B and now takes C, hitting the bound.
        b.push("C");
        b.push("D");

        // Whatever was evicted, it must not be the in-flight remainder.
        assert_eq!(
            b.pop_front(),
            Some("A-remainder"),
            "the in-flight remainder was discarded"
        );
    }

    #[test]
    #[should_panic(expected = "an in-flight item is already held")]
    fn restoring_twice_without_popping_is_a_programming_error() {
        let mut b: Backlog<u8> = Backlog::new(2);
        b.push(1);
        let x = b.pop_front().unwrap();
        b.restore_front(x);
        b.restore_front(9);
    }

    #[test]
    fn in_flight_counts_toward_len_and_emptiness() {
        let mut b: Backlog<u8> = Backlog::new(2);
        assert!(b.is_empty());
        b.push(1);
        let x = b.pop_front().unwrap();
        assert!(b.is_empty());
        b.restore_front(x);
        assert!(!b.is_empty());
        assert_eq!(b.len(), 1);
        b.push(2);
        assert_eq!(b.len(), 2);
        assert_eq!(b.pop_front(), Some(1), "in-flight must come out first");
        assert_eq!(b.pop_front(), Some(2));
    }

    #[test]
    #[should_panic(expected = "max_len must be > 0")]
    fn zero_capacity_is_a_programming_error() {
        Backlog::<u8>::new(0);
    }
}
