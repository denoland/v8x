//! QuickJS-backed ArrayBuffer::Allocator shims.
#![allow(non_snake_case)]

use crate::Allocator;
use crate::array_buffer::RustAllocatorVtable;
use crate::support::{SharedPtrBase, UniquePtr, long};
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

struct QjsArrayBufferAllocator {
  handle: *const c_void,
  vtable: *const RustAllocatorVtable<c_void>,
}

unsafe extern "C" {
  fn calloc(count: usize, size: usize) -> *mut c_void;
  fn free(ptr: *mut c_void);
}

#[inline]
fn as_qjs_allocator(
  ptr: *mut Allocator,
) -> Option<&'static QjsArrayBufferAllocator> {
  unsafe { (ptr as *const QjsArrayBufferAllocator).as_ref() }
}

#[inline]
pub(crate) fn allocator_shared_copy(
  ptr: *const SharedPtrBase<Allocator>,
) -> SharedPtrBase<Allocator> {
  std__shared_ptr__v8__ArrayBuffer__Allocator__COPY(ptr)
}

#[inline]
pub(crate) fn allocator_shared_get(
  ptr: *const SharedPtrBase<Allocator>,
) -> *mut Allocator {
  std__shared_ptr__v8__ArrayBuffer__Allocator__get(ptr)
}

#[inline]
pub(crate) fn allocator_is_rust(ptr: *mut Allocator) -> bool {
  as_qjs_allocator(ptr).is_some_and(|a| !a.vtable.is_null())
}

pub(crate) unsafe fn allocator_allocate(
  ptr: *mut Allocator,
  byte_length: usize,
  zeroed: bool,
) -> *mut c_void {
  let Some(alloc) = as_qjs_allocator(ptr) else {
    return ptr::null_mut();
  };
  if byte_length == 0 {
    return ptr::null_mut();
  }
  if alloc.vtable.is_null() {
    return unsafe { calloc(byte_length, 1) };
  }

  let handle = unsafe { &*(alloc.handle) };
  if zeroed {
    unsafe { ((*alloc.vtable).allocate)(handle, byte_length) }
  } else {
    unsafe { ((*alloc.vtable).allocate_uninitialized)(handle, byte_length) }
  }
}

pub(crate) unsafe fn allocator_free(
  ptr: *mut Allocator,
  data: *mut c_void,
  byte_length: usize,
) {
  if data.is_null() {
    return;
  }
  let Some(alloc) = as_qjs_allocator(ptr) else {
    unsafe { free(data) };
    return;
  };
  if alloc.vtable.is_null() {
    unsafe { free(data) };
    return;
  }

  let handle = unsafe { &*(alloc.handle) };
  unsafe { ((*alloc.vtable).free)(handle, data, byte_length) };
}

// The allocator `std::shared_ptr` is modelled with a real, atomically
// refcounted control block so that `clone()` / drop behave like C++'s
// shared_ptr — i.e. `use_count()` is accurate and the underlying allocator is
// freed exactly once (when the last reference drops). The previous
// implementation bitwise-copied the two words and freed on every `reset`, which
// (a) reported a bogus use_count of 1 and (b) double-freed when more than one
// `SharedRef`/`SharedPtr` pointed at the same allocator — an abort that took
// down the whole `test_api` binary (`backing_store_segfault` et al.).
//
// `SharedPtrBase<Allocator>` is `[usize; 2]`:
//   word[0] = the `*mut Allocator` object pointer (0 when null)
//   word[1] = a `*mut AtomicUsize` control block (0 when null)
fn read_words(ptr: *const SharedPtrBase<Allocator>) -> (usize, usize) {
  if ptr.is_null() {
    return (0, 0);
  }
  let w = ptr as *const usize;
  unsafe { (*w, *w.add(1)) }
}

unsafe fn write_words(
  ptr: *mut SharedPtrBase<Allocator>,
  obj: usize,
  ctrl: usize,
) {
  let w = ptr as *mut usize;
  unsafe {
    *w = obj;
    *w.add(1) = ctrl;
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Allocator__NewDefaultAllocator()
-> *mut Allocator {
  Box::into_raw(Box::new(QjsArrayBufferAllocator {
    handle: ptr::null(),
    vtable: ptr::null(),
  })) as *mut Allocator
}

/// Every isolate group can have its own allocator here (see
/// `IsolateGroup::CanCreateNewGroups`), and they need no group-specific state.
#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Allocator__NewDefaultAllocator__with_group(
  _group: *const super::isolate::IsolateGroupHandle,
) -> *mut Allocator {
  v8__ArrayBuffer__Allocator__NewDefaultAllocator()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Allocator__NewRustAllocator(
  handle: *const c_void,
  vtable: *const RustAllocatorVtable<c_void>,
) -> *mut Allocator {
  Box::into_raw(Box::new(QjsArrayBufferAllocator { handle, vtable }))
    as *mut Allocator
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Allocator__DELETE(this: *mut Allocator) {
  if this.is_null() {
    return;
  }
  let alloc = unsafe { Box::from_raw(this as *mut QjsArrayBufferAllocator) };
  if !alloc.vtable.is_null() {
    unsafe { ((*alloc.vtable).drop)(alloc.handle) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__ArrayBuffer__Allocator__COPY(
  ptr: *const SharedPtrBase<Allocator>,
) -> SharedPtrBase<Allocator> {
  let (obj, ctrl) = read_words(ptr);
  if ctrl != 0 {
    unsafe { (*(ctrl as *const AtomicUsize)).fetch_add(1, Ordering::Relaxed) };
  }
  let mut out: SharedPtrBase<Allocator> = Default::default();
  unsafe { write_words(&mut out, obj, ctrl) };
  out
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__ArrayBuffer__Allocator__CONVERT__std__unique_ptr(
  unique_ptr: UniquePtr<Allocator>,
) -> SharedPtrBase<Allocator> {
  let raw = unique_ptr.into_raw();
  let mut out: SharedPtrBase<Allocator> = Default::default();
  if raw.is_null() {
    return out;
  }
  let ctrl = Box::into_raw(Box::new(AtomicUsize::new(1)));
  unsafe { write_words(&mut out, raw as usize, ctrl as usize) };
  out
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__ArrayBuffer__Allocator__get(
  ptr: *const SharedPtrBase<Allocator>,
) -> *mut Allocator {
  read_words(ptr).0 as *mut Allocator
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__ArrayBuffer__Allocator__reset(
  ptr: *mut SharedPtrBase<Allocator>,
) {
  if ptr.is_null() {
    return;
  }
  let (obj, ctrl) = read_words(ptr);
  if ctrl != 0 {
    // Decrement; free the allocator + control block only on the last reference.
    let prev =
      unsafe { (*(ctrl as *const AtomicUsize)).fetch_sub(1, Ordering::AcqRel) };
    if prev == 1 {
      if obj != 0 {
        v8__ArrayBuffer__Allocator__DELETE(obj as *mut Allocator);
      }
      unsafe { drop(Box::from_raw(ctrl as *mut AtomicUsize)) };
    }
  }
  unsafe { write_words(ptr, 0, 0) };
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__ArrayBuffer__Allocator__use_count(
  ptr: *const SharedPtrBase<Allocator>,
) -> long {
  let (obj, ctrl) = read_words(ptr);
  if ctrl != 0 {
    unsafe { (*(ctrl as *const AtomicUsize)).load(Ordering::Acquire) as long }
  } else if obj != 0 {
    1
  } else {
    0
  }
}
