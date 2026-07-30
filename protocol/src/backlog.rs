use std::collections::VecDeque;

/// A bounded FIFO that evicts its oldest element when full and counts how often
/// that happened.
///
/// This replaces relying on the kernel's autotuned `SO_SNDBUF` for implicit
/// buffering. That approach was uncontrollable, unobservable, and its failure
/// mode was dropping the client connection.
pub struct Backlog<T> {
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
            items: VecDeque::with_capacity(max_len),
            max_len,
            overflow_count: 0,
        }
    }

    /// Appends to the tail, evicting the **oldest** element when full.
    pub fn push(&mut self, item: T) {
        if self.items.len() == self.max_len {
            self.items.pop_front();
            self.overflow_count += 1;
        }
        self.items.push_back(item);
    }

    /// Returns an item to the head, for holding the remainder of a partial write.
    /// When full this evicts the **newest** element, so the item just restored is
    /// guaranteed to stay at the head.
    pub fn push_front(&mut self, item: T) {
        if self.items.len() == self.max_len {
            self.items.pop_back();
            self.overflow_count += 1;
        }
        self.items.push_front(item);
    }

    pub fn front(&self) -> Option<&T> {
        self.items.front()
    }

    pub fn pop_front(&mut self) -> Option<T> {
        self.items.pop_front()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
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
    fn push_front_returns_item_to_head_without_counting_overflow() {
        let mut b = Backlog::new(3);
        b.push(2);
        b.push(3);
        b.push_front(1);
        assert_eq!(b.overflow_count(), 0);
        assert_eq!(b.pop_front(), Some(1));
    }

    /// push_front must respect the bound too, or the partial-write retry path
    /// would let the queue grow without limit.
    #[test]
    fn push_front_at_capacity_evicts_newest_and_counts() {
        let mut b = Backlog::new(2);
        b.push(2);
        b.push(3);
        b.push_front(1);
        assert_eq!(b.len(), 2);
        assert_eq!(b.overflow_count(), 1);
        assert_eq!(b.pop_front(), Some(1), "restored head must survive");
        assert_eq!(b.pop_front(), Some(2));
    }

    #[test]
    #[should_panic(expected = "max_len must be > 0")]
    fn zero_capacity_is_a_programming_error() {
        Backlog::<u8>::new(0);
    }
}
