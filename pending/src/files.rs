use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::block::{Block, DurableStore, StoreFault, SEGMENT_VALUES};

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
    files: [Option<File>; SEGMENT_VALUES],
    /// Segments written to since the last sync, one bit each. At most two are ever set — the day being
    /// written and, for one block, the day before it.
    dirty: u64,
    /// Whether a file has come into being since the last sync, so the directory needs one too.
    created: bool,
    /// Reads asked for and not yet answered. The `pread` happens in `poll` rather than here, so nothing is
    /// buffered per outstanding read — which makes this the simplest correct implementation and the slowest.
    /// **`SE-OQ-4` is the choice that replaces it**: a thread pool that reads while the engine works, or
    /// io_uring. Choosing now would answer that question without measuring it.
    submitted: VecDeque<(u64, u8, u64)>,
    queue_depth: usize,
}

impl FileStore {
    /// Takes the directory already opened by whoever validated it, so nothing here can fail at start-up on a
    /// thread that has no way to report it (rule 6).
    pub fn new(dir: File, path: PathBuf, queue_depth: usize) -> Self {
        Self {
            dir,
            path,
            files: std::array::from_fn(|_| None),
            dirty: 0,
            created: false,
            submitted: VecDeque::new(),
            queue_depth: queue_depth.max(1),
        }
    }

    /// The name a segment's file has. The segment number and nothing else: the offset within the file is a
    /// function of the address (§16), so a name that carried a block number would be a second place the
    /// layout lived.
    fn name_of(&self, segment: u8) -> PathBuf {
        self.path.join(format!("seg-{segment:02}.blk"))
    }

    /// The segment's file, opened if this store has not touched it yet. Lazy because a restart begins with no
    /// open files and the blocks are still there: the index a snapshot restored names them, and the offset
    /// needs nothing but the address.
    fn file_of(&mut self, segment: u8) -> Result<&File, StoreFault> {
        if self.files[segment as usize].is_none() {
            let opened = Self::options()
                .open(self.name_of(segment))
                .map_err(|_| StoreFault::Missing)?;
            self.files[segment as usize] = Some(opened);
        }
        self.files[segment as usize]
            .as_ref()
            .ok_or(StoreFault::Missing)
    }

    fn options() -> OpenOptions {
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_DIRECT);
        }
        options
    }

    fn note_write(&mut self, segment: u8) {
        self.dirty |= 1 << segment;
    }
}

impl DurableStore for FileStore {
    /// `create_new` is `O_EXCL`, and it failing is information rather than an inconvenience: a segment's file
    /// already existing means a previous life left it there, and writing over its front would leave a mix of
    /// two days that nothing points into and nothing frees. So it refuses, which seals — until there is a
    /// start-up reconcile, this is what stands in for one (§16).
    fn open_with(&mut self, segment: u8, offset: u64, block: &Block) -> Result<(), StoreFault> {
        let file = Self::options()
            .create_new(true)
            .open(self.name_of(segment))
            .map_err(|_| StoreFault::Device)?;
        file.write_at(block, offset)
            .map_err(|_| StoreFault::Device)?;
        self.files[segment as usize] = Some(file);
        self.created = true;
        self.note_write(segment);
        Ok(())
    }

    fn append(&mut self, segment: u8, offset: u64, block: &Block) -> Result<(), StoreFault> {
        self.file_of(segment)?
            .write_at(block, offset)
            .map_err(|_| StoreFault::Device)?;
        self.note_write(segment);
        Ok(())
    }

    fn read_at(&mut self, segment: u8, offset: u64, into: &mut Block) -> Result<(), StoreFault> {
        let read = self
            .file_of(segment)?
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

    fn submit(&mut self, handle: u64, segment: u8, offset: u64, _now: u64) -> bool {
        if self.submitted.len() >= self.queue_depth {
            return false;
        }
        self.submitted.push_back((handle, segment, offset));
        true
    }

    fn poll(&mut self, _now: u64, into: &mut Block) -> Option<Result<u64, StoreFault>> {
        let (handle, segment, offset) = self.submitted.pop_front()?;
        Some(self.read_at(segment, offset, into).map(|()| handle))
    }

    fn inflight(&self) -> usize {
        self.submitted.len()
    }

    /// The file's bytes, then the directory's entries, and that order is the whole of why this call takes no
    /// argument: on a filesystem a block can be durable inside a file whose *name* is not, so durability is a
    /// fact about the store at a moment rather than a watermark per segment (§16).
    ///
    /// `sync_all` rather than `sync_data` because appending changes the file's length, and a length is
    /// metadata: `fdatasync` does not promise it.
    fn sync(&mut self) -> Result<(), StoreFault> {
        for segment in 0..SEGMENT_VALUES {
            if self.dirty & (1 << segment) == 0 {
                continue;
            }
            let Some(file) = self.files[segment].as_ref() else {
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

    /// Closed and unlinked. The unlink itself is not synced, and does not need to be: `reclaim` uses no clock
    /// and no cursor, so a file that comes back after a crash is found and freed again on the next pass.
    fn remove(&mut self, segment: u8) -> Result<(), StoreFault> {
        self.files[segment as usize] = None;
        match std::fs::remove_file(self.name_of(segment)) {
            Ok(()) => Ok(()),
            // Already gone is the state this is asking for.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(StoreFault::Device),
        }
    }
}

/// A directory a `FileStore` may be built on, opened and checked before anything is spawned.
///
/// Opened here rather than on the worker's thread because a directory that cannot be used is a configuration
/// error, and a configuration error has to be refused at start-up where somebody can be told — not swallowed
/// by a thread whose only way to report it would be to panic (rule 6).
pub fn open_directory(path: &Path) -> Result<(File, PathBuf), std::io::Error> {
    std::fs::create_dir_all(path)?;
    let dir = File::open(path)?;
    Ok((dir, path.to_path_buf()))
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

        fn store(&self) -> Box<FileStore> {
            let (dir, path) = open_directory(&self.0).expect("the scratch directory opens");
            Box::new(FileStore::new(dir, path, 8))
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
        log.sync();

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

    /// **The test the absolute offset was chosen for.** A second store over the same directory reads the
    /// blocks the first wrote, knowing nothing but the addresses — no range restored, no directory scanned, no
    /// superblock. §16 argued it; this is the argument being run.
    #[test]
    fn a_second_store_over_the_same_directory_reads_what_the_first_wrote() {
        let scratch = Scratch::new();
        let kept = {
            let mut log = RecordLog::new(scratch.store(), 1, 0);
            let kept = seal_blocks(&mut log, RECORDS_PER_BLOCK * 3);
            log.sync();
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
        log.sync();

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
        log.sync();
        assert_eq!(scratch.files().len(), 1);

        log.free_segment(0);
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
            log.sync();
        }
        assert_eq!(scratch.files().len(), 1, "the first life left its file");

        let mut restarted = RecordLog::new(scratch.store(), 1, 0);
        seal_blocks(&mut restarted, RECORDS_PER_BLOCK * 3);
        assert!(
            restarted.take_fault(),
            "a file a previous life left behind was written over instead of refused"
        );
    }
}
