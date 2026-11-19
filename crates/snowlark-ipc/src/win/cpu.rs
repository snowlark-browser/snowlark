// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CPU time as the processor counts it, for measurements that must not depend on what else the
//! machine is doing (`docs/perf-counters.md`).
//!
//! Windows charges each thread the time-stamp counter ticks of the intervals it was scheduled
//! (`QueryThreadCycleTime`): a thread that waits, or that another process's thread displaced, is
//! not charged. The counter runs at a fixed rate, so a core that runs slower (an efficiency core,
//! a lower turbo step, a hyperthread whose sibling is busy) charges more ticks for the same work;
//! `pin_to_performance_cores` takes the first source of that out, and `dedicate_core` lets two
//! processes measured against each other share one core, so that they see the same frequency,
//! caches and contention. Hardware event counters (instructions retired) are not readable from
//! user mode without an administrator configuring them, so they are not here.

use windows::Win32::System::SystemInformation::{
    CpuSetInformation, GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, GetProcessAffinityMask, SetProcessAffinityMask,
    SetProcessDefaultCpuSets, SetThreadAffinityMask, SetThreadPriority, SetThreadSelectedCpuSets,
    THREAD_PRIORITY_HIGHEST,
};
use windows::Win32::System::WindowsProgramming::{QueryProcessCycleTime, QueryThreadCycleTime};

/// The cycles charged to the calling thread so far.
pub fn thread_cycles() -> u64 {
    let mut cycles = 0u64;
    // SAFETY: the pseudo handle of the current thread is always valid, and `cycles` is a live u64
    // the call writes.
    let _ = unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut cycles) };
    cycles
}

/// The cycles charged to every thread of this process so far, those that ended included. A running thread's
/// latest cycles reach this count later than they reach the thread's own, so over a short interval the
/// process's difference can read a little below one of its threads'.
pub fn process_cycles() -> u64 {
    let mut cycles = 0u64;
    // SAFETY: as in `thread_cycles`, with the current process's pseudo handle.
    let _ = unsafe { QueryProcessCycleTime(GetCurrentProcess(), &mut cycles) };
    cycles
}

/// The cycles of the calling thread and of the process at one moment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cycles {
    pub thread: u64,
    pub process: u64,
}

impl Cycles {
    /// Now, for the calling thread.
    pub fn now() -> Cycles {
        Cycles {
            thread: thread_cycles(),
            process: process_cycles(),
        }
    }

    /// What was charged from `earlier` to `self`.
    pub fn since(self, earlier: Cycles) -> Cycles {
        Cycles {
            thread: self.thread.saturating_sub(earlier.thread),
            process: self.process.saturating_sub(earlier.process),
        }
    }

    /// Adds another interval's.
    pub fn add(&mut self, other: Cycles) {
        self.thread += other.thread;
        self.process += other.process;
    }
}

/// A logical processor of processor group 0, as the system's CPU sets describe it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuSet {
    /// The CPU set's id (what the CPU set functions take).
    pub id: u32,
    /// The logical processor's index (its bit in an affinity mask).
    pub processor: u8,
    /// The physical core (hyperthreads of one core share it).
    pub core: u8,
    /// Higher is faster: the performance cores of a hybrid processor.
    pub efficiency_class: u8,
}

/// The logical processors of processor group 0.
pub fn cpu_sets() -> Vec<CpuSet> {
    let mut len = 0u32;
    // SAFETY: a null buffer of length 0 only asks for the length, written to `len`.
    let _ = unsafe { GetSystemCpuSetInformation(None, 0, &mut len, None, None) };
    if len == 0 {
        return Vec::new();
    }
    // (u64 elements keep the records aligned for the struct's u64 field)
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `len` bytes, the length the call asked for.
    let ok = unsafe {
        GetSystemCpuSetInformation(
            Some(buf.as_mut_ptr().cast::<SYSTEM_CPU_SET_INFORMATION>()),
            len,
            &mut len,
            None,
            None,
        )
    };
    if !ok.as_bool() {
        return Vec::new();
    }
    let bytes = buf.as_ptr().cast::<u8>();
    let mut sets = Vec::new();
    let mut offset = 0usize;
    while offset + std::mem::size_of::<u32>() * 2 <= len as usize {
        // SAFETY: each record starts with its size and type and lies inside the `len` bytes the
        // call wrote; a CPU set record (the only type read) is a whole SYSTEM_CPU_SET_INFORMATION
        // at an offset that keeps it 8-byte aligned (records are multiples of 8 bytes).
        let record = unsafe { &*bytes.add(offset).cast::<SYSTEM_CPU_SET_INFORMATION>() };
        let size = record.Size as usize;
        if size == 0 || offset + size > len as usize {
            break;
        }
        if record.Type == CpuSetInformation {
            // SAFETY: the record is a CPU set's, so the union holds `CpuSet`.
            let set = unsafe { record.Anonymous.CpuSet };
            if set.Group == 0 && set.LogicalProcessorIndex < 64 {
                sets.push(CpuSet {
                    id: set.Id,
                    processor: set.LogicalProcessorIndex,
                    core: set.CoreIndex,
                    efficiency_class: set.EfficiencyClass,
                });
            }
        }
        offset += size;
    }
    sets
}

/// The logical processors (of processor group 0) of the highest efficiency class: the performance
/// cores of a hybrid processor, every processor of another. As a bit mask by processor index.
pub fn performance_core_mask() -> u64 {
    let cpus: Vec<(u8, u8)> = cpu_sets()
        .iter()
        .map(|c| (c.processor, c.efficiency_class))
        .collect();
    mask_of_class(&cpus)
}

/// The CPU set ids for the rest of a process whose thread has `core` to itself: the other cores of
/// the fastest class, or every other core when that class has no other.
fn ids_off_core(sets: &[CpuSet], core: u8) -> Vec<u32> {
    let top = sets.iter().map(|c| c.efficiency_class).max().unwrap_or(0);
    let fast: Vec<u32> = sets
        .iter()
        .filter(|c| c.core != core && c.efficiency_class == top)
        .map(|c| c.id)
        .collect();
    if !fast.is_empty() {
        return fast;
    }
    sets.iter()
        .filter(|c| c.core != core)
        .map(|c| c.id)
        .collect()
}

/// Runs the calling thread on logical processor `processor` only, at a raised priority, and moves
/// the process's other threads, those to come included, off that processor's core (to the other
/// cores of its class). Two processes given the two hyperthreads of one core then run their
/// measured threads side by side on it. Errs when the processor does not exist or the system
/// refuses.
pub fn dedicate_core(processor: u8) -> Result<(), String> {
    let sets = cpu_sets();
    let Some(target) = sets.iter().find(|c| c.processor == processor).copied() else {
        return Err(format!("no logical processor {processor}"));
    };
    let others = ids_off_core(&sets, target.core);
    // SAFETY: the pseudo handles of the current process and thread, a slice of CPU set ids the
    // system gave, and a mask with one processor that exists.
    unsafe {
        if !others.is_empty()
            && !SetProcessDefaultCpuSets(GetCurrentProcess(), Some(&others)).as_bool()
        {
            return Err("SetProcessDefaultCpuSets failed".into());
        }
        if !SetThreadSelectedCpuSets(GetCurrentThread(), &[target.id]).as_bool() {
            return Err("SetThreadSelectedCpuSets failed".into());
        }
        if SetThreadAffinityMask(GetCurrentThread(), 1usize << processor) == 0 {
            return Err(format!(
                "processor {processor} is outside the process's affinity"
            ));
        }
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST)
            .map_err(|e| format!("SetThreadPriority: {e}"))?;
    }
    Ok(())
}

/// The processors of the highest efficiency class among `cpus` (processor, class), as a mask.
fn mask_of_class(cpus: &[(u8, u8)]) -> u64 {
    let Some(top) = cpus.iter().map(|&(_, class)| class).max() else {
        return 0;
    };
    cpus.iter()
        .filter(|&&(_, class)| class == top)
        .fold(0u64, |mask, &(cpu, _)| mask | 1u64 << cpu)
}

/// Restricts this process's threads, those to come included, to the performance cores (within
/// what the process may already use). Returns the mask it runs on now, or `None` when there was
/// nothing to restrict or the system refused.
pub fn pin_to_performance_cores() -> Option<u64> {
    let (mut process, mut system) = (0usize, 0usize);
    // SAFETY: the current process's pseudo handle, and two live usize the call writes.
    unsafe { GetProcessAffinityMask(GetCurrentProcess(), &mut process, &mut system) }.ok()?;
    let mask = performance_core_mask() as usize & process;
    if mask == 0 || mask == process {
        return None;
    }
    // SAFETY: the current process's pseudo handle and a non-empty subset of its own mask.
    unsafe { SetProcessAffinityMask(GetCurrentProcess(), mask) }.ok()?;
    Some(mask as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_of_class_keeps_the_fastest_class() {
        assert_eq!(mask_of_class(&[]), 0);
        assert_eq!(mask_of_class(&[(0, 1), (1, 1), (2, 0), (3, 0)]), 0b0011);
        assert_eq!(mask_of_class(&[(0, 0), (1, 0)]), 0b0011);
        assert_eq!(mask_of_class(&[(5, 2), (63, 2), (1, 0)]), 1 << 5 | 1 << 63);
    }

    #[test]
    fn a_thread_is_charged_for_its_work_and_not_for_sleeping() {
        let start = Cycles::now();
        let mut x = 0u64;
        for i in 0..20_000_000u64 {
            x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(i));
        }
        let busy = Cycles::now().since(start);
        assert!(busy.thread > 1_000_000, "{busy:?}");
        // (the process's count lags its running threads': see `process_cycles`)
        assert!(busy.process >= busy.thread / 2, "{busy:?}");
        let start = Cycles::now();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let slept = Cycles::now().since(start);
        // (200 ms awake would be hundreds of millions of cycles)
        assert!(slept.thread < 20_000_000, "{slept:?}");
    }

    #[test]
    fn the_rest_of_the_process_goes_to_the_other_fast_cores() {
        let set = |id, processor, core, efficiency_class| CpuSet {
            id,
            processor,
            core,
            efficiency_class,
        };
        // (two performance cores of two hyperthreads, two efficiency cores)
        let sets = [
            set(100, 0, 0, 1),
            set(101, 1, 0, 1),
            set(102, 2, 2, 1),
            set(103, 3, 2, 1),
            set(104, 4, 4, 0),
            set(105, 5, 5, 0),
        ];
        assert_eq!(ids_off_core(&sets, 0), [102, 103]);
        assert_eq!(ids_off_core(&sets, 2), [100, 101]);
        // (a lone fast core: the rest goes anywhere else)
        assert_eq!(ids_off_core(&sets[2..], 2), [104, 105]);
    }

    #[test]
    fn a_dedicated_thread_runs_on_its_processor() {
        // (in a thread of its own: the test harness's threads keep their placement, though the
        // process default changes for the threads of this test binary that come later)
        let sets = cpu_sets();
        let last = sets.iter().map(|c| c.processor).max().unwrap();
        let on = std::thread::spawn(move || {
            dedicate_core(last).unwrap();
            let mut seen = Vec::new();
            for _ in 0..50 {
                // SAFETY: no arguments; it returns the processor the thread runs on.
                seen.push(unsafe {
                    windows::Win32::System::Threading::GetCurrentProcessorNumber()
                });
                std::thread::yield_now();
            }
            seen
        })
        .join()
        .unwrap();
        assert!(on.iter().all(|&p| p == last as u32), "{on:?}");
        assert!(dedicate_core(200).is_err());
    }

    #[test]
    fn the_performance_cores_are_some_of_the_processors() {
        let mask = performance_core_mask();
        assert_ne!(mask, 0);
        let all = std::thread::available_parallelism().map_or(1, |n| n.get());
        assert!(mask.count_ones() as usize <= all);
    }
}
