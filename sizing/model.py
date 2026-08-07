"""What a deployment of this ledger costs in memory and on disk.

The split this file rests on: **the code owns what one unit costs, and this file owns how many.**
Unit costs are `size_of`, published by `ledgerfio layout --json` and cached in `units.json`; nothing
here may hard-code one, because a struct that changes would leave a number that is wrong and silent.
Counts are the other half, and they are arithmetic on a rate, a lifetime and a retention — none of
which the code knows.

Two things are deliberately not here. **Throughput and tail latency**, because they are not arithmetic:
design notes §10 records a hardware profile that was built, measured and refused, and `ledgersim` runs
the real reactor on a virtual clock instead. And **any unit cost written as a literal**, for the reason
above.

A part is either *derived* — its count follows from demand — or a *dial*, a ceiling somebody configures.
For a dial this reports what the demand requires and what the dial currently is, because a dial below
its requirement is the sizing answer, not a detail under it.
"""

import json
import math
import os

UNITS_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "units.json")
SECONDS_PER_DAY = 86_400


def load_units(path=UNITS_PATH):
    """The published unit costs. Refresh with `make sizing-units` after changing a sized struct."""
    with open(path) as handle:
        return json.load(handle)


def buckets_for(entries):
    """`hashbrown`'s rule, and the reason a hash table's cost is a staircase rather than a line.

    Kept identical to `ledger_base::sizing::buckets_for`. The published `bucket_rule` string is what
    says the two are meant to agree; `check_bucket_rule` is what fails if they stop.
    """
    if entries <= 0:
        return 0
    if entries <= 3:
        return 4
    if entries <= 7:
        return 8
    return 1 << (entries * 8 // 7 - 1).bit_length()


def index_slots_for(holds, load_target=0.90):
    """Slots the cuckoo index allocates for `holds` live at the target load.

    Not `buckets_for`: this table rounds its **bucket** count to a power of two and each bucket is four
    slots, so the step is four times coarser than a hash table's and lands in different places.
    """
    if holds <= 0:
        return 0
    needed = math.ceil(holds / load_target)
    buckets = max(2, 1 << (math.ceil(needed / 4) - 1).bit_length())
    return buckets * 4


class Lifetimes:
    """How long holds live, as a curve rather than a mean — which is what makes three of this model's
    inputs stop being inputs.

    Given "half are voided within the hour", three questions answer themselves:

    - **What reaches the disk.** A record whose hold resolved before its block was flushed is dropped by
      compaction and never written. So the written share is `1 - resolved_by(flush_window)`, and in a
      measured run that is the `died in buffer 98%` line rather than anything about retention.
    - **What a resolution costs.** A resolution inside the residency window is answered from memory; one
      after it reads the device. The device read rate is the resolutions that fall in the gap between
      the two windows, which is a subtraction on this curve.
    - **How many holds are live at once.** Little's law wants a mean life, and the mean of a lifetime is
      the area above its curve. Asking for the mean directly invites a number nobody derived, and one
      that is wrong by orders of magnitude when the distribution has a tail — which this one does, by
      construction: retention exists because some holds never resolve.

    Points are `(hours, cumulative share resolved by then)`, and the shape between them is taken as
    linear. That is a declaration about a business, so it is stated rather than fitted: nobody here has
    the data, and a fitted curve would look like evidence.
    """

    def __init__(self, points):
        self.points = sorted(points)
        if not self.points:
            raise ValueError("a lifetime curve needs at least one point")
        hours = [hour for hour, _ in self.points]
        shares = [share for _, share in self.points]
        if any(hour <= 0 for hour in hours):
            raise ValueError("a point at or before zero hours says nothing")
        if shares != sorted(shares) or not 0 <= shares[0] or shares[-1] > 1:
            raise ValueError("the resolved share must rise and stay within 0..1")

    def resolved_by(self, hours):
        """The share resolved by `hours`. Flat after the last point: what the curve does not say, it
        does not guess, and the flat tail is what becomes the survivors."""
        if hours <= 0:
            return 0.0
        previous_hour, previous_share = 0.0, 0.0
        for hour, share in self.points:
            if hours <= hour:
                span = hour - previous_hour
                within = (hours - previous_hour) / span if span else 1.0
                return previous_share + within * (share - previous_share)
            previous_hour, previous_share = hour, share
        return previous_share

    def mean_life_hours(self, cap_hours):
        """The area above the curve up to `cap_hours` — a hold still live at the cap is counted as
        living exactly that long, because that is when expiry takes it."""
        total, previous_hour, previous_share = 0.0, 0.0, 0.0
        for hour, share in self.points:
            hour = min(hour, cap_hours)
            if hour <= previous_hour:
                continue
            span = hour - previous_hour
            # Trapezoid on the live share, which falls from 1 - previous_share to 1 - share.
            total += span * ((1 - previous_share) + (1 - share)) / 2
            previous_hour, previous_share = hour, share
            if hour >= cap_hours:
                return total
        return total + (cap_hours - previous_hour) * (1 - previous_share)


class Load:
    """How much traffic the **busiest window of a given width** carries. A curve, for the same reason
    lifetimes are one: every window in this ledger has a different width, and one number cannot answer
    for all of them.

    A day is not flat, so neither of the two obvious shortcuts works, and both were in this file:

    - Multiplying the busiest *hour* by the window's width assumes that many equally busy hours in a
      row. Over-sizes a four-hour flush window by whatever the day's shape is worth.
    - Dividing the *day* by twenty-four assumes a flat day. Under-sizes a two-hour residency window by
      the same factor, in the other direction.

    Points are `(hours, transactions in the busiest window that wide)`. The width of the first point is
    what the peak rate is read from — a peak is just the busiest very short window.

    Two things are checked, because both are ways of stating a day that cannot exist: a wider window
    cannot carry less, and it cannot carry a *higher average rate* than a narrower one.
    """

    def __init__(self, points):
        self.points = sorted(points)
        if len(self.points) < 2:
            raise ValueError("a load curve needs a short window and a long one")
        previous_hours, previous_tx = 0.0, 0.0
        for hours, tx in self.points:
            if hours <= 0:
                raise ValueError("a window of no width carries nothing")
            if tx < previous_tx:
                raise ValueError(f"the busiest {hours}h carries less than the busiest {previous_hours}h")
            if previous_hours and tx / hours > previous_tx / previous_hours * 1.0000001:
                raise ValueError(
                    f"the busiest {hours}h averages a higher rate than the busiest {previous_hours}h, "
                    "which no day does"
                )
            previous_hours, previous_tx = hours, tx

    def busiest(self, hours):
        """Transactions in the busiest window `hours` wide. Below the first point the rate is taken as
        flat -- the peak rate is the most this says about a window shorter than it was given."""
        if hours <= 0:
            return 0.0
        first_hours, first_tx = self.points[0]
        if hours <= first_hours:
            return first_tx * (hours / first_hours)
        previous_hours, previous_tx = first_hours, first_tx
        for point_hours, point_tx in self.points[1:]:
            if hours <= point_hours:
                span = point_hours - previous_hours
                within = (hours - previous_hours) / span if span else 1.0
                return previous_tx + within * (point_tx - previous_tx)
            previous_hours, previous_tx = point_hours, point_tx
        # Past the last point the day repeats: nothing here knows about a weekly shape.
        return previous_tx * (hours / previous_hours)

    @property
    def peak_rate(self):
        """Transactions a second in the shortest window given, which is what a peak is."""
        hours, tx = self.points[0]
        return tx / (hours * 3_600)

    @property
    def daily(self):
        return self.busiest(24)


class Demand:
    """What a deployment brings. Every field is something a business knows or can be asked for.

    Two curves and three scalars. The curves are here because the two things a deployment is asked
    about — *how much traffic* and *how long holds live* — are both read at several points by this
    model, and a point each let those readings disagree.

    The inputs, and which structure each one moves:

    - `load` — the [`Load`] curve. It replaced four numbers (a peak, how long the peak lasts, the
      busiest hour and the day), which were four readings of it. Every window-shaped structure asks it
      at that window's own width, so changing a window changes the count it is sized by — which is the
      thing four fixed numbers could not express.

    - `hold_share` — of every transaction, the share that **creates a hold**. A plain transfer creates
      none, and neither does the settle that resolves one. Holds are what the index counts, so this
      scales the index, the blocks and the disk together.
    - `records_per_hold` — records one hold appends over its whole life: **one, plus one per partial
      settle**, because append-only means a changed remainder is a new version rather than an edit.
      Resolving in full appends nothing. Scales the blocks and the disk, not the index.
    - `lifetimes` — the [`Lifetimes`] curve, and it replaced two inputs that were guesses. How long a
      hold lives and what share survives to retention are the same fact read at two points, so asking
      for them separately let them disagree.
    - `commit_latency_seconds` — submit to commit. Times the peak rate it is requests in flight, which
      is what the slot pool has to cover. Nothing else uses it — and it is an *input* here rather than
      an answer, because latency is `ledgersim`'s question, not this file's.
    """

    def __init__(
        self,
        load,
        lifetimes,
        accounts,
        hold_share=1.0,
        records_per_hold=1.0,
        commit_latency_seconds=0.010,
    ):
        self.load = load
        self.lifetimes = lifetimes
        self.accounts = accounts
        self.hold_share = hold_share
        self.records_per_hold = records_per_hold
        self.commit_latency_seconds = commit_latency_seconds

    @property
    def peak_rate(self):
        return self.load.peak_rate

    @property
    def daily_tx(self):
        return self.load.daily

    def holds_in_busiest(self, hours):
        """Holds created in the busiest window that wide -- which is what every window-shaped
        structure is sized by, at its own width."""
        return self.load.busiest(hours) * self.hold_share

    def records_in_busiest(self, hours):
        return self.holds_in_busiest(hours) * self.records_per_hold

    def sanity(self):
        """Contradictions worth refusing before they become a confident wrong answer.

        Most of what used to be checked here is now impossible to state: `Load` refuses a curve whose
        wider windows carry less, or average a higher rate, than its narrower ones. What is left is the
        one thing a single curve can still get wrong.
        """
        problems = []
        if self.hold_share > 1 or self.hold_share < 0:
            problems.append("the share of transactions creating a hold is not a share")
        if self.records_per_hold < 1:
            problems.append("a hold appends at least the record that creates it")
        return problems


class Policy:
    """What the operator chooses. Each is a bound with a name, not a tuning knob.

    `flush_window_hours` is a **recovery** bound — how much a restart replays. `residency_hours` is a
    **latency** bound — how far back an answer comes from memory rather than the device. `idem_window_hours`
    is a **duplicate-detection** bound, and it is the one the code does not enforce yet: the map has no
    rotating generations, so today it only grows.
    """

    def __init__(
        self,
        retention_days=2,
        grace_days=1,
        flush_window_hours=1,
        residency_hours=24,
        idem_window_hours=1,
        snapshot_every_effects=1_000_000,
    ):
        self.retention_days = retention_days
        self.grace_days = grace_days
        self.flush_window_hours = flush_window_hours
        self.residency_hours = residency_hours
        self.idem_window_hours = idem_window_hours
        self.snapshot_every_effects = snapshot_every_effects

    @property
    def lifetime_days(self):
        return self.retention_days + self.grace_days


class Dials:
    """Ceilings the node is configured with. **These are outputs of a sizing exercise, not inputs.**

    They are here so a plan can be checked against them: a dial below what demand requires is where the
    node refuses work, and reporting the requirement without the dial beside it leaves the reader to
    guess whether it is already met.
    """

    def __init__(
        self,
        work_slots=65_536,
        client_queue=65_536,
        batch_max=10_000,
        batch_queued=10_000,
        raft_in_flight=8,
        engine_write_backlog=16_384,
        overlay_soft_limit=1_048_576,
        index_budget_bytes=8 << 30,
        read_cache_blocks=64,
        write_lane_blocks=0,
        read_pool_blocks=0,
    ):
        self.work_slots = work_slots
        self.client_queue = client_queue
        self.batch_max = batch_max
        self.batch_queued = batch_queued
        self.raft_in_flight = raft_in_flight
        self.engine_write_backlog = engine_write_backlog
        self.overlay_soft_limit = overlay_soft_limit
        # `MemoryPending` refuses a declared capacity whose index would exceed this, at start-up rather
        # than when it fills -- so a plan over it is a node that will not come up.
        self.index_budget_bytes = index_budget_bytes
        self.read_cache_blocks = read_cache_blocks
        self.write_lane_blocks = write_lane_blocks
        self.read_pool_blocks = read_pool_blocks


class Line:
    """One structure's answer: how many, what one costs, and where the count came from."""

    def __init__(self, name, owner, unit, unit_bytes, count, kind, why, dial=None):
        self.name = name
        self.owner = owner
        self.unit = unit
        self.unit_bytes = unit_bytes
        self.count = count
        self.kind = kind
        self.why = why
        self.dial = dial

    @property
    def bytes(self):
        return self.count * self.unit_bytes

    @property
    def over_dial(self):
        return self.dial is not None and self.count > self.dial


class Sizing:
    """Every sized structure at one demand, one policy and one set of dials.

    Overrides are for asking "what if this struct were smaller"; anything overridden is carried into the
    report as overridden, because a hypothetical printed beside measurements reads as a measurement.
    """

    def __init__(self, demand, policy, dials=None, units=None, overrides=None):
        self.demand = demand
        self.policy = policy
        self.dials = dials or Dials()
        self.units = units or load_units()
        self.overrides = dict(overrides or {})
        self.costs = {part["name"]: part for part in self.units["parts"]}
        unknown = set(self.overrides) - set(self.costs)
        if unknown:
            raise KeyError(f"no such sized part: {sorted(unknown)}")
        self.lines = self._build()

    # --- what the demand implies, before any structure is priced ---

    @property
    def holds_created_per_second(self):
        return self.demand.peak_rate * self.demand.hold_share

    # --- the three shares, all read off one curve at three different points ---

    @property
    def resolved_by_flush(self):
        """Share resolved before their block is flushed, so compaction drops the record and it never
        reaches the store. A measured run calls this `died in buffer`."""
        return self.demand.lifetimes.resolved_by(self.policy.flush_window_hours)

    @property
    def resolved_by_residency(self):
        """Share resolved while their record is still in memory, so the resolution costs no device
        read. This is what the residency window buys, and it is why residency is an **answer** here
        rather than an input somebody picked."""
        return self.demand.lifetimes.resolved_by(self.policy.residency_hours)

    @property
    def resolved_by_retention(self):
        """Share a client resolves at all. The rest are voided by expiry."""
        return self.demand.lifetimes.resolved_by(self.policy.retention_days * 24)

    @property
    def survivor_share(self):
        return 1 - self.resolved_by_retention

    @property
    def written_share(self):
        return 1 - self.resolved_by_flush

    # --- holds ---

    @property
    def mean_hold_life_hours(self):
        """The area above the lifetime curve, capped at retention -- expiry takes what is left."""
        return self.demand.lifetimes.mean_life_hours(self.policy.retention_days * 24)

    @property
    def live_holds(self):
        """Little's law over the **daily** rate, because the mean life is measured in hours and no peak
        lasts hours. What the peak adds on top is the requests in flight, which is a separate line.

        Sized at the day rather than the peak on purpose: one term at the peak rate over a mean life of
        days gives billions where the answer is millions, and the index cannot grow.
        """
        holds_per_hour = self.holds_per_day / 24
        return math.ceil(holds_per_hour * self.mean_hold_life_hours)

    @property
    def requests_in_flight(self):
        return math.ceil(self.demand.peak_rate * self.demand.commit_latency_seconds)

    @property
    def holds_per_day(self):
        return self.demand.daily_tx * self.demand.hold_share

    @property
    def records_per_day(self):
        """Records come from holds, never from transactions directly, and that is why the two inputs
        are `hold_share` and `records_per_hold` rather than one `records_per_tx`.

        A transaction that resolves a hold in full appends nothing -- the removal is not a record. What
        appends is the hold itself and each *partial* settle, since append-only means a changed
        remainder is a new version at a new address.
        """
        return self.holds_per_day * self.demand.records_per_hold

    @property
    def unwritten_records(self):
        """What the flush window holds: the records of the busiest window **that wide**.

        Nothing is subtracted -- a record dies *in* this buffer rather than before reaching it, so the
        whole window's arrivals are here whatever share of them will survive.
        """
        return math.ceil(self.demand.records_in_busiest(self.policy.flush_window_hours))

    @property
    def resident_records(self):
        """Written records inside the residency window, asked of the load curve at **that** width.

        Dividing a day by twenty-four to get a two-hour window was the mistake in the other direction
        from the flush buffer's: it assumes a flat day and under-sizes exactly where the window is
        narrow enough for the day's shape to matter.
        """
        return math.ceil(
            self.demand.records_in_busiest(self.policy.residency_hours) * self.written_share
        )

    @property
    def stored_records(self):
        """What the segment files hold: every day's written records for as long as the day is kept.

        Driven by `written_share`, not by the survivors -- the earlier version of this used the share
        alive at *retention*, which is the wrong subtraction by the whole width of the residency
        window. In a measured run 98% of records die in the buffer with an hour-wide flush window, and
        that is this number's real lever.
        """
        return math.ceil(
            self.records_per_day * self.written_share * self.policy.lifetime_days
        )

    @property
    def live_records_on_disk(self):
        """Records in the segment files that the index still points at.

        **One per live hold, not `records_per_hold` per live hold**: append-only means a partial settle
        writes a new version, and the moment it does the old one is dead. A hold has exactly one live
        record however many it has appended.

        Minus the live holds young enough that their record has not been flushed yet -- those are in
        the buffer, counted there, and would be counted twice here.
        """
        in_buffer = self.demand.holds_in_busiest(self.policy.flush_window_hours)
        return max(0.0, self.live_holds - in_buffer)

    @property
    def dead_records_on_disk(self):
        """What the segment files hold that nothing points at any more, and **it is most of them**.

        A segment is a day, and a day is freed whole -- so the few holds that live out their retention
        keep their whole day's file alive, dead records and all. That is the price of the property §12
        bought: a block once written is never rewritten, which is what lets an index slot hold an
        address and nothing else, and lets compaction, the expiry walk and the sweep all decide by
        comparing addresses.

        It also says where the disk figure is *not* sensitive. Making segments finer than a day does
        not help: the surviving share is in every slice equally, so an hour-wide segment is just as
        unfreeable as a day-wide one. The two levers that do move it are the flush window (what reaches
        the disk at all) and retention (how long a day's file is kept).
        """
        return max(0.0, self.stored_records - self.live_records_on_disk)

    # --- the device, which the curve is what makes computable ---

    @property
    def resolution_reads_per_second(self):
        """Resolutions that land in the gap between the two windows: after the record left memory, and
        before expiry would have taken the hold. Each one is a device read.

        This is the answer residency is chosen for. Widening residency moves the subtraction and buys
        reads at a price in blocks, which `residency_curve` prints as the trade it is.
        """
        holds_per_second = self.holds_per_day / SECONDS_PER_DAY
        in_gap = max(0.0, self.resolved_by_retention - self.resolved_by_residency)
        return holds_per_second * in_gap

    @property
    def sweep_reads_per_second(self):
        """The expiry sweep reading a day's blocks to find what to void. Small beside the resolutions
        and it is worth showing anyway: it was blamed for ninety thousand reads that turned out to be
        the re-offers', which is a mistake this line makes harder to repeat."""
        survivors_per_day = self.holds_per_day * self.survivor_share
        return self.blocks_for(survivors_per_day * self.demand.records_per_hold) / SECONDS_PER_DAY

    @property
    def device_reads_per_second(self):
        return self.resolution_reads_per_second + self.sweep_reads_per_second

    @property
    def records_per_block(self):
        """Derived from the record size rather than read off the dump, so an override of it reaches the
        disk figure. Identical to the published number when nothing is overridden -- and the reason to
        derive it anyway is that `pending record` is the one unit cost somebody would want to try
        shrinking, and an override that silently changed nothing would be worse than no override."""
        return self.units["block_bytes"] // self.unit_bytes("pending record")

    def blocks_for(self, records):
        return math.ceil(records / self.records_per_block)

    def unit_bytes(self, name):
        return self.overrides.get(name, self.costs[name]["bytes"])

    # --- the structures ---

    def _line(self, name, count, kind, why, dial=None):
        cost = self.costs[name]
        return Line(
            name,
            cost["owner"],
            cost["unit"],
            self.unit_bytes(name),
            math.ceil(count),
            kind,
            why,
            dial,
        )

    def _build(self):
        demand, policy, dials = self.demand, self.policy, self.dials
        in_flight = self.requests_in_flight
        return [
            self._line(
                "work slots",
                in_flight,
                "derived",
                "peak rate x commit latency: one slot per request outstanding",
                dials.work_slots,
            ),
            self._line(
                "deferred dispatches",
                dials.work_slots,
                "dial",
                "bounded by the slot pool; every slot could be waiting to dispatch",
            ),
            self._line(
                "lane state",
                demand.accounts,
                "derived",
                "one lane per account, and it is the working set rather than the load",
            ),
            self._line(
                "open batch effects",
                dials.batch_queued,
                "dial",
                "judged effects allowed to wait for consensus before intake pauses",
            ),
            self._line(
                "batches awaiting consensus",
                dials.raft_in_flight,
                "dial",
                "proposals outstanding at once",
            ),
            self._line(
                "spare batch buffers",
                (dials.raft_in_flight + 1) * dials.batch_max,
                "dial",
                "one recycled buffer per proposal in flight, each the batch ceiling wide",
            ),
            self._line(
                "ack backlog",
                dials.client_queue,
                "dial",
                "answers a client that stopped reading can leave behind before intake pauses",
            ),
            self._line(
                "queued pending writes",
                dials.engine_write_backlog,
                "dial",
                "committed decisions the engine has not taken; the bound is what makes it backpressure",
            ),
            self._line(
                "account records",
                demand.accounts,
                "derived",
                "every account is resident; no rate and no lifetime enters this",
            ),
            self._line(
                "account index",
                buckets_for(demand.accounts),
                "derived",
                "a hash table over the working set: buckets, not accounts",
            ),
            self._line(
                "idem keys",
                buckets_for(math.ceil(demand.load.busiest(policy.idem_window_hours))),
                "derived",
                "the busiest window this wide -- change the window and this follows; nothing enforces it yet",
            ),
            self._line(
                "pending index",
                index_slots_for(self.live_holds),
                "derived",
                "live holds at the 0.90 load target; the table never grows, so this is a ceiling",
            ),
            self._line(
                "pending budget groups",
                buckets_for(0),
                "derived",
                "live budget groups; zero unless the workload declares them",
            ),
            self._line(
                "pending overlay",
                buckets_for(dials.overlay_soft_limit),
                "dial",
                "the overlay's own soft limit; it evicts down to this rather than growing",
            ),
            self._line(
                "pending writeback buffer",
                self.blocks_for(self.unwritten_records),
                "derived",
                "the busiest window this wide -- a recovery bound, and nothing is subtracted: a record dies here",
            ),
            self._line(
                "pending resident blocks",
                self.blocks_for(self.resident_records),
                "derived",
                "written records inside the residency window -- what compaction let through, not the survivors",
            ),
            self._line(
                "pending stored blocks",
                self.blocks_for(self.stored_records),
                "derived",
                "every day's written records for as long as the day is kept -- this is the disk figure",
            ),
            self._line(
                "volume read cache",
                dials.read_cache_blocks,
                "dial",
                "blocks kept from answered reads",
            ),
            self._line(
                "volume write lane",
                dials.write_lane_blocks,
                "dial",
                "the lane's own buffers; zero unless the store is a directory",
            ),
            self._line(
                "volume read pool",
                dials.read_pool_blocks,
                "dial",
                "one buffer per read thread; zero unless the store is a directory",
            ),
            self._line(
                "kept log",
                policy.snapshot_every_effects,
                "dial",
                "effects since the last snapshot; there is no compaction, so nothing else bounds it",
            ),
            self._line(
                "proposals in flight",
                dials.raft_in_flight,
                "dial",
                "one per outstanding proposal",
            ),
        ]

    # --- answers ---

    #: The one structure that becomes disk rather than memory once the store is a directory. Named
    #: once, because two places deciding which side of the line it falls on is how a total stops adding
    #: up.
    DISK_PART = "pending stored blocks"

    @property
    def memory_bytes(self):
        return sum(line.bytes for line in self.lines if line.name != self.DISK_PART)

    @property
    def disk_bytes(self):
        return dict(self.lines_by_name)[self.DISK_PART].bytes

    @property
    def memory_by_component(self):
        """Memory per owning component, in the order a request meets them.

        Worth its own answer rather than a total: which component is large decides what a deployment
        can do about it. Accounts are the working set and cannot be traded away; the pending engine is
        a rate and a retention, both of which are policy.
        """
        order, totals = [], {}
        for line in self.lines:
            if line.name == self.DISK_PART:
                continue
            if line.owner not in totals:
                order.append(line.owner)
                totals[line.owner] = 0
            totals[line.owner] += line.bytes
        return [(owner, totals[owner]) for owner in order]

    @property
    def lines_by_name(self):
        return [(line.name, line) for line in self.lines]

    @property
    def breaches(self):
        """Where a plan meets a limit the node enforces, in the two shapes it takes: a count past a
        pool, and the index's declared byte budget -- which is refused at start-up, not under load."""
        found = [
            f"{line.name} needs {line.count:,} but the dial is {line.dial:,}"
            for line in self.lines
            if line.over_dial
        ]
        index = dict(self.lines_by_name)["pending index"]
        if index.bytes > self.dials.index_budget_bytes:
            found.append(
                f"engine index needs {gigabytes(index.bytes):.2f} GB but the declared budget is "
                f"{gigabytes(self.dials.index_budget_bytes):.2f} GB -- the node refuses this at start-up"
            )
        return found


def gigabytes(value):
    return value / 1_000_000_000


def size(value):
    """Bytes at whatever scale reads: these span six orders of magnitude in one table, and a raw byte
    count wide enough for the largest runs into its neighbour for every other row."""
    for unit, at in (("TB", 1e12), ("GB", 1e9), ("MB", 1e6), ("KB", 1e3)):
        if value >= at:
            return f"{value / at:,.2f} {unit}"
    return f"{value:,.0f} B"


def report(sizing):
    """The whole answer as text, overrides and dial breaches included rather than in a footnote."""
    out = []
    problems = sizing.demand.sanity()
    for problem in problems:
        out.append(f"!! {problem}")
    if sizing.overrides:
        out.append(
            "!! overridden unit costs, so these are not measurements: "
            + ", ".join(
                f"{name}={value}B (measured {sizing.costs[name]['bytes']}B)"
                for name, value in sorted(sizing.overrides.items())
            )
        )
    out.append(f"unit costs from {sizing.units['source']} at {sizing.units['commit'][:12]}")
    out.append("")
    out.append(f"{'structure':<26}{'count':>16}{'B/unit':>9}{'size':>13}  where the count comes from")
    for line in sizing.lines:
        flag = "  << OVER DIAL" if line.over_dial else ""
        out.append(
            f"{line.name:<26}{line.count:>16,}{line.unit_bytes:>9,}{size(line.bytes):>13}"
            f"  {line.kind}: {line.why}{flag}"
        )
    out.append(
        f"{'':<26}{'':>16}{'':>9}{'-' * 13}"
    )
    out.append(
        f"{'memory + disk':<26}{'':>16}{'':>9}"
        f"{size(sizing.memory_bytes + sizing.disk_bytes):>13}"
    )
    out.append("")
    out.append("what the lifetime curve says, read at the three windows that matter")
    out.append(
        f"  resolved within the flush window ({sizing.policy.flush_window_hours}h): "
        f"{sizing.resolved_by_flush:.0%} -- these never reach the disk"
    )
    out.append(
        f"  resolved within residency ({sizing.policy.residency_hours}h): "
        f"{sizing.resolved_by_residency:.0%} -- these cost no device read"
    )
    out.append(
        f"  resolved by retention ({sizing.policy.retention_days}d): "
        f"{sizing.resolved_by_retention:.0%}, so expiry voids {sizing.survivor_share:.0%}"
    )
    out.append(f"  mean hold life {sizing.mean_hold_life_hours:.1f}h")
    out.append("")
    out.append("device reads a second, at the peak")
    out.append(
        f"  resolutions past residency  {sizing.resolution_reads_per_second:>12,.0f}/s"
    )
    out.append(f"  the expiry sweep            {sizing.sweep_reads_per_second:>12,.0f}/s")
    out.append(f"  total                       {sizing.device_reads_per_second:>12,.0f}/s")
    out.append("")
    out.append("memory by component")
    for owner, total in sizing.memory_by_component:
        share = total / sizing.memory_bytes * 100 if sizing.memory_bytes else 0
        out.append(f"  {owner:<26}{size(total):>13}{share:>7.0f}%")
    out.append(f"  {'':<26}{'-' * 13}")
    out.append(f"  {'memory':<26}{size(sizing.memory_bytes):>13}")
    out.append(
        f"  {'disk (segment files)':<26}{size(sizing.disk_bytes):>13}"
        f"   {sizing.stored_records:,} records at "
        f"{sizing.unit_bytes('pending record')}B, {sizing.records_per_block} to a "
        f"{sizing.units['block_bytes']}B block"
    )
    dead_share = (
        sizing.dead_records_on_disk / sizing.stored_records if sizing.stored_records else 0
    )
    per_record = sizing.disk_bytes / sizing.stored_records if sizing.stored_records else 0
    out.append(
        f"  {'  of which still live':<26}"
        f"{size(sizing.live_records_on_disk * per_record):>13}"
        f"   {sizing.live_records_on_disk:,.0f} records the index still points at"
    )
    out.append(
        f"  {'  of which dead':<26}"
        f"{size(sizing.dead_records_on_disk * per_record):>13}"
        f"   {dead_share:.0%} -- a segment is a day and is freed whole, so the few holds that"
    )
    out.append(
        f"  {'':<26}{'':>13}   live out their retention keep the whole day's file alive"
    )
    out.append(f"live holds {sizing.live_holds:,}, requests in flight {sizing.requests_in_flight:,}")
    for breach in sizing.breaches:
        out.append(f"!! {breach}")
    return "\n".join(out)


def print_unit_costs(units=None, overrides=None):
    """Every published unit cost, with any override beside it. Printed rather than buried, because a
    number nobody can see is one nobody questions -- and these are the numbers a reader would want to
    ask "what if this were smaller" about."""
    units = units or load_units()
    overrides = overrides or {}
    print(f"{'structure':<28}{'unit':<9}{'measured':>9}{'used':>9}  what one is")
    owner = None
    for part in units["parts"]:
        if part["owner"] != owner:
            owner = part["owner"]
            print(f"\n[{owner}]")
        used = overrides.get(part["name"], part["bytes"])
        mark = "  <-- OVERRIDDEN" if used != part["bytes"] else ""
        print(
            f"{part['name']:<28}{part['unit']:<9}{part['bytes']:>9,}{used:>9,}"
            f"  {part['what']}{mark}"
        )
    print()
    print(
        f"a bucket is next_pow2(entries * 8/7) of them; "
        f"{units['records_per_block']} records fit a {units['block_bytes']}B block "
        f"(derived from the record size, so overriding it moves the disk figure)"
    )


def residency_curve(demand, policy, hours, dials=None, units=None):
    """The trade residency actually is: blocks of memory against device reads a second.

    Printed as a curve rather than solved for, because what a read is worth is a property of the device
    and of the tail somebody is trying to hold -- neither of which is in this file. Design notes §10 is
    the same refusal one level down.
    """
    rows = []
    for residency in hours:
        one = Sizing(demand, _with_windows(policy, residency_hours=residency), dials, units)
        blocks = dict(one.lines_by_name)["pending resident blocks"]
        rows.append(
            {
                "hours": residency,
                "hit_share": one.resolved_by_residency,
                "blocks": blocks.count,
                "memory_bytes": blocks.bytes,
                "reads_per_second": one.resolution_reads_per_second,
            }
        )
    return rows


def print_residency_curve(rows):
    print(f"{'residency':>10}{'answered from memory':>22}{'blocks':>14}{'GB':>8}{'device reads/s':>16}")
    for row in rows:
        print(
            f"{row['hours']:>9}h{row['hit_share']:>21.0%}{row['blocks']:>14,}"
            f"{gigabytes(row['memory_bytes']):>8.2f}{row['reads_per_second']:>16,.0f}"
        )


def _with_windows(policy, **changed):
    fields = dict(
        retention_days=policy.retention_days,
        grace_days=policy.grace_days,
        flush_window_hours=policy.flush_window_hours,
        residency_hours=policy.residency_hours,
        idem_window_hours=policy.idem_window_hours,
        snapshot_every_effects=policy.snapshot_every_effects,
    )
    fields.update(changed)
    return Policy(**fields)


def flush_window_curve(demand, policy, hours, dials=None, units=None):
    """What widening the flush window buys, which is not what a reader expects.

    A wider window is more memory *and less disk*, because a record whose hold resolves before the
    block is flushed is dropped by compaction and never written. Half these holds resolve within the
    hour, so an hour-wide window already discards half the records; four hours discards seventy
    percent. It costs recovery time -- the window is how much a restart replays -- which is a bound
    nothing here prices.
    """
    rows = []
    for window in hours:
        one = Sizing(demand, _with_windows(policy, flush_window_hours=window), dials, units)
        buffer = dict(one.lines_by_name)["pending writeback buffer"]
        rows.append(
            {
                "hours": window,
                "written_share": one.written_share,
                "buffer_bytes": buffer.bytes,
                "disk_bytes": one.disk_bytes,
                "memory_bytes": one.memory_bytes,
                "dead_share": (
                    one.dead_records_on_disk / one.stored_records if one.stored_records else 0
                ),
            }
        )
    return rows


def print_flush_window_curve(rows):
    print(
        f"{'flush window':>13}{'reaches disk':>14}{'buffer GB':>12}{'memory GB':>12}"
        f"{'disk GB':>10}{'of it dead':>12}"
    )
    for row in rows:
        print(
            f"{row['hours']:>12}h{row['written_share']:>14.0%}"
            f"{gigabytes(row['buffer_bytes']):>12.2f}{gigabytes(row['memory_bytes']):>12.2f}"
            f"{gigabytes(row['disk_bytes']):>10.1f}{row['dead_share']:>12.0%}"
        )


def check_bucket_rule(units=None):
    """That this file's staircase is still the one the code publishes.

    The rule is the difference between a table costing what a model says and twice that, so it is
    checked rather than trusted -- it is the one piece of the code's arithmetic reproduced here.
    """
    units = units or load_units()
    expected = "next_power_of_two(entries * 8 / 7)"
    if units["bucket_rule"] != expected:
        raise AssertionError(
            f"the published bucket rule is {units['bucket_rule']!r}, "
            f"but this model implements {expected!r}"
        )


#: Half voided inside the hour, most within the day, and a tail that never resolves -- which is what
#: retention exists for. Stated, not fitted: nobody here has the data, and a fitted curve would look
#: like evidence.
EXAMPLE_LIFETIMES = Lifetimes([(1, 0.50), (4, 0.70), (24, 0.88), (24 * 7, 0.96)])

#: A day with a shape. 200k/s at the peak, and by the hour that peak is long over -- the busiest hour
#: carries a tenth of the day, the busiest four hours a third. Every window-shaped structure reads this
#: at its own width, so changing a window changes what it is sized by.
EXAMPLE_LOAD = Load(
    [
        (1 / 60, 200_000 * 60),  # the busiest minute, at the peak rate
        (1, 30_000_000),  # the busiest hour: a tenth of the day
        (4, 90_000_000),  # the busiest four: a third
        (24, 300_000_000),  # the day
    ]
)

EXAMPLE_DEMAND = Demand(
    load=EXAMPLE_LOAD,
    lifetimes=EXAMPLE_LIFETIMES,
    accounts=100_000_000,
    hold_share=0.90,
    records_per_hold=1.2,
    commit_latency_seconds=0.010,
)

#: Thirty days, because that is the decision this is here to make visible rather than default away.
EXAMPLE_POLICY = Policy(retention_days=30, grace_days=1)


if __name__ == "__main__":
    check_bucket_rule()
    print(
        report(
            Sizing(
                EXAMPLE_DEMAND,
                EXAMPLE_POLICY,
            )
        )
    )
    print()
    print_residency_curve(
        residency_curve(EXAMPLE_DEMAND, EXAMPLE_POLICY, [1, 2, 4, 8, 24, 72, 168])
    )
    print()
    print_flush_window_curve(
        flush_window_curve(EXAMPLE_DEMAND, EXAMPLE_POLICY, [0.25, 1, 4, 12, 24])
    )
