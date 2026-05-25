use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Debug)]
pub struct SystemBytesGenerator {
    current: AtomicU32,
}

impl Default for SystemBytesGenerator {
    fn default() -> Self {
        Self {
            current: AtomicU32::new(1),
        }
    }
}

impl SystemBytesGenerator {
    pub fn next(&self) -> u32 {
        self.current
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(if current == u32::MAX { 1 } else { current + 1 })
            })
            .unwrap()
    }

    #[cfg(test)]
    fn from_initial(value: u32) -> Self {
        Self {
            current: AtomicU32::new(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SystemBytesGenerator;
    use std::sync::Arc;

    #[test]
    fn independent_generators_start_at_one() {
        let first = SystemBytesGenerator::default();
        let second = SystemBytesGenerator::default();

        assert_eq!(first.next(), 1);
        assert_eq!(first.next(), 2);
        assert_eq!(second.next(), 1);
        assert_eq!(second.next(), 2);
    }

    #[test]
    fn cloned_arc_shares_sequence() {
        let first = Arc::new(SystemBytesGenerator::default());
        let second = Arc::clone(&first);

        assert_eq!(first.next(), 1);
        assert_eq!(second.next(), 2);
        assert_eq!(first.next(), 3);
    }

    #[test]
    fn wraps_from_max_back_to_one() {
        let generator = SystemBytesGenerator::from_initial(u32::MAX);

        assert_eq!(generator.next(), u32::MAX);
        assert_eq!(generator.next(), 1);
        assert_eq!(generator.next(), 2);
    }
}
