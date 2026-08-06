//! How much memory a component is holding, and for what.
//!
//! One number cannot answer a sizing question: "how much RAM" is not answerable, "how much RAM for
//! which structure, at what count" is. So a component reports named parts, each carrying the entries
//! it holds now, the most it has ever held, and the bytes behind them.
//!
//! Two kinds of part, because the bytes are knowable to different precision. A contiguous buffer is
//! exact — capacity times the element size is what was allocated. A hash table is not: it rounds to a
//! power-of-two bucket count and keeps a control byte per bucket, so its bytes are derived from
//! capacity rather than read off it. Every report says which parts are which, because a sizing answer
//! that hides its own precision is worse than a missing one.
//!
//! This is the contract for counting, not an implementation of one: it has no idea what any component
//! holds. Each owner fills in its own parts, so a layout change touches only the module that made it.

use std::mem::size_of;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A map's occupancy, published by the thread that owns it so another thread can report it. Relaxed
/// throughout: a number that sizes a machine does not have to be simultaneous with anything, and the
/// alternative — a lock the owner takes as it works — would put contention on a hot path to answer a
/// question nobody asks per request. One writer only, which is what makes the peak's read-max-write
/// safe.
#[derive(Debug, Default)]
pub struct MapGauge {
    entries: AtomicUsize,
    capacity: AtomicUsize,
    peak: AtomicUsize,
}

impl MapGauge {
    pub fn publish(&self, entries: usize, capacity: usize) {
        self.entries.store(entries, Ordering::Relaxed);
        self.capacity.store(capacity, Ordering::Relaxed);
        let peak = self.peak.load(Ordering::Relaxed).max(entries);
        self.peak.store(peak, Ordering::Relaxed);
    }

    pub fn entries(&self) -> usize {
        self.entries.load(Ordering::Relaxed)
    }

    pub fn capacity(&self) -> usize {
        self.capacity.load(Ordering::Relaxed)
    }

    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }
}

/// One named structure a component holds.
#[derive(Debug, Clone, Copy)]
pub struct Part {
    pub name: &'static str,
    /// Entries live now, and the most that were ever live at once. The peak is the sizing answer: a
    /// structure sized for its mean overflows.
    pub entries: usize,
    pub peak_entries: usize,
    /// Entries it has room for. Against the peak, this says whether a bound somebody chose was the
    /// thing the run ran into — which is the difference between measuring a ledger and measuring a
    /// harness. Zero when the structure has no meaningful ceiling.
    pub capacity: usize,
    pub bytes: usize,
    /// False for a hash table, whose allocation is derived rather than read.
    pub exact: bool,
}

impl Part {
    /// How close the peak came to the room available. A part that filled what it was given may have
    /// been the limit rather than a witness to it.
    pub fn fill(&self) -> f64 {
        if self.capacity == 0 {
            return 0.0;
        }
        self.peak_entries as f64 / self.capacity as f64
    }
}

/// What one component is holding. Built by the component that owns the structures, so nothing outside
/// it has to know their shapes.
#[derive(Debug, Default, Clone)]
pub struct Footprint {
    parts: Vec<Part>,
}

impl Footprint {
    pub fn new() -> Self {
        Self { parts: Vec::new() }
    }

    /// A contiguous buffer: `capacity` elements were allocated whether or not they are in use, so the
    /// bytes are exact and `entries` is what is live inside them.
    pub fn buffer<T>(&mut self, name: &'static str, entries: usize, capacity: usize, peak: usize) {
        self.parts.push(Part {
            name,
            entries,
            peak_entries: peak,
            capacity,
            bytes: capacity * size_of::<T>(),
            exact: true,
        });
    }

    /// A hash table. `hashbrown` rounds the requested capacity up to a power-of-two bucket count at
    /// seven-eighths load and keeps one control byte per bucket, so the bytes follow from the bucket
    /// count — which is derived here, since the map reports usable capacity rather than buckets. Close
    /// enough to size a machine by, and marked so nobody quotes it as exact.
    pub fn hash_table<K, V>(
        &mut self,
        name: &'static str,
        entries: usize,
        capacity: usize,
        peak: usize,
    ) {
        let buckets = Self::buckets(capacity);
        self.parts.push(Part {
            name,
            entries,
            peak_entries: peak,
            capacity,
            bytes: buckets * (size_of::<(K, V)>() + 1),
            exact: false,
        });
    }

    /// A part whose bytes the owner works out itself, for a structure that is neither. `capacity` is
    /// the room it has, or zero when it has no ceiling worth comparing a peak against.
    pub fn other(
        &mut self,
        name: &'static str,
        entries: usize,
        peak: usize,
        capacity: usize,
        bytes: usize,
    ) {
        self.parts.push(Part {
            name,
            entries,
            peak_entries: peak,
            capacity,
            bytes,
            exact: true,
        });
    }

    /// A hash table on another thread, read from what its owner published.
    pub fn gauged_table<K, V>(&mut self, name: &'static str, gauge: &MapGauge) {
        self.hash_table::<K, V>(name, gauge.entries(), gauge.capacity(), gauge.peak());
    }

    pub fn parts(&self) -> &[Part] {
        &self.parts
    }

    pub fn bytes(&self) -> usize {
        self.parts.iter().map(|part| part.bytes).sum()
    }

    /// Whether every part's bytes are exact, so a report can say once whether the total is.
    pub fn exact(&self) -> bool {
        self.parts.iter().all(|part| part.exact)
    }

    /// `hashbrown`'s capacity-to-buckets rule, which is what decides the allocation.
    fn buckets(capacity: usize) -> usize {
        match capacity {
            0 => 0,
            1..=3 => 4,
            4..=7 => 8,
            capacity => (capacity * 8 / 7).next_power_of_two(),
        }
    }
}

/// The most entries a structure has held at once. A sizing answer needs the peak, not the current
/// value: a run reports whatever it happened to be holding when it was asked.
#[derive(Debug, Default, Clone, Copy)]
pub struct Peak(usize);

impl Peak {
    pub fn saw(&mut self, entries: usize) {
        self.0 = self.0.max(entries);
    }

    pub fn entries(self) -> usize {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::{Footprint, Peak};

    /// A buffer's bytes are what was allocated, not what is in use — that is the whole point of asking
    /// capacity rather than length.
    #[test]
    fn a_buffer_is_priced_by_its_capacity() {
        let mut footprint = Footprint::new();
        footprint.buffer::<u64>("slots", 10, 1_024, 700);
        assert_eq!(footprint.bytes(), 1_024 * 8);
        assert_eq!(footprint.parts()[0].peak_entries, 700);
        assert!(footprint.exact());
        // Against the room it had, so a report can say whether the bound was the limit.
        assert!((footprint.parts()[0].fill() - 700.0 / 1_024.0).abs() < 1e-9);
    }

    /// A hash table costs its bucket count, which is above its capacity, and says it is approximate.
    #[test]
    fn a_hash_table_is_priced_by_its_buckets_and_says_it_is_approximate() {
        let mut footprint = Footprint::new();
        footprint.hash_table::<u64, u64>("idem", 700, 1_000, 900);
        // 1000 * 8 / 7 rounds up to 2048 buckets, each holding a pair and a control byte.
        assert_eq!(footprint.bytes(), 2_048 * 17);
        assert!(!footprint.exact());
    }

    #[test]
    fn a_peak_remembers_the_largest_it_was_shown() {
        let mut peak = Peak::default();
        peak.saw(4);
        peak.saw(9);
        peak.saw(2);
        assert_eq!(peak.entries(), 9);
    }
}
