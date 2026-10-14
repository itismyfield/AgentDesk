const REPORT_LIMIT: usize = 4096;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Admission {
    Allowed,
    CapacityDeferred,
}

pub(super) struct Capacity {
    pub canonical: usize,
    pub archived: usize,
    pub invalid: usize,
    pub complete: bool,
}

impl Capacity {
    pub(super) fn total(&self) -> usize {
        self.canonical
            .saturating_add(self.archived)
            .saturating_add(self.invalid)
    }

    // Reservation includes both a new attempt and any archive required before replacement.
    pub(super) fn new_attempt(&self, additional_files: usize) -> Admission {
        let total = self.total();
        if !self.complete || total >= REPORT_LIMIT || additional_files > REPORT_LIMIT - total {
            Admission::CapacityDeferred
        } else {
            Admission::Allowed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_counts_every_kind_and_preserves_r0() {
        let mut count = Capacity {
            canonical: 2000,
            archived: 2000,
            invalid: 95,
            complete: true,
        };
        assert_eq!(count.total(), 4095);
        assert_eq!(count.new_attempt(1), Admission::Allowed);
        assert_eq!(count.new_attempt(2), Admission::CapacityDeferred);
        count.invalid += 1;
        assert_eq!(count.new_attempt(0), Admission::CapacityDeferred);
        assert_eq!(count.new_attempt(1), Admission::CapacityDeferred);
        count.canonical = 6000;
        let before = count.total();
        assert_eq!(count.new_attempt(0), Admission::CapacityDeferred);
        assert_eq!(count.total(), before);
        count.canonical = usize::MAX;
        assert_eq!(count.total(), usize::MAX);
        assert_eq!(count.new_attempt(usize::MAX), Admission::CapacityDeferred);
        count = Capacity {
            canonical: 0,
            archived: 0,
            invalid: 0,
            complete: false,
        };
        assert_eq!(count.new_attempt(1), Admission::CapacityDeferred);
    }
}
