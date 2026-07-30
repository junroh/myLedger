/// Where the OS lets us say it, the reactor thread should own a core: measurements taken
/// while the scheduler is free to migrate the thread mix core-local effects with placement
/// noise. The calls come from `libc` rather than declarations written here, because a signature
/// that is subtly wrong is undefined behaviour the compiler cannot see.
pub struct ThreadPolicy;

impl ThreadPolicy {
    /// Applies the strongest placement the platform offers to the calling thread and
    /// returns what was actually applied, for the record in a report.
    pub fn apply(cpu: Option<usize>) -> &'static str {
        match cpu {
            Some(cpu) if sys::pin_current(cpu) => Self::PINNED,
            _ => Self::fallback(),
        }
    }

    fn fallback() -> &'static str {
        if sys::prefer_performance_cores() {
            Self::PERFORMANCE_QOS
        } else {
            Self::SCHEDULER_DEFAULT
        }
    }
}

impl ThreadPolicy {
    /// Every placement a report may show. A number measured under one of these is not comparable
    /// with a number measured under another, which is why they are named and printed.
    pub const PINNED: &'static str = "pinned";
    pub const PERFORMANCE_QOS: &'static str = "performance-qos";
    pub const SCHEDULER_DEFAULT: &'static str = "scheduler-default";
}

#[cfg(target_os = "linux")]
mod sys {
    pub fn pin_current(cpu: usize) -> bool {
        if cpu >= libc::CPU_SETSIZE as usize {
            return false;
        }
        // Safety: zeroed is a valid empty set, and the set outlives the call. Pid 0 is the caller.
        unsafe {
            let mut set: libc::cpu_set_t = core::mem::zeroed();
            libc::CPU_SET(cpu, &mut set);
            libc::sched_setaffinity(0, core::mem::size_of::<libc::cpu_set_t>(), &set) == 0
        }
    }

    pub fn prefer_performance_cores() -> bool {
        false
    }
}

/// macOS exposes no affinity control on Apple Silicon; raising the QoS class is the only
/// way to keep a thread off the efficiency cores.
#[cfg(target_os = "macos")]
mod sys {
    pub fn pin_current(_cpu: usize) -> bool {
        false
    }

    pub fn prefer_performance_cores() -> bool {
        // Safety: sets a policy on the calling thread and touches no memory we own.
        unsafe {
            libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
                == 0
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod sys {
    pub fn pin_current(_cpu: usize) -> bool {
        false
    }

    pub fn prefer_performance_cores() -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Placement is reported with every measurement, so it has to be one of the names a reader can
    /// compare against — never an empty string or a silent lie about pinning. On a platform without
    /// affinity control, asking for a core falls back rather than failing.
    #[test]
    fn a_placement_is_always_one_of_the_named_ones() {
        let known = [
            ThreadPolicy::PINNED,
            ThreadPolicy::PERFORMANCE_QOS,
            ThreadPolicy::SCHEDULER_DEFAULT,
        ];
        for request in [None, Some(0), Some(usize::MAX)] {
            let placement = ThreadPolicy::apply(request);
            assert!(known.contains(&placement), "unknown placement {placement:?}");
        }
        assert_ne!(
            ThreadPolicy::apply(Some(usize::MAX)),
            ThreadPolicy::PINNED,
            "a core that cannot exist must not report as pinned"
        );
    }
}
