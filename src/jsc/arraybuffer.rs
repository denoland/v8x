#![allow(non_snake_case, unused)]

use crate::jsc::core::{
  ctx_of, current_ctx, current_iso, intern, intern_ctx, iso_state, jsval,
};
use crate::jsc::jsc_sys::*;
use crate::support::{
  Maybe, MaybeBool, SharedPtrBase, SharedRef, UniquePtr, long,
};
use crate::{
  Allocator, ArrayBuffer, ArrayBufferView, BackingStore,
  BackingStoreDeleterCallback, Context, DataView, RealIsolate,
  SharedArrayBuffer, Uint8ClampedArray, Value,
};
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

#[repr(C)]
#[derive(Copy, Clone)]
struct MemorySpan {
  data: *mut u8,
  size: usize,
}

// Fallback detach tracking. JSC's C API has no detach primitive, so
// `v8__ArrayBuffer__Detach` neuters the buffer at the JS level via
// `ArrayBuffer.prototype.transfer()`. On a JSC build/framework where that path
// does NOT actually neuter the buffer (a wrapped no-copy buffer JSC refuses to
// transfer — observed on `JavaScriptCore.framework`), we still must report the
// buffer as detached for the C-ABI contract (`byte_length()==0`,
// `was_detached()==true`).
//
// The marker is stored as a `DontEnum` property ON THE ARRAYBUFFER OBJECT
// ITSELF, NOT in a side table keyed by object pointer: the rusty_v8 harness
// runs every `test_api` test in ONE shared process, and JSC reuses freed object
// pointers, so a pointer-keyed set leaks "detached" onto unrelated later
// buffers (e.g. `backing_store_data`). A per-object property is GC-scoped to
// the exact buffer and cannot alias another. `DETACHED_ANY` gates the property
// lookup so the common (nothing-detached, e.g. deno_core) hot path skips it.
const DETACH_PROP: &[u8] = b"__v8x_detached\0";
static DETACHED_ANY: AtomicUsize = AtomicUsize::new(0);

fn mark_detached(ctx: JSContextRef, obj: JSObjectRef) {
  unsafe {
    let key = JSStringCreateWithUTF8CString(
      DETACH_PROP.as_ptr() as *const std::os::raw::c_char
    );
    let mut exc: JSValueRef = ptr::null();
    // 4 == kJSPropertyAttributeDontEnum
    JSObjectSetProperty(
      ctx,
      obj,
      key,
      JSValueMakeBoolean(ctx, true),
      4,
      &mut exc,
    );
    JSStringRelease(key);
  }
  DETACHED_ANY.fetch_add(1, Ordering::SeqCst);
}

fn is_marked_detached(ctx: JSContextRef, obj: JSObjectRef) -> bool {
  if DETACHED_ANY.load(Ordering::SeqCst) == 0 {
    return false;
  }
  unsafe {
    let key = JSStringCreateWithUTF8CString(
      DETACH_PROP.as_ptr() as *const std::os::raw::c_char
    );
    let has = JSObjectHasProperty(ctx, obj, key);
    JSStringRelease(key);
    has
  }
}

/// `v8::ArrayBuffer::kMaxByteLength` (2^53 - 1 on 64-bit targets).
const MAX_BYTE_LENGTH: usize = crate::binding::v8__TypedArray__kMaxByteLength;

struct BsInner {
  refcount: AtomicUsize,
  data: *mut c_void,
  byte_length: usize,
  is_shared: bool,

  /// The length the store may grow to. Equal to `byte_length` unless the
  /// store is resizable, in which case `data` already spans this many bytes.
  max_byte_length: usize,
  resizable: bool,

  deleter: BackingStoreDeleterCallback,
  deleter_data: *mut c_void,

  owns_malloc: bool,
}

unsafe extern "C" {
  fn malloc(size: usize) -> *mut c_void;
  fn calloc(count: usize, size: usize) -> *mut c_void;
  fn free(ptr: *mut c_void);
}

unsafe extern "C" fn noop_deleter(
  _data: *mut c_void,
  _len: usize,
  _deleter_data: *mut c_void,
) {
}

impl BsInner {
  fn boxed(
    data: *mut c_void,
    byte_length: usize,
    is_shared: bool,
    deleter: BackingStoreDeleterCallback,
    deleter_data: *mut c_void,
    owns_malloc: bool,
  ) -> *mut BsInner {
    Box::into_raw(Box::new(BsInner {
      refcount: AtomicUsize::new(1),
      data,
      byte_length,
      is_shared,
      max_byte_length: byte_length,
      resizable: false,
      deleter,
      deleter_data,
      owns_malloc,
    }))
  }

  fn new_allocated(byte_length: usize, is_shared: bool) -> *mut BsInner {
    let data = if byte_length == 0 {
      ptr::null_mut()
    } else {
      unsafe { calloc(byte_length, 1) }
    };
    BsInner::boxed(
      data,
      byte_length,
      is_shared,
      noop_deleter,
      ptr::null_mut(),
      true,
    )
  }

  /// Allocates a store per `BackingStoreInitializationMode`. Returns null if
  /// the length exceeds what an ArrayBuffer may hold or allocation fails.
  fn try_allocate(
    byte_length: usize,
    is_shared: bool,
    mode: crate::BackingStoreInitializationMode,
  ) -> *mut BsInner {
    if byte_length > MAX_BYTE_LENGTH {
      return ptr::null_mut();
    }
    let data = if byte_length == 0 {
      ptr::null_mut()
    } else {
      let p = match mode {
        crate::BackingStoreInitializationMode::ZeroInitialized => unsafe {
          calloc(byte_length, 1)
        },
        crate::BackingStoreInitializationMode::Uninitialized => unsafe {
          malloc(byte_length)
        },
      };
      if p.is_null() {
        return ptr::null_mut();
      }
      p
    };
    BsInner::boxed(
      data,
      byte_length,
      is_shared,
      noop_deleter,
      ptr::null_mut(),
      true,
    )
  }

  unsafe fn destroy(ptr: *mut BsInner) {
    if ptr.is_null() {
      return;
    }
    let b = unsafe { Box::from_raw(ptr) };
    if !b.data.is_null() {
      if b.owns_malloc {
        unsafe { free(b.data) };
      } else {
        unsafe { (b.deleter)(b.data, b.byte_length, b.deleter_data) };
      }
    }
  }
}

#[inline]
fn bs_inner<'a>(p: *const BackingStore) -> Option<&'a BsInner> {
  unsafe { (p as *const BsInner).as_ref() }
}

#[inline]
fn sp_get(p: *const SharedPtrBase<BackingStore>) -> *mut BsInner {
  if p.is_null() {
    return ptr::null_mut();
  }
  unsafe { *(p as *const usize) as *mut BsInner }
}

#[inline]
fn sp_set(p: *mut SharedPtrBase<BackingStore>, inner: *mut BsInner) {
  unsafe {
    let words = p as *mut usize;
    *words = inner as usize;
    *words.add(1) = 0;
  }
}

#[inline]
fn make_shared_ref(inner: *mut BsInner) -> SharedRef<BackingStore> {
  let base: SharedPtrBase<BackingStore> = Default::default();

  let mut sref = unsafe {
    std::mem::transmute_copy::<
      SharedPtrBase<BackingStore>,
      SharedRef<BackingStore>,
    >(&base)
  };
  std::mem::forget(base);
  sp_set(
    &mut sref as *mut SharedRef<BackingStore>
      as *mut SharedPtrBase<BackingStore>,
    inner,
  );
  sref
}

fn backing_store_for_buffer(
  ctx: JSContextRef,
  buf: JSValueRef,
) -> SharedRef<BackingStore> {
  let obj = buf as JSObjectRef;
  let (data, len) = unsafe {
    (
      JSObjectGetArrayBufferBytesPtr(ctx, obj, ptr::null_mut()),
      JSObjectGetArrayBufferByteLength(ctx, obj, ptr::null_mut()),
    )
  };
  let inner =
    BsInner::boxed(data, len, false, noop_deleter, ptr::null_mut(), false);
  if let Some(max) = buffer_max_byte_length(ctx, buf) {
    unsafe {
      (*inner).resizable = true;
      (*inner).max_byte_length = max;
    }
  }
  make_shared_ref(inner)
}

/// `maxByteLength` of a resizable ArrayBuffer or growable SharedArrayBuffer,
/// or `None` for a fixed-length one.
fn buffer_max_byte_length(ctx: JSContextRef, buf: JSValueRef) -> Option<usize> {
  let r = unsafe {
    crate::jsc::core::call_js_fn(
      ctx,
      c"(function(b){try{return b.resizable===true||b.growable===true?\
         b.maxByteLength:-1}catch(e){return -1}})",
      &[buf],
    )
  }
  .ok()?;
  let n = unsafe { JSValueToNumber(ctx, r, ptr::null_mut()) };
  if n >= 0.0 { Some(n as usize) } else { None }
}

/// `new ArrayBuffer(len, {maxByteLength: max})` holding a copy of `data`.
///
/// JSC's C API can only wrap foreign memory in a fixed-length buffer, so a
/// resizable backing store cannot be shared with the ArrayBuffer made from it
/// the way V8 shares it. The buffer starts out with the store's contents,
/// but later writes on either side are not seen by the other.
fn make_resizable_buffer_copy(
  ctx: JSContextRef,
  data: *const c_void,
  len: usize,
  max: usize,
) -> JSValueRef {
  let args = unsafe {
    [
      JSValueMakeNumber(ctx, len as f64),
      JSValueMakeNumber(ctx, max as f64),
    ]
  };
  let Ok(buf) = (unsafe {
    crate::jsc::core::call_js_fn(
      ctx,
      c"(function(l,m){return new ArrayBuffer(l,{maxByteLength:m})})",
      &args,
    )
  }) else {
    return ptr::null();
  };
  if len > 0 && !data.is_null() {
    let dst = unsafe {
      JSObjectGetArrayBufferBytesPtr(ctx, buf as JSObjectRef, ptr::null_mut())
    };
    if !dst.is_null() {
      unsafe {
        ptr::copy_nonoverlapping(data as *const u8, dst as *mut u8, len)
      };
    }
  }
  buf
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__New__with_byte_length(
  isolate: *mut RealIsolate,
  byte_length: usize,
) -> *const ArrayBuffer {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }

  // Always back the buffer with a non-null allocation: a NoCopy ArrayBuffer
  // whose data pointer is null is treated as DETACHED by JSC, so any view over
  // a zero-length buffer would throw "ArrayBuffer has been detached". Allocate
  // at least one byte while still reporting byte_length to JSC.
  let data = unsafe { calloc(byte_length.max(1), 1) };
  unsafe extern "C" fn dealloc(bytes: *mut c_void, _ctx: *mut c_void) {
    if !bytes.is_null() {
      unsafe { free(bytes) };
    }
  }
  let obj = unsafe {
    JSObjectMakeArrayBufferWithBytesNoCopy(
      ctx,
      data,
      byte_length,
      Some(dealloc),
      ptr::null_mut(),
      ptr::null_mut(),
    )
  };
  intern_ctx::<ArrayBuffer>(ctx, obj as JSValueRef)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__New__with_backing_store(
  isolate: *mut RealIsolate,
  backing_store: *const SharedRef<BackingStore>,
) -> *const ArrayBuffer {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() || backing_store.is_null() {
    return ptr::null();
  }
  let inner = sp_get(backing_store as *const SharedPtrBase<BackingStore>);
  if inner.is_null() {
    return ptr::null();
  }
  let (data, len) = unsafe { ((*inner).data, (*inner).byte_length) };
  if unsafe { (*inner).resizable } {
    let max = unsafe { (*inner).max_byte_length };
    let buf = make_resizable_buffer_copy(ctx, data, len, max);
    if buf.is_null() {
      return ptr::null();
    }
    return intern_ctx::<ArrayBuffer>(ctx, buf);
  }

  unsafe { (*inner).refcount.fetch_add(1, Ordering::SeqCst) };
  unsafe extern "C" fn dealloc(_bytes: *mut c_void, ctx: *mut c_void) {
    crate::jsc::core::ffi_guard(
      || {
        let inner = ctx as *mut BsInner;
        if inner.is_null() {
          return;
        }
        if unsafe { (*inner).refcount.fetch_sub(1, Ordering::SeqCst) } == 1 {
          unsafe { BsInner::destroy(inner) };
        }
      },
      || (),
    )
  }
  let obj = unsafe {
    JSObjectMakeArrayBufferWithBytesNoCopy(
      ctx,
      data,
      len,
      Some(dealloc),
      inner as *mut c_void,
      ptr::null_mut(),
    )
  };
  intern_ctx::<ArrayBuffer>(ctx, obj as JSValueRef)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__ByteLength(
  this: *const ArrayBuffer,
) -> usize {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  let obj = jsval(this) as JSObjectRef;
  if is_marked_detached(ctx, obj) {
    return 0;
  }
  unsafe { JSObjectGetArrayBufferByteLength(ctx, obj, ptr::null_mut()) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Data(
  this: *const ArrayBuffer,
) -> *mut c_void {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null_mut();
  }
  let obj = jsval(this) as JSObjectRef;
  if is_marked_detached(ctx, obj) {
    return ptr::null_mut();
  }
  unsafe { JSObjectGetArrayBufferBytesPtr(ctx, obj, ptr::null_mut()) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__IsDetachable(
  this: *const ArrayBuffer,
) -> bool {
  !this.is_null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__WasDetached(
  this: *const ArrayBuffer,
) -> bool {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return false;
  }
  let obj = jsval(this) as JSObjectRef;
  if is_marked_detached(ctx, obj) {
    return true;
  }
  // A detached ArrayBuffer reports a null bytes pointer in JSC. Freshly
  // created (incl. zero-length) buffers always have a non-null backing — our
  // `New__with_byte_length` allocates `byte_length.max(1)` precisely so an
  // empty-but-live buffer is distinguishable from a detached one here.
  let data =
    unsafe { JSObjectGetArrayBufferBytesPtr(ctx, obj, ptr::null_mut()) };
  data.is_null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Detach(
  this: *const ArrayBuffer,
  key: *const Value,
) -> MaybeBool {
  let _ = key;
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return MaybeBool::Nothing;
  }
  let obj = jsval(this) as JSObjectRef;
  // Already detached (marked, or null bytes ptr): detaching twice is a no-op.
  if is_marked_detached(ctx, obj) {
    return MaybeBool::JustTrue;
  }
  let data =
    unsafe { JSObjectGetArrayBufferBytesPtr(ctx, obj, ptr::null_mut()) };
  if data.is_null() {
    return MaybeBool::JustTrue;
  }
  // JSC's C API exposes no detach primitive, so detach at the JS level via
  // `ArrayBuffer.prototype.transfer()` (ES2024), which neuters the receiver.
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let src = b"(function(buf){buf.transfer();})\0";
    let fs = JSStringCreateWithUTF8CString(
      src.as_ptr() as *const std::os::raw::c_char
    );
    let fnv =
      JSEvaluateScript(ctx, fs, ptr::null_mut(), ptr::null_mut(), 1, &mut exc);
    JSStringRelease(fs);
    if exc.is_null() {
      let fnobj = JSValueToObject(ctx, fnv, &mut exc);
      if !fnobj.is_null() {
        let args = [jsval(this)];
        JSObjectCallAsFunction(
          ctx,
          fnobj,
          ptr::null_mut(),
          1,
          args.as_ptr(),
          &mut exc,
        );
      }
    }
  }
  // If `transfer()` did not actually neuter the buffer (build lacking transfer,
  // or a no-copy buffer JSC refused to transfer), fall back to the marker set
  // so the C-ABI still observes the detach. When transfer worked, bytes ptr is
  // now null and we leave the set untouched (keeps the deno_core path clean).
  let after =
    unsafe { JSObjectGetArrayBufferBytesPtr(ctx, obj, ptr::null_mut()) };
  if !after.is_null() {
    mark_detached(ctx, obj);
  }
  MaybeBool::JustTrue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__GetBackingStore(
  this: *const ArrayBuffer,
) -> SharedRef<BackingStore> {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return make_shared_ref(BsInner::new_allocated(0, false));
  }
  backing_store_for_buffer(ctx, jsval(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__NewBackingStore__with_byte_length(
  isolate: *mut RealIsolate,
  byte_length: usize,
) -> *mut BackingStore {
  BsInner::new_allocated(byte_length, false) as *mut BackingStore
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__NewBackingStore__with_data(
  data: *mut c_void,
  byte_length: usize,
  deleter: BackingStoreDeleterCallback,
  deleter_data: *mut c_void,
) -> *mut BackingStore {
  BsInner::boxed(data, byte_length, false, deleter, deleter_data, false)
    as *mut BackingStore
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BackingStore__Data(
  this: *const BackingStore,
) -> *mut c_void {
  bs_inner(this).map_or(ptr::null_mut(), |b| b.data)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BackingStore__ByteLength(
  this: *const BackingStore,
) -> usize {
  bs_inner(this).map_or(0, |b| b.byte_length)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BackingStore__IsShared(
  this: *const BackingStore,
) -> bool {
  bs_inner(this).map_or(false, |b| b.is_shared)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BackingStore__IsResizableByUserJavaScript(
  this: *const BackingStore,
) -> bool {
  bs_inner(this).is_some_and(|b| b.resizable)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BackingStore__MaxByteLength(
  this: *const BackingStore,
) -> usize {
  bs_inner(this).map_or(0, |b| b.max_byte_length)
}

/// V8's `kOutOfMemory` failure mode: a failed allocation is fatal.
fn backing_store_oom(byte_length: usize) -> ! {
  panic!(
    "v8x: out of memory allocating an ArrayBuffer backing store of \
     {byte_length} bytes"
  )
}

fn new_backing_store_with_mode(
  byte_length: usize,
  is_shared: bool,
  initialization_mode: crate::BackingStoreInitializationMode,
  on_failure: crate::BackingStoreOnFailureMode,
) -> *mut BackingStore {
  let inner =
    BsInner::try_allocate(byte_length, is_shared, initialization_mode);
  if inner.is_null()
    && matches!(on_failure, crate::BackingStoreOnFailureMode::OutOfMemory)
  {
    backing_store_oom(byte_length);
  }
  inner as *mut BackingStore
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__NewBackingStore__with_mode(
  _isolate: *mut RealIsolate,
  byte_length: usize,
  initialization_mode: crate::BackingStoreInitializationMode,
  on_failure: crate::BackingStoreOnFailureMode,
) -> *mut BackingStore {
  new_backing_store_with_mode(
    byte_length,
    false,
    initialization_mode,
    on_failure,
  )
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__NewBackingStore__with_mode(
  _isolate: *mut RealIsolate,
  byte_length: usize,
  initialization_mode: crate::BackingStoreInitializationMode,
  on_failure: crate::BackingStoreOnFailureMode,
) -> *mut BackingStore {
  new_backing_store_with_mode(
    byte_length,
    true,
    initialization_mode,
    on_failure,
  )
}

/// A zero-filled store that reserves `max_byte_length` bytes up front, as
/// V8's does, so growing it never moves the data.
#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__NewResizableBackingStore(
  byte_length: usize,
  max_byte_length: usize,
) -> *mut BackingStore {
  assert!(
    byte_length <= max_byte_length && max_byte_length <= MAX_BYTE_LENGTH,
    "v8x: invalid resizable backing store length {byte_length} / maximum \
     {max_byte_length}"
  );
  let data = unsafe { calloc(max_byte_length.max(1), 1) };
  if data.is_null() {
    backing_store_oom(max_byte_length);
  }
  let inner = BsInner::boxed(
    data,
    byte_length,
    false,
    noop_deleter,
    ptr::null_mut(),
    true,
  );
  unsafe {
    (*inner).resizable = true;
    (*inner).max_byte_length = max_byte_length;
  }
  inner as *mut BackingStore
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__MaybeNew(
  isolate: *mut RealIsolate,
  byte_length: usize,
  initialization_mode: crate::BackingStoreInitializationMode,
) -> *const ArrayBuffer {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() || byte_length > MAX_BYTE_LENGTH {
    return ptr::null();
  }
  // As in `New__with_byte_length`, never hand JSC a null data pointer.
  let data = match initialization_mode {
    crate::BackingStoreInitializationMode::ZeroInitialized => unsafe {
      calloc(byte_length.max(1), 1)
    },
    crate::BackingStoreInitializationMode::Uninitialized => unsafe {
      malloc(byte_length.max(1))
    },
  };
  if data.is_null() {
    return ptr::null();
  }
  unsafe extern "C" fn dealloc(bytes: *mut c_void, _ctx: *mut c_void) {
    if !bytes.is_null() {
      unsafe { free(bytes) };
    }
  }
  let obj = unsafe {
    JSObjectMakeArrayBufferWithBytesNoCopy(
      ctx,
      data,
      byte_length,
      Some(dealloc),
      ptr::null_mut(),
      ptr::null_mut(),
    )
  };
  if obj.is_null() {
    unsafe { free(data) };
    return ptr::null();
  }
  intern_ctx::<ArrayBuffer>(ctx, obj as JSValueRef)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__IsResizableByUserJavaScript(
  this: *const ArrayBuffer,
) -> bool {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return false;
  }
  buffer_max_byte_length(ctx, jsval(this)).is_some()
}

/// Immutable ArrayBuffers (the TC39 proposal) report `immutable === true`;
/// JavaScriptCore builds without the proposal never create one.
#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__IsImmutable(
  this: *const ArrayBuffer,
) -> bool {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return false;
  }
  let r = unsafe {
    crate::jsc::core::call_js_fn(
      ctx,
      c"(function(b){try{return b.immutable===true}catch(e){return false}})",
      &[jsval(this)],
    )
  };
  matches!(r, Ok(v) if unsafe { JSValueToBoolean(ctx, v) })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BackingStore__DELETE(this: *mut BackingStore) {
  let inner = this as *mut BsInner;
  if inner.is_null() {
    return;
  }
  if unsafe { (*inner).refcount.fetch_sub(1, Ordering::SeqCst) } == 1 {
    unsafe { BsInner::destroy(inner) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__BackingStore__COPY(
  ptr: *const SharedPtrBase<BackingStore>,
) -> SharedPtrBase<BackingStore> {
  let inner = sp_get(ptr);
  if !inner.is_null() {
    unsafe { (*inner).refcount.fetch_add(1, Ordering::SeqCst) };
  }
  let mut out: SharedPtrBase<BackingStore> = Default::default();
  sp_set(&mut out, inner);
  out
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__BackingStore__CONVERT__std__unique_ptr(
  unique_ptr: UniquePtr<BackingStore>,
) -> SharedPtrBase<BackingStore> {
  let raw = unique_ptr.into_raw() as *mut BsInner;
  let mut out: SharedPtrBase<BackingStore> = Default::default();
  sp_set(&mut out, raw);
  out
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__BackingStore__get(
  ptr: *const SharedPtrBase<BackingStore>,
) -> *mut BackingStore {
  sp_get(ptr) as *mut BackingStore
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__BackingStore__reset(
  ptr: *mut SharedPtrBase<BackingStore>,
) {
  let inner = sp_get(ptr);
  if !inner.is_null() {
    if unsafe { (*inner).refcount.fetch_sub(1, Ordering::SeqCst) } == 1 {
      unsafe { BsInner::destroy(inner) };
    }
  }
  if !ptr.is_null() {
    sp_set(ptr, ptr::null_mut());
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__BackingStore__use_count(
  ptr: *const SharedPtrBase<BackingStore>,
) -> long {
  let inner = sp_get(ptr);
  if inner.is_null() {
    0
  } else {
    unsafe { (*inner).refcount.load(Ordering::SeqCst) as long }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Allocator__NewDefaultAllocator()
-> *mut Allocator {
  Box::into_raw(Box::new(0u8)) as *mut Allocator
}

/// V8 only has a group-scoped allocator with multi-cage pointer compression,
/// and returns null otherwise. This backend has a single isolate group.
#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Allocator__NewDefaultAllocator__with_group(
  _group: *const c_void,
) -> *mut Allocator {
  ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Allocator__DELETE(this: *mut Allocator) {
  if !this.is_null() {
    unsafe { drop(Box::from_raw(this as *mut u8)) };
  }
}

// The allocator `std::shared_ptr` is modelled with a real, atomically
// refcounted control block so that `clone()` / drop behave like C++'s
// shared_ptr — i.e. `use_count()` is accurate and the underlying allocator is
// freed exactly once (when the last reference drops). The previous
// implementation bitwise-copied the two words and freed on every `reset`, which
// (a) reported a bogus use_count of 1 and (b) double-freed when more than one
// `SharedRef`/`SharedPtr` pointed at the same allocator — an abort that took
// down the whole `test_api` binary (`backing_store_segfault` et al.). Mirrors
// the QuickJS backend's `src/quickjs/allocator.rs`.
//
// `SharedPtrBase<Allocator>` is `[usize; 2]`:
//   word[0] = the `*mut Allocator` object pointer (0 when null)
//   word[1] = a `*mut AtomicUsize` control block (0 when null)
fn alloc_read_words(ptr: *const SharedPtrBase<Allocator>) -> (usize, usize) {
  if ptr.is_null() {
    return (0, 0);
  }
  let w = ptr as *const usize;
  unsafe { (*w, *w.add(1)) }
}

unsafe fn alloc_write_words(
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
pub extern "C" fn std__shared_ptr__v8__ArrayBuffer__Allocator__COPY(
  ptr: *const SharedPtrBase<Allocator>,
) -> SharedPtrBase<Allocator> {
  let (obj, ctrl) = alloc_read_words(ptr);
  if ctrl != 0 {
    unsafe { (*(ctrl as *const AtomicUsize)).fetch_add(1, Ordering::Relaxed) };
  }
  let mut out: SharedPtrBase<Allocator> = Default::default();
  unsafe { alloc_write_words(&mut out, obj, ctrl) };
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
  unsafe { alloc_write_words(&mut out, raw as usize, ctrl as usize) };
  out
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__ArrayBuffer__Allocator__get(
  ptr: *const SharedPtrBase<Allocator>,
) -> *mut Allocator {
  alloc_read_words(ptr).0 as *mut Allocator
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__ArrayBuffer__Allocator__reset(
  ptr: *mut SharedPtrBase<Allocator>,
) {
  if ptr.is_null() {
    return;
  }
  let (obj, ctrl) = alloc_read_words(ptr);
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
  unsafe { alloc_write_words(ptr, 0, 0) };
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__ArrayBuffer__Allocator__use_count(
  ptr: *const SharedPtrBase<Allocator>,
) -> long {
  let (obj, ctrl) = alloc_read_words(ptr);
  if ctrl != 0 {
    unsafe { (*(ctrl as *const AtomicUsize)).load(Ordering::Acquire) as long }
  } else if obj != 0 {
    1
  } else {
    0
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__Buffer(
  this: *const ArrayBufferView,
) -> *const ArrayBuffer {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  let buf = unsafe {
    JSObjectGetTypedArrayBuffer(
      ctx,
      jsval(this) as JSObjectRef,
      ptr::null_mut(),
    )
  };
  if buf.is_null() {
    return ptr::null();
  }
  intern_ctx::<ArrayBuffer>(ctx, buf as JSValueRef)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__Buffer__Data(
  this: *const ArrayBufferView,
) -> *mut c_void {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null_mut();
  }

  let buf = unsafe {
    JSObjectGetTypedArrayBuffer(
      ctx,
      jsval(this) as JSObjectRef,
      ptr::null_mut(),
    )
  };
  if buf.is_null() {
    return ptr::null_mut();
  }
  unsafe { JSObjectGetArrayBufferBytesPtr(ctx, buf, ptr::null_mut()) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__ByteLength(
  this: *const ArrayBufferView,
) -> usize {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  unsafe {
    JSObjectGetTypedArrayByteLength(
      ctx,
      jsval(this) as JSObjectRef,
      ptr::null_mut(),
    )
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__ByteOffset(
  this: *const ArrayBufferView,
) -> usize {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  unsafe {
    JSObjectGetTypedArrayByteOffset(
      ctx,
      jsval(this) as JSObjectRef,
      ptr::null_mut(),
    )
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TypedArray__Length(
  this: *const crate::TypedArray,
) -> usize {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  unsafe {
    JSObjectGetTypedArrayLength(
      ctx,
      jsval(this) as JSObjectRef,
      ptr::null_mut(),
    )
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__HasBuffer(
  this: *const ArrayBufferView,
) -> bool {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return false;
  }
  let buf = unsafe {
    JSObjectGetTypedArrayBuffer(
      ctx,
      jsval(this) as JSObjectRef,
      ptr::null_mut(),
    )
  };
  !buf.is_null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__GetContents(
  this: *const ArrayBufferView,
  storage: MemorySpan,
) -> MemorySpan {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return MemorySpan {
      data: ptr::null_mut(),
      size: 0,
    };
  }
  let obj = jsval(this) as JSObjectRef;
  // Compute the data pointer as ArrayBuffer base + byteOffset rather than
  // trusting JSObjectGetTypedArrayBytesPtr: for a typed array that is a VIEW
  // over an existing (e.g. pooled) ArrayBuffer, JSC returns the buffer base
  // (byteOffset NOT applied), so ops read from offset 0. Node's Buffer pools
  // many small buffers into one ArrayBuffer at increasing byteOffsets, so this
  // made every non-first pooled Buffer decode read stale bytes (e.g.
  // `Buffer.from("foo")` => "Hel"). Base + byteOffset is correct regardless.
  let (off, len) = unsafe {
    (
      JSObjectGetTypedArrayByteOffset(ctx, obj, ptr::null_mut()),
      JSObjectGetTypedArrayByteLength(ctx, obj, ptr::null_mut()),
    )
  };
  // Prefer ArrayBuffer base + byteOffset (correct for pooled VIEWS, where
  // JSObjectGetTypedArrayBytesPtr drops the offset). Fall back to the typed
  // array data pointer when the view has no materializable ArrayBuffer (e.g.
  // DataView / detached); that pointer already accounts for any offset.
  let base = unsafe {
    let buffer = JSObjectGetTypedArrayBuffer(ctx, obj, ptr::null_mut());
    if buffer.is_null() {
      ptr::null_mut()
    } else {
      JSObjectGetArrayBufferBytesPtr(ctx, buffer, ptr::null_mut())
    }
  };
  let data = if !base.is_null() {
    unsafe { (base as *mut u8).add(off) }
  } else {
    unsafe {
      JSObjectGetTypedArrayBytesPtr(ctx, obj, ptr::null_mut()) as *mut u8
    }
  };
  if data.is_null() {
    return MemorySpan {
      data: ptr::null_mut(),
      size: 0,
    };
  }
  MemorySpan { data, size: len }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__New__with_backing_store(
  isolate: *mut RealIsolate,
  backing_store: *const SharedRef<BackingStore>,
) -> *const SharedArrayBuffer {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() || backing_store.is_null() {
    return ptr::null();
  }
  let inner = sp_get(backing_store as *const SharedPtrBase<BackingStore>);
  if inner.is_null() {
    return ptr::null();
  }
  let (data, len) = unsafe { ((*inner).data, (*inner).byte_length) };
  unsafe { (*inner).refcount.fetch_add(1, Ordering::SeqCst) };
  unsafe extern "C" fn dealloc(_bytes: *mut c_void, ctx: *mut c_void) {
    crate::jsc::core::ffi_guard(
      || {
        let inner = ctx as *mut BsInner;
        if inner.is_null() {
          return;
        }
        if unsafe { (*inner).refcount.fetch_sub(1, Ordering::SeqCst) } == 1 {
          unsafe { BsInner::destroy(inner) };
        }
      },
      || (),
    )
  }
  let obj = unsafe {
    JSObjectMakeArrayBufferWithBytesNoCopy(
      ctx,
      data,
      len,
      Some(dealloc),
      inner as *mut c_void,
      ptr::null_mut(),
    )
  };
  intern_ctx::<SharedArrayBuffer>(ctx, obj as JSValueRef)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__GetBackingStore(
  this: *const SharedArrayBuffer,
) -> SharedRef<BackingStore> {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return make_shared_ref(BsInner::new_allocated(0, true));
  }

  let mut sref = backing_store_for_buffer(ctx, jsval(this));
  let inner = sp_get(
    &sref as *const SharedRef<BackingStore>
      as *const SharedPtrBase<BackingStore>,
  );
  if !inner.is_null() {
    unsafe { (*inner).is_shared = true };
  }
  sref
}

#[inline]
fn make_typed_array(
  buf: *const ArrayBuffer,
  byte_offset: usize,
  length: usize,
  ty: JSTypedArrayType,
) -> JSValueRef {
  let ctx = current_ctx();
  if ctx.is_null() || buf.is_null() {
    return ptr::null();
  }
  // A zero-length view needs no shared storage, and JSC's
  // ...WithArrayBufferAndOffset path rejects offset==length==0 over an empty
  // buffer as out-of-bounds. Build a standalone empty typed array instead —
  // semantically identical (no elements to alias) and always succeeds.
  if length == 0 {
    let obj = unsafe { JSObjectMakeTypedArray(ctx, ty, 0, ptr::null_mut()) };
    return obj as JSValueRef;
  }
  let obj = unsafe {
    JSObjectMakeTypedArrayWithArrayBufferAndOffset(
      ctx,
      ty,
      jsval(buf) as JSObjectRef,
      byte_offset,
      length,
      ptr::null_mut(),
    )
  };
  obj as JSValueRef
}

macro_rules! typed_array_new {
  ($fn_name:ident, $ty_name:ident, $jsc_ty:expr) => {
    #[unsafe(no_mangle)]
    pub extern "C" fn $fn_name(
      buf_ptr: *const ArrayBuffer,
      byte_offset: usize,
      length: usize,
    ) -> *const crate::$ty_name {
      let v = make_typed_array(buf_ptr, byte_offset, length, $jsc_ty);
      intern::<crate::$ty_name>(v)
    }
  };
}

typed_array_new!(v8__Uint8Array__New, Uint8Array, kJSTypedArrayTypeUint8Array);
typed_array_new!(v8__Int8Array__New, Int8Array, kJSTypedArrayTypeInt8Array);
typed_array_new!(
  v8__Uint16Array__New,
  Uint16Array,
  kJSTypedArrayTypeUint16Array
);
typed_array_new!(v8__Int16Array__New, Int16Array, kJSTypedArrayTypeInt16Array);
typed_array_new!(
  v8__Uint32Array__New,
  Uint32Array,
  kJSTypedArrayTypeUint32Array
);
typed_array_new!(v8__Int32Array__New, Int32Array, kJSTypedArrayTypeInt32Array);
typed_array_new!(
  v8__Float32Array__New,
  Float32Array,
  kJSTypedArrayTypeFloat32Array
);
typed_array_new!(
  v8__Float64Array__New,
  Float64Array,
  kJSTypedArrayTypeFloat64Array
);
typed_array_new!(
  v8__BigInt64Array__New,
  BigInt64Array,
  kJSTypedArrayTypeBigInt64Array
);
typed_array_new!(
  v8__BigUint64Array__New,
  BigUint64Array,
  kJSTypedArrayTypeBigUint64Array
);

// Views over a SharedArrayBuffer. JSC's typed array constructors take either
// kind of buffer, so these share `make_typed_array` with the ArrayBuffer
// versions.
macro_rules! typed_array_new_shared {
  ($fn_name:ident, $ty_name:ident, $jsc_ty:expr) => {
    #[unsafe(no_mangle)]
    pub extern "C" fn $fn_name(
      buf_ptr: *const SharedArrayBuffer,
      byte_offset: usize,
      length: usize,
    ) -> *const crate::$ty_name {
      let v = make_typed_array(
        buf_ptr as *const ArrayBuffer,
        byte_offset,
        length,
        $jsc_ty,
      );
      intern::<crate::$ty_name>(v)
    }
  };
}

typed_array_new_shared!(
  v8__Uint8Array__New__with_shared_buffer,
  Uint8Array,
  kJSTypedArrayTypeUint8Array
);
typed_array_new_shared!(
  v8__Uint8ClampedArray__New__with_shared_buffer,
  Uint8ClampedArray,
  kJSTypedArrayTypeUint8ClampedArray
);
typed_array_new_shared!(
  v8__Int8Array__New__with_shared_buffer,
  Int8Array,
  kJSTypedArrayTypeInt8Array
);
typed_array_new_shared!(
  v8__Uint16Array__New__with_shared_buffer,
  Uint16Array,
  kJSTypedArrayTypeUint16Array
);
typed_array_new_shared!(
  v8__Int16Array__New__with_shared_buffer,
  Int16Array,
  kJSTypedArrayTypeInt16Array
);
typed_array_new_shared!(
  v8__Uint32Array__New__with_shared_buffer,
  Uint32Array,
  kJSTypedArrayTypeUint32Array
);
typed_array_new_shared!(
  v8__Int32Array__New__with_shared_buffer,
  Int32Array,
  kJSTypedArrayTypeInt32Array
);
typed_array_new_shared!(
  v8__Float32Array__New__with_shared_buffer,
  Float32Array,
  kJSTypedArrayTypeFloat32Array
);
typed_array_new_shared!(
  v8__Float64Array__New__with_shared_buffer,
  Float64Array,
  kJSTypedArrayTypeFloat64Array
);
typed_array_new_shared!(
  v8__BigInt64Array__New__with_shared_buffer,
  BigInt64Array,
  kJSTypedArrayTypeBigInt64Array
);
typed_array_new_shared!(
  v8__BigUint64Array__New__with_shared_buffer,
  BigUint64Array,
  kJSTypedArrayTypeBigUint64Array
);

#[unsafe(no_mangle)]
pub extern "C" fn v8__Float16Array__New__with_shared_buffer(
  buf_ptr: *const SharedArrayBuffer,
  byte_offset: usize,
  length: usize,
) -> *const crate::Float16Array {
  v8__Float16Array__New(buf_ptr as *const ArrayBuffer, byte_offset, length)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__DataView__New__with_shared_buffer(
  sab: *const SharedArrayBuffer,
  byte_offset: usize,
  length: usize,
) -> *const DataView {
  v8__DataView__New(sab as *const ArrayBuffer, byte_offset, length)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__Data(
  this: *const SharedArrayBuffer,
) -> *mut c_void {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null_mut();
  }
  unsafe {
    JSObjectGetArrayBufferBytesPtr(
      ctx,
      jsval(this) as JSObjectRef,
      ptr::null_mut(),
    )
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__SetDetachKey(
  _this: *const ArrayBuffer,
  _key: *const Value,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__CopyContents(
  this: *const ArrayBufferView,
  dest: *mut c_void,
  byte_length: crate::support::int,
) -> usize {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() || dest.is_null() || byte_length <= 0 {
    return 0;
  }
  let obj = jsval(this) as JSObjectRef;
  let (off, len) = unsafe {
    (
      JSObjectGetTypedArrayByteOffset(ctx, obj, ptr::null_mut()),
      JSObjectGetTypedArrayByteLength(ctx, obj, ptr::null_mut()),
    )
  };
  // Mirror `GetContents`: prefer ArrayBuffer base + byteOffset. For a typed
  // array that is a VIEW over an existing buffer (e.g. `new Uint8Array(buf, 2,
  // 4)` or a pooled Node Buffer), JSObjectGetTypedArrayBytesPtr returns the
  // buffer base with the byteOffset NOT applied, so copying from it reads the
  // wrong bytes. Fall back to the typed-array data pointer only when the view
  // has no materializable ArrayBuffer (that pointer already accounts for any
  // offset).
  let base = unsafe {
    let buffer = JSObjectGetTypedArrayBuffer(ctx, obj, ptr::null_mut());
    if buffer.is_null() {
      ptr::null_mut()
    } else {
      JSObjectGetArrayBufferBytesPtr(ctx, buffer, ptr::null_mut())
    }
  };
  let src = if !base.is_null() {
    unsafe { (base as *mut u8).add(off) }
  } else {
    unsafe {
      JSObjectGetTypedArrayBytesPtr(ctx, obj, ptr::null_mut()) as *mut u8
    }
  };
  if src.is_null() {
    return 0;
  }
  let n = len.min(byte_length as usize);
  unsafe {
    ptr::copy_nonoverlapping(src as *const u8, dest as *mut u8, n);
  }
  n
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__DataView__New(
  arraybuffer: *const ArrayBuffer,
  byte_offset: usize,
  length: usize,
) -> *const DataView {
  let ctx = current_ctx();
  if ctx.is_null() || arraybuffer.is_null() {
    return ptr::null();
  }

  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let src = b"(function(buf,off,len){return new DataView(buf,off,len);})\0";
    let fs = JSStringCreateWithUTF8CString(
      src.as_ptr() as *const std::os::raw::c_char
    );
    let fnv =
      JSEvaluateScript(ctx, fs, ptr::null_mut(), ptr::null_mut(), 1, &mut exc);
    JSStringRelease(fs);
    if !exc.is_null() {
      return ptr::null();
    }
    let fnobj = JSValueToObject(ctx, fnv, &mut exc);
    if fnobj.is_null() {
      return ptr::null();
    }
    let args = [
      jsval(arraybuffer),
      JSValueMakeNumber(ctx, byte_offset as f64),
      JSValueMakeNumber(ctx, length as f64),
    ];
    let v = JSObjectCallAsFunction(
      ctx,
      fnobj,
      ptr::null_mut(),
      3,
      args.as_ptr(),
      &mut exc,
    );
    if !exc.is_null() || v.is_null() {
      return ptr::null();
    }
    intern_ctx::<DataView>(ctx, v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__ByteLength(
  this: *const SharedArrayBuffer,
) -> usize {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  unsafe {
    JSObjectGetArrayBufferByteLength(
      ctx,
      jsval(this) as JSObjectRef,
      ptr::null_mut(),
    )
  }
}

typed_array_new!(
  v8__Uint8ClampedArray__New,
  Uint8ClampedArray,
  kJSTypedArrayTypeUint8ClampedArray
);

#[unsafe(no_mangle)]
pub extern "C" fn v8__Float16Array__New(
  buf_ptr: *const ArrayBuffer,
  byte_offset: usize,
  length: usize,
) -> *const crate::Float16Array {
  let ctx = current_ctx();
  if ctx.is_null() || buf_ptr.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let src =
      b"(function(buf,off,len){return new Float16Array(buf,off,len);})\0";
    let fs = JSStringCreateWithUTF8CString(
      src.as_ptr() as *const std::os::raw::c_char
    );
    let fnv =
      JSEvaluateScript(ctx, fs, ptr::null_mut(), ptr::null_mut(), 1, &mut exc);
    JSStringRelease(fs);
    if !exc.is_null() {
      return ptr::null();
    }
    let fnobj = JSValueToObject(ctx, fnv, &mut exc);
    if fnobj.is_null() {
      return ptr::null();
    }
    let args = [
      jsval(buf_ptr),
      JSValueMakeNumber(ctx, byte_offset as f64),
      JSValueMakeNumber(ctx, length as f64),
    ];
    let v = JSObjectCallAsFunction(
      ctx,
      fnobj,
      ptr::null_mut(),
      3,
      args.as_ptr(),
      &mut exc,
    );
    if !exc.is_null() || v.is_null() {
      return ptr::null();
    }
    intern_ctx::<crate::Float16Array>(ctx, v)
  }
}

// ---------------------------------------------------------------------------
// Link-stubs for v8 C-ABI symbols that `test_api.rs` references but this
// backend doesn't implement yet. Each returns a benign default
// (null / 0 / false / `Nothing`) so the target LINKS and the many tests that
// don't touch these paths run; tests that do exercise them fail gracefully
// without crashing. Promote individual stubs to real implementations over time.
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__NewBackingStore__with_data(
  _data: *mut std::os::raw::c_void,
  _byte_length: usize,
  _deleter: *const std::os::raw::c_void,
  _deleter_data: *mut std::os::raw::c_void,
) -> *mut std::os::raw::c_void {
  std::ptr::null_mut()
}

/// `new SharedArrayBuffer(n)`: the C API can only make plain ArrayBuffers.
#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__New__with_byte_length(
  isolate: *mut RealIsolate,
  byte_length: usize,
) -> *const SharedArrayBuffer {
  if isolate.is_null() {
    return ptr::null();
  }
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  let len = unsafe { JSValueMakeNumber(ctx, byte_length as f64) };
  match unsafe {
    crate::jsc::core::call_js_fn(
      ctx,
      c"(function(n){return new SharedArrayBuffer(n)})",
      &[len],
    )
  } {
    Ok(v) => intern_ctx::<SharedArrayBuffer>(ctx, v),
    Err(_) => ptr::null(),
  }
}
