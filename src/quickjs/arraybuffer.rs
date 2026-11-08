#![allow(non_snake_case, unused)]

use crate::binding::memory_span_t;
use crate::quickjs::core::{
  adjust_external_memory, ctx_of, current_ctx, current_iso, intern, iso_state,
  jsval_of,
};
use crate::quickjs::exception::clear_pending;
use crate::quickjs::quickjs_sys::*;
use crate::support::{MaybeBool, SharedPtrBase, SharedRef, UniquePtr, long};
use crate::{
  Allocator, ArrayBuffer, ArrayBufferView, BackingStore,
  BackingStoreDeleterCallback, Context, DataView, RealIsolate,
  SharedArrayBuffer, Value,
};
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

// Registry of live "alias" backing stores (created by `backing_store_for_buffer`)
// keyed by the QuickJS ArrayBuffer data pointer they alias. When that buffer is
// detached (deno's `#[buffer(detach)]` path: get_backing_store THEN detach),
// QuickJS frees the data out from under the still-outstanding backing store.
// On detach we look the pointer up here and "steal" the bytes into a process-heap
// malloc copy each aliasing store owns, so the SharedRef deno carries survives the
// detach and is safe to read on another thread (worker postMessage transport).
fn alias_registry() -> &'static Mutex<HashMap<usize, Vec<usize>>> {
  static REG: OnceLock<Mutex<HashMap<usize, Vec<usize>>>> = OnceLock::new();
  REG.get_or_init(|| Mutex::new(HashMap::new()))
}

fn registry_add(key: usize, inner: *mut BsInner) {
  if key == 0 {
    return;
  }
  alias_registry()
    .lock()
    .unwrap()
    .entry(key)
    .or_default()
    .push(inner as usize);
}

fn registry_remove(key: usize, inner: *mut BsInner) {
  if key == 0 {
    return;
  }
  let mut map = alias_registry().lock().unwrap();
  if let Some(v) = map.get_mut(&key) {
    if let Some(pos) = v.iter().position(|&p| p == inner as usize) {
      v.swap_remove(pos);
    }
    if v.is_empty() {
      map.remove(&key);
    }
  }
}

fn owner_registry() -> &'static Mutex<HashMap<usize, usize>> {
  static REG: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();
  REG.get_or_init(|| Mutex::new(HashMap::new()))
}

fn owner_add(key: usize, inner: *mut BsInner) {
  if key == 0 {
    return;
  }
  owner_registry().lock().unwrap().insert(key, inner as usize);
}

fn owner_get(key: usize) -> *mut BsInner {
  if key == 0 {
    return ptr::null_mut();
  }
  owner_registry()
    .lock()
    .unwrap()
    .get(&key)
    .copied()
    .unwrap_or(0) as *mut BsInner
}

fn owner_remove(key: usize, inner: *mut BsInner) {
  if key == 0 {
    return;
  }
  let mut map = owner_registry().lock().unwrap();
  if map.get(&key).copied() == Some(inner as usize) {
    map.remove(&key);
  }
}

#[allow(non_camel_case_types)]
type JSTypedArrayEnum = i32;
const JS_TYPED_ARRAY_UINT8C: JSTypedArrayEnum = 0;
const JS_TYPED_ARRAY_INT8: JSTypedArrayEnum = 1;
const JS_TYPED_ARRAY_UINT8: JSTypedArrayEnum = 2;
const JS_TYPED_ARRAY_INT16: JSTypedArrayEnum = 3;
const JS_TYPED_ARRAY_UINT16: JSTypedArrayEnum = 4;
const JS_TYPED_ARRAY_INT32: JSTypedArrayEnum = 5;
const JS_TYPED_ARRAY_UINT32: JSTypedArrayEnum = 6;
const JS_TYPED_ARRAY_BIG_INT64: JSTypedArrayEnum = 7;
const JS_TYPED_ARRAY_BIG_UINT64: JSTypedArrayEnum = 8;
const JS_TYPED_ARRAY_FLOAT16: JSTypedArrayEnum = 9;
const JS_TYPED_ARRAY_FLOAT32: JSTypedArrayEnum = 10;
const JS_TYPED_ARRAY_FLOAT64: JSTypedArrayEnum = 11;

#[allow(non_camel_case_types)]
type JSFreeArrayBufferDataFunc = Option<
  unsafe extern "C" fn(
    rt: *mut JSRuntime,
    opaque: *mut c_void,
    ptr: *mut c_void,
  ),
>;

unsafe extern "C" {
  fn JS_NewArrayBuffer(
    ctx: *mut JSContext,
    buf: *mut u8,
    len: usize,
    free_func: JSFreeArrayBufferDataFunc,
    opaque: *mut c_void,
    is_shared: bool,
  ) -> JSValue;
  fn JS_GetArrayBuffer(
    ctx: *mut JSContext,
    psize: *mut usize,
    obj: JSValue,
  ) -> *mut u8;
  fn JS_DetachArrayBuffer(ctx: *mut JSContext, obj: JSValue);
  fn JS_DefinePropertyValueStr(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: *const std::os::raw::c_char,
    val: JSValue,
    flags: i32,
  ) -> i32;
  fn JS_NewTypedArray(
    ctx: *mut JSContext,
    argc: i32,
    argv: *mut JSValue,
    array_type: JSTypedArrayEnum,
  ) -> JSValue;
  fn JS_GetTypedArrayBuffer(
    ctx: *mut JSContext,
    obj: JSValue,
    pbyte_offset: *mut usize,
    pbyte_length: *mut usize,
    pbytes_per_element: *mut usize,
  ) -> JSValue;
  fn JS_GetTypedArrayType(obj: JSValue) -> i32;
  fn JS_IsStrictEqual(ctx: *mut JSContext, op1: JSValue, op2: JSValue) -> bool;
}

unsafe extern "C" {
  fn malloc(size: usize) -> *mut c_void;
  fn calloc(count: usize, size: usize) -> *mut c_void;
  fn free(ptr: *mut c_void);
}

struct BsInner {
  refcount: AtomicUsize,
  data: *mut c_void,
  byte_length: usize,
  is_shared: bool,
  is_resizable_by_user_javascript: bool,
  // The length `byte_length` may grow to. Equal to `byte_length` unless the
  // store is resizable (V8 reports a fixed store's own length as its maximum).
  max_byte_length: usize,

  deleter: BackingStoreDeleterCallback,
  deleter_data: *mut c_void,

  owns_malloc: bool,

  allocator: SharedPtrBase<Allocator>,
  allocator_isolate: *mut RealIsolate,

  retained_ctx: *mut JSContext,
  retained_val: JSValue,
}

struct AllocatorBuffer {
  isolate: *mut RealIsolate,
  allocator: *mut crate::array_buffer::Allocator,
  byte_length: usize,
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
    BsInner::boxed_with_allocator(
      data,
      byte_length,
      is_shared,
      deleter,
      deleter_data,
      owns_malloc,
      Default::default(),
    )
  }

  fn boxed_with_allocator(
    data: *mut c_void,
    byte_length: usize,
    is_shared: bool,
    deleter: BackingStoreDeleterCallback,
    deleter_data: *mut c_void,
    owns_malloc: bool,
    allocator: SharedPtrBase<Allocator>,
  ) -> *mut BsInner {
    Box::into_raw(Box::new(BsInner {
      refcount: AtomicUsize::new(1),
      data,
      byte_length,
      is_shared,
      is_resizable_by_user_javascript: false,
      max_byte_length: byte_length,
      deleter,
      deleter_data,
      owns_malloc,
      allocator,
      allocator_isolate: ptr::null_mut(),
      retained_ctx: ptr::null_mut(),
      retained_val: jsv_undefined(),
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

  unsafe fn destroy(ptr: *mut BsInner) {
    if ptr.is_null() {
      return;
    }
    let key = unsafe { (*ptr).data } as usize;
    owner_remove(key, ptr);
    registry_remove(key, ptr);
    let b = unsafe { Box::from_raw(ptr) };
    if !b.data.is_null() {
      let allocator = super::allocator::allocator_shared_get(&b.allocator);
      if !allocator.is_null() {
        if !b.allocator_isolate.is_null()
          && super::allocator::allocator_is_rust(allocator)
        {
          iso_state(b.allocator_isolate)
            .pending_array_buffer_frees
            .push((allocator, b.data, b.byte_length));
        } else {
          unsafe {
            super::allocator::allocator_free(allocator, b.data, b.byte_length)
          };
        }
      } else if b.owns_malloc {
        unsafe { free(b.data) };
      } else {
        unsafe { (b.deleter)(b.data, b.byte_length, b.deleter_data) };
      }
    }

    if !b.retained_ctx.is_null() {
      unsafe { JS_FreeValue(b.retained_ctx, b.retained_val) };
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
  ctx: *mut JSContext,
  buf: JSValue,
) -> SharedRef<BackingStore> {
  let mut len: usize = 0;
  let data = unsafe { JS_GetArrayBuffer(ctx, &mut len, buf) } as *mut c_void;

  let owner = owner_get(data as usize);
  if !owner.is_null() {
    unsafe { (*owner).refcount.fetch_add(1, Ordering::SeqCst) };
    return make_shared_ref(owner);
  }

  let inner =
    BsInner::boxed(data, len, false, noop_deleter, ptr::null_mut(), false);

  unsafe {
    (*inner).is_resizable_by_user_javascript =
      is_resizable_by_user_javascript(ctx, buf, false);
    if (*inner).is_resizable_by_user_javascript {
      (*inner).max_byte_length =
        read_len_prop(ctx, buf, c"maxByteLength".as_ptr()).max(len);
    }
    (*inner).retained_ctx = ctx;
    (*inner).retained_val = JS_DupValue(ctx, buf);
  }
  registry_add(data as usize, inner);
  make_shared_ref(inner)
}

/// Move alias backing stores off QuickJS-owned memory before their runtime is
/// destroyed. V8 backing stores may outlive their isolate, so retaining a
/// JSValue and freeing it later through a dead JSContext is not valid.
pub(crate) unsafe fn release_backing_stores_for_runtime(rt: *mut JSRuntime) {
  if rt.is_null() {
    return;
  }

  let inners = {
    let mut registry = alias_registry().lock().unwrap();
    let mut inners = Vec::new();
    registry.retain(|_, entries| {
      entries.retain(|entry| {
        let inner = *entry as *mut BsInner;
        let matches = !inner.is_null()
          && unsafe {
            !(*inner).retained_ctx.is_null()
              && JS_GetRuntime((*inner).retained_ctx) == rt
          };
        if matches {
          inners.push(inner);
        }
        !matches
      });
      !entries.is_empty()
    });
    inners
  };

  for inner in inners {
    let b = unsafe { &mut *inner };
    let copy = if b.byte_length == 0 {
      ptr::null_mut()
    } else {
      let copy = unsafe { malloc(b.byte_length) };
      if copy.is_null() {
        std::process::abort();
      }
      unsafe { ptr::copy_nonoverlapping(b.data, copy, b.byte_length) };
      copy
    };

    b.data = copy;
    b.owns_malloc = true;
    b.deleter = noop_deleter;
    b.deleter_data = ptr::null_mut();
    unsafe { JS_FreeValue(b.retained_ctx, b.retained_val) };
    b.retained_ctx = ptr::null_mut();
    b.retained_val = jsv_undefined();
  }
}

unsafe fn is_resizable_by_user_javascript(
  ctx: *mut JSContext,
  buf: JSValue,
  is_shared: bool,
) -> bool {
  let prop_name = if is_shared {
    c"growable".as_ptr()
  } else {
    c"resizable".as_ptr()
  };
  let prop = unsafe { JS_GetPropertyStr(ctx, buf, prop_name) };
  if prop.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return false;
  }
  let resizable = unsafe { JS_ToBool(ctx, prop) } != 0;
  unsafe { JS_FreeValue(ctx, prop) };
  resizable
}

unsafe fn read_len_prop(
  ctx: *mut JSContext,
  view: JSValue,
  name: *const std::os::raw::c_char,
) -> usize {
  let v = unsafe { JS_GetPropertyStr(ctx, view, name) };
  if v.tag == JS_TAG_EXCEPTION {
    unsafe { clear_pending(ctx) };
    return 0;
  }
  let mut out: i64 = 0;
  let result = unsafe { JS_ToInt64(ctx, &mut out, v) };
  unsafe { JS_FreeValue(ctx, v) };
  if result < 0 {
    unsafe { clear_pending(ctx) };
    return 0;
  }
  out.max(0) as usize
}

/// Resolve a view's underlying ArrayBuffer + byte offset/length. `JS_GetTypedArrayBuffer`
/// rejects `DataView` (it is not a typed array), so on failure fall back to the view's
/// own `.buffer`/`.byteOffset`/`.byteLength` — otherwise a DataView reads as 0 bytes
/// (e.g. `crypto.subtle.digest` over a DataView hashed the empty string). Returns the
/// buffer (+1 ref) or `JS_EXCEPTION`.
unsafe fn view_buffer(
  ctx: *mut JSContext,
  view: JSValue,
  poff: *mut usize,
  plen: *mut usize,
) -> JSValue {
  let buf =
    unsafe { JS_GetTypedArrayBuffer(ctx, view, poff, plen, ptr::null_mut()) };
  if buf.tag != JS_TAG_EXCEPTION {
    return buf;
  }
  let exc = unsafe { JS_GetException(ctx) };
  unsafe { JS_FreeValue(ctx, exc) };
  let bufp = unsafe { JS_GetPropertyStr(ctx, view, c"buffer".as_ptr()) };
  if bufp.tag != JS_TAG_OBJECT {
    unsafe { JS_FreeValue(ctx, bufp) };
    return jsv_exception();
  }
  if !poff.is_null() {
    unsafe { *poff = read_len_prop(ctx, view, c"byteOffset".as_ptr()) };
  }
  if !plen.is_null() {
    unsafe { *plen = read_len_prop(ctx, view, c"byteLength".as_ptr()) };
  }
  bufp
}

unsafe extern "C" fn bs_free_func(
  _rt: *mut JSRuntime,
  opaque: *mut c_void,
  _ptr: *mut c_void,
) {
  let inner = opaque as *mut BsInner;
  if inner.is_null() {
    return;
  }
  if unsafe { (*inner).refcount.fetch_sub(1, Ordering::SeqCst) } == 1 {
    unsafe { BsInner::destroy(inner) };
  } else {
    unsafe {
      (*inner).allocator_isolate = ptr::null_mut();
    }
  }
}

fn detach_key_matches(
  ctx: *mut JSContext,
  buf: JSValue,
  key: *const Value,
) -> bool {
  let has_key =
    unsafe { JS_GetPropertyStr(ctx, buf, c"__v8x_detach_key_set".as_ptr()) };
  let must_match = unsafe { JS_ToBool(ctx, has_key) } != 0;
  unsafe { JS_FreeValue(ctx, has_key) };
  if !must_match {
    return true;
  }
  if key.is_null() {
    return false;
  }

  let expected =
    unsafe { JS_GetPropertyStr(ctx, buf, c"__v8x_detach_key".as_ptr()) };
  if expected.tag == JS_TAG_EXCEPTION {
    unsafe { JS_FreeValue(ctx, expected) };
    return false;
  }
  let ok = unsafe { JS_IsStrictEqual(ctx, expected, jsval_of(key)) };
  unsafe { JS_FreeValue(ctx, expected) };
  ok
}

unsafe extern "C" fn malloc_free_func(
  _rt: *mut JSRuntime,
  _opaque: *mut c_void,
  ptr: *mut c_void,
) {
  if !ptr.is_null() {
    unsafe { free(ptr) };
  }
}

unsafe extern "C" fn allocator_free_func(
  _rt: *mut JSRuntime,
  opaque: *mut c_void,
  ptr: *mut c_void,
) {
  if opaque.is_null() {
    return;
  }
  let buf = unsafe { Box::from_raw(opaque as *mut AllocatorBuffer) };
  if !buf.isolate.is_null() {
    iso_state(buf.isolate).pending_array_buffer_frees.push((
      buf.allocator,
      ptr,
      buf.byte_length,
    ));
    return;
  }
  unsafe {
    super::allocator::allocator_free(buf.allocator, ptr, buf.byte_length);
  }
}

fn allocator_for_isolate(
  isolate: *mut RealIsolate,
) -> *mut crate::array_buffer::Allocator {
  if isolate.is_null() {
    return ptr::null_mut();
  }
  let st = iso_state(isolate);
  super::allocator::allocator_shared_get(&st.array_buffer_allocator)
}

fn maybe_collect_external_array_buffers(isolate: *mut RealIsolate) {
  if isolate.is_null() {
    return;
  }
  let st = iso_state(isolate);
  if st.external_memory.load(Ordering::SeqCst) > 64 * 1024 * 1024 {
    unsafe { JS_RunGC(st.rt) };
    release_pending_allocator_buffers(isolate);
  }
}

pub(crate) fn release_pending_allocator_buffers(isolate: *mut RealIsolate) {
  if isolate.is_null() {
    return;
  }
  let pending =
    std::mem::take(&mut iso_state(isolate).pending_array_buffer_frees);
  let mut released = 0usize;
  for (allocator, data, byte_length) in pending {
    unsafe {
      super::allocator::allocator_free(allocator, data, byte_length);
    }
    released = released.saturating_add(byte_length);
  }
  if released != 0 {
    adjust_external_memory(iso_state(isolate), -(released as i64));
  }
}

fn new_array_buffer_value(
  ctx: *mut JSContext,
  isolate: *mut RealIsolate,
  byte_length: usize,
) -> JSValue {
  let allocator_shared = if isolate.is_null() {
    SharedPtrBase::<Allocator>::default()
  } else {
    super::allocator::allocator_shared_copy(
      &iso_state(isolate).array_buffer_allocator,
    )
  };
  let allocator = super::allocator::allocator_shared_get(&allocator_shared);
  let allocator_is_rust = super::allocator::allocator_is_rust(allocator);
  if allocator_is_rust {
    maybe_collect_external_array_buffers(isolate);
  }
  let data = if allocator.is_null() {
    if byte_length == 0 {
      ptr::null_mut()
    } else {
      unsafe { calloc(byte_length, 1) }
    }
  } else {
    unsafe {
      super::allocator::allocator_allocate(allocator, byte_length, true)
    }
  };
  if data.is_null() && byte_length != 0 {
    return unsafe { JS_ThrowOutOfMemory(ctx) };
  }

  let inner = BsInner::boxed_with_allocator(
    data,
    byte_length,
    false,
    noop_deleter,
    ptr::null_mut(),
    allocator.is_null(),
    allocator_shared,
  );
  if allocator_is_rust {
    unsafe {
      (*inner).allocator_isolate = isolate;
    }
  }
  let obj = unsafe {
    JS_NewArrayBuffer(
      ctx,
      data as *mut u8,
      byte_length,
      Some(bs_free_func),
      inner as *mut c_void,
      false,
    )
  };
  if obj.tag == JS_TAG_EXCEPTION {
    unsafe { bs_free_func(ptr::null_mut(), inner as *mut c_void, data) };
    return obj;
  }
  owner_add(data as usize, inner);
  if allocator_is_rust && !isolate.is_null() {
    adjust_external_memory(iso_state(isolate), byte_length as i64);
  }
  obj
}

unsafe extern "C" fn allocator_array_buffer_constructor(
  ctx: *mut JSContext,
  _this_val: JSValue,
  argc: std::os::raw::c_int,
  argv: *mut JSValue,
) -> JSValue {
  let mut byte_length: i64 = 0;
  if argc > 0 && !argv.is_null() {
    if unsafe { JS_ToInt64(ctx, &mut byte_length, *argv) } < 0 {
      return jsv_exception();
    }
  }
  if byte_length < 0 {
    return unsafe {
      JS_ThrowRangeError(ctx, c"Invalid ArrayBuffer length".as_ptr())
    };
  }
  new_array_buffer_value(ctx, current_iso(), byte_length as usize)
}

pub(crate) fn install_array_buffer_constructor(
  isolate: *mut RealIsolate,
  ctx: *mut JSContext,
  global: JSValue,
) {
  let allocator = allocator_for_isolate(isolate);
  if !super::allocator::allocator_is_rust(allocator) {
    return;
  }
  unsafe {
    let original = JS_GetPropertyStr(ctx, global, c"ArrayBuffer".as_ptr());
    if original.tag == JS_TAG_EXCEPTION {
      return;
    }

    let wrapper = JS_NewCFunction2(
      ctx,
      allocator_array_buffer_constructor,
      c"ArrayBuffer".as_ptr(),
      1,
      JS_CFUNC_CONSTRUCTOR,
      0,
    );
    if wrapper.tag == JS_TAG_EXCEPTION {
      JS_FreeValue(ctx, original);
      return;
    }

    let prototype = JS_GetPropertyStr(ctx, original, c"prototype".as_ptr());
    if prototype.tag != JS_TAG_EXCEPTION {
      let prototype_for_constructor = JS_DupValue(ctx, prototype);
      JS_SetPropertyStr(ctx, wrapper, c"prototype".as_ptr(), prototype);
      JS_SetPropertyStr(
        ctx,
        prototype_for_constructor,
        c"constructor".as_ptr(),
        JS_DupValue(ctx, wrapper),
      );
      JS_FreeValue(ctx, prototype_for_constructor);
    }

    let is_view = JS_GetPropertyStr(ctx, original, c"isView".as_ptr());
    if is_view.tag != JS_TAG_EXCEPTION {
      JS_SetPropertyStr(ctx, wrapper, c"isView".as_ptr(), is_view);
    }

    JS_DefinePropertyValueStr(
      ctx,
      global,
      c"ArrayBuffer".as_ptr(),
      wrapper,
      JS_PROP_C_W_E,
    );
    JS_FreeValue(ctx, original);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__New__with_byte_length(
  isolate: *mut RealIsolate,
  byte_length: usize,
) -> *const ArrayBuffer {
  let st = iso_state(isolate);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  if ctx.is_null() {
    return ptr::null();
  }

  let obj = new_array_buffer_value(ctx, isolate, byte_length);
  if obj.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  intern::<ArrayBuffer>(obj)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__New__with_backing_store(
  isolate: *mut RealIsolate,
  backing_store: *const SharedRef<BackingStore>,
) -> *const ArrayBuffer {
  let st = iso_state(isolate);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  if ctx.is_null() || backing_store.is_null() {
    return ptr::null();
  }
  let inner = sp_get(backing_store as *const SharedPtrBase<BackingStore>);
  if inner.is_null() {
    return ptr::null();
  }
  if unsafe { (*inner).is_resizable_by_user_javascript && !(*inner).is_shared }
  {
    return unsafe { resizable_array_buffer_for_store(ctx, inner) };
  }
  let (data, len, deleter_data) =
    unsafe { ((*inner).data, (*inner).byte_length, (*inner).deleter_data) };

  // Two distinct cases reach here, distinguished by whether the backing store
  // OWNS its bytes (it carries a deleter with non-null deleter_data — e.g. an
  // op return value via `new_backing_store_from_bytes`, a transient Box deno
  // hands to JS and never reads back) vs deno-owned bytes exposed for SHARING
  // (`new_backing_store_from_ptr` with a no-op/null deleter — timer_expiry,
  // timer_info, tick_info: Rust writes, JS reads, or vice versa).
  //   - Owned/transient: COPY into a self-contained QuickJS buffer. Deno frees
  //     its allocation on its own schedule (independent of this engine's GC),
  //     so aliasing use-after-frees — observed as garbage fetch/Blob bodies.
  //   - Shared: ALIAS the external pointer; a copy would desync the two sides.
  // (SharedArrayBuffer keeps aliasing via its own shim for cross-thread use.)
  if !deleter_data.is_null() {
    let obj = if data.is_null() || len == 0 {
      unsafe { JS_NewArrayBufferCopy(ctx, ptr::null(), 0) }
    } else {
      unsafe { JS_NewArrayBufferCopy(ctx, data as *const u8, len) }
    };
    if obj.tag == JS_TAG_EXCEPTION {
      return ptr::null();
    }
    return intern::<ArrayBuffer>(obj);
  }

  unsafe { (*inner).refcount.fetch_add(1, Ordering::SeqCst) };
  let obj = unsafe {
    JS_NewArrayBuffer(
      ctx,
      data as *mut u8,
      len,
      Some(bs_free_func),
      inner as *mut c_void,
      false,
    )
  };
  if obj.tag == JS_TAG_EXCEPTION {
    unsafe { bs_free_func(ptr::null_mut(), inner as *mut c_void, data) };
    return ptr::null();
  }
  owner_add(data as usize, inner);
  intern::<ArrayBuffer>(obj)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__ByteLength(
  this: *const ArrayBuffer,
) -> usize {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  let mut len: usize = 0;
  let data = unsafe { JS_GetArrayBuffer(ctx, &mut len, jsval_of(this)) };
  if data.is_null() {
    unsafe { clear_pending(ctx) };
  }
  len
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Data(
  this: *const ArrayBuffer,
) -> *mut c_void {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null_mut();
  }
  let mut len: usize = 0;
  let data = unsafe { JS_GetArrayBuffer(ctx, &mut len, jsval_of(this)) };
  if data.is_null() {
    unsafe { clear_pending(ctx) };
  }
  data as *mut c_void
}

// Some ArrayBuffers must report `IsDetachable() == false` — notably the backing
// store of a `WebAssembly.Memory`, which V8 keeps non-detachable so deno's
// `to_v8_slice_detachable` rejects ops that would steal its bytes. QuickJS has no
// per-buffer "detachable" flag, so we tag such buffers with a hidden (non-enum,
// non-writable, non-configurable) own property and look it up here. The
// `HAS_NONDETACHABLE` gate keeps the common all-detachable path (every plain
// `ArrayBuffer` deno hands us) a single atomic load with no property lookup.
pub(crate) static HAS_NONDETACHABLE: AtomicBool = AtomicBool::new(false);

/// Mark `buf` (a JSValue holding an ArrayBuffer) as non-detachable.
pub(crate) fn mark_buffer_nondetachable(ctx: *mut JSContext, buf: JSValue) {
  unsafe {
    JS_DefinePropertyValueStr(
      ctx,
      buf,
      c"__v8x_nondetach".as_ptr(),
      jsv_bool(true),
      0,
    );
  }
  HAS_NONDETACHABLE.store(true, Ordering::Relaxed);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__IsDetachable(
  this: *const ArrayBuffer,
) -> bool {
  if this.is_null() {
    return false;
  }
  if !HAS_NONDETACHABLE.load(Ordering::Relaxed) {
    return true;
  }
  let ctx = current_ctx();
  if ctx.is_null() {
    return true;
  }
  let prop = unsafe {
    JS_GetPropertyStr(ctx, jsval_of(this), c"__v8x_nondetach".as_ptr())
  };
  let nondetach = unsafe { JS_ToBool(ctx, prop) } != 0;
  unsafe { JS_FreeValue(ctx, prop) };
  !nondetach
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__WasDetached(
  this: *const ArrayBuffer,
) -> bool {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return false;
  }
  let mut len: usize = 0;
  let data = unsafe { JS_GetArrayBuffer(ctx, &mut len, jsval_of(this)) };
  if data.is_null() {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return true;
  }
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Detach(
  this: *const ArrayBuffer,
  key: *const Value,
) -> MaybeBool {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return MaybeBool::Nothing;
  }
  let buf = jsval_of(this);
  if !detach_key_matches(ctx, buf, key) {
    return MaybeBool::Nothing;
  }
  // Steal the bytes for any outstanding alias backing stores before QuickJS frees
  // them: get_backing_store()-then-detach() (deno's transferable path) otherwise
  // leaves the SharedRef deno keeps pointing at freed QuickJS heap — fatal once
  // that buffer crosses to a worker thread.
  let mut len: usize = 0;
  let data = unsafe { JS_GetArrayBuffer(ctx, &mut len, buf) } as *mut c_void;
  if !data.is_null() {
    let inners = alias_registry().lock().unwrap().remove(&(data as usize));
    if let Some(inners) = inners {
      for inner_usize in inners {
        let inner = inner_usize as *mut BsInner;
        if inner.is_null() {
          continue;
        }
        unsafe {
          let blen = (*inner).byte_length;
          let copy = if blen > 0 {
            let m = malloc(blen);
            if !m.is_null() {
              ptr::copy_nonoverlapping(data, m, blen);
            }
            m
          } else {
            ptr::null_mut()
          };
          (*inner).data = copy;
          (*inner).owns_malloc = true;
          (*inner).deleter = noop_deleter;
          (*inner).deleter_data = ptr::null_mut();
          // Drop the dup that pinned the (now-detached) JS buffer; the bytes live
          // in our malloc copy now, so there is nothing cross-thread to free later.
          if !(*inner).retained_ctx.is_null() {
            JS_FreeValue((*inner).retained_ctx, (*inner).retained_val);
            (*inner).retained_ctx = ptr::null_mut();
            (*inner).retained_val = jsv_undefined();
          }
        }
      }
    }
  }
  unsafe { JS_DetachArrayBuffer(ctx, buf) };
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
  backing_store_for_buffer(ctx, jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__NewBackingStore__with_byte_length(
  isolate: *mut RealIsolate,
  byte_length: usize,
) -> *mut BackingStore {
  let _ = isolate;
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
  bs_inner(this).map_or(ptr::null_mut(), |b| {
    let (data, len) = refresh_resizable_alias(b);
    if len == 0 { ptr::null_mut() } else { data }
  })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BackingStore__ByteLength(
  this: *const BackingStore,
) -> usize {
  bs_inner(this).map_or(0, |b| refresh_resizable_alias(b).1)
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
  bs_inner(this).is_some_and(|b| b.is_resizable_by_user_javascript)
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
pub extern "C" fn v8__ArrayBufferView__Buffer(
  this: *const ArrayBufferView,
) -> *const ArrayBuffer {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }

  let buf = unsafe {
    view_buffer(ctx, jsval_of(this), ptr::null_mut(), ptr::null_mut())
  };
  if buf.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  intern::<ArrayBuffer>(buf)
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
    view_buffer(ctx, jsval_of(this), ptr::null_mut(), ptr::null_mut())
  };
  if buf.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null_mut();
  }
  let mut len: usize = 0;
  let data = unsafe { JS_GetArrayBuffer(ctx, &mut len, buf) as *mut c_void };

  unsafe { JS_FreeValue(ctx, buf) };
  if data.is_null() {
    unsafe { clear_pending(ctx) };
  }
  data
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__ByteLength(
  this: *const ArrayBufferView,
) -> usize {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  let mut len: usize = 0;
  let buf =
    unsafe { view_buffer(ctx, jsval_of(this), ptr::null_mut(), &mut len) };
  if buf.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return 0;
  }
  unsafe { JS_FreeValue(ctx, buf) };
  len
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__ByteOffset(
  this: *const ArrayBufferView,
) -> usize {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  let mut off: usize = 0;
  let buf =
    unsafe { view_buffer(ctx, jsval_of(this), &mut off, ptr::null_mut()) };
  if buf.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return 0;
  }
  unsafe { JS_FreeValue(ctx, buf) };
  off
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
    view_buffer(ctx, jsval_of(this), ptr::null_mut(), ptr::null_mut())
  };
  if buf.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return false;
  }
  let ok = buf.tag == JS_TAG_OBJECT;
  unsafe { JS_FreeValue(ctx, buf) };
  ok
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__GetContents(
  this: *const ArrayBufferView,
  storage: memory_span_t,
) -> memory_span_t {
  let _ = storage;
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return memory_span_t {
      data: ptr::null_mut(),
      size: 0,
    };
  }
  let mut off: usize = 0;
  let mut len: usize = 0;
  let buf = unsafe { view_buffer(ctx, jsval_of(this), &mut off, &mut len) };
  if buf.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return memory_span_t {
      data: ptr::null_mut(),
      size: 0,
    };
  }
  let mut buf_len: usize = 0;
  let base = unsafe { JS_GetArrayBuffer(ctx, &mut buf_len, buf) };
  unsafe { JS_FreeValue(ctx, buf) };
  if base.is_null() {
    // QuickJS reports detached backing stores as an exception. V8's
    // ArrayBufferView::GetContents contract instead returns an empty span and
    // does not leave an exception pending in the isolate.
    unsafe { clear_pending(ctx) };
    return memory_span_t {
      data: ptr::null_mut(),
      size: 0,
    };
  }
  let owner = owner_get(base as usize);
  let externally_backed = !owner.is_null()
    && unsafe {
      (*owner).retained_ctx.is_null()
        && !(*owner).owns_malloc
        && super::allocator::allocator_shared_get(&(*owner).allocator).is_null()
    };
  if len == 0 && buf_len == 0 && !externally_backed {
    return memory_span_t {
      data: storage.data,
      size: 0,
    };
  }
  memory_span_t {
    data: unsafe { base.add(off) },
    size: len,
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__New__with_backing_store(
  isolate: *mut RealIsolate,
  backing_store: *const SharedRef<BackingStore>,
) -> *const SharedArrayBuffer {
  let st = iso_state(isolate);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  if ctx.is_null() || backing_store.is_null() {
    return ptr::null();
  }
  let inner = sp_get(backing_store as *const SharedPtrBase<BackingStore>);
  if inner.is_null() {
    return ptr::null();
  }
  let (data, len) = unsafe { ((*inner).data, (*inner).byte_length) };
  unsafe {
    (*inner).is_shared = true;
    (*inner).refcount.fetch_add(1, Ordering::SeqCst);
  }
  let obj = unsafe {
    JS_NewArrayBuffer(
      ctx,
      data as *mut u8,
      len,
      Some(bs_free_func),
      inner as *mut c_void,
      true,
    )
  };
  if obj.tag == JS_TAG_EXCEPTION {
    unsafe { bs_free_func(ptr::null_mut(), inner as *mut c_void, data) };
    return ptr::null();
  }
  owner_add(data as usize, inner);
  intern::<SharedArrayBuffer>(obj)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__GetBackingStore(
  this: *const SharedArrayBuffer,
) -> SharedRef<BackingStore> {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return make_shared_ref(BsInner::new_allocated(0, true));
  }

  let sref = backing_store_for_buffer(ctx, jsval_of(this));
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
  ty: JSTypedArrayEnum,
) -> JSValue {
  let ctx = current_ctx();
  if ctx.is_null() || buf.is_null() {
    return JSValue {
      u: JSValueUnion { int32: 0 },
      tag: JS_TAG_NULL,
    };
  }

  let mut argv: [JSValue; 3] = [
    jsval_of(buf),
    unsafe { JS_NewInt64(ctx, byte_offset as i64) },
    unsafe { JS_NewInt64(ctx, length as i64) },
  ];
  let v = unsafe { JS_NewTypedArray(ctx, 3, argv.as_mut_ptr(), ty) };

  unsafe { JS_FreeValue(ctx, argv[1]) };
  unsafe { JS_FreeValue(ctx, argv[2]) };
  v
}

macro_rules! typed_array_new {
  ($fn_name:ident, $ty_name:ident, $qjs_ty:expr) => {
    #[unsafe(no_mangle)]
    pub extern "C" fn $fn_name(
      buf_ptr: *const ArrayBuffer,
      byte_offset: usize,
      length: usize,
    ) -> *const crate::$ty_name {
      let v = make_typed_array(buf_ptr, byte_offset, length, $qjs_ty);
      if v.tag == JS_TAG_EXCEPTION {
        let ctx = current_ctx();
        if !ctx.is_null() {
          let exc = unsafe { JS_GetException(ctx) };
          unsafe { JS_FreeValue(ctx, exc) };
        }
        return ptr::null();
      }
      if v.tag != JS_TAG_OBJECT {
        return ptr::null();
      }
      intern::<crate::$ty_name>(v)
    }
  };
}

typed_array_new!(v8__Uint8Array__New, Uint8Array, JS_TYPED_ARRAY_UINT8);
typed_array_new!(v8__Int8Array__New, Int8Array, JS_TYPED_ARRAY_INT8);
typed_array_new!(v8__Uint16Array__New, Uint16Array, JS_TYPED_ARRAY_UINT16);
typed_array_new!(v8__Int16Array__New, Int16Array, JS_TYPED_ARRAY_INT16);
typed_array_new!(v8__Uint32Array__New, Uint32Array, JS_TYPED_ARRAY_UINT32);
typed_array_new!(v8__Int32Array__New, Int32Array, JS_TYPED_ARRAY_INT32);
typed_array_new!(v8__Float32Array__New, Float32Array, JS_TYPED_ARRAY_FLOAT32);
typed_array_new!(v8__Float64Array__New, Float64Array, JS_TYPED_ARRAY_FLOAT64);
typed_array_new!(
  v8__BigInt64Array__New,
  BigInt64Array,
  JS_TYPED_ARRAY_BIG_INT64
);
typed_array_new!(
  v8__BigUint64Array__New,
  BigUint64Array,
  JS_TYPED_ARRAY_BIG_UINT64
);
typed_array_new!(
  v8__Uint8ClampedArray__New,
  Uint8ClampedArray,
  JS_TYPED_ARRAY_UINT8C
);
typed_array_new!(v8__Float16Array__New, Float16Array, JS_TYPED_ARRAY_FLOAT16);

#[unsafe(no_mangle)]
pub extern "C" fn v8__TypedArray__Length(
  this: *const crate::TypedArray,
) -> usize {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  let mut byte_len: usize = 0;
  let mut bpe: usize = 0;
  let buf = unsafe {
    JS_GetTypedArrayBuffer(
      ctx,
      jsval_of(this),
      ptr::null_mut(),
      &mut byte_len,
      &mut bpe,
    )
  };
  if buf.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return 0;
  }
  unsafe { JS_FreeValue(ctx, buf) };
  if bpe == 0 { byte_len } else { byte_len / bpe }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__DataView__New(
  buffer: *const ArrayBuffer,
  byte_offset: usize,
  length: usize,
) -> *const DataView {
  let ctx = current_ctx();
  if ctx.is_null() || buffer.is_null() {
    return ptr::null();
  }
  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let ctor = JS_GetPropertyStr(ctx, global, c"DataView".as_ptr());
    JS_FreeValue(ctx, global);
    if ctor.tag == JS_TAG_EXCEPTION || !JS_IsConstructor(ctx, ctor) {
      JS_FreeValue(ctx, ctor);
      return ptr::null();
    }

    let buf = JS_DupValue(ctx, jsval_of(buffer));
    let mut args = [
      buf,
      JS_NewInt64(ctx, byte_offset as i64),
      JS_NewInt64(ctx, length as i64),
    ];
    let v = JS_CallConstructor(ctx, ctor, 3, args.as_mut_ptr());
    JS_FreeValue(ctx, ctor);
    JS_FreeValue(ctx, args[0]);
    if v.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return ptr::null();
    }
    intern::<DataView>(v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__CopyContents(
  this: *const ArrayBufferView,
  dest: *mut c_void,
  byte_length: i32,
) -> usize {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() || dest.is_null() || byte_length <= 0 {
    return 0;
  }
  let mut off: usize = 0;
  let mut len: usize = 0;
  let buf = unsafe { view_buffer(ctx, jsval_of(this), &mut off, &mut len) };
  if buf.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return 0;
  }
  let mut buf_len: usize = 0;
  let base = unsafe { JS_GetArrayBuffer(ctx, &mut buf_len, buf) };
  unsafe { JS_FreeValue(ctx, buf) };
  if base.is_null() {
    unsafe { clear_pending(ctx) };
    return 0;
  }
  let n = std::cmp::min(len, byte_length as usize);
  unsafe {
    ptr::copy_nonoverlapping(base.add(off), dest as *mut u8, n);
  }
  n
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__ByteLength(
  this: *const SharedArrayBuffer,
) -> usize {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  let mut len: usize = 0;
  unsafe { JS_GetArrayBuffer(ctx, &mut len, jsval_of(this)) };
  len
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__SetDetachKey(
  this: *const ArrayBuffer,
  key: *const Value,
) {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() || key.is_null() {
    return;
  }
  unsafe {
    JS_DefinePropertyValueStr(
      ctx,
      jsval_of(this),
      c"__v8x_detach_key".as_ptr(),
      JS_DupValue(ctx, jsval_of(key)),
      JS_PROP_CONFIGURABLE | JS_PROP_WRITABLE,
    );
    JS_DefinePropertyValueStr(
      ctx,
      jsval_of(this),
      c"__v8x_detach_key_set".as_ptr(),
      JS_NewBool(ctx, 1),
      JS_PROP_CONFIGURABLE | JS_PROP_WRITABLE,
    );
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__BackingStore__use_count(
  ptr: *const SharedPtrBase<BackingStore>,
) -> long {
  let inner = sp_get(ptr);
  if inner.is_null() {
    return 0;
  }
  unsafe { (*inner).refcount.load(Ordering::SeqCst) as long }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__NewBackingStore__with_byte_length(
  _isolate: *mut RealIsolate,
  byte_length: usize,
) -> *mut BackingStore {
  BsInner::new_allocated(byte_length, true) as *mut BackingStore
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__NewBackingStore__with_data(
  data: *mut c_void,
  byte_length: usize,
  deleter: BackingStoreDeleterCallback,
  deleter_data: *mut c_void,
) -> *mut BackingStore {
  BsInner::boxed(data, byte_length, true, deleter, deleter_data, false)
    as *mut BackingStore
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__New__with_byte_length(
  isolate: *mut RealIsolate,
  byte_length: usize,
) -> *const SharedArrayBuffer {
  let st = iso_state(isolate);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  if ctx.is_null() {
    return ptr::null();
  }

  let inner = BsInner::new_allocated(byte_length, true);
  let data = unsafe { (*inner).data };
  let obj = unsafe {
    JS_NewArrayBuffer(
      ctx,
      data as *mut u8,
      byte_length,
      Some(bs_free_func),
      inner as *mut c_void,
      true,
    )
  };
  if obj.tag == JS_TAG_EXCEPTION {
    unsafe { bs_free_func(ptr::null_mut(), inner as *mut c_void, data) };
    return ptr::null();
  }
  owner_add(data as usize, inner);
  intern::<SharedArrayBuffer>(obj)
}

/// Bind a resizable backing store to a JS `ArrayBuffer`.
///
/// QuickJS only resizes buffers whose memory it allocated itself, so the store
/// cannot be handed to `JS_NewArrayBuffer` the way fixed stores are. Instead
/// the engine allocates a resizable buffer of the same shape, the store's bytes
/// move into it, and the store becomes an alias of that buffer, exactly like a
/// store obtained from `GetBackingStore`. Later reads of the store's data and
/// length go back to the buffer, because a resize may move the bytes.
unsafe fn resizable_array_buffer_for_store(
  ctx: *mut JSContext,
  inner: *mut BsInner,
) -> *const ArrayBuffer {
  let b = unsafe { &mut *inner };
  if !b.retained_ctx.is_null() {
    // Already bound: V8 hands out another ArrayBuffer over the same memory,
    // and the closest QuickJS equivalent is the buffer the store aliases.
    return intern::<ArrayBuffer>(unsafe {
      JS_DupValue(b.retained_ctx, b.retained_val)
    });
  }
  let obj = unsafe {
    let global = JS_GetGlobalObject(ctx);
    let ctor = JS_GetPropertyStr(ctx, global, c"ArrayBuffer".as_ptr());
    JS_FreeValue(ctx, global);
    let options = JS_NewObject(ctx);
    JS_SetPropertyStr(
      ctx,
      options,
      c"maxByteLength".as_ptr(),
      JS_NewInt64(ctx, b.max_byte_length as i64),
    );
    let mut args = [JS_NewInt64(ctx, b.byte_length as i64), options];
    let obj = JS_CallConstructor(ctx, ctor, 2, args.as_mut_ptr());
    JS_FreeValue(ctx, ctor);
    JS_FreeValue(ctx, args[0]);
    JS_FreeValue(ctx, args[1]);
    obj
  };
  if obj.tag == JS_TAG_EXCEPTION {
    unsafe { clear_pending(ctx) };
    return ptr::null();
  }
  let mut len: usize = 0;
  let data = unsafe { JS_GetArrayBuffer(ctx, &mut len, obj) } as *mut c_void;
  if data.is_null() {
    unsafe { clear_pending(ctx) };
    unsafe { JS_FreeValue(ctx, obj) };
    return ptr::null();
  }
  let copy_len = len.min(b.byte_length);
  if copy_len != 0 && !b.data.is_null() {
    unsafe { ptr::copy_nonoverlapping(b.data, data, copy_len) };
  }
  let old_data = b.data;
  owner_remove(old_data as usize, inner);
  registry_remove(old_data as usize, inner);
  if !old_data.is_null() {
    if b.owns_malloc {
      unsafe { free(old_data) };
    } else {
      unsafe { (b.deleter)(old_data, b.byte_length, b.deleter_data) };
    }
  }
  b.data = data;
  b.byte_length = len;
  b.owns_malloc = false;
  b.deleter = noop_deleter;
  b.deleter_data = ptr::null_mut();
  b.retained_ctx = ctx;
  b.retained_val = unsafe { JS_DupValue(ctx, obj) };
  registry_add(data as usize, inner);
  intern::<ArrayBuffer>(obj)
}

/// Re-read the data pointer and length of a store that aliases a resizable
/// JS buffer, since `resize()` reallocates the buffer's memory.
fn refresh_resizable_alias(b: &BsInner) -> (*mut c_void, usize) {
  if !b.is_resizable_by_user_javascript || b.retained_ctx.is_null() {
    return (b.data, b.byte_length);
  }
  let mut len: usize = 0;
  let data =
    unsafe { JS_GetArrayBuffer(b.retained_ctx, &mut len, b.retained_val) };
  if data.is_null() {
    unsafe { clear_pending(b.retained_ctx) };
    return (ptr::null_mut(), 0);
  }
  (data as *mut c_void, len)
}

fn try_new_allocated(
  byte_length: usize,
  is_shared: bool,
  on_failure: crate::BackingStoreOnFailureMode,
) -> *mut BackingStore {
  let fail = || {
    if matches!(on_failure, crate::BackingStoreOnFailureMode::ReturnNull) {
      return ptr::null_mut();
    }
    // V8 treats this as a fatal out-of-memory condition.
    eprintln!("Fatal JavaScript out of memory: NewBackingStore");
    std::process::abort();
  };
  if byte_length > crate::binding::v8__TypedArray__kMaxByteLength {
    return fail();
  }
  let data = if byte_length == 0 {
    ptr::null_mut()
  } else {
    unsafe { calloc(byte_length, 1) }
  };
  if data.is_null() && byte_length != 0 {
    return fail();
  }
  BsInner::boxed(
    data,
    byte_length,
    is_shared,
    noop_deleter,
    ptr::null_mut(),
    true,
  ) as *mut BackingStore
}

// The initialization mode only matters for speed: the memory comes from
// `calloc` either way, so "uninitialized" stores read as zero.
#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__NewBackingStore__with_mode(
  _isolate: *mut RealIsolate,
  byte_length: usize,
  _initialization_mode: crate::BackingStoreInitializationMode,
  on_failure: crate::BackingStoreOnFailureMode,
) -> *mut BackingStore {
  try_new_allocated(byte_length, false, on_failure)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__NewBackingStore__with_mode(
  _isolate: *mut RealIsolate,
  byte_length: usize,
  _initialization_mode: crate::BackingStoreInitializationMode,
  on_failure: crate::BackingStoreOnFailureMode,
) -> *mut BackingStore {
  try_new_allocated(byte_length, true, on_failure)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__NewResizableBackingStore(
  byte_length: usize,
  max_byte_length: usize,
) -> *mut BackingStore {
  let inner = BsInner::new_allocated(byte_length, false);
  unsafe {
    (*inner).is_resizable_by_user_javascript = true;
    (*inner).max_byte_length = max_byte_length.max(byte_length);
  }
  inner as *mut BackingStore
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BackingStore__MaxByteLength(
  this: *const BackingStore,
) -> usize {
  bs_inner(this)
    .map_or(0, |b| b.max_byte_length.max(refresh_resizable_alias(b).1))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__MaybeNew(
  isolate: *mut RealIsolate,
  byte_length: usize,
  _initialization_mode: crate::BackingStoreInitializationMode,
) -> *const ArrayBuffer {
  if byte_length > crate::binding::v8__TypedArray__kMaxByteLength {
    return ptr::null();
  }
  let st = iso_state(isolate);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  if ctx.is_null() {
    return ptr::null();
  }
  let obj = new_array_buffer_value(ctx, isolate, byte_length);
  if obj.tag == JS_TAG_EXCEPTION {
    // MaybeNew reports failure through the empty handle, not an exception.
    unsafe { clear_pending(ctx) };
    return ptr::null();
  }
  intern::<ArrayBuffer>(obj)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__IsResizableByUserJavaScript(
  this: *const ArrayBuffer,
) -> bool {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return false;
  }
  unsafe { is_resizable_by_user_javascript(ctx, jsval_of(this), false) }
}

unsafe extern "C" {
  fn JS_IsImmutableArrayBuffer(obj: JSValue) -> std::os::raw::c_int;
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__IsImmutable(
  this: *const ArrayBuffer,
) -> bool {
  !this.is_null() && unsafe { JS_IsImmutableArrayBuffer(jsval_of(this)) } > 0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__Data(
  this: *const SharedArrayBuffer,
) -> *mut c_void {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null_mut();
  }
  let mut len: usize = 0;
  let data = unsafe { JS_GetArrayBuffer(ctx, &mut len, jsval_of(this)) };
  if data.is_null() {
    unsafe { clear_pending(ctx) };
  }
  data as *mut c_void
}

macro_rules! typed_array_new_with_shared_buffer {
  ($fn_name:ident, $ty_name:ident, $qjs_ty:expr) => {
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
        $qjs_ty,
      );
      if v.tag == JS_TAG_EXCEPTION {
        let ctx = current_ctx();
        if !ctx.is_null() {
          unsafe { clear_pending(ctx) };
        }
        return ptr::null();
      }
      if v.tag != JS_TAG_OBJECT {
        return ptr::null();
      }
      intern::<crate::$ty_name>(v)
    }
  };
}

typed_array_new_with_shared_buffer!(
  v8__Uint8Array__New__with_shared_buffer,
  Uint8Array,
  JS_TYPED_ARRAY_UINT8
);
typed_array_new_with_shared_buffer!(
  v8__Int8Array__New__with_shared_buffer,
  Int8Array,
  JS_TYPED_ARRAY_INT8
);
typed_array_new_with_shared_buffer!(
  v8__Uint16Array__New__with_shared_buffer,
  Uint16Array,
  JS_TYPED_ARRAY_UINT16
);
typed_array_new_with_shared_buffer!(
  v8__Int16Array__New__with_shared_buffer,
  Int16Array,
  JS_TYPED_ARRAY_INT16
);
typed_array_new_with_shared_buffer!(
  v8__Uint32Array__New__with_shared_buffer,
  Uint32Array,
  JS_TYPED_ARRAY_UINT32
);
typed_array_new_with_shared_buffer!(
  v8__Int32Array__New__with_shared_buffer,
  Int32Array,
  JS_TYPED_ARRAY_INT32
);
typed_array_new_with_shared_buffer!(
  v8__Float16Array__New__with_shared_buffer,
  Float16Array,
  JS_TYPED_ARRAY_FLOAT16
);
typed_array_new_with_shared_buffer!(
  v8__Float32Array__New__with_shared_buffer,
  Float32Array,
  JS_TYPED_ARRAY_FLOAT32
);
typed_array_new_with_shared_buffer!(
  v8__Float64Array__New__with_shared_buffer,
  Float64Array,
  JS_TYPED_ARRAY_FLOAT64
);
typed_array_new_with_shared_buffer!(
  v8__BigInt64Array__New__with_shared_buffer,
  BigInt64Array,
  JS_TYPED_ARRAY_BIG_INT64
);
typed_array_new_with_shared_buffer!(
  v8__BigUint64Array__New__with_shared_buffer,
  BigUint64Array,
  JS_TYPED_ARRAY_BIG_UINT64
);
typed_array_new_with_shared_buffer!(
  v8__Uint8ClampedArray__New__with_shared_buffer,
  Uint8ClampedArray,
  JS_TYPED_ARRAY_UINT8C
);

#[unsafe(no_mangle)]
pub extern "C" fn v8__DataView__New__with_shared_buffer(
  sab: *const SharedArrayBuffer,
  byte_offset: usize,
  length: usize,
) -> *const DataView {
  v8__DataView__New(sab as *const ArrayBuffer, byte_offset, length)
}
