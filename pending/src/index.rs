use std::hash::BuildHasher;

use ledger_base::{FxBuildHasher, LineFit, Prng, TxId};

use crate::block::BlockAddr;

/// Slots per bucket. Four ways over two candidate buckets is what makes a load factor near one
/// reachable while a probe stays inside two buckets — a generic hash table reaches the same average
/// with a probe that has no bound, and the bound is the point.
const WAYS: usize = 4;

/// Relocations one insert may cascade through before it gives up. The cap is what bounds an insert, and
/// an insert is on the path that applies committed decisions in order — so it is a latency budget, not a
/// tuning dial: a hop is a random bucket read, so this is the worst an insert may cost in cache misses.
///
/// A hundred and twenty-eight, measured rather than inherited. At the target load factor a cascade this
/// long always finds a home — the longest observed stops short of the cap, which is what says the cap is
/// not the binding constraint. The thirty-two it replaces came from the Python simulator's config and had
/// never been measured; at thirty-two, one insert in seven thousand cannot be placed at all.
const MAX_HOPS: u32 = 128;

/// What one slot costs, so a configuration can be refused before it is allocated rather than after.
pub const SLOT_BYTES: usize = core::mem::size_of::<Slot>();

/// What the table is sized against. The ten percent left empty is not waste: it is the headroom that
/// absorbs a lifetime distribution drifting and a mass expiry falling behind, and reaching it means the
/// worst case the configuration declared has arrived.
pub const LOAD_TARGET: f64 = 0.90;

/// A fingerprint, a record address, and one bit saying whether this slot's fingerprint is shared.
///
/// ```text
///  63          48 47        47 46                              0
/// | fingerprint  | ambiguous | address (segment | block | index) |
/// ```
///
/// Sixteen bits of fingerprint do not identify a key: at the design's scale roughly a hundred thousand
/// pairs of live keys share both a fingerprint and a bucket. So correctness cannot rest on a match being
/// identity — instead the ambiguity is **detected** when the second of a pair is inserted, and the bit
/// says so. An unmarked slot is known to be the only one with its fingerprint in its bucket, so finding
/// it needs no record; a marked one is disambiguated by reading. At scale that is a few thousandths of a
/// percent of holds.
///
/// Eight bytes rather than sixteen because the alternative bought only one thing that mattered — a
/// rehash that needs no keys — and there is no rehash: the table is sized from configuration and never
/// grows. Four ways then make a thirty-two-byte bucket, which divides both target line sizes, so a probe
/// is one line per bucket either way.
type Slot = u64;

const EMPTY: Slot = 0;
const FINGERPRINT_SHIFT: u32 = 48;
const AMBIGUOUS: u64 = 1 << 47;
const ADDRESS_MASK: u64 = AMBIGUOUS - 1;

/// What a probe could conclude on its own.
enum Found {
    Absent,
    /// The one slot with this fingerprint in these buckets, and it is not marked as shared.
    At((usize, usize)),
    /// More than one candidate, or one that is marked: a record has to say which.
    Ambiguous(Candidates),
}

/// An entry a full cascade left without a home. It is not in the table and it is not lost: whoever asked
/// for the insert is holding it, and there is nowhere else for it to go — the table was sized for a
/// declared maximum and that maximum has been passed.
#[derive(Debug, Clone, Copy)]
pub struct Homeless {
    pub address: BlockAddr,
}

/// The addresses a probe turned up, without allocating: two buckets of four ways is the ceiling, and a
/// second entry means two keys share a fingerprint.
#[derive(Debug, Clone, Copy)]
pub struct Candidates {
    addrs: [BlockAddr; 2 * WAYS],
    /// Bucket and way. The bucket is a `u32` because a table sized for the design has hundreds of
    /// millions of them, and a narrower field truncates silently — which it did.
    slots: [(u32, u8); 2 * WAYS],
    len: usize,
}

impl Default for Candidates {
    fn default() -> Self {
        Self {
            addrs: [BlockAddr::from_raw(0); 2 * WAYS],
            slots: [(0u32, 0u8); 2 * WAYS],
            len: 0,
        }
    }
}

impl Candidates {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn address(&self, at: usize) -> BlockAddr {
        self.addrs[at]
    }

    fn slot(&self, at: usize) -> (usize, usize) {
        let (bucket, way) = self.slots[at];
        (bucket as usize, way as usize)
    }

    fn push(&mut self, addr: BlockAddr, bucket: usize, way: usize) {
        self.addrs[self.len] = addr;
        self.slots[self.len] = (bucket as u32, way as u8);
        self.len += 1;
    }
}

/// Four slots, aligned so an array of buckets never crosses a line: one probe is one line's worth of
/// comparisons on both target line sizes.
#[repr(align(32))]
#[derive(Clone, Copy, Default)]
struct Bucket {
    slots: [Slot; WAYS],
}

ledger_base::layout_claim!(BUCKET_LAYOUT: Bucket, size = 32, LineFit::Inside);

/// Where each hold is, by transaction id: a bucketed cuckoo index over record addresses. It answers
/// *where*, never *what*, and it answers without reading anything.
pub struct HoldTable {
    buckets: Vec<Bucket>,
    mask: u64,
    live: usize,
    /// Which way a full bucket gives up. A fixed seed, because a table that evicted differently
    /// between two runs could not be compared with itself — the same reason the hasher is unseeded.
    victims: Prng,
    max_hops: u32,
    hops: u64,
    worst_hops: u32,
    ambiguous: u64,
}

/// Enough for a test or a local run without being asked. A deployment derives its size from what the
/// business declared — the table does not grow, so being asked is the point — and this is only the size
/// nobody asked for.
pub const DEFAULT_SLOTS: usize = 1 << 20;

impl Default for HoldTable {
    fn default() -> Self {
        Self::with_slots(DEFAULT_SLOTS)
    }
}

impl HoldTable {
    /// Rounded up to a power of two of buckets, and never fewer than two: with one bucket an entry
    /// would have one home instead of two and there would be nothing to kick to.
    pub fn with_slots(slots: usize) -> Self {
        Self::with_capacity(slots, MAX_HOPS)
    }

    /// A cap of its own, for measuring what a longer cascade buys. Nothing but a bench should choose it:
    /// the cap is the worst an insert may cost on the in-order path.
    pub fn with_capacity(slots: usize, max_hops: u32) -> Self {
        let buckets = slots.div_ceil(WAYS).next_power_of_two().max(2);
        Self {
            buckets: vec![Bucket::default(); buckets],
            mask: buckets as u64 - 1,
            live: 0,
            victims: Prng::new(0x9E37_79B9),
            max_hops: max_hops.max(1),
            hops: 0,
            worst_hops: 0,
            ambiguous: 0,
        }
    }

    pub fn addr_of(
        &self,
        key: TxId,
        verify: &mut dyn FnMut(BlockAddr) -> bool,
    ) -> Option<BlockAddr> {
        let slot = self.resolve(key, verify)?;
        Some(address_of(self.slot_word(slot)))
    }

    /// Whether this key's entry points at exactly this address. Read-free and exact — an address
    /// belongs to one record, so no fingerprint collision can make this true by accident. It is how
    /// compaction tells a record the index still needs from one that has been resolved or superseded.
    pub fn points_at(&self, key: TxId, addr: BlockAddr) -> bool {
        self.slot_at(key, addr).is_some()
    }

    /// Moves a key's entry from one address to another, which is what compaction does to a survivor.
    /// Read-free for the same reason. False means the index did not point at `old`, which is a caller
    /// that decided a record was alive and then lost the race — nothing here can repair that, so it is
    /// counted rather than guessed at.
    pub fn replace(&mut self, key: TxId, old: BlockAddr, new: BlockAddr) -> bool {
        let Some((bucket, way)) = self.slot_at(key, old) else {
            return false;
        };
        let slot = self.buckets[bucket].slots[way];
        self.buckets[bucket].slots[way] = pack(fingerprint_of(slot), new) | (slot & AMBIGUOUS);
        true
    }

    fn slot_at(&self, key: TxId, addr: BlockAddr) -> Option<(usize, usize)> {
        let (_, home, alternate) = self.locate(key);
        for bucket in [home, alternate] {
            for way in 0..WAYS {
                let slot = self.buckets[bucket].slots[way];
                if slot != EMPTY && address_of(slot) == addr {
                    return Some((bucket, way));
                }
            }
        }
        None
    }

    /// Takes in a key the store has never held. This is the one moment uniqueness can be checked for
    /// free: anything already in these buckets with this fingerprint is a different key, so both slots
    /// are marked and every later operation on either reads a record to tell them apart.
    pub fn insert_new(&mut self, key: TxId, addr: BlockAddr) -> Result<(), Homeless> {
        let (fingerprint, home, alternate) = self.locate(key);
        let clashes = self.candidates(key);
        let mut slot = pack(fingerprint, addr);
        if !clashes.is_empty() {
            slot |= AMBIGUOUS;
            for at in 0..clashes.len() {
                let (bucket, way) = clashes.slot(at);
                self.buckets[bucket].slots[way] |= AMBIGUOUS;
            }
            self.ambiguous += 1;
        }
        match self.place(slot, home, alternate) {
            Ok(()) => {
                self.live += 1;
                Ok(())
            }
            Err(displaced) => Err(Homeless {
                address: address_of(displaced),
            }),
        }
    }

    /// Points a key the store already holds at a new address. A partial resolution takes this path: the
    /// record is appended again and the index follows it, so nothing is rewritten in place. `verify` is
    /// only called when the fingerprint turns out to be shared, which is what the marking is for.
    pub fn repoint(
        &mut self,
        key: TxId,
        addr: BlockAddr,
        verify: &mut dyn FnMut(BlockAddr) -> bool,
    ) -> bool {
        let Some((bucket, way)) = self.resolve(key, verify) else {
            return false;
        };
        let slot = self.buckets[bucket].slots[way];
        self.buckets[bucket].slots[way] = pack(fingerprint_of(slot), addr) | (slot & AMBIGUOUS);
        true
    }

    /// Which slot is this key's, reading a record only when the marking says the fingerprint is shared.
    fn resolve(
        &self,
        key: TxId,
        verify: &mut dyn FnMut(BlockAddr) -> bool,
    ) -> Option<(usize, usize)> {
        match self.find(key) {
            Found::Absent => None,
            Found::At(slot) => Some(slot),
            Found::Ambiguous(candidates) => (0..candidates.len())
                .find(|&at| verify(candidates.address(at)))
                .map(|at| candidates.slot(at)),
        }
    }

    /// Slots marked as sharing a fingerprint. Zero in any deployment that has not seen a collision, and
    /// the number of holds that pay a record read to be identified.
    pub fn ambiguous(&self) -> u64 {
        self.ambiguous
    }

    /// A cuckoo slot is cleared outright: there is no probe sequence for a removal to interrupt, so
    /// no tombstone, which is what keeps a table that turns over as fast as it fills usable. The
    /// record itself stays where it is until its segment expires.
    pub fn remove(
        &mut self,
        key: TxId,
        verify: &mut dyn FnMut(BlockAddr) -> bool,
    ) -> Option<BlockAddr> {
        let (bucket, way) = self.resolve(key, verify)?;
        let slot = self.buckets[bucket].slots[way];
        self.buckets[bucket].slots[way] = EMPTY;
        self.live -= 1;
        Some(address_of(slot))
    }

    pub fn len(&self) -> usize {
        self.live
    }

    /// Slots, not buckets: what a load factor is measured against.
    pub fn slots(&self) -> usize {
        self.buckets.len() * WAYS
    }

    pub fn load_factor(&self) -> f64 {
        self.live as f64 / self.slots() as f64
    }

    /// What insertion cost in relocations, and the longest cascade seen. The second is the one to watch:
    /// it lengthens as the table fills, so it reaches the cap before inserts start failing.
    pub fn kick_stats(&self) -> (u64, u32) {
        (self.hops, self.worst_hops)
    }

    /// Both candidate buckets and the fingerprint, from one hash: the home takes the low bits and the
    /// fingerprint the high ones, so they are independent without hashing twice.
    fn locate(&self, key: TxId) -> (u64, usize, usize) {
        let hash = FxBuildHasher.hash_one(key.raw());
        let fingerprint = fingerprint_from(hash);
        let home = (hash & self.mask) as usize;
        (fingerprint, home, self.alternate(home, fingerprint))
    }

    /// The other bucket an entry may live in, from its fingerprint alone — which is why a relocation
    /// needs no key, and why the pair is symmetric: the alternate of the alternate is the original.
    fn alternate(&self, bucket: usize, fingerprint: u64) -> usize {
        let step = FxBuildHasher.hash_one(fingerprint) & self.mask;
        // A zero step would make a bucket its own alternate, leaving the entry one home.
        bucket ^ if step == 0 { 1 } else { step } as usize
    }

    /// Every address whose fingerprint matches, in probe order. With a whole hash for a fingerprint a
    /// second match is vanishingly unlikely — far below the rate at which the machine gets a bit wrong —
    /// but the fetch path still walks them in turn, because answering absent on a collision would reject
    /// a hold that exists and the walk costs nothing when there is only one.
    pub fn candidates(&self, key: TxId) -> Candidates {
        let (fingerprint, home, alternate) = self.locate(key);
        let mut found = Candidates::default();
        for bucket in [home, alternate] {
            for way in 0..WAYS {
                let slot = self.buckets[bucket].slots[way];
                if slot != EMPTY && fingerprint_of(slot) == fingerprint {
                    found.push(address_of(slot), bucket, way);
                }
            }
        }
        found
    }

    /// The slot this key owns, when the index alone can say. `Ambiguous` means two live keys share this
    /// fingerprint and a bucket, so only a record can tell them apart and the caller has to read one.
    fn find(&self, key: TxId) -> Found {
        let candidates = self.candidates(key);
        if candidates.is_empty() {
            return Found::Absent;
        }
        if candidates.len() == 1 && !is_ambiguous(self.slot_word(candidates.slot(0))) {
            return Found::At(candidates.slot(0));
        }
        Found::Ambiguous(candidates)
    }

    fn slot_word(&self, (bucket, way): (usize, usize)) -> Slot {
        self.buckets[bucket].slots[way]
    }

    /// `Err` carries the entry left without a home, which the caller has to place somewhere.
    fn place(&mut self, mut slot: Slot, home: usize, alternate: usize) -> Result<(), Slot> {
        if self.put(home, slot) || self.put(alternate, slot) {
            return Ok(());
        }
        let mut bucket = home;
        let mut came_from = alternate;
        for hop in 1..=self.max_hops {
            let way = self.victim(bucket, came_from);
            core::mem::swap(&mut slot, &mut self.buckets[bucket].slots[way]);
            came_from = bucket;
            bucket = self.alternate(bucket, fingerprint_of(slot));
            if self.put(bucket, slot) {
                self.hops += u64::from(hop);
                self.worst_hops = self.worst_hops.max(hop);
                return Ok(());
            }
        }
        Err(slot)
    }

    /// Which way a full bucket gives up. Any entry whose other home is the bucket we just came from
    /// would send the cascade straight back, so those are passed over while another is available — a
    /// cascade that oscillates spends its cap without exploring anything.
    fn victim(&mut self, bucket: usize, came_from: usize) -> usize {
        let start = (self.victims.next_u64() % WAYS as u64) as usize;
        for step in 0..WAYS {
            let way = (start + step) % WAYS;
            let fingerprint = fingerprint_of(self.buckets[bucket].slots[way]);
            if self.alternate(bucket, fingerprint) != came_from {
                return way;
            }
        }
        start
    }

    fn put(&mut self, bucket: usize, slot: Slot) -> bool {
        for way in 0..WAYS {
            if self.buckets[bucket].slots[way] == EMPTY {
                self.buckets[bucket].slots[way] = slot;
                return true;
            }
        }
        false
    }
}

const fn pack(fingerprint: u64, addr: BlockAddr) -> Slot {
    (fingerprint << FINGERPRINT_SHIFT) | (addr.raw() & ADDRESS_MASK)
}

const fn fingerprint_of(slot: Slot) -> u64 {
    slot >> FINGERPRINT_SHIFT
}

const fn address_of(slot: Slot) -> BlockAddr {
    BlockAddr::from_raw(slot & ADDRESS_MASK)
}

const fn is_ambiguous(slot: Slot) -> bool {
    slot & AMBIGUOUS != 0
}

/// Zero marks an empty slot, so it is not a fingerprint any key may have.
const fn fingerprint_from(hash: u64) -> u64 {
    match hash >> FINGERPRINT_SHIFT {
        0 => 1,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing in these tests shares a fingerprint, so the index answers on its own and this is never
    /// called. A test that needed it would be testing the ambiguous path, which has its own.
    fn never(_: BlockAddr) -> bool {
        panic!("the index read a record for an unambiguous key");
    }

    #[test]
    fn a_hold_comes_back_by_its_own_key_and_a_repoint_moves_it() {
        let mut table = HoldTable::default();
        let first = BlockAddr::new(0, 1, 2);
        table.insert_new(TxId(7), first).expect("room");
        assert_eq!(table.addr_of(TxId(7), &mut never), Some(first));
        assert_eq!(table.addr_of(TxId(8), &mut never), None);

        // What a partial resolution does: the record is appended again and the index follows it.
        let second = BlockAddr::new(0, 9, 3);
        assert!(table.repoint(TxId(7), second, &mut never));
        assert_eq!(table.len(), 1, "a repoint is not a second entry");
        assert_eq!(table.addr_of(TxId(7), &mut never), Some(second));

        assert_eq!(table.remove(TxId(7), &mut never), Some(second));
        assert_eq!(table.addr_of(TxId(7), &mut never), None);
        assert_eq!(table.remove(TxId(7), &mut never), None);
        assert_eq!(table.len(), 0);
    }

    /// The property the whole structure rests on: every key that went in comes back pointing at its own
    /// record, with a probe that never leaves two buckets. The table is sized up front — it does not
    /// grow — so this also says the sizing rule holds at the target load factor.
    #[test]
    fn a_table_filled_to_its_target_keeps_every_key() {
        let slots = 1 << 16;
        let holds = (slots as f64 * LOAD_TARGET) as u64;
        let mut table = HoldTable::with_slots(slots);
        let mut refused = 0;
        for key in 1..=holds {
            if table
                .insert_new(TxId(u128::from(key)), BlockAddr::from_raw(key))
                .is_err()
            {
                refused += 1;
            }
        }
        assert_eq!(
            refused, 0,
            "a table at its target load factor refused an insert"
        );
        assert_eq!(table.len(), holds as usize);
        for key in 1..=holds {
            assert_eq!(
                table.addr_of(TxId(u128::from(key)), &mut never),
                Some(BlockAddr::from_raw(key)),
                "key {key} points at someone else's record"
            );
        }
        let (_, worst) = table.kick_stats();
        assert!(
            worst < MAX_HOPS,
            "the cascade reached its cap, so the cap is the constraint"
        );
    }

    /// Two keys that share a fingerprint and a bucket. Sixteen bits make that findable by search, which
    /// is the point: correctness cannot rest on it being rare. The second insert marks both, and from
    /// then on the two are told apart by reading a record — and only those two.
    #[test]
    fn a_shared_fingerprint_is_detected_and_disambiguated() {
        // Big enough to hold the keys a clash takes: with sixty-five thousand buckets and sixteen-bit
        // fingerprints, a few hundred thousand keys are expected to turn up several.
        let mut table = HoldTable::with_slots(1 << 18);
        let mut pair = None;
        for step in 1..230_000u128 {
            let key = TxId(step);
            if !table.candidates(key).is_empty() {
                let other = table.candidates(key).address(0);
                pair = Some((TxId(u128::from(other.raw())), key));
                break;
            }
            table
                .insert_new(key, BlockAddr::from_raw(step as u64))
                .expect("room");
        }
        let (first, second) = pair.expect("no shared fingerprint found");
        assert!(
            table.ambiguous() == 0,
            "nothing marked before the clash was inserted"
        );

        table
            .insert_new(second, BlockAddr::from_raw(second.raw() as u64))
            .expect("room for the second of the pair");
        assert_eq!(table.ambiguous(), 1, "the clash was not noticed");

        // Each is found by reading a record, and each finds its own.
        let mut verify_first = |addr: BlockAddr| addr.raw() == first.raw() as u64;
        let mut verify_second = |addr: BlockAddr| addr.raw() == second.raw() as u64;
        assert_eq!(
            table.addr_of(first, &mut verify_first),
            Some(BlockAddr::from_raw(first.raw() as u64))
        );
        assert_eq!(
            table.addr_of(second, &mut verify_second),
            Some(BlockAddr::from_raw(second.raw() as u64))
        );

        // And removing one leaves the other where it was — the failure this whole mechanism prevents.
        table
            .remove(first, &mut verify_first)
            .expect("the first of the pair");
        assert_eq!(
            table.addr_of(second, &mut verify_second),
            Some(BlockAddr::from_raw(second.raw() as u64)),
            "removing one of a colliding pair took the other's slot"
        );
    }
}
