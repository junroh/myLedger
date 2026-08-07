//! What one of a thing costs, so a sizing answer is arithmetic rather than a remembered number.
//!
//! `footprint` reports what a *run* held. That answers "was this run's ceiling right" and nothing
//! about a deployment nobody has run: a machine is sized from a rate, a lifetime and a retention, none
//! of which a three-second run has. The arithmetic that turns those into bytes needs one fact per
//! structure — **what one unit of it costs** — and that fact is `size_of`, known at build time.
//!
//! So this is the unit half and only the unit half. **How many** is the sizing model's job and it does
//! not live here: the counts come from a rate and a policy, and a formula for them in this crate would
//! be a prediction with no measurement behind it (design notes §10 refused exactly that shape once).
//!
//! The declaration a consumer actually depends on is the **list**, not the numbers in it. A structure
//! added to a component and not added here reads as zero bytes to whatever is summing, which is the
//! failure that has no symptom — so a consumer is expected to refuse a name it does not know rather
//! than skip it, and the list is what makes that possible.

use std::mem::size_of;

/// What a structure charges per unit, and what its unit is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizedPart {
    /// The name the component's `Footprint` reports it under, so a prediction and a run line up by
    /// name rather than by the order somebody wrote them in.
    pub name: &'static str,
    /// What **one unit of it is**, in a phrase. Declared by the crate that owns the structure, because
    /// that is the only place that knows — a sizing model can say how many there are and never what
    /// they are, and a table of names and byte counts is a table nobody can check.
    pub what: &'static str,
    pub unit: Unit,
    pub bytes: usize,
}

impl SizedPart {
    pub const fn new(name: &'static str, unit: Unit, bytes: usize, what: &'static str) -> Self {
        Self {
            name,
            what,
            unit,
            bytes,
        }
    }

    /// A hash table's cost per **bucket**, not per entry — the two differ by up to a factor of two and
    /// the difference is the whole reason `Unit::Bucket` exists.
    pub const fn table<K, V>(name: &'static str, what: &'static str) -> Self {
        Self::new(name, Unit::Bucket, bucket_bytes::<K, V>(), what)
    }
}

/// What the count multiplying `bytes` is counted in. It is not decoration: each unit reaches bytes by
/// a different route, and a consumer that treats them alike is wrong by more than rounding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// A pool slot for one request in flight.
    Slot,
    /// One account, so the count is the working set rather than the load.
    Account,
    /// One committed effect.
    Effect,
    /// One consensus batch.
    Batch,
    /// One entry in a contiguous buffer.
    Entry,
    /// One hash-table bucket — see [`buckets_for`], which is where the count stops being linear.
    Bucket,
    /// One 4KB block, on disk or in memory.
    Block,
    /// One 80-byte record inside a block.
    Record,
}

impl Unit {
    pub const fn name(self) -> &'static str {
        match self {
            Unit::Slot => "slot",
            Unit::Account => "account",
            Unit::Effect => "effect",
            Unit::Batch => "batch",
            Unit::Entry => "entry",
            Unit::Bucket => "bucket",
            Unit::Block => "block",
            Unit::Record => "record",
        }
    }
}

/// `hashbrown`'s per-bucket cost: the pair plus its control byte.
///
/// Written once and used by both sides on purpose. `Footprint::hash_table` prices a live table with
/// it and a `SizedPart` declares it, and the two saying different things would be a sizing answer that
/// disagrees with the run it was checked against — for the same structure, in the same build.
pub const fn bucket_bytes<K, V>() -> usize {
    size_of::<(K, V)>() + 1
}

/// Buckets a table of `entries` allocates, which is `hashbrown`'s rule and **a staircase**: the count
/// is rounded up to a power of two, so one percent more entries can double the memory. A sizing answer
/// that averages bytes per entry hides the step it is standing next to, which is the number somebody
/// needs.
pub const fn buckets_for(entries: usize) -> usize {
    match entries {
        0 => 0,
        1..=3 => 4,
        4..=7 => 8,
        entries => (entries * 8 / 7).next_power_of_two(),
    }
}

/// Whether a list of parts is one a consumer can sum: no duplicate names, and nothing free.
pub const fn parts_are_sound(parts: &[SizedPart]) -> bool {
    let mut index = 0;
    while index < parts.len() {
        if parts[index].bytes == 0 || parts[index].what.is_empty() {
            return false;
        }
        let mut earlier = 0;
        while earlier < index {
            if str_eq(parts[index].name, parts[earlier].name) {
                return false;
            }
            earlier += 1;
        }
        index += 1;
    }
    true
}

/// `str::eq` is not const, and the check above has to run at build time to be worth having.
const fn str_eq(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    if left.len() != right.len() {
        return false;
    }
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{bucket_bytes, buckets_for, parts_are_sound, SizedPart, Unit};

    /// The rule the whole staircase rests on, at the two places it matters: just under a power of two
    /// and just over it.
    #[test]
    fn a_table_rounds_its_buckets_up_to_a_power_of_two() {
        assert_eq!(buckets_for(0), 0);
        assert_eq!(buckets_for(7), 8);
        // 7/8 load: 917,504 entries is the most a 2^20 table takes.
        assert_eq!(buckets_for(917_504), 1 << 20);
        assert_eq!(buckets_for(917_505), 1 << 21);
    }

    /// The control byte is the part a `size_of` alone would miss, and it is 3% of a small pair.
    #[test]
    fn a_bucket_costs_its_pair_and_a_control_byte() {
        assert_eq!(bucket_bytes::<u64, u32>(), 17);
        assert_eq!(bucket_bytes::<u128, u64>(), 33);
    }

    #[test]
    fn a_list_with_two_parts_of_one_name_is_refused() {
        const ONE: SizedPart = SizedPart::new("slots", Unit::Slot, 8, "one request in flight");
        assert!(parts_are_sound(&[ONE]));
        assert!(!parts_are_sound(&[ONE, ONE]));
        assert!(!parts_are_sound(&[SizedPart::new(
            "free",
            Unit::Slot,
            0,
            "nothing"
        )]));
        assert!(
            !parts_are_sound(&[SizedPart::new("mute", Unit::Slot, 8, "")]),
            "a part with no description is a row a reader cannot check"
        );
    }
}
