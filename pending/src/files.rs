use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{JoinHandle, Thread};

use ledger_base::{channel, Consumer, Producer};

use crate::block::{
    Block, DurableStore, ObjectId, StoreFault, VolumeStats, BLOCK_BYTES, OBJECT_VALUES,
};

/// The snapshot a restart reads, and the one being written. Two names because the replacement is a rename
/// (§19), and they are here rather than beside the dump because naming an object is the store's job — a
/// directory holding this volume's files holds these two the same way it holds a day's.
const CURRENT_SNAPSHOT: &str = "pending.snapshot";
const PARTIAL_SNAPSHOT: &str = "pending.snapshot.part";

/// A read handed to a pool thread, and the same buffer handed back with it.
///
/// The buffer travels as a `Box`, so what moves through the queue is a pointer and not four kilobytes — and it
/// comes back down with the next ask, which is what makes the steady state allocation-free. Sharing one slot
/// array between the worker and the threads instead would need `unsafe`, which rule 7 does not allow here.
struct Ask {
    handle: u64,
    /// Shared rather than borrowed: `read_at` takes `&self`, so a file needs no lock to be read by several
    /// threads at once, and an `Arc` clone is two atomics against a read that is about to touch a device.
    file: Arc<File>,
    offset: u64,
    buffer: Box<Block>,
}

struct Done {
    handle: u64,
    buffer: Box<Block>,
    read: Result<usize, ()>,
}

/// One thread's two queues and the handle to wake it.
struct Lane {
    asks: Producer<Ask>,
    dones: Consumer<Done>,
    thread: Thread,
    outstanding: usize,
}

/// `pread` on N threads: the portable backend of the design's three, and the one this machine can run.
///
/// **Why N queue pairs rather than one queue.** `base`'s ring is single-producer single-consumer, and one
/// shared request queue read by several threads is neither. A pair per thread keeps every queue SPSC — the one
/// lock-free structure here that is measured and tested — and costs a round-robin instead of a lock. Nothing
/// knows which thread will be free first, and asking would cost more than the imbalance.
///
/// **How many threads.** Little's law on the *store read* rate, which is the share of lookups that miss both
/// memory windows rather than the lookup rate itself: threads ≈ reads a second × the latency of one. The
/// design's figures are 0.5ms and a miss rate that leaves tens of thousands a second, which is where its
/// sixteen comes from. A configuration that forces every read to miss — `--residency 1` with a resolve age —
/// needs about two hundred at the same latency, and sixteen threads should visibly fail to keep up with it.
/// That is a check on the arithmetic rather than a problem with the pool.
struct ReadPool {
    lanes: Vec<Lane>,
    next: usize,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    /// Buffers not currently with a thread. Bounded by the depth the pool was built for.
    spare: Vec<Box<Block>>,
}

impl ReadPool {
    fn new(threads: usize, depth: usize) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let mut lanes = Vec::with_capacity(threads);
        let mut handles = Vec::with_capacity(threads);
        // At least one slot per thread, and the depth spread over them.
        let per_lane = depth.div_ceil(threads).max(1);
        for index in 0..threads {
            let (asks, ask_rx) = channel::<Ask>(per_lane);
            let (done_tx, dones) = channel::<Done>(per_lane);
            let stopping = Arc::clone(&stop);
            let handle = std::thread::Builder::new()
                .name(format!("pending-read-{index}"))
                .spawn(move || Self::serve(ask_rx, done_tx, stopping))
                .expect("a read thread");
            lanes.push(Lane {
                asks,
                dones,
                thread: handle.thread().clone(),
                outstanding: 0,
            });
            handles.push(handle);
        }
        Self {
            lanes,
            next: 0,
            stop,
            threads: handles,
            spare: (0..depth.max(1)).map(|_| Block::zeroed()).collect(),
        }
    }

    /// One thread: take a read, do it, hand it back. It parks rather than spinning, which is the right thing
    /// on this side of the queue — a thread about to block in `pread` has no reason to burn a core waiting for
    /// work, and the *worker* never blocks because `unpark` takes no lock it can contend on.
    fn serve(asks: Consumer<Ask>, dones: Producer<Done>, stop: Arc<AtomicBool>) {
        while !stop.load(Ordering::Relaxed) {
            let Some(mut ask) = asks.pop() else {
                // Parked with no timeout, and that needs no lost-wakeup guard: `unpark` leaves a token when it
                // arrives before the `park`, so a thread that was about to sleep returns at once instead. A
                // timeout here was a self-inflicted cost — every idle thread waking twenty thousand times a
                // second, which is scheduler churn against the four cores this machine has for the reactor.
                std::thread::park();
                continue;
            };
            let read = ask
                .file
                .read_at(&mut ask.buffer, ask.offset)
                .map_err(|_| ());
            let mut done = Done {
                handle: ask.handle,
                buffer: ask.buffer,
                read,
            };
            // Cannot overflow — the queues are as deep as the reads outstanding on this lane — but a push
            // that failed would drop a completion the caller is waiting for, so it is retried rather than
            // trusted.
            while let Err(returned) = dones.push(done) {
                done = returned;
                std::thread::yield_now();
            }
        }
    }

    fn submit(&mut self, handle: u64, file: Arc<File>, offset: u64) -> bool {
        let Some(buffer) = self.spare.pop() else {
            return false;
        };
        let lane = self.next % self.lanes.len();
        self.next = self.next.wrapping_add(1);
        let ask = Ask {
            handle,
            file,
            offset,
            buffer,
        };
        match self.lanes[lane].asks.push(ask) {
            Ok(()) => {
                self.lanes[lane].outstanding += 1;
                self.lanes[lane].thread.unpark();
                true
            }
            Err(refused) => {
                self.spare.push(refused.buffer);
                false
            }
        }
    }

    /// The next completion any thread has finished. Round-robin from where the last one came, so no lane is
    /// starved by a busier neighbour.
    fn poll(&mut self, into: &mut Block) -> Option<Result<u64, StoreFault>> {
        for step in 0..self.lanes.len() {
            let lane = (self.next + step) % self.lanes.len();
            let Some(done) = self.lanes[lane].dones.pop() else {
                continue;
            };
            self.lanes[lane].outstanding -= 1;
            let answer = match done.read {
                Err(()) => Err(StoreFault::Device),
                Ok(read) if read == into.len() => {
                    into.copy_from_slice(&done.buffer);
                    Ok(done.handle)
                }
                // A short read is the offset being past what the file holds, which is this node's own record
                // of where blocks are disagreeing with the store.
                Ok(_) => Err(StoreFault::Missing),
            };
            self.spare.push(done.buffer);
            return Some(answer);
        }
        None
    }

    fn outstanding(&self) -> usize {
        self.lanes.iter().map(|lane| lane.outstanding).sum()
    }
}

impl Drop for ReadPool {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for lane in &self.lanes {
            lane.thread.unpark();
        }
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// One write, on its way to the lane. The buffer travels as a `Box` and comes back with the completion, so
/// what moves through the queue is a pointer rather than four kilobytes and the steady state allocates
/// nothing.
struct WriteAsk {
    handle: u64,
    op: LaneOp,
    buffer: Box<Block>,
}

/// What the lane can be asked for. **Everything that changes the volume**, in one queue, because the
/// order between them is what the queue is for: a write must follow the creation of the object it goes
/// in, a barrier must follow the writes it claims to cover, and a removal or a rename must not overtake
/// the writes to the object it renames or removes.
#[derive(Clone, Copy)]
enum LaneOp {
    Write {
        object: ObjectId,
        offset: u64,
        creating: bool,
    },
    /// Everything asked for before it is made durable.
    Barrier,
    Remove(ObjectId),
    Rename(ObjectId, ObjectId),
}

struct WriteDone {
    handle: u64,
    buffer: Box<Block>,
    ok: bool,
}

/// `pwrite` and `fsync` on **one** thread, in the order they were asked for.
///
/// **One thread, not a pool, and that is the difference from the read side.** Reads commute; writes do not.
/// A segment's first block brings the segment into being, so it has to land before the ones after it, and a
/// barrier has to follow every write it claims to have covered — coverage rests on that (§15), and a barrier
/// that overtook a write would name blocks a restart cannot read. One thread serving one queue keeps both
/// orders for free. Design notes §20.
///
/// The lane owns the files it writes, and the directory: a barrier is the file's bytes and then the
/// directory's entries, so both have to be on the side that issues it. The main side opens its own handles
/// for reading, which costs a descriptor and buys single ownership on each side.
struct WriteLane {
    asks: Producer<WriteAsk>,
    dones: Consumer<WriteDone>,
    thread: Thread,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// Buffers not currently with the thread. Bounded by the depth the lane was built for.
    spare: Vec<Box<Block>>,
    outstanding: usize,
}

/// What the lane thread owns while it runs.
struct LaneFiles {
    dir: File,
    path: PathBuf,
    files: [Option<File>; OBJECT_VALUES],
    /// Objects written since the last barrier, one bit each, and whether a file came into being — a newly
    /// created file's *name* is not durable until the directory is synced.
    dirty: u128,
    created: bool,
}

impl LaneFiles {
    /// **A cached handle is invalidated by the next `creating` write and by nothing else**, which is what
    /// makes caching them safe beside a `remove` and a `rename` the other side of the store issues. Both of
    /// those leave the name free, and every object is brought into being again by a write that says it is
    /// creating: a day's first block after `free_segment`, a dump's first chunk after the last one was
    /// published or given up on. So the stale handle here is replaced before it can be written through.
    fn write(&mut self, object: ObjectId, offset: u64, creating: bool, block: &Block) -> bool {
        if creating {
            // `create_new` is `O_EXCL`, and it failing is information rather than an inconvenience: a
            // segment's file already existing means a previous life left it there, and writing over its
            // front would leave a mix of two days that nothing points into and nothing frees (§16).
            let Ok(file) = FileStore::options()
                .create_new(true)
                .open(FileStore::name_in(&self.path, object))
            else {
                return false;
            };
            self.files[object.index()] = Some(file);
            self.created = true;
        }
        if self.files[object.index()].is_none() {
            let Ok(file) = FileStore::options().open(FileStore::name_in(&self.path, object)) else {
                return false;
            };
            self.files[object.index()] = Some(file);
        }
        let Some(file) = self.files[object.index()].as_ref() else {
            return false;
        };
        if file.write_at(block, offset).is_err() {
            return false;
        }
        self.dirty |= 1 << object.index();
        true
    }

    fn barrier(&mut self) -> bool {
        for object in 0..OBJECT_VALUES {
            if self.dirty & (1 << object) == 0 {
                continue;
            }
            let Some(file) = self.files[object].as_ref() else {
                continue;
            };
            if file.sync_all().is_err() {
                return false;
            }
        }
        if self.created && self.dir.sync_all().is_err() {
            return false;
        }
        self.dirty = 0;
        self.created = false;
        true
    }

    /// Already gone is the state this is asking for, so it is not a failure.
    fn remove(&mut self, object: ObjectId) -> bool {
        self.files[object.index()] = None;
        let name = FileStore::name_in(&self.path, object);
        match std::fs::remove_file(&name) {
            Ok(()) => true,
            Err(err) => err.kind() == std::io::ErrorKind::NotFound,
        }
    }

    /// The name, then the directory: a name is not durable until the directory holding it is, and that
    /// `fsync` is why this belongs on the lane rather than on the thread that answers lookups.
    fn rename(&mut self, from: ObjectId, to: ObjectId) -> bool {
        if std::fs::rename(
            FileStore::name_in(&self.path, from),
            FileStore::name_in(&self.path, to),
        )
        .is_err()
        {
            return false;
        }
        self.files[from.index()] = None;
        self.files[to.index()] = None;
        self.dir.sync_all().is_ok()
    }
}

impl WriteLane {
    fn new(dir: File, path: PathBuf, depth: usize) -> Self {
        let (asks, ask_rx) = channel::<WriteAsk>(depth.max(1));
        let (done_tx, dones) = channel::<WriteDone>(depth.max(1));
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("pending-write".to_owned())
            .spawn(move || {
                let mut owned = LaneFiles {
                    dir,
                    path,
                    files: std::array::from_fn(|_| None),
                    dirty: 0,
                    created: false,
                };
                Self::serve(ask_rx, done_tx, stopping, &mut owned)
            })
            .expect("a write thread");
        Self {
            asks,
            dones,
            thread: handle.thread().clone(),
            stop,
            handle: Some(handle),
            spare: (0..depth.max(1)).map(|_| Block::zeroed()).collect(),
            outstanding: 0,
        }
    }

    /// Take one, do it, hand it back. Parked rather than spinning, for the same reason the read threads are:
    /// a thread about to block in `pwrite` has no reason to burn a core waiting for work, and `unpark` leaves
    /// a token so there is no lost wakeup to guard against.
    fn serve(
        asks: Consumer<WriteAsk>,
        dones: Producer<WriteDone>,
        stop: Arc<AtomicBool>,
        owned: &mut LaneFiles,
    ) {
        while !stop.load(Ordering::Relaxed) {
            let Some(ask) = asks.pop() else {
                std::thread::park();
                continue;
            };
            let ok = match ask.op {
                LaneOp::Write {
                    object,
                    offset,
                    creating,
                } => owned.write(object, offset, creating, &ask.buffer),
                LaneOp::Barrier => owned.barrier(),
                LaneOp::Remove(object) => owned.remove(object),
                LaneOp::Rename(from, to) => owned.rename(from, to),
            };
            let mut done = WriteDone {
                handle: ask.handle,
                buffer: ask.buffer,
                ok,
            };
            // Cannot overflow — the queues are as deep as what can be outstanding — but a push that failed
            // would drop a completion the caller is waiting on for ever, so it is retried rather than
            // trusted.
            while let Err(returned) = dones.push(done) {
                done = returned;
                std::thread::yield_now();
            }
        }
    }

    fn submit(&mut self, handle: u64, op: LaneOp, block: Option<&Block>) -> bool {
        let Some(mut buffer) = self.spare.pop() else {
            return false;
        };
        if let Some(block) = block {
            buffer.copy_from_slice(block);
        }
        let ask = WriteAsk { handle, op, buffer };
        match self.asks.push(ask) {
            Ok(()) => {
                self.outstanding += 1;
                self.thread.unpark();
                true
            }
            Err(refused) => {
                self.spare.push(refused.buffer);
                false
            }
        }
    }

    fn poll(&mut self) -> Option<(u64, Result<(), StoreFault>)> {
        let done = self.dones.pop()?;
        self.outstanding -= 1;
        self.spare.push(done.buffer);
        Some((
            done.handle,
            if done.ok {
                Ok(())
            } else {
                Err(StoreFault::Device)
            },
        ))
    }
}

impl Drop for WriteLane {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.unpark();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// One file per segment, in one directory. The backing a deployment has, against `MemoryStore`'s which
/// nothing survives.
///
/// **No `unsafe`, and that is not a happy accident** — rule 7 confines it to `base::spsc`. Everything a real
/// store needs has a safe form in `std`'s unix extensions: `FileExt::{read_at, write_at}` are `pread` and
/// `pwrite`, `File::sync_all` is `fsync`, opening the directory and syncing that is how a newly created file's
/// name is made durable, and `remove_file` is `unlink`. The only thing that comes from `libc` is the value of
/// `O_DIRECT`, handed to `custom_flags`.
///
/// **Direct IO, because the residency window is already this engine's cache.** The page cache would be a
/// second copy of it, so reads and writes go past it — which requires the offset, the length and the buffer
/// *address* to be block-aligned. The first two are whole blocks by construction and the third is why `Block`
/// is aligned to its own size. On macOS there is no `O_DIRECT`; `F_NOCACHE` is the equivalent and needs
/// `fcntl`, which would need `unsafe`, so it is not used and a run here is not a device's numbers (§16).
pub struct FileStore {
    /// Open for its own sake: a file that has just been created is not durable until its directory is
    /// synced, and syncing a directory means having it open.
    dir: File,
    path: PathBuf,
    /// Shared so a pool thread can read one without a lock: `read_at` takes `&self`.
    files: [Option<Arc<File>>; OBJECT_VALUES],
    /// Objects written to since the last sync, one bit each. At most three are ever set — the day being
    /// written, for one block the day before it, and a snapshot being dumped onto this volume.
    dirty: u128,
    /// Whether a file has come into being since the last sync, so the directory needs one too.
    created: bool,
    /// Reads asked for and not yet answered, when there is no pool. The `pread` then happens in `poll`, so
    /// nothing overlaps: the simplest correct implementation and the slowest, and the baseline any backend is
    /// measured against.
    submitted: VecDeque<(u64, ObjectId, u64)>,
    /// `pread` on N threads, absent when the pool is zero threads wide.
    ///
    /// A field rather than a trait, for now. The design abstracts the read backend and names three — io_uring,
    /// libaio, a thread pool — and this is the third; the trait arrives with the second implementation, not
    /// before it (rule 4). It lives *inside* the backing rather than above it because io_uring cannot be
    /// anywhere else: it owns the descriptors and issues the reads itself, so a decorator over a synchronous
    /// `read_at` could never become one.
    pool: Option<ReadPool>,
    queue_depth: usize,
    /// Writes and barriers taken and not yet collected, when there is no lane. Bounded by `queue_depth`,
    /// which is what turns a backing that cannot keep up into backpressure rather than a queue that grows
    /// (rule 12).
    written: VecDeque<(u64, Result<(), StoreFault>)>,
    /// `pwrite` and `fsync` on a thread of their own, absent when the lane was not asked for.
    ///
    /// **Zero threads is kept, and it is not a leftover.** It is the synchronous baseline every number is
    /// compared against — the same role `--store-read-threads 0` plays for reads — and it is what a virtual
    /// clock can run, since a real thread underneath a simulated one measures neither. Design notes §20.
    lane: Option<WriteLane>,
    stats: VolumeStats,
}

impl FileStore {
    /// Takes the directory already opened by whoever validated it, so nothing here can fail at start-up on a
    /// thread that has no way to report it (rule 6).
    pub fn new(
        dir: File,
        path: PathBuf,
        queue_depth: usize,
        read_threads: usize,
        write_lane: bool,
    ) -> Self {
        let queue_depth = queue_depth.max(1);
        let lane = write_lane.then(|| {
            let owned = File::open(&path).expect("the directory this store was opened on");
            WriteLane::new(owned, path.clone(), queue_depth)
        });
        Self {
            dir,
            path,
            files: std::array::from_fn(|_| None),
            dirty: 0,
            created: false,
            submitted: VecDeque::new(),
            pool: (read_threads > 0).then(|| ReadPool::new(read_threads, queue_depth)),
            queue_depth,
            written: VecDeque::new(),
            lane,
            stats: VolumeStats::default(),
        }
    }

    /// The name an object's file has, and the whole of this store's namespace. A day is its segment number
    /// and nothing else — the offset within the file is a function of the address (§16), so a name carrying a
    /// block number would be a second place the layout lived — and the snapshot's two are the fixed names
    /// §19's replacement-by-rename needs.
    ///
    /// An associated function on the path rather than a method, because the write lane owns a path of its
    /// own and must name a file exactly as this side does.
    pub(crate) fn name_in(path: &Path, object: ObjectId) -> PathBuf {
        match object {
            ObjectId::SNAPSHOT_CURRENT => path.join(CURRENT_SNAPSHOT),
            ObjectId::SNAPSHOT_PARTIAL => path.join(PARTIAL_SNAPSHOT),
            day => path.join(format!("seg-{:02}.blk", day.index())),
        }
    }

    fn name_of(&self, object: ObjectId) -> PathBuf {
        Self::name_in(&self.path, object)
    }

    /// The object's file, opened if this store has not touched it yet. Lazy because a restart begins with no
    /// open files and the blocks are still there: the index a snapshot restored names them, and the offset
    /// needs nothing but the address.
    fn file_of(&mut self, object: ObjectId) -> Result<Arc<File>, StoreFault> {
        if self.files[object.index()].is_none() {
            let opened = Self::options()
                .open(self.name_of(object))
                .map_err(|_| StoreFault::Missing)?;
            self.files[object.index()] = Some(Arc::new(opened));
        }
        self.files[object.index()]
            .as_ref()
            .cloned()
            .ok_or(StoreFault::Missing)
    }

    pub(crate) fn options() -> OpenOptions {
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_DIRECT);
        }
        options
    }

    fn note_write(&mut self, object: ObjectId) {
        self.dirty |= 1 << object.index();
    }

    /// The `pread` itself. **Counting is the caller's**, because the same work is an inline read when
    /// `read_at` asks for it and the queue's when `poll` does, and a method that counted both would count
    /// one of them twice.
    fn read_block(
        &mut self,
        object: ObjectId,
        offset: u64,
        into: &mut Block,
    ) -> Result<(), StoreFault> {
        let read = self
            .file_of(object)?
            .read_at(into, offset)
            .map_err(|_| StoreFault::Device)?;
        // A short read is the offset being past what the file holds, which is this node's own record of where
        // blocks are disagreeing with the store rather than the device refusing.
        if read == into.len() {
            Ok(())
        } else {
            Err(StoreFault::Missing)
        }
    }

    fn rename_now(&mut self, from: ObjectId, to: ObjectId) -> Result<(), StoreFault> {
        std::fs::rename(self.name_of(from), self.name_of(to)).map_err(|_| StoreFault::Device)?;
        self.files[from.index()] = None;
        self.files[to.index()] = None;
        self.dir.sync_all().map_err(|_| StoreFault::Device)?;
        Ok(())
    }
}

impl FileStore {
    /// `create_new` is `O_EXCL`, and it failing is information rather than an inconvenience: a segment's file
    /// already existing means a previous life left it there, and writing over its front would leave a mix of
    /// two days that nothing points into and nothing frees. So it refuses, which seals — until there is a
    /// start-up reconcile, this is what stands in for one (§16).
    fn open_with(
        &mut self,
        object: ObjectId,
        offset: u64,
        block: &Block,
    ) -> Result<(), StoreFault> {
        let file = Self::options()
            .create_new(true)
            .open(self.name_of(object))
            .map_err(|_| StoreFault::Device)?;
        file.write_at(block, offset)
            .map_err(|_| StoreFault::Device)?;
        self.files[object.index()] = Some(Arc::new(file));
        self.created = true;
        self.note_write(object);
        Ok(())
    }

    fn append(&mut self, object: ObjectId, offset: u64, block: &Block) -> Result<(), StoreFault> {
        self.file_of(object)?
            .write_at(block, offset)
            .map_err(|_| StoreFault::Device)?;
        self.note_write(object);
        Ok(())
    }

    /// The file's bytes, then the directory's entries, and that order is why the barrier takes no segment:
    /// on a filesystem a block can be durable inside a file whose *name* is not, so durability is a fact
    /// about the store at a moment rather than a watermark per segment (§16).
    ///
    /// `sync_all` rather than `sync_data` because appending changes the file's length, and a length is
    /// metadata: `fdatasync` does not promise it.
    fn barrier(&mut self) -> Result<(), StoreFault> {
        for object in 0..OBJECT_VALUES {
            if self.dirty & (1 << object) == 0 {
                continue;
            }
            let Some(file) = self.files[object].as_ref() else {
                continue;
            };
            file.sync_all().map_err(|_| StoreFault::Device)?;
        }
        if self.created {
            self.dir.sync_all().map_err(|_| StoreFault::Device)?;
        }
        self.dirty = 0;
        self.created = false;
        Ok(())
    }
}

impl DurableStore for FileStore {
    /// Done here and answered from `poll_written`, which is the synchronous baseline the way zero read
    /// threads is: the `pwrite` still happens on the caller's thread. What the shape buys is that the lane
    /// §20 asks for replaces the body of this method and nothing above it changes.
    fn submit_write(
        &mut self,
        handle: u64,
        object: ObjectId,
        offset: u64,
        block: &Block,
        creating: bool,
        _now: u64,
    ) -> bool {
        if let Some(lane) = self.lane.as_mut() {
            let taken = lane.submit(
                handle,
                LaneOp::Write {
                    object,
                    offset,
                    creating,
                },
                Some(block),
            );
            match taken {
                true => self.stats.took_write(block.len(), self.writes_inflight()),
                false => self.stats.writes_refused += 1,
            }
            return taken;
        }
        if self.written.len() >= self.queue_depth {
            self.stats.writes_refused += 1;
            return false;
        }
        self.stats.took_write(block.len(), self.written.len() + 1);
        let done = if creating {
            self.open_with(object, offset, block)
        } else {
            self.append(object, offset, block)
        };
        self.stats.answered_write(done.is_ok());
        self.written.push_back((handle, done));
        true
    }

    fn submit_barrier(&mut self, handle: u64, _now: u64) -> bool {
        if let Some(lane) = self.lane.as_mut() {
            let taken = lane.submit(handle, LaneOp::Barrier, None);
            match taken {
                true => self.stats.took_barrier(self.writes_inflight()),
                false => self.stats.writes_refused += 1,
            }
            return taken;
        }
        if self.written.len() >= self.queue_depth {
            self.stats.writes_refused += 1;
            return false;
        }
        self.stats.took_barrier(self.written.len() + 1);
        let done = self.barrier();
        self.stats.answered_write(done.is_ok());
        self.written.push_back((handle, done));
        true
    }

    fn poll_written(&mut self, _now: u64) -> Option<(u64, Result<(), StoreFault>)> {
        let answered = match self.lane.as_mut() {
            Some(lane) => lane.poll(),
            None => self.written.pop_front(),
        };
        // The lane answers on its own thread, so a completion is where its outcome is first seen here.
        if let Some((_, outcome)) = answered.as_ref() {
            if self.lane.is_some() {
                self.stats.answered_write(outcome.is_ok());
            }
        }
        answered
    }

    /// The lane is the whole of the answer: with one, a write is handed to a thread and the caller goes
    /// on; without one, the `pwrite` happens right here.
    fn writes_are_queued(&self) -> bool {
        self.lane.is_some()
    }

    fn stats(&self) -> VolumeStats {
        self.stats
    }

    fn writes_inflight(&self) -> usize {
        match self.lane.as_ref() {
            Some(lane) => lane.outstanding,
            None => self.written.len(),
        }
    }

    fn read_at(
        &mut self,
        object: ObjectId,
        offset: u64,
        into: &mut Block,
    ) -> Result<(), StoreFault> {
        let read = self
            .file_of(object)?
            .read_at(into, offset)
            .map_err(|_| StoreFault::Device)?;
        // A short read is the offset being past what the file holds, which is this node's own record of where
        // blocks are disagreeing with the store rather than the device refusing.
        if read == into.len() {
            Ok(())
        } else {
            Err(StoreFault::Missing)
        }
    }

    fn submit(&mut self, handle: u64, object: ObjectId, offset: u64, _now: u64) -> bool {
        if self.pool.is_some() {
            // An object with no file is a read this cannot take at all, which is backpressure's answer rather
            // than a fault's: nothing has been promised, so the caller keeps the command and asks again.
            let Ok(file) = self.file_of(object) else {
                self.stats.reads_refused += 1;
                return false;
            };
            let pool = self.pool.as_mut().expect("just checked");
            let taken = pool.submit(handle, file, offset);
            match taken {
                true => {
                    let depth = self.inflight();
                    self.stats.took_read(depth);
                }
                false => self.stats.reads_refused += 1,
            }
            return taken;
        }
        if self.submitted.len() >= self.queue_depth {
            self.stats.reads_refused += 1;
            return false;
        }
        self.stats.took_read(self.submitted.len() + 1);
        self.submitted.push_back((handle, object, offset));
        true
    }

    fn poll(&mut self, _now: u64, into: &mut Block) -> Option<Result<u64, StoreFault>> {
        if let Some(pool) = self.pool.as_mut() {
            let answered = pool.poll(into);
            if let Some(outcome) = answered.as_ref() {
                self.stats.answered_read(outcome.is_ok());
            }
            return answered;
        }
        let (handle, object, offset) = self.submitted.pop_front()?;
        let answered = self.read_block(object, offset, into).map(|()| handle);
        self.stats.answered_read(answered.is_ok());
        Some(answered)
    }

    fn inflight(&self) -> usize {
        match self.pool.as_ref() {
            Some(pool) => pool.outstanding(),
            None => self.submitted.len(),
        }
    }

    /// Closed and unlinked. The unlink itself is not synced, and does not need to be: `reclaim` uses no clock
    /// and no cursor, so a file that comes back after a crash is found and freed again on the next pass.
    /// On the lane where there is one, so it takes its turn behind the writes to the object it removes.
    /// Where there is not, it happens here and is answered from the same queue the writes are.
    fn submit_remove(&mut self, handle: u64, object: ObjectId, _now: u64) -> bool {
        self.stats.removes += 1;
        if let Some(lane) = self.lane.as_mut() {
            return lane.submit(handle, LaneOp::Remove(object), None);
        }
        if self.written.len() >= self.queue_depth {
            self.stats.writes_refused += 1;
            return false;
        }
        self.files[object.index()] = None;
        let done = match std::fs::remove_file(self.name_of(object)) {
            Ok(()) => Ok(()),
            // Already gone is the state this is asking for.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(StoreFault::Device),
        };
        self.written.push_back((handle, done));
        true
    }

    /// `rename` and then the directory, because the name is the thing being published and a name is
    /// durable only once the directory holding it is. The bytes are already durable — the caller's barrier
    /// is what made them so, and the queue is what keeps this behind it.
    ///
    /// Both cached handles go: the one for `to` would otherwise still be the file the name was taken
    /// from, and a read of the published object would answer with the snapshot before it.
    fn submit_rename(&mut self, handle: u64, from: ObjectId, to: ObjectId, _now: u64) -> bool {
        self.stats.renames += 1;
        if let Some(lane) = self.lane.as_mut() {
            return lane.submit(handle, LaneOp::Rename(from, to), None);
        }
        if self.written.len() >= self.queue_depth {
            self.stats.writes_refused += 1;
            return false;
        }
        let done = self.rename_now(from, to);
        self.written.push_back((handle, done));
        true
    }

    /// The file's length, which is the whole of the answer: offsets are absolute, so a segment's file ends
    /// where its last block does and the filesystem has been keeping that all along.
    fn blocks_in(&mut self, object: ObjectId) -> u64 {
        std::fs::metadata(self.name_of(object))
            .map(|at| at.len() / BLOCK_BYTES as u64)
            .unwrap_or(0)
    }

    fn exists(&mut self, object: ObjectId) -> bool {
        self.files[object.index()].is_some() || self.name_of(object).exists()
    }
}

/// A directory a `FileStore` may be built on, opened and checked before anything is spawned.
///
/// Opened here rather than on the worker's thread because a directory that cannot be used is a configuration
/// error, and a configuration error has to be refused at start-up where somebody can be told — not swallowed
/// by a thread whose only way to report it would be to panic (rule 6).
/// Canonical, because the path is what says whether two backings are one volume: two spellings of one
/// directory are one disk and have to compare equal (`OpenBacking::same_volume`).
pub fn open_directory(path: &Path) -> Result<(File, PathBuf), std::io::Error> {
    std::fs::create_dir_all(path)?;
    let path = std::fs::canonicalize(path)?;
    let dir = File::open(&path)?;
    Ok((dir, path))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use ledger_base::ports::{ApplyIndex, HoldData};
    use ledger_base::{AccountId, Amount, BudgetGroup, TxId};

    use super::*;
    use crate::block::{RecordLog, RECORDS_PER_BLOCK};

    /// A directory of its own per test, removed with it. Named from the process and a counter rather than a
    /// random number, because a test that could collide with another run of itself is one that fails for a
    /// reason nobody will find.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ledger-files-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("a scratch directory");
            Self(path)
        }

        /// Synchronous by default: a test about the format and the offsets should not depend on threads, and
        /// the one test that is about the pool asks for it.
        fn store(&self) -> Box<FileStore> {
            self.store_with(0)
        }

        fn store_with(&self, read_threads: usize) -> Box<FileStore> {
            self.store_lane(read_threads, false)
        }

        fn store_lane(&self, read_threads: usize, write_lane: bool) -> Box<FileStore> {
            let (dir, path) = open_directory(&self.0).expect("the scratch directory opens");
            Box::new(FileStore::new(dir, path, 32, read_threads, write_lane))
        }

        fn files(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.0)
                .expect("the scratch directory reads")
                .map(|entry| {
                    entry
                        .expect("an entry")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn hold(amount: Amount) -> HoldData {
        HoldData {
            debit_account: AccountId(11),
            credit_account: AccountId(22),
            amount,
            remaining: amount,
            ledger: 3,
            budget: BudgetGroup::ABSENT,
            budget_members: 0,
            budget_remaining: 0,
        }
    }

    /// Fills the buffer past its window and carries survivors on, which is what seals blocks to the store.
    /// Answers the addresses those survivors landed at.
    fn seal_blocks(log: &mut RecordLog, records: usize) -> Vec<(TxId, RecordAddrs)> {
        for index in 0..records {
            log.append(TxId(index as u128 + 1), &hold(100), ApplyIndex(1));
        }
        let mut kept = Vec::new();
        for index in 0..records {
            let key = TxId(index as u128 + 1);
            kept.push((key, log.keep(key, &hold(100), ApplyIndex(1))));
        }
        kept
    }

    type RecordAddrs = crate::block::RecordAddr;

    /// Every record written comes back, field for field, from files rather than from memory. The same claim
    /// `MemoryStore` carries, against the backing that has to hold it.
    #[test]
    fn every_record_comes_back_from_the_files_it_was_written_to() {
        let scratch = Scratch::new();
        // No residency, so a sealed block is only on disk and a read has to go there.
        let mut log = RecordLog::new(scratch.store(), 1, 0);
        let kept = seal_blocks(&mut log, RECORDS_PER_BLOCK * 3);
        log.sync(0);
        log.collect_writes(0);

        let mut read = 0;
        for (key, addr) in &kept {
            if log.try_read(*addr).is_some() {
                continue;
            }
            let (found, back) = log.read(*addr).expect("a record the store was given");
            assert_eq!(found, *key, "the wrong record came back from {addr:?}");
            assert_eq!(back.amount, 100);
            read += 1;
        }
        assert!(
            read > 0,
            "nothing was read from the files, so this proved nothing"
        );
        assert_eq!(log.traffic().store_faults, 0);
        assert_eq!(log.traffic().store_corruptions, 0);
    }

    /// The pool answers the same records the synchronous path does, and answers them out of the order they
    /// were asked in. Both halves matter: the first is that concurrency changed nothing about correctness, and
    /// the second is that it is concurrency at all — a pool whose completions came back in order would be the
    /// synchronous path with extra threads.
    #[test]
    fn a_read_pool_answers_the_same_records_and_not_in_the_order_asked() {
        let scratch = Scratch::new();
        // Written synchronously, so what the pool is being asked about is only the reading.
        let addrs = {
            let mut log = RecordLog::new(scratch.store(), 1, 0);
            // Twenty blocks, because only the first record of each is read here — one read per block is what
            // makes the completions distinguishable.
            let kept = seal_blocks(&mut log, RECORDS_PER_BLOCK * 20);
            log.sync(0);
            log.collect_writes(0);
            kept
        };
        let mut store = scratch.store_with(4);
        let sealed: Vec<_> = addrs
            .iter()
            .filter(|(_, addr)| addr.index() == 0)
            .take(16)
            .collect();
        assert!(sealed.len() > 4, "not enough sealed blocks to interleave");

        for (handle, (_, addr)) in sealed.iter().enumerate() {
            assert!(
                store.submit(
                    handle as u64,
                    ObjectId::segment(addr.segment()),
                    addr.block_offset(),
                    0
                ),
                "the pool would not take read {handle}"
            );
        }
        let mut order = Vec::new();
        let mut into = Block::zeroed();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while order.len() < sealed.len() {
            assert!(
                std::time::Instant::now() < deadline,
                "the pool answered {} of {} reads",
                order.len(),
                sealed.len()
            );
            let Some(answered) = store.poll(0, &mut into) else {
                std::thread::yield_now();
                continue;
            };
            let handle = answered.expect("a read of a block the store has");
            let (key, _) = crate::block::decode(&into[..crate::block::RECORD_BYTES], sealed[0].1);
            assert_eq!(
                key, sealed[handle as usize].0,
                "read {handle} came back with another block's record"
            );
            order.push(handle);
        }
        assert_ne!(
            order,
            (0..sealed.len() as u64).collect::<Vec<_>>(),
            "every completion came back in the order it was asked for, so nothing overlapped"
        );
    }

    /// The write lane answers the same records the synchronous path does, and the barrier that follows a
    /// write really does make it durable — through a thread rather than on the caller's.
    ///
    /// **What this is for is the ordering, not the speed.** A segment's first block brings the segment into
    /// being, so it has to land before the ones after it, and a barrier has to follow every write it claims
    /// to cover. One thread serving one queue is what keeps both; a pool would keep neither. So the test
    /// submits a segment's blocks in order across a barrier and asks for every record back.
    #[test]
    fn a_write_lane_keeps_the_order_writes_need_and_the_records_come_back() {
        let scratch = Scratch::new();
        let addrs = {
            let mut log = RecordLog::new(scratch.store_lane(0, true), 1, 0);
            let kept = seal_blocks(&mut log, RECORDS_PER_BLOCK * 4);
            log.submit_writes(0);
            log.sync(0);
            // The lane is a thread, so the answers arrive when it has done them rather than when they were
            // asked for. Nothing else in this test may proceed until they have.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while log.writes_outstanding() > 0 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the lane left {} writes unanswered",
                    log.writes_outstanding()
                );
                log.collect_writes(0);
                std::thread::yield_now();
            }
            log.collect_writes(0);
            assert_eq!(log.traffic().store_faults, 0, "the lane refused a write");
            kept
        };

        // A second store over the same directory, reading what the lane wrote — which is only possible if
        // every block landed and the segment was brought into being by the first of them.
        let mut restarted = RecordLog::new(scratch.store(), 1, 0);
        let mut read = 0;
        for (key, addr) in &addrs {
            let Some((found, back)) = restarted.read(*addr) else {
                continue;
            };
            assert_eq!(found, *key, "the wrong record came back from {addr:?}");
            assert_eq!(back.amount, 100);
            read += 1;
        }
        assert!(
            read >= RECORDS_PER_BLOCK,
            "the lane wrote {read} records that could be read back, so the order or the barrier did not hold"
        );
        assert_eq!(restarted.traffic().store_corruptions, 0);
    }

    /// **The test the absolute offset was chosen for.** A second store over the same directory reads the
    /// blocks the first wrote, knowing nothing but the addresses — no range restored, no directory scanned, no
    /// superblock. §16 argued it; this is the argument being run.
    #[test]
    fn a_second_store_over_the_same_directory_reads_what_the_first_wrote() {
        let scratch = Scratch::new();
        let kept = {
            let mut log = RecordLog::new(scratch.store(), 1, 0);
            let kept = seal_blocks(&mut log, RECORDS_PER_BLOCK * 3);
            log.sync(0);
            log.collect_writes(0);
            kept
        };

        // A fresh log over the same files: its own ranges are empty and its buffer holds nothing, so every
        // read has to be derived from the address alone.
        let mut restarted = RecordLog::new(scratch.store(), 1, 0);
        let mut read = 0;
        for (key, addr) in &kept {
            let Some((found, back)) = restarted.read(*addr) else {
                continue;
            };
            assert_eq!(found, *key, "the wrong record came back after a restart");
            assert_eq!(back.amount, 100);
            read += 1;
        }
        assert!(
            read >= RECORDS_PER_BLOCK,
            "a restart read {read} records back, so the offsets did not survive it"
        );
        assert_eq!(restarted.traffic().store_corruptions, 0);
    }

    /// A day that is not the first begins at the offset of its own first block, so its file ends where the
    /// store's last block ends and the space in front of it was never written to. **That is this code's claim
    /// and the whole of it**: the offset is a function of the address, and nothing is put in the gap.
    ///
    /// Whether the gap costs space is the filesystem's to answer, not this store's, so it is not asserted here.
    /// Measured on APFS: a hole of 16MB or more is left unallocated and one of 8MB or less is zero-filled, and
    /// a design day is 2.9M blocks — 11.9GB — so the holes a deployment makes are three orders past that
    /// threshold. A test-sized day is a few hundred kilobytes and pays for its hole, which is why a test that
    /// asserted sparseness would be asserting something about the run's size.
    ///
    /// It takes two days to check at all. The first day's blocks start at zero, so its file has no gap.
    #[test]
    fn a_later_day_begins_at_its_own_first_block_and_nothing_fills_the_gap() {
        let scratch = Scratch::new();
        let mut log = RecordLog::new(scratch.store(), 1, 0);
        seal_blocks(&mut log, RECORDS_PER_BLOCK * 4);
        log.open_day(1);
        seal_blocks(&mut log, RECORDS_PER_BLOCK * 4);
        log.sync(0);
        log.collect_writes(0);

        let names = scratch.files();
        assert_eq!(names.len(), 2, "two days, two files: {names:?}");
        let blocks = log.blocks_in_day(0) + log.blocks_in_day(1);
        let later = std::fs::metadata(scratch.0.join("seg-01.blk"))
            .expect("the second day's file has metadata");
        assert_eq!(
            later.len(),
            blocks * crate::block::BLOCK_BYTES as u64,
            "the second day's file ends at {} rather than where the store's last block does, so an offset \
             was not the block number times the block size",
            later.len()
        );
        // And the first day's file ends where its own blocks do, which is the same claim from the side that
        // has no gap.
        let first = std::fs::metadata(scratch.0.join("seg-00.blk"))
            .expect("the first day's file has metadata");
        assert_eq!(
            first.len(),
            log.blocks_in_day(0) * crate::block::BLOCK_BYTES as u64
        );
    }

    /// A day handed back leaves no file. The only way the store shrinks, and on a filesystem it is `unlink`.
    #[test]
    fn a_freed_segment_leaves_no_file_behind() {
        let scratch = Scratch::new();
        let mut log = RecordLog::new(scratch.store(), 1, 0);
        seal_blocks(&mut log, RECORDS_PER_BLOCK * 3);
        log.sync(0);
        log.collect_writes(0);
        assert_eq!(scratch.files().len(), 1);

        // Queued behind whatever the log still owes that day, so freeing is offered and answered like
        // every other thing this log asks the volume for.
        log.free_segment(0);
        log.submit_writes(0);
        log.collect_writes(0);
        assert!(
            scratch.files().is_empty(),
            "freeing a day left its file: {:?}",
            scratch.files()
        );
    }

    /// A segment whose file a previous life left behind is refused rather than written over. Until there is a
    /// start-up reconcile this is what stands in for one: a mix of two days' blocks that nothing points into
    /// and nothing frees would be worse than a seal.
    #[test]
    fn a_segment_file_left_behind_is_refused_rather_than_written_over() {
        let scratch = Scratch::new();
        {
            let mut log = RecordLog::new(scratch.store(), 1, 0);
            seal_blocks(&mut log, RECORDS_PER_BLOCK * 3);
            log.sync(0);
            log.collect_writes(0);
        }
        assert_eq!(scratch.files().len(), 1, "the first life left its file");

        let mut restarted = RecordLog::new(scratch.store(), 1, 0);
        seal_blocks(&mut restarted, RECORDS_PER_BLOCK * 3);
        // Closing a block no longer writes it (§20): the refusal is the store's answer to the write, so the
        // closed blocks have to be submitted and collected for there to be one.
        restarted.submit_writes(0);
        restarted.collect_writes(0);
        assert!(
            restarted.take_fault(),
            "a file a previous life left behind was written over instead of refused"
        );
    }
}
