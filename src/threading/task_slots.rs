//! Thread-pool work items that stay owned by (and shared with) their
//! scheduler while they run: [`SharedTask`] is the queue node, [`TaskSlots`]
//! a fixed set of them addressed by index.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::thread_pool::{Batch, Node, Task, ThreadPool};

/// An intrusive pool node for a value other threads keep referring to while
/// it is queued. The pool links it through the `UnsafeCell`; `queued` makes
/// "one outstanding schedule per node" structural rather than a caller rule.
#[repr(C)]
pub struct SharedTask {
    /// First, so the pool's `*mut Task` is also this struct's address.
    task: UnsafeCell<Task>,
    queued: AtomicBool,
}

// SAFETY: `task` is only written (by us in `try_queue`, then by the pool)
// between a successful `try_queue` and `release`, which `queued` serializes.
unsafe impl Sync for SharedTask {}
// SAFETY: as above.
unsafe impl Send for SharedTask {}

impl SharedTask {
    pub const fn new(callback: unsafe fn(*mut Task)) -> Self {
        Self {
            task: UnsafeCell::new(Task {
                node: Node {
                    next: core::ptr::null_mut(),
                },
                callback,
            }),
            queued: AtomicBool::new(false),
        }
    }

    /// Claim the node for one trip through the pool, to run `callback`;
    /// `None` if it is already queued or running.
    #[inline]
    pub fn try_queue(&self, callback: unsafe fn(*mut Task)) -> Option<*mut Task> {
        if self.queued.swap(true, Ordering::AcqRel) {
            return None;
        }
        let task = self.task.get();
        // SAFETY: we hold the claim, so nothing else touches the node.
        unsafe { (*task).callback = callback };
        Some(task)
    }

    /// The callback is done with the node; it may be queued again.
    #[inline]
    pub fn release(&self) {
        self.queued.store(false, Ordering::Release);
    }
}

/// One unit of work in a [`TaskSlots`]. `run` executes on a worker thread
/// while the scheduling thread keeps `&Self`.
///
/// # Safety
/// `run` may only touch state that is synchronized with every other holder
/// of `&Self` (the value is shared across threads for the run even if it is
/// not `Sync`). [`slot_task!`](crate::slot_task) states that at the type.
pub unsafe trait SlotTask: Sized {
    fn run(&self);
}

/// Implements [`SlotTask`] by forwarding to an inherent `fn run_slot(&self)`,
/// the type's worker-thread entry point: it must only touch state
/// synchronized with the scheduling thread.
#[macro_export]
macro_rules! slot_task {
    ([$($gen:tt)*] $ty:ty) => {
        // SAFETY: see the macro doc — `run_slot` is the type's cross-thread entry point.
        unsafe impl<$($gen)*> $crate::task_slots::SlotTask for $ty {
            #[inline]
            fn run(&self) {
                <$ty>::run_slot(self)
            }
        }
    };
    ($ty:ty) => {
        $crate::slot_task!([] $ty);
    };
}

#[repr(C)]
struct Slot<T> {
    /// First, so the pool's `*mut Task` is also the slot's address.
    task: SharedTask,
    /// The owning [`TaskSlots`]' outstanding-work count.
    pending: *const AtomicUsize,
    value: T,
}

/// `len` tasks addressed by index; [`schedule`](Self::schedule) runs slot
/// `i`'s [`SlotTask::run`] on the pool. A slot that is already queued or
/// running is not queued again. Dropping waits for every outstanding run,
/// so a queued slot never outlives its storage.
pub struct TaskSlots<T: SlotTask> {
    slots: Box<[Slot<T>]>,
    /// Queued-or-running slots; boxed so the slots can point at it.
    pending: Box<AtomicUsize>,
}

// SAFETY: `Slot::pending` points into our own `Box<AtomicUsize>`; everything
// else is `T` (bounded) and `SharedTask` (`Sync`).
unsafe impl<T: SlotTask + Sync> Sync for TaskSlots<T> {}
// SAFETY: as above.
unsafe impl<T: SlotTask + Send> Send for TaskSlots<T> {}

impl<T: SlotTask> TaskSlots<T> {
    pub fn new(values: impl IntoIterator<Item = T>) -> Self {
        let pending = Box::new(AtomicUsize::new(0));
        let pending_ptr: *const AtomicUsize = &raw const *pending;
        Self {
            slots: values
                .into_iter()
                .map(|value| Slot {
                    task: SharedTask::new(run_slot::<T>),
                    pending: pending_ptr,
                    value,
                })
                .collect(),
            pending,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    #[inline]
    pub fn get(&self, index: usize) -> &T {
        &self.slots[index].value
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.slots.iter().map(|slot| &slot.value)
    }

    fn claim(&self, index: usize) -> Option<*mut Task> {
        let slot: *const Slot<T> = &raw const self.slots[index];
        // SAFETY: `slot` is a live element of `self.slots`.
        unsafe { &(*slot).task }.try_queue(run_slot::<T>)?;
        self.pending.fetch_add(1, Ordering::AcqRel);
        // The whole-slot pointer (not a reborrow of the `task` field) so the
        // callback's cast back to `Slot<T>` has provenance over `value`.
        Some(slot.cast::<Task>().cast_mut())
    }

    /// Add slot `index` to `batch` (no-op if it is already queued or running).
    pub fn push(&self, index: usize, batch: &mut Batch) {
        if let Some(task) = self.claim(index) {
            batch.push(Batch::from(task));
        }
    }

    /// Run slot `index` on `pool` (no-op if it is already queued or running).
    pub fn schedule(&self, pool: &ThreadPool, index: usize) {
        if let Some(task) = self.claim(index) {
            pool.schedule(Batch::from(task));
        }
    }
}

impl<T: SlotTask> Drop for TaskSlots<T> {
    fn drop(&mut self) {
        // A queued slot points into `slots`; wait it out rather than free it.
        while self.pending.load(Ordering::Acquire) != 0 {
            std::thread::yield_now();
        }
    }
}

unsafe fn run_slot<T: SlotTask>(task: *mut Task) {
    // SAFETY: `task` is the address of a `#[repr(C)] Slot<T>` (see `claim`)
    // inside a `TaskSlots<T>` that `Drop` keeps alive while `pending` is
    // non-zero; the pool only runs what `claim` queued.
    let slot = unsafe { &*task.cast::<Slot<T>>() };
    slot.value.run();
    let pending = slot.pending;
    slot.task.release();
    // SAFETY: `pending` is the owning `TaskSlots`' boxed counter, alive until
    // it reads zero (see `Drop`); `slot` is not touched past `release`.
    unsafe { &*pending }.fetch_sub(1, Ordering::AcqRel);
}
