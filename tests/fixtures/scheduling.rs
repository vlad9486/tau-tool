// SPDX-FileCopyrightText: 2026 Vladyslav Melnyk
// SPDX-License-Identifier: GPL-3.0-or-later
// See LICENSE for the full license text.

// Host checks for actual slot validation and thread handoff code, plus a small
// exhaustive interleaving model. Compiled by tests/scheduling.rs; no RISC-V assembly.
#![allow(dead_code)]
extern crate self as tau;
#[path = "@COMMON@"]
mod common;
#[path = "@ERROR@"]
mod error;
pub use self::error::{ModuleError, WaitError};

mod vmem {
    use std::collections::BTreeMap;
    pub const PAGE: usize = 4096;
    #[derive(Clone, Copy)]
    pub struct Root(pub core::num::NonZeroUsize);
    #[derive(Default)]
    pub struct WindowAccess {
        pub mappings: BTreeMap<usize, (usize, usize)>,
    }
    impl WindowAccess {
        pub fn resolve(&mut self, _: Root, address: usize) -> Option<(usize, usize)> {
            self.mappings
                .get(&(address & !(PAGE - 1)))
                .map(|&(p, f)| (p + address % PAGE, f))
        }
        pub fn page_pointer(&mut self, physical: usize) -> *mut u8 {
            physical as *mut u8
        }
        pub fn wait_mut(&mut self, physical: usize) -> &mut [u8] {
            unsafe { core::slice::from_raw_parts_mut(physical as *mut u8, PAGE) }
        }
        pub fn page_mut(&mut self, physical: usize) -> &mut [u8] {
            unsafe { core::slice::from_raw_parts_mut(physical as *mut u8, PAGE) }
        }
    }
    pub fn user_range(address: usize, bytes: usize) -> bool {
        bytes != 0
            && address
                .checked_add(bytes - 1)
                .is_some_and(|end| end < (1 << 38))
    }
}
mod state {
    use std::{
        alloc::{alloc_zeroed, dealloc, Layout},
        cell::RefCell,
    };
    #[derive(Default)]
    pub struct Context {
        allocations: RefCell<Vec<(usize, Layout)>>,
    }
    impl Context {
        pub fn block(&self, _: usize, order: u32) -> Result<usize, ()> {
            if order > 10 {
                return Err(());
            }
            let layout = Layout::from_size_align(4096 << order, 4096).unwrap();
            let address = unsafe { alloc_zeroed(layout) } as usize;
            if address == 0 {
                return Err(());
            }
            self.allocations.borrow_mut().push((address, layout));
            Ok(address)
        }
        pub fn free_block(&self, _: usize, address: usize, _: u32) -> Result<(), ()> {
            let mut allocations = self.allocations.borrow_mut();
            let index = allocations
                .iter()
                .position(|&(p, _)| p == address)
                .ok_or(())?;
            let (_, layout) = allocations.remove(index);
            unsafe { dealloc(address as *mut u8, layout) };
            Ok(())
        }
        pub fn blocks(&self) -> usize {
            self.allocations.borrow().len()
        }
    }
    impl Drop for Context {
        fn drop(&mut self) {
            for &(address, layout) in self.allocations.get_mut().iter() {
                unsafe { dealloc(address as *mut u8, layout) };
            }
        }
    }
}
mod space {
    pub use super::vmem::*;
}
#[path = "@WAITING@"]
mod waiting;
mod matter {
    pub use super::waiting::*;
}
#[path = "@THREAD@"]
mod thread;

use self::{
    vmem::{Root, WindowAccess},
    state::Context,
    waiting::WaitTable,
    thread::{ActiveWait, Phase, Resume, ThreadState},
};

impl Clone for ActiveWait {
    fn clone(&self) -> Self {
        ActiveWait {
            module: self.module,
            table: self.table.unsafe_clone(),
            slot: self.slot,
            sequence: self.sequence,
        }
    }
}

impl Clone for ThreadState {
    fn clone(&self) -> Self {
        ThreadState {
            phase: self.phase,
            root: self.root,
            resume: self.resume,
            message: self.message,
            sequence: self.sequence,
            wait: self.wait.clone(),
        }
    }
}

fn root() -> Root {
    Root(core::num::NonZeroUsize::new(1).unwrap())
}
fn parked(phase: Phase) -> ThreadState {
    ThreadState {
        phase,
        root: root(),
        resume: Resume::Syscall,
        message: [0; 6],
        sequence: 1,
        wait: Some(ActiveWait {
            module: 0x1000,
            table: WaitTable::EMPTY,
            slot: 0,
            sequence: 1,
        }),
    }
}

#[test]
fn slot_abi_and_all_pointer_boundaries() {
    assert_eq!(size_of::<common::WaitSlot>(), 64);
    assert_eq!(align_of::<common::WaitSlot>(), 64);
    assert_eq!(core::mem::offset_of!(common::WaitSlot, occupied), 63);
    let context = Context::default();
    let mut window = WindowAccess::default();
    let backing = context.block(0, 0).unwrap();
    window.mappings.insert(0x10000, (backing, 22));
    let table = WaitTable::prepare(&mut window, &context, 0, 0, 0x10000, 64).unwrap();
    for index in 0..64 {
        assert_eq!(table.index(0x10000 + index * 64), Ok(index));
        for byte in 1..64 {
            assert_eq!(
                table.index(0x10000 + index * 64 + byte),
                Err(WaitError::InvalidWaiter)
            );
        }
    }
    for pointer in [0, 0xffc0, 0x11000, usize::MAX] {
        assert_eq!(table.index(pointer), Err(WaitError::InvalidWaiter));
    }
    assert_eq!(WaitTable::EMPTY.index(0), Err(WaitError::InvalidWaiter));
    // Do not create an invalid Rust bool when userspace supplies a hostile byte.
    for byte in 0..=255u8 {
        unsafe { (backing as *mut u8).add(63).write(byte) };
        let expected = match byte {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(WaitError::InvalidWaiter),
        };
        assert_eq!(table.occupied(&mut window, root(), 0), expected);
    }
    table.release(&context, 0);
    assert_eq!(context.blocks(), 1);
}

#[test]
fn table_validation_precedes_allocation_and_records_cross_pages() {
    let context = Context::default();
    let mut window = WindowAccess::default();
    let first = context.block(0, 0).unwrap();
    let second = context.block(0, 0).unwrap();
    window.mappings.insert(0x10000, (first, 22));
    window.mappings.insert(0x11000, (second, 22));
    for (base, count) in [
        (0, 1),
        (0x10001, 1),
        (0x10000, usize::MAX),
        (usize::MAX - 63, 2),
        ((1 << 30) - 64, 2),
    ] {
        assert!(WaitTable::prepare(&mut window, &context, 0, 0, base, count).is_err());
        assert_eq!(context.blocks(), 2);
    }
    assert_eq!(
        WaitTable::prepare(&mut window, &context, 0, 0, 0x10000, 0).unwrap().count,
        0
    );
    assert_eq!(context.blocks(), 2);
    let table = WaitTable::prepare(&mut window, &context, 0, 0, 0x10000, 128).unwrap();
    for index in 0..128 {
        let mut record = unsafe { table.read(&mut window, index) };
        assert!(!record.pending && record.thread_id == 0 && record.sequence == 0);
        record.pending = true;
        record.thread_id = index + 1;
        record.sequence = index + 10;
        unsafe { table.write(&mut window, index, record) };
    }
    for index in 0..128 {
        let record = unsafe { table.read(&mut window, index) };
        assert!(record.pending);
        assert_eq!(record.thread_id, index + 1);
        assert_eq!(record.sequence, index + 10);
    }
    table.release(&context, 0);
    assert_eq!(context.blocks(), 2);
}

#[test]
fn early_completion_preserves_hart_ownership_and_late_completion_enqueues_once() {
    let mut early = parked(Phase::Parking);
    assert!(!early.complete([0, 1, 0, 0, 0, 0]));
    assert_eq!(early.phase, Phase::Parking);
    assert!(!early.commit_parking());
    assert_eq!(early.phase, Phase::Running);
    assert!(early.wait.is_none());
    let mut late = parked(Phase::Parking);
    assert!(late.commit_parking());
    assert_eq!(late.phase, Phase::Parked);
    assert!(late.complete([0, 0, 0, 0, 0, 0]));
    assert_eq!(late.phase, Phase::Runnable);
    assert!(late.wait.is_none()); // no outstanding deadline or slot reference
}

// Each userspace reservation, readiness check/publication, notification, and
// parking commitment is a distinct step. This explores Notify both before Wait
// and between recording Parking and relinquishing running ownership.
#[derive(Clone)]
struct Model {
    client: u8,
    server: u8,
    reserved: bool,
    ready: bool,
    pending: bool,
    parked: ThreadState,
    queued: usize,
    running: bool,
}
fn explore(model: Model, terminal: &mut usize, early: &mut usize) {
    assert!(model.queued <= 1);
    assert!(!(model.queued != 0 && model.running));
    if model.client == 4 && model.server == 2 {
        assert!(model.ready);
        assert_ne!(model.parked.phase, Phase::Parked, "lost wakeup");
        *terminal += 1;
        return;
    }
    if model.client < 4 {
        let mut next = model.clone();
        match next.client {
            0 => {
                next.reserved = true;
                next.client = 1;
            }
            1 => {
                next.client = if next.ready { 4 } else { 2 };
            }
            2 => {
                if next.pending {
                    next.pending = false;
                    next.client = 4;
                } else {
                    next.parked = parked(Phase::Parking);
                    next.client = 3;
                }
            }
            3 => {
                if next.parked.commit_parking() {
                    next.running = false;
                }
                next.client = 4;
            }
            _ => unreachable!(),
        }
        explore(next, terminal, early);
    }
    // The server obtains the request/slot only after reservation/publication.
    if model.reserved && model.server < 2 {
        let mut next = model;
        if next.server == 0 {
            next.ready = true;
        } else if next.parked.wait.is_some() {
            if next.parked.phase == Phase::Parking {
                *early += 1;
            }
            if next.parked.complete([0, 1, 0, 0, 0, 0]) {
                next.queued += 1;
            }
        } else {
            next.pending = true;
        }
        next.server += 1;
        explore(next, terminal, early);
    }
}
#[test]
fn all_readiness_notification_and_parking_interleavings_preserve_progress() {
    let mut initial = parked(Phase::Running);
    initial.wait = None;
    let mut terminal = 0;
    let mut early = 0;
    explore(
        Model {
            client: 0,
            server: 0,
            reserved: false,
            ready: false,
            pending: false,
            parked: initial,
            queued: 0,
            running: true,
        },
        &mut terminal,
        &mut early,
    );
    assert!(terminal > 5 && early > 0);
}

#[test]
fn stale_sequences_cannot_match_reused_slots_or_other_modules() {
    let old = parked(Phase::Parked).wait.unwrap();
    let mut later = old.clone();
    let matches = |a: &ActiveWait, b: &ActiveWait| {
        a.module == b.module && a.slot == b.slot && a.sequence == b.sequence
    };
    later.sequence += 1;
    assert!(!matches(&old, &later));
    later = old.clone();
    later.module += 4096;
    assert!(!matches(&old, &later));
    later = old.clone();
    later.slot += 1;
    assert!(!matches(&old, &later));
    assert!(matches(&old, &old));
}
