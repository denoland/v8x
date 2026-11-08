//! Family: Locker
//!
//! `v8::Locker` and `v8::Unlocker` over a QuickJS runtime.
//!
//! A `JSRuntime` is not thread safe, but it can move between threads as long
//! as only one thread uses it at a time. That is the contract `v8::Locker`
//! states, so the shim implements it with a per-isolate mutex. Two QuickJS
//! specifics ride along:
//!
//! - The runtime records the stack bounds it uses for its overflow check when
//!   it is created. Those bounds belong to the creating thread, so a runtime
//!   that migrates must be told the new ones with `JS_UpdateStackTop`, or the
//!   check compares against a foreign stack and either fires immediately or
//!   never fires at all. The lock calls it whenever the entering thread
//!   differs from the last one that ran the runtime.
//! - `v8__Locker__IsLocked` sits on the `Global` clone, drop, hash and compare
//!   paths, so the owning thread is published in an atomic and read without
//!   taking the mutex.

use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use crate::isolate::RealIsolate;

use super::core::iso_state;
use super::quickjs_sys::JS_UpdateStackTop;

/// Identifies a thread by a small integer. `std::thread::ThreadId::as_u64` is
/// unstable, and a `u64` thread-local has no destructor, so reading this stays
/// valid even from a late TLS destructor where `LocalKey::with` would panic
/// for a droppable type.
fn thread_id() -> u64 {
  static NEXT: AtomicU64 = AtomicU64::new(1);
  thread_local! {
    static ID: u64 = NEXT.fetch_add(1, Ordering::Relaxed);
  }
  ID.with(|id| *id)
}

/// The isolate's lock. Zero in `owner` means unlocked; ids start at 1.
pub(crate) struct IsolateLock {
  owner: Mutex<Owner>,
  released: Condvar,
  /// `owner.tid` republished for lock-free reads. Written under `owner`.
  owner_tid: AtomicU64,
}

struct Owner {
  tid: u64,
  /// How many times the owner took the lock. `v8::Locker` is recursive in
  /// C++, so the count is tracked even though the Rust wrapper rejects
  /// recursive locking.
  depth: usize,
  /// The last thread to hold the lock, which is the thread whose stack bounds
  /// the runtime currently holds.
  last_tid: u64,
}

impl IsolateLock {
  pub(crate) fn new() -> Self {
    Self {
      owner: Mutex::new(Owner {
        tid: 0,
        depth: 0,
        last_tid: 0,
      }),
      released: Condvar::new(),
      owner_tid: AtomicU64::new(0),
    }
  }

  fn acquire(&self, isolate: *mut RealIsolate) {
    let me = thread_id();
    let mut owner = self.owner.lock().unwrap();
    while owner.tid != 0 && owner.tid != me {
      owner = self.released.wait(owner).unwrap();
    }
    owner.tid = me;
    owner.depth += 1;
    self.owner_tid.store(me, Ordering::Release);
    let migrated = owner.last_tid != me;
    owner.last_tid = me;
    // Under the mutex on purpose: the runtime must not be executing anywhere
    // else while its stack bounds are rewritten.
    if migrated && owner.depth == 1 {
      let rt = iso_state(isolate).rt;
      if !rt.is_null() {
        unsafe { JS_UpdateStackTop(rt) };
      }
    }
  }

  /// Give up one level of the lock.
  fn release(&self) {
    let mut owner = self.owner.lock().unwrap();
    owner.depth -= 1;
    if owner.depth == 0 {
      owner.tid = 0;
      self.owner_tid.store(0, Ordering::Release);
      self.released.notify_one();
    }
  }

  /// Give up every level at once and report how many there were, for
  /// `v8::Unlocker`, which releases the lock outright however deeply the
  /// thread holds it.
  fn release_all(&self) -> usize {
    let mut owner = self.owner.lock().unwrap();
    let depth = owner.depth;
    owner.depth = 0;
    owner.tid = 0;
    self.owner_tid.store(0, Ordering::Release);
    self.released.notify_one();
    depth
  }

  fn reacquire(&self, isolate: *mut RealIsolate, depth: usize) {
    self.acquire(isolate);
    if depth > 1 {
      let mut owner = self.owner.lock().unwrap();
      owner.depth = depth;
    }
  }

  fn held_by_current_thread(&self) -> bool {
    self.owner_tid.load(Ordering::Acquire) == thread_id()
  }
}

fn lock_of<'a>(isolate: *mut RealIsolate) -> &'a IsolateLock {
  &iso_state(isolate).lock
}

/// `RawLocker` in `vendor/rusty_v8/src/locker.rs`: two words the Rust side
/// allocates and never reads. Slot 0 holds the isolate, slot 1 is unused.
#[repr(C)]
pub struct RawLocker([*mut RealIsolate; 2]);

/// `RawUnlocker`: one word, holding the isolate its window belongs to. The
/// released depth rides on a thread-local stack instead, because unlock
/// windows are strictly nested per thread and one word cannot carry both.
#[repr(C)]
pub struct RawUnlocker([*mut RealIsolate; 1]);

thread_local! {
  static UNLOCK_DEPTHS: std::cell::RefCell<Vec<usize>> =
    const { std::cell::RefCell::new(Vec::new()) };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Locker__CONSTRUCT(
  buf: *mut RawLocker,
  isolate: *mut RealIsolate,
) {
  lock_of(isolate).acquire(isolate);
  unsafe { (*buf).0 = [isolate, std::ptr::null_mut()] };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Locker__DESTRUCT(this: *mut RawLocker) {
  let isolate = unsafe { (*this).0[0] };
  if isolate.is_null() {
    return;
  }
  lock_of(isolate).release();
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Locker__IsLocked(isolate: *const RealIsolate) -> bool {
  if isolate.is_null() {
    return false;
  }
  lock_of(isolate as *mut RealIsolate).held_by_current_thread()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Unlocker__CONSTRUCT(
  buf: *mut RawUnlocker,
  isolate: *mut RealIsolate,
) {
  let depth = lock_of(isolate).release_all();
  UNLOCK_DEPTHS.with(|d| d.borrow_mut().push(depth));
  unsafe { (*buf).0 = [isolate] };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Unlocker__DESTRUCT(this: *mut RawUnlocker) {
  let isolate = unsafe { (*this).0[0] };
  if isolate.is_null() {
    return;
  }
  let depth = UNLOCK_DEPTHS.with(|d| d.borrow_mut().pop()).unwrap_or(1);
  lock_of(isolate).reacquire(isolate, depth);
}
