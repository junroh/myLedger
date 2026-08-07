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


class Demand:
    """What a deployment brings. Every field is something a business knows or can be asked for.

    The three rates are not one rate. A peak decides what must be held in flight, the busiest hour
    decides the windows an hour wide, and the day decides retention — and a deployment whose peak is
    eighty times its mean is sized eighty times wrong by whichever one it picks.
    """

    def __init__(
        self,
        peak_rate,
        peak_seconds,
        busiest_hour_tx,
        daily_tx,
        accounts,
        records_per_tx=1.0,
        hold_share=1.0,
        short_life_seconds=1.0,
        survivor_share=0.5,
        commit_latency_seconds=0.010,
    ):
        self.peak_rate = peak_rate
        self.peak_seconds = peak_seconds
        self.busiest_hour_tx = busiest_hour_tx
        self.daily_tx = daily_tx
        self.accounts = accounts
        self.records_per_tx = records_per_tx
        self.hold_share = hold_share
        self.short_life_seconds = short_life_seconds
        self.survivor_share = survivor_share
        self.commit_latency_seconds = commit_latency_seconds

    def sanity(self):
        """Contradictions worth refusing before they become a confident wrong answer."""
        problems = []
        if self.busiest_hour_tx > self.daily_tx:
            problems.append("the busiest hour holds more than the day")
        if self.peak_rate * self.peak_seconds > self.busiest_hour_tx:
            problems.append("the peak alone exceeds the busiest hour")
        if self.busiest_hour_tx < self.daily_tx / 24:
            problems.append("the busiest hour is below the daily mean, so it is not the busiest")
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

    @property
    def mean_hold_life_seconds(self):
        """Dominated by the survivors, and that is the point of splitting the two.

        A hold that resolves in a second and one that runs out its retention differ by five orders of
        magnitude, so a single mean nobody derived hides which one is being sized for.
        """
        survivor = self.policy.lifetime_days * SECONDS_PER_DAY
        return (
            1 - self.demand.survivor_share
        ) * self.demand.short_life_seconds + self.demand.survivor_share * survivor

    @property
    def live_holds(self):
        """Little's law, in **two terms driven by two different rates**, which is the correction that
        matters most here.

        The short-lived holds are outstanding only while the peak lasts, so they follow the peak rate.
        The survivors accumulate for their whole lifetime, which is days -- and nothing sustains a peak
        for days, so they follow the daily volume. One term at the peak rate over the survivors' mean
        life gives 38 billion holds where the answer is 450 million: an eighty-six-fold error, and the
        index is the structure that cannot grow.
        """
        return math.ceil(self.short_lived_holds + self.surviving_holds)

    @property
    def short_lived_holds(self):
        outstanding = self.holds_created_per_second * self.demand.short_life_seconds
        return outstanding * (1 - self.demand.survivor_share)

    @property
    def surviving_holds(self):
        holds_per_day = self.demand.daily_tx * self.demand.hold_share
        return holds_per_day * self.demand.survivor_share * self.policy.lifetime_days

    @property
    def requests_in_flight(self):
        return math.ceil(self.demand.peak_rate * self.demand.commit_latency_seconds)

    @property
    def records_per_day(self):
        return self.demand.daily_tx * self.demand.records_per_tx

    @property
    def unwritten_records(self):
        """What the flush window holds, and the window is an hour wide — so this follows the busiest
        hour, not the peak second and not the daily mean."""
        hour_records = self.demand.busiest_hour_tx * self.demand.records_per_tx
        return math.ceil(hour_records * self.policy.flush_window_hours)

    @property
    def resident_records(self):
        """Survivors only -- residency holds what was written and compacted, not everything appended.

        Driven by the **day**, not by the busiest hour repeated twenty-four times: the window is a day
        wide, and no day is twenty-four busiest hours.
        """
        return math.ceil(
            self.records_per_day
            * self.demand.survivor_share
            * (self.policy.residency_hours / 24)
        )

    @property
    def stored_records(self):
        """What the segment files hold: a day's survivors for every day still inside the lifetime."""
        return math.ceil(
            self.records_per_day * self.demand.survivor_share * self.policy.lifetime_days
        )

    def blocks_for(self, records):
        return math.ceil(records / self.units["records_per_block"])

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
                buckets_for(
                    math.ceil(demand.busiest_hour_tx * policy.idem_window_hours)
                ),
                "derived",
                "the busiest hour's transactions -- the window is an hour, and nothing enforces it yet",
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
                "the flush window's records: a recovery bound, so it follows the busiest hour",
            ),
            self._line(
                "pending resident blocks",
                self.blocks_for(self.resident_records),
                "derived",
                "survivors inside the residency window: a latency bound",
            ),
            self._line(
                "pending stored blocks",
                self.blocks_for(self.stored_records),
                "derived",
                "every day's survivors still inside retention + grace -- this is the disk figure",
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
    out.append(
        f"{'structure':<28}{'count':>14}{'B/unit':>8}{'bytes':>14}  where the count comes from"
    )
    for line in sizing.lines:
        flag = "  << OVER DIAL" if line.over_dial else ""
        out.append(
            f"{line.name:<28}{line.count:>14,}{line.unit_bytes:>8}{line.bytes:>14,}"
            f"  {line.kind}: {line.why}{flag}"
        )
    out.append("")
    out.append("memory by component")
    for owner, total in sizing.memory_by_component:
        share = total / sizing.memory_bytes * 100 if sizing.memory_bytes else 0
        out.append(f"  {owner:<26}{gigabytes(total):>8.2f} GB{share:>7.0f}%")
    out.append(f"  {'total':<26}{gigabytes(sizing.memory_bytes):>8.2f} GB")
    out.append("")
    out.append(
        f"disk {gigabytes(sizing.disk_bytes):.2f} GB in segment files "
        f"({sizing.stored_records:,} records at {sizing.unit_bytes('pending record')}B, "
        f"{sizing.units['records_per_block']} to a {sizing.units['block_bytes']}B block)"
    )
    out.append(f"live holds {sizing.live_holds:,}, requests in flight {sizing.requests_in_flight:,}")
    for breach in sizing.breaches:
        out.append(f"!! {breach}")
    return "\n".join(out)


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


if __name__ == "__main__":
    check_bucket_rule()
    # 300k/s peaking for ten minutes, 300M a day: the shape that started this, where peak and mean are
    # eighty-six times apart and picking either one alone is wrong by that factor.
    print(
        report(
            Sizing(
                Demand(
                    peak_rate=300_000,
                    peak_seconds=60,
                    busiest_hour_tx=30_000_000,
                    daily_tx=300_000_000,
                    accounts=10_000_000,
                ),
                Policy(),
            )
        )
    )
