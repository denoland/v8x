//! Family "misc" (QuickJS backend): SnapshotCreator / StartupData / CppHeap /
//! cppgc / WeakCallbackInfo / Proxy / JSON / Wasm* / Task / IdleTask / Global.
//!
//! QuickJS has no equivalent for V8 snapshots, cppgc (Oilpan), the WebAssembly
//! C++ internals or the C++ task abstractions, so most of these are safe inert
//! defaults (see the `TODO(qjs)` markers). The pieces QuickJS *can* back are
//! implemented for real:
//!   * `v8__Global__New` / `v8__Global__Reset` — `JS_DupValue` / `JS_FreeValue`
//!     against a stable context, with a process-wide protect refcount so the
//!     value outlives handle scopes and survives GC.
//!   * `v8__JSON__Parse` — `JS_ParseJSON`.
//!   * `v8__SnapshotCreator__GetIsolate` / `v8__WeakCallbackInfo__GetIsolate`
//!     surface the current isolate.
//!
//! Mirrors the C-ABI shape of the JSC backend (`src/misc.rs`) but routes
//! every JSValue through the QuickJS refcount helpers in `core`.
#![allow(non_snake_case, unused)]

use crate::binding::RustObj;
use crate::quickjs::core::{
  PersistentCell, PersistentHandle, adjust_external_memory, ctx_of,
  current_ctx, current_iso, intern, intern_dup, iso_state, jsval_of,
  release_external_string_memory,
};
use crate::quickjs::quickjs_sys::*;
use crate::{
  Context, Data, FunctionTemplate, Object, RealIsolate, String as V8String,
  Value,
};

use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

struct WeakCallbackInfoShim {
  isolate: *mut RealIsolate,
  parameter: *const c_void,
  second_pass: Option<crate::quickjs::core::WeakCallback>,
}

struct ProxyRevokeEntry {
  proxy: JSValue,
  revoke: JSValue,
  revoked: bool,
}

thread_local! {
  static PROXY_REVOKES: std::cell::RefCell<Vec<ProxyRevokeEntry>> =
    const { std::cell::RefCell::new(Vec::new()) };
}

#[repr(C)]
struct JSPropertyEnum {
  is_enumerable: bool,
  atom: JSAtom,
}

#[repr(C)]
struct JSPropertyDescriptorQjs {
  flags: c_int,
  value: JSValue,
  getter: JSValue,
  setter: JSValue,
}

const JS_GPN_STRING_MASK: c_int = 1 << 0;
const JS_GPN_SYMBOL_MASK: c_int = 1 << 1;
const JS_GPN_ENUM_ONLY: c_int = 1 << 4;
const JS_PROP_GETSET_QJS: c_int = 1 << 4;
const JS_PROP_CONFIGURABLE_QJS: c_int = 1 << 0;
const JS_PROP_WRITABLE_QJS: c_int = 1 << 1;
const JS_PROP_HAS_CONFIGURABLE_QJS: c_int = 1 << 8;
const JS_PROP_HAS_WRITABLE_QJS: c_int = 1 << 9;
const JS_PROP_HAS_ENUMERABLE_QJS: c_int = 1 << 10;
const JS_PROP_HAS_VALUE_QJS: c_int = 1 << 13;
const HEAP_SNAPSHOT_MAX_DEPTH: usize = 5;
const HEAP_SNAPSHOT_ARRAY_SAMPLE: u32 = 64;

unsafe extern "C" {
  fn v82jsc_clear_error_backtrace(ctx: *mut JSContext, error: JSValue);
  fn JS_GetOwnPropertyNames(
    ctx: *mut JSContext,
    ptab: *mut *mut JSPropertyEnum,
    plen: *mut u32,
    obj: JSValue,
    flags: c_int,
  ) -> c_int;
  fn JS_GetProperty(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: JSAtom,
  ) -> JSValue;
  fn JS_GetOwnProperty(
    ctx: *mut JSContext,
    desc: *mut JSPropertyDescriptorQjs,
    obj: JSValue,
    prop: JSAtom,
  ) -> c_int;
  fn JS_DefinePropertyValueStr(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: *const c_char,
    val: JSValue,
    flags: c_int,
  ) -> c_int;
  fn rusty_v8_RustObj_trace(obj: *const RustObj, visitor: *mut c_void);
  fn rusty_v8_RustObj_get_name(obj: *const RustObj) -> *const c_char;
  fn rusty_v8_RustObj_drop(obj: *mut RustObj);
}

fn same_value_identity(a: JSValue, b: JSValue) -> bool {
  if a.tag != b.tag {
    return false;
  }
  match a.tag {
    JS_TAG_OBJECT
    | JS_TAG_STRING
    | JS_TAG_STRING_ROPE
    | JS_TAG_SYMBOL
    | JS_TAG_BIG_INT
    | JS_TAG_MODULE
    | JS_TAG_FUNCTION_BYTECODE => unsafe { a.u.ptr == b.u.ptr },
    JS_TAG_FLOAT64 => unsafe { a.u.float64.to_bits() == b.u.float64.to_bits() },
    _ => unsafe { a.u.int32 == b.u.int32 },
  }
}

fn register_proxy_revoke(ctx: *mut JSContext, proxy: JSValue, revoke: JSValue) {
  PROXY_REVOKES.with(|entries| {
    entries.borrow_mut().push(ProxyRevokeEntry {
      proxy: unsafe { JS_DupValue(ctx, proxy) },
      revoke,
      revoked: false,
    });
  });
}

fn proxy_revoked_from_table(proxy: JSValue) -> Option<bool> {
  PROXY_REVOKES.with(|entries| {
    entries
      .borrow()
      .iter()
      .find(|entry| same_value_identity(entry.proxy, proxy))
      .map(|entry| entry.revoked)
  })
}

fn proxy_revoke_function(
  ctx: *mut JSContext,
  proxy: JSValue,
) -> Option<JSValue> {
  PROXY_REVOKES.with(|entries| {
    let mut entries = entries.borrow_mut();
    let entry = entries
      .iter_mut()
      .find(|entry| same_value_identity(entry.proxy, proxy))?;
    entry.revoked = true;
    Some(unsafe { JS_DupValue(ctx, entry.revoke) })
  })
}

fn is_strongly_reachable_from_handles(
  isolate: *mut RealIsolate,
  weak_handle: *const Data,
) -> bool {
  if isolate.is_null() || weak_handle.is_null() {
    return false;
  }
  let weak_slot = weak_handle as *const JSValue;
  let weak_value = unsafe { *weak_slot };
  let st = iso_state(isolate);
  st.handles.iter().any(|&slot| {
    !slot.is_null() && same_value_identity(unsafe { *slot }, weak_value)
  }) || st.persistent_handles.iter().any(|handle| {
    let slot = handle.slot;
    !handle.is_weak
      && !slot.is_null()
      && !std::ptr::addr_eq(slot, weak_slot)
      && same_value_identity(unsafe { *slot }, weak_value)
  })
}

fn collect_weak_handles(isolate: *mut RealIsolate) {
  if isolate.is_null() {
    return;
  }

  let weak_handles = {
    let st = iso_state(isolate);
    std::mem::take(&mut st.weak_handles)
  };

  let mut survivors = Vec::new();
  for weak in weak_handles {
    let is_live = if jsv_is_undefined(&weak.weak_ref) {
      is_strongly_reachable_from_handles(isolate, weak.handle)
    } else {
      unsafe { v82jsc_weak_ref_is_live(weak.weak_ref) }
    };
    if is_live {
      if !jsv_is_undefined(&weak.weak_ref) {
        let cell = weak.handle as *mut PersistentCell;
        let target = unsafe { v82jsc_weak_ref_target(weak.weak_ref) };
        if target.tag != JS_TAG_EXCEPTION {
          unsafe { (*cell).value = target };
        }
      }
      survivors.push(weak);
      continue;
    }

    unsafe { JS_FreeValue(weak.context, weak.weak_ref) };
    let mut info = WeakCallbackInfoShim {
      isolate,
      parameter: weak.parameter,
      second_pass: None,
    };
    unsafe { (weak.callback)(&info as *const _ as *const c_void) };
    if let Some(second_pass) = info.second_pass {
      unsafe { second_pass(&info as *const _ as *const c_void) };
    }
  }

  if !survivors.is_empty() {
    iso_state(isolate).weak_handles.extend(survivors);
  }
}

fn release_weak_handle_targets(isolate: *mut RealIsolate) {
  if isolate.is_null() {
    return;
  }
  for weak in &iso_state(isolate).weak_handles {
    if jsv_is_undefined(&weak.weak_ref) || weak.handle.is_null() {
      continue;
    }
    let cell = weak.handle as *mut PersistentCell;
    unsafe {
      JS_FreeValue((*cell).context, (*cell).value);
      (*cell).value = jsv_undefined();
    }
  }
}

fn run_gc_callbacks(
  isolate: *mut RealIsolate,
  callbacks: &[crate::quickjs::core::GcCallbackEntry],
) {
  let raw = crate::isolate::UnsafeRawIsolatePtr::from_real_ptr(isolate);
  for entry in callbacks {
    unsafe {
      (entry.callback)(
        raw,
        entry.gc_type_filter,
        crate::gc::GCCallbackFlags(0),
        entry.data,
      );
    }
  }
}

unsafe extern "C" {

  fn JS_ParseJSON(
    ctx: *mut JSContext,
    buf: *const c_char,
    buf_len: usize,
    filename: *const c_char,
  ) -> JSValue;
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__initialize_process(_platform: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__shutdown_process() {}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__heap__enable_detached_garbage_collections_for_testing(
  _heap: *mut c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__heap__collect_garbage_for_testing(
  _heap: *mut c_void,
  _stack_state: u8,
) {
  cppgc_collect_garbage();
}

#[derive(Clone, Copy)]
struct CppGcObject {
  ptr: usize,
  size: usize,
  align: usize,
  wrapper_key: Option<usize>,
  weak_wrapper: JSValue,
}

thread_local! {
  static CPPGC_OBJECTS: std::cell::RefCell<Vec<CppGcObject>> =
    const { std::cell::RefCell::new(Vec::new()) };
  static CPPGC_MEMBERS: std::cell::RefCell<HashMap<usize, usize>> =
    std::cell::RefCell::new(HashMap::new());
  static CPPGC_WEAK_MEMBERS: std::cell::RefCell<HashMap<usize, usize>> =
    std::cell::RefCell::new(HashMap::new());
  static CPPGC_TRACE_MARKS: std::cell::Cell<*mut HashSet<usize>> =
    const { std::cell::Cell::new(ptr::null_mut()) };
}

fn cppgc_member_construct(
  members: &std::cell::RefCell<HashMap<usize, usize>>,
  member: *mut c_void,
  obj: *mut c_void,
) {
  if member.is_null() {
    return;
  }
  unsafe { ptr::write_unaligned(member as *mut u32, 0) };
  let key = member as usize;
  if obj.is_null() {
    members.borrow_mut().remove(&key);
  } else {
    members.borrow_mut().insert(key, obj as usize);
  }
}

fn cppgc_member_get(
  members: &std::cell::RefCell<HashMap<usize, usize>>,
  member: *const c_void,
) -> *mut c_void {
  if member.is_null() {
    return ptr::null_mut();
  }
  members
    .borrow()
    .get(&(member as usize))
    .copied()
    .map(|ptr| ptr as *mut c_void)
    .unwrap_or(ptr::null_mut())
}

fn cppgc_mark_object(obj: *mut RustObj) {
  if obj.is_null() {
    return;
  }
  let ptr = obj as usize;
  CPPGC_TRACE_MARKS.with(|marks| {
    let marks_ptr = marks.get();
    if marks_ptr.is_null() {
      return;
    }
    let should_trace = unsafe {
      let marks = &mut *marks_ptr;
      marks.insert(ptr)
    };
    if should_trace {
      let mut visitor = 0usize;
      unsafe {
        rusty_v8_RustObj_trace(
          obj as *const RustObj,
          &mut visitor as *mut _ as *mut c_void,
        );
      }
    }
  });
}

fn cppgc_mark_wrapper_key(wrapper_key: usize) -> bool {
  let objects = CPPGC_OBJECTS.with(|objects| {
    objects
      .borrow()
      .iter()
      .filter(|object| object.wrapper_key == Some(wrapper_key))
      .map(|object| object.ptr)
      .collect::<Vec<_>>()
  });
  let found = !objects.is_empty();
  for object in objects {
    cppgc_mark_object(object as *mut RustObj);
  }
  found
}

unsafe fn new_native_weak_ref(ctx: *mut JSContext, target: JSValue) -> JSValue {
  let weak = unsafe { v82jsc_new_weak_ref(ctx, target) };
  if weak.tag == JS_TAG_EXCEPTION {
    let exception = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exception) };
    return jsv_undefined();
  }
  weak
}

unsafe fn cppgc_weak_wrapper_is_live(weak: JSValue) -> bool {
  if jsv_is_undefined(&weak) {
    // WeakRef is a standard QuickJS intrinsic. Be conservative if creating it
    // failed so a live native object can never become a dangling wrapper.
    return true;
  }
  unsafe { v82jsc_weak_ref_is_live(weak) }
}

fn cppgc_value_has_wrapper_marker(ctx: *mut JSContext, value: JSValue) -> bool {
  if ctx.is_null() || !jsv_is_object(&value) {
    return false;
  }
  let marker =
    unsafe { JS_GetPropertyStr(ctx, value, c"__qjs_cppgc_wrapped".as_ptr()) };
  if marker.tag == JS_TAG_EXCEPTION {
    return false;
  }
  let marked = unsafe { JS_ToBool(ctx, marker) != 0 };
  unsafe { JS_FreeValue(ctx, marker) };
  marked
}

fn cppgc_mark_js_value(ctx: *mut JSContext, value: JSValue) -> bool {
  if !jsv_is_object(&value) {
    return false;
  }
  if !cppgc_value_has_wrapper_marker(ctx, value) {
    return false;
  }
  let key = unsafe { value.u.ptr } as usize;
  if key != 0 {
    return cppgc_mark_wrapper_key(key);
  }
  false
}

unsafe fn cppgc_mark_js_graph(
  ctx: *mut JSContext,
  value: JSValue,
  depth: usize,
  seen_objects: &mut HashSet<usize>,
) {
  if ctx.is_null() || depth > HEAP_SNAPSHOT_MAX_DEPTH || !jsv_is_object(&value)
  {
    return;
  }
  let key = unsafe { value.u.ptr } as usize;
  if key == 0 || !seen_objects.insert(key) {
    return;
  }

  cppgc_mark_js_value(ctx, value);

  let mut ptab: *mut JSPropertyEnum = ptr::null_mut();
  let mut plen = 0u32;
  let flags = JS_GPN_STRING_MASK | JS_GPN_SYMBOL_MASK | JS_GPN_ENUM_ONLY;
  let rc =
    unsafe { JS_GetOwnPropertyNames(ctx, &mut ptab, &mut plen, value, flags) };
  if rc != 0 || ptab.is_null() {
    return;
  }
  for index in 0..plen as usize {
    let atom = unsafe { (*ptab.add(index)).atom };
    let property = unsafe { JS_GetProperty(ctx, value, atom) };
    unsafe {
      cppgc_mark_js_graph(ctx, property, depth + 1, seen_objects);
      JS_FreeValue(ctx, property);
      JS_FreeAtom(ctx, atom);
    }
  }
  unsafe { js_free(ctx, ptab as *mut c_void) };
}

unsafe fn cppgc_mark_global_properties(ctx: *mut JSContext, global: JSValue) {
  let mut ptab: *mut JSPropertyEnum = ptr::null_mut();
  let mut plen = 0u32;
  let flags = JS_GPN_STRING_MASK | JS_GPN_SYMBOL_MASK | JS_GPN_ENUM_ONLY;
  let rc =
    unsafe { JS_GetOwnPropertyNames(ctx, &mut ptab, &mut plen, global, flags) };
  if rc != 0 || ptab.is_null() {
    return;
  }
  for index in 0..plen as usize {
    let atom = unsafe { (*ptab.add(index)).atom };
    let property = unsafe { JS_GetProperty(ctx, global, atom) };
    cppgc_mark_js_value(ctx, property);
    unsafe {
      JS_FreeValue(ctx, property);
      JS_FreeAtom(ctx, atom);
    }
  }
  unsafe { js_free(ctx, ptab as *mut c_void) };
}

fn cppgc_mark_traced_reference(ref_: *const c_void) {
  if ref_.is_null() {
    return;
  }
  let slot = unsafe { (ref_ as *const usize).read_unaligned() };
  if slot == 0 {
    return;
  }
  let value = unsafe { *(slot as *const JSValue) };
  let ctx = stable_ctx();
  if ctx.is_null() {
    return;
  }
  let mut seen_objects = HashSet::new();
  unsafe { cppgc_mark_js_graph(ctx, value, 0, &mut seen_objects) };
}

fn cppgc_collect_garbage() {
  let ctx = stable_ctx();
  let mut marks = HashSet::new();
  CPPGC_TRACE_MARKS.with(|trace_marks| {
    let previous = trace_marks.replace(&mut marks);
    if !ctx.is_null() {
      let roots = CPPGC_OBJECTS.with(|objects| {
        objects
          .borrow()
          .iter()
          .filter(|object| {
            object.wrapper_key.is_some()
              && unsafe { cppgc_weak_wrapper_is_live(object.weak_wrapper) }
          })
          .map(|object| object.ptr)
          .collect::<Vec<_>>()
      });
      for root in roots {
        cppgc_mark_object(root as *mut RustObj);
      }
    }
    trace_marks.set(previous);
  });

  let mut dropped = Vec::new();
  CPPGC_OBJECTS.with(|objects| {
    let mut objects = objects.borrow_mut();
    let mut index = 0;
    while index < objects.len() {
      if marks.contains(&objects[index].ptr) {
        index += 1;
      } else {
        dropped.push(objects.remove(index));
      }
    }
  });

  for object in dropped {
    unsafe {
      if !ctx.is_null() {
        JS_FreeValue(ctx, object.weak_wrapper);
      }
      rusty_v8_RustObj_drop(object.ptr as *mut RustObj);
      if let Ok(layout) =
        std::alloc::Layout::from_size_align(object.size, object.align)
      {
        std::alloc::dealloc(object.ptr as *mut u8, layout);
      }
    }
  }
}

pub(crate) fn cppgc_register_wrapper(wrapper: JSValue, value: *const RustObj) {
  if value.is_null() || !jsv_is_object(&wrapper) {
    return;
  }
  let ctx = stable_ctx();
  if !ctx.is_null() {
    let marker = unsafe { JS_NewBool(ctx, 1) };
    let flags = JS_PROP_CONFIGURABLE_QJS
      | JS_PROP_WRITABLE_QJS
      | JS_PROP_HAS_CONFIGURABLE_QJS
      | JS_PROP_HAS_WRITABLE_QJS
      | JS_PROP_HAS_ENUMERABLE_QJS
      | JS_PROP_HAS_VALUE_QJS;
    unsafe {
      JS_DefinePropertyValueStr(
        ctx,
        wrapper,
        c"__qjs_cppgc_wrapped".as_ptr(),
        marker,
        flags,
      );
    }
  }
  let wrapper_key = unsafe { wrapper.u.ptr } as usize;
  let weak_wrapper = if ctx.is_null() {
    jsv_undefined()
  } else {
    unsafe { new_native_weak_ref(ctx, wrapper) }
  };
  CPPGC_OBJECTS.with(|objects| {
    if let Some(object) = objects
      .borrow_mut()
      .iter_mut()
      .find(|object| object.ptr == value as usize)
    {
      if !ctx.is_null() {
        unsafe { JS_FreeValue(ctx, object.weak_wrapper) };
      }
      object.wrapper_key = Some(wrapper_key);
      object.weak_wrapper = weak_wrapper;
    } else if !ctx.is_null() {
      unsafe { JS_FreeValue(ctx, weak_wrapper) };
    }
  });
}

pub(crate) fn cppgc_wrapper_matches(
  value: *mut RustObj,
  wrapper: JSValue,
) -> bool {
  if value.is_null() || !jsv_is_object(&wrapper) {
    return false;
  }
  let wrapper_key = unsafe { wrapper.u.ptr } as usize;
  CPPGC_OBJECTS.with(|objects| {
    objects.borrow().iter().any(|object| {
      object.ptr == value as usize
        && object.wrapper_key == Some(wrapper_key)
        && unsafe { cppgc_weak_wrapper_is_live(object.weak_wrapper) }
    })
  })
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Member__CONSTRUCT(
  member: *mut c_void,
  obj: *mut c_void,
) {
  CPPGC_MEMBERS.with(|members| cppgc_member_construct(members, member, obj));
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Member__DESTRUCT(member: *mut c_void) {
  if !member.is_null() {
    unsafe { ptr::write_unaligned(member as *mut u32, 0) };
    CPPGC_MEMBERS.with(|members| {
      members.borrow_mut().remove(&(member as usize));
    });
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Member__Get(member: *const c_void) -> *mut c_void {
  CPPGC_MEMBERS.with(|members| cppgc_member_get(members, member))
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Member__Assign(member: *mut c_void, obj: *mut c_void) {
  CPPGC_MEMBERS.with(|members| cppgc_member_construct(members, member, obj));
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakMember__CONSTRUCT(
  member: *mut c_void,
  obj: *mut c_void,
) {
  CPPGC_WEAK_MEMBERS
    .with(|members| cppgc_member_construct(members, member, obj));
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakMember__DESTRUCT(member: *mut c_void) {
  if !member.is_null() {
    unsafe { ptr::write_unaligned(member as *mut u32, 0) };
    CPPGC_WEAK_MEMBERS.with(|members| {
      members.borrow_mut().remove(&(member as usize));
    });
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakMember__Get(member: *const c_void) -> *mut c_void {
  CPPGC_WEAK_MEMBERS.with(|members| cppgc_member_get(members, member))
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakMember__Assign(
  member: *mut c_void,
  obj: *mut c_void,
) {
  CPPGC_WEAK_MEMBERS
    .with(|members| cppgc_member_construct(members, member, obj));
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Visitor__Trace__Member(
  _visitor: *mut c_void,
  member: *const c_void,
) {
  let obj = CPPGC_MEMBERS.with(|members| cppgc_member_get(members, member));
  cppgc_mark_object(obj as *mut RustObj);
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Visitor__Trace__WeakMember(
  _visitor: *mut c_void,
  _member: *const c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__RequestGarbageCollectionForTesting(
  isolate: *mut RealIsolate,
  _type: usize,
) {
  // Best-effort: run the engine GC. Our cppgc shim doesn't reclaim native
  // wrappers, so the cppgc-specific collection assertions still won't hold,
  // but this satisfies the many `test_api` tests that just need a GC to run.
  if isolate.is_null() {
    return;
  }
  let st = iso_state(isolate);
  let prologue_callbacks = st.gc_prologue_callbacks.clone();
  run_gc_callbacks(isolate, &prologue_callbacks);
  release_weak_handle_targets(isolate);
  // Two passes: freeing a cppgc-wrapped object can release references held by
  // its native member (e.g. a wrapper wrapping another wrapper), and those only
  // become collectable after the first sweep + cppgc drop round.
  for _ in 0..2 {
    if !st.rt.is_null() {
      unsafe { JS_RunGC(st.rt) };
    }
    cppgc_collect_garbage();
  }
  release_external_string_memory(st);
  let epilogue_callbacks = st.gc_epilogue_callbacks.clone();
  run_gc_callbacks(isolate, &epilogue_callbacks);
  collect_weak_handles(isolate);
}

thread_local! {
    static DUMMY_CPP_HEAP: std::cell::Cell<*mut c_void> =
        const { std::cell::Cell::new(ptr::null_mut()) };
}

fn current_cpp_heap() -> *mut c_void {
  DUMMY_CPP_HEAP.with(|c| {
    let mut h = c.get();
    if h.is_null() {
      h = Box::into_raw(Box::new(0u64)) as *mut c_void;
      c.set(h);
    }
    h
  })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CppHeap__Create(
  _platform: *mut c_void,
  _marking_support: u8,
  _sweeping_support: u8,
) -> *mut c_void {
  current_cpp_heap()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CppHeap__Terminate(_heap: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CppHeap__DELETE(_heap: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetCppHeap(
  _isolate: *mut RealIsolate,
) -> *mut c_void {
  current_cpp_heap()
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__make_garbage_collectable(
  _heap: *mut c_void,
  additional_bytes: usize,
  align: usize,
) -> *mut c_void {
  const RUST_OBJ_SIZE: usize = 8;
  let size = RUST_OBJ_SIZE + additional_bytes;
  let align = align.max(8);
  let Ok(layout) = std::alloc::Layout::from_size_align(size, align) else {
    return ptr::null_mut();
  };
  let ptr = unsafe { std::alloc::alloc_zeroed(layout) as *mut c_void };
  if !ptr.is_null() {
    CPPGC_OBJECTS.with(|objects| {
      objects.borrow_mut().push(CppGcObject {
        ptr: ptr as usize,
        size,
        align,
        wrapper_key: None,
        weak_wrapper: jsv_undefined(),
      });
    });
  }
  ptr
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Persistent__CONSTRUCT(
  obj: *mut c_void,
) -> *mut c_void {
  Box::into_raw(Box::new(obj)) as *mut c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Persistent__DESTRUCT(this: *mut c_void) {
  if !this.is_null() {
    unsafe { drop(Box::from_raw(this as *mut *mut c_void)) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Persistent__Assign(
  this: *mut c_void,
  obj: *mut c_void,
) {
  if !this.is_null() {
    unsafe { *(this as *mut *mut c_void) = obj };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Persistent__Get(this: *const c_void) -> *mut c_void {
  if this.is_null() {
    return ptr::null_mut();
  }
  unsafe { *(this as *const *mut c_void) }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakPersistent__CONSTRUCT(
  obj: *mut c_void,
) -> *mut c_void {
  cppgc__Persistent__CONSTRUCT(obj)
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakPersistent__DESTRUCT(this: *mut c_void) {
  cppgc__Persistent__DESTRUCT(this)
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakPersistent__Assign(
  this: *mut c_void,
  obj: *mut c_void,
) {
  cppgc__Persistent__Assign(this, obj)
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakPersistent__Get(
  this: *const c_void,
) -> *mut c_void {
  cppgc__Persistent__Get(this)
}

#[cfg(test)]
mod cppgc_persistent_tests {
  use super::*;

  #[test]
  fn persistent_uses_an_independent_pointer_slot() {
    let mut first = 1u8;
    let mut second = 2u8;
    let first = (&mut first as *mut u8).cast::<c_void>();
    let second = (&mut second as *mut u8).cast::<c_void>();

    let persistent = cppgc__Persistent__CONSTRUCT(first);
    assert!(!persistent.is_null());
    assert_ne!(persistent, first);
    assert_eq!(cppgc__Persistent__Get(persistent), first);

    cppgc__Persistent__Assign(persistent, second);
    assert_eq!(cppgc__Persistent__Get(persistent), second);
    cppgc__Persistent__DESTRUCT(persistent);

    let weak = cppgc__WeakPersistent__CONSTRUCT(first);
    assert_eq!(cppgc__WeakPersistent__Get(weak), first);
    cppgc__WeakPersistent__Assign(weak, second);
    assert_eq!(cppgc__WeakPersistent__Get(weak), second);
    cppgc__WeakPersistent__DESTRUCT(weak);
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn collected_wrapper_does_not_match_stale_native_record() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let wrapper = JS_NewObject(ctx);
      let weak_wrapper = new_native_weak_ref(ctx, wrapper);
      assert!(!jsv_is_undefined(&weak_wrapper));
      let native = 8usize as *mut RustObj;
      CPPGC_OBJECTS.with(|objects| {
        objects.borrow_mut().push(CppGcObject {
          ptr: native as usize,
          size: 0,
          align: 8,
          wrapper_key: Some(wrapper.u.ptr as usize),
          weak_wrapper,
        });
      });

      assert!(cppgc_wrapper_matches(native, wrapper));
      JS_FreeValue(ctx, wrapper);
      JS_RunGC(rt);
      assert!(!cppgc_wrapper_matches(native, wrapper));

      CPPGC_OBJECTS.with(|objects| {
        objects
          .borrow_mut()
          .retain(|object| object.ptr != native as usize);
      });
      JS_FreeValue(ctx, weak_wrapper);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Visitor__Trace__TracedReference(
  _visitor: *mut c_void,
  ref_: *const c_void,
) {
  cppgc_mark_traced_reference(ref_);
}

fn stable_ctx() -> *mut JSContext {
  let ctx = current_ctx();
  if !ctx.is_null() {
    return ctx;
  }
  let iso = current_iso();
  if iso.is_null() {
    return ptr::null_mut();
  }
  let st = iso_state(iso);
  st.contexts.last().copied().unwrap_or(st.ctx)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__New(
  _isolate: *mut RealIsolate,
  data: *const Data,
) -> *const Data {
  if data.is_null() {
    return ptr::null();
  }

  if super::core::is_non_value_handle(data) {
    return data;
  }
  let ctx = stable_ctx();
  if ctx.is_null() {
    return ptr::null();
  }

  let v = jsval_of(data);
  let cell = Box::into_raw(Box::new(PersistentCell {
    value: unsafe { JS_DupValue(ctx, v) },
    context: ctx,
    isolate: _isolate,
  }));
  let slot = unsafe { &mut (*cell).value } as *mut JSValue;
  if !_isolate.is_null() {
    let st = iso_state(_isolate);
    st.persistent_handles.push(PersistentHandle {
      slot,
      is_weak: false,
    });
    st.global_handles.fetch_add(1, Ordering::SeqCst);
  }
  slot as *const Data
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__NewWeak(
  isolate: *mut RealIsolate,
  data: *const Data,
  parameter: *const c_void,
  callback: unsafe extern "C" fn(*const c_void),
) -> *const Data {
  let handle = v8__Global__New(isolate, data);
  let mut weak_ref = jsv_undefined();
  let mut context = ptr::null_mut();
  if !handle.is_null() && !isolate.is_null() {
    let st = iso_state(isolate);
    if let Some(persistent) =
      st.persistent_handles.iter_mut().find(|persistent| {
        std::ptr::addr_eq(persistent.slot, handle as *const JSValue)
      })
    {
      persistent.is_weak = true;
      let cell = handle as *const PersistentCell;
      context = unsafe { (*cell).context };
      weak_ref = unsafe { new_native_weak_ref(context, (*cell).value) };
    }
    st.weak_handles.push(crate::quickjs::core::WeakHandle {
      handle,
      parameter,
      callback,
      weak_ref,
      context,
    });
  }
  handle
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__Reset(data: *const Data) {
  if data.is_null() {
    return;
  }

  if super::core::is_non_value_handle(data) {
    return;
  }

  let cell = data as *mut PersistentCell;
  let iso = unsafe { (*cell).isolate };
  if !iso.is_null() {
    let st = iso_state(iso);
    if let Some(index) =
      st.weak_handles.iter().position(|weak| weak.handle == data)
    {
      let weak = st.weak_handles.remove(index);
      unsafe { JS_FreeValue(weak.context, weak.weak_ref) };
    }
    st.persistent_handles
      .retain(|handle| !std::ptr::addr_eq(handle.slot, data as *const JSValue));
    if st.global_handles.load(Ordering::SeqCst) > 0 {
      st.global_handles.fetch_sub(1, Ordering::SeqCst);
    }
  }

  unsafe {
    let ctx = (*cell).context;
    let v = (*cell).value;
    if !ctx.is_null() {
      JS_FreeValue(ctx, v);
    }
    drop(Box::from_raw(cell));
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TracedReference__CONSTRUCT(this: *mut usize) {
  if !this.is_null() {
    unsafe { this.write_unaligned(0) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TracedReference__DESTRUCT(this: *mut usize) {
  if this.is_null() {
    return;
  }
  unsafe {
    let cell = this.read_unaligned() as *mut JSValue;
    if !cell.is_null() {
      let v = *cell;
      let ctx = stable_ctx();
      if !ctx.is_null() {
        JS_FreeValue(ctx, v);
      }
      drop(Box::from_raw(cell));
      this.write_unaligned(0);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TracedReference__Reset(
  this: *mut usize,
  _isolate: *mut RealIsolate,
  data: *const Data,
) {
  if this.is_null() {
    return;
  }
  let ctx = stable_ctx();
  unsafe {
    let old = this.read_unaligned() as *mut JSValue;
    if !old.is_null() {
      if !ctx.is_null() {
        JS_FreeValue(ctx, *old);
      }
      drop(Box::from_raw(old));
      this.write_unaligned(0);
    }
    if data.is_null() || ctx.is_null() {
      return;
    }

    if super::core::is_non_value_handle(data) {
      return;
    }
    let dup = JS_DupValue(ctx, jsval_of(data));
    if std::env::var_os("QJS_DEBUG_TR").is_some() {
      eprintln!(
        "[QJS TracedRef::Reset] store tag={} ptr={:?}",
        dup.tag, dup.u.ptr
      );
    }
    let cell = Box::into_raw(Box::new(dup));
    this.write_unaligned(cell as usize);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TracedReference__Get(
  this: *const usize,
  _isolate: *mut RealIsolate,
) -> *const Data {
  if this.is_null() {
    return ptr::null();
  }
  let slot = unsafe { this.read_unaligned() };
  if slot == 0 {
    if std::env::var_os("QJS_DEBUG_TR").is_some() {
      eprintln!("[QJS TracedRef::Get] EMPTY");
    }
    return ptr::null();
  }
  if std::env::var_os("QJS_DEBUG_TR").is_some() {
    let v = unsafe { *(slot as *const JSValue) };
    eprintln!("[QJS TracedRef::Get] tag={} ptr={:?}", v.tag, unsafe {
      v.u.ptr
    });
  }

  slot as *const Data
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__JSON__Parse(
  context: *const Context,
  json_string: *const V8String,
) -> *const Value {
  if context.is_null() || json_string.is_null() {
    return ptr::null();
  }
  let ctx = ctx_of(context);
  unsafe {
    let mut len: usize = 0;
    let cstr = JS_ToCStringLen(ctx, &mut len, jsval_of(json_string));
    if cstr.is_null() {
      return ptr::null();
    }
    let fname = c"<json>";
    let parsed = JS_ParseJSON(ctx, cstr, len, fname.as_ptr());
    let input = std::slice::from_raw_parts(cstr as *const u8, len);
    let invalid_leading_token = input
      .iter()
      .copied()
      .find(|byte| !byte.is_ascii_whitespace())
      .filter(|byte| {
        !matches!(
          byte,
          b'{' | b'[' | b'"' | b't' | b'f' | b'n' | b'-' | b'0'..=b'9'
        )
      });
    let input_preview = invalid_leading_token.map(|token| {
      let preview_len = input.len().min(10);
      let preview = std::string::String::from_utf8_lossy(&input[..preview_len]);
      let suffix = if input.len() > preview_len { "..." } else { "" };
      format!(
        "Unexpected token '{}', {:?}{} is not valid JSON",
        char::from(token).escape_default(),
        preview,
        suffix,
      )
    });
    JS_FreeCString(ctx, cstr);
    if parsed.tag == JS_TAG_EXCEPTION {
      let exception = JS_GetException(ctx);
      if let Some(message) = input_preview
        && let Ok(message) = std::ffi::CString::new(message)
      {
        JS_SetPropertyStr(
          ctx,
          exception,
          c"message".as_ptr(),
          JS_NewString(ctx, message.as_ptr()),
        );
      }
      v82jsc_clear_error_backtrace(ctx, exception);
      JS_Throw(ctx, exception);
      return ptr::null();
    }

    intern::<Value>(parsed)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__GetHandler(this: *const c_void) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  let v = unsafe { JS_GetProxyHandler(ctx, jsval_of(this as *const Value)) };
  if v.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  intern::<Value>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__GetTarget(this: *const c_void) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  let v = unsafe { JS_GetProxyTarget(ctx, jsval_of(this as *const Value)) };
  if v.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  intern::<Value>(v)
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct RawStartupDataAbi {
  data: *const c_char,
  raw_size: std::os::raw::c_int,
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__CONSTRUCT(
  buf: *mut c_void,
  params: *const c_void,
) {
  // Snapshots are unsupported on quickjs (no heap serializer); the creator is
  // just a plain isolate. CreateBlob returns an empty blob and the embedder
  // falls back to New-init.
  let iso = crate::quickjs::core::v8__Isolate__New(params);
  if !iso.is_null() {
    crate::quickjs::core::iso_state(iso).is_snapshot_creator = true;
  }
  crate::quickjs::core::v8__Isolate__Enter(iso);
  if !buf.is_null() {
    unsafe { *(buf as *mut *mut RealIsolate) = iso };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__DESTRUCT(this: *mut c_void) {
  if this.is_null() {
    return;
  }
  let slot = this as *mut *mut RealIsolate;
  let iso = unsafe { *slot };
  if iso.is_null() {
    return;
  }
  if crate::quickjs::core::v8__Isolate__GetCurrent() == iso {
    crate::quickjs::core::v8__Isolate__Exit(iso);
  }
  crate::quickjs::core::v8__Isolate__Dispose(iso);
  unsafe { *slot = ptr::null_mut() };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__GetIsolate(
  this: *const c_void,
) -> *mut c_void {
  if !this.is_null() {
    let iso = unsafe { *(this as *const *mut RealIsolate) };
    if !iso.is_null() {
      return iso as *mut c_void;
    }
  }
  current_iso() as *mut c_void
}

fn creator_iso(this: *const c_void) -> *mut RealIsolate {
  if !this.is_null() {
    let iso = unsafe { *(this as *const *mut RealIsolate) };
    if !iso.is_null() {
      return iso;
    }
  }
  current_iso()
}

fn omit_unchanged_context_global(
  st: &crate::quickjs::core::IsoState,
  context: usize,
  capture: &mut super::snapshot::ContextSnapshot,
) {
  let Some(baseline) = st.snap_context_baselines.get(&context) else {
    return;
  };
  let Some(current) = capture.global_object.as_deref() else {
    return;
  };
  let Some(baseline) = baseline.global_object.as_deref() else {
    return;
  };
  if current == baseline {
    capture.global_object = None;
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__CreateBlob(
  this: *mut c_void,
  _function_code_handling: u32,
) -> RawStartupDataAbi {
  let iso = creator_iso(this);
  if iso.is_null() {
    return RawStartupDataAbi {
      data: ptr::null(),
      raw_size: 0,
    };
  }
  let st = crate::quickjs::core::iso_state(iso);
  let default_context =
    st.snap_default_context_capture.as_ref().map(|capture| {
      Arc::new(finalize_context_snapshot(
        st,
        st.snap_default_context,
        capture,
      ))
    });
  let contexts = st
    .snap_context_captures
    .iter()
    .enumerate()
    .map(|(index, capture)| {
      Arc::new(finalize_context_snapshot(
        st,
        st.snap_contexts.get(index).copied(),
        capture,
      ))
    })
    .collect();
  let blob = super::snapshot::SnapshotBlob {
    default_context,
    contexts,
    isolate_data: st
      .snap_isolate_data
      .iter()
      .cloned()
      .map(Into::into)
      .collect(),
  };
  let (data, raw_size) =
    super::snapshot::leak_blob(super::snapshot::encode_blob(&blob));
  RawStartupDataAbi {
    data: data as *const c_char,
    raw_size,
  }
}

fn finalize_context_snapshot(
  st: &crate::quickjs::core::IsoState,
  context: Option<usize>,
  capture: &super::snapshot::ContextSnapshot,
) -> super::snapshot::ContextSnapshot {
  let Some(context) = context else {
    return capture.clone();
  };
  let context_data = st
    .snap_context_data
    .get(&context)
    .cloned()
    .unwrap_or_default();
  let context_values = st
    .snap_context_data_values
    .get(&context)
    .cloned()
    .unwrap_or_default();
  if context_values.iter().any(Option::is_some) {
    super::snapshot::capture_context_with_data_roots(
      context as *mut JSContext,
      &st.external_references,
      &context_data,
      &context_values,
    )
  } else {
    // Re-capture at blob time: V8 serializes contexts in CreateBlob, so
    // mutations made after SetDefaultContext/AddContext (e.g. globals added
    // once the context is set) must be part of the snapshot. The capture taken
    // when the context was registered only serves as a fallback below.
    let mut fresh = super::snapshot::capture_context(
      context as *mut JSContext,
      &st.external_references,
      &[],
    );
    omit_unchanged_context_global(st, context, &mut fresh);
    fresh.context_data = context_data.into_iter().map(Into::into).collect();
    fresh.context_data_refs = vec![false; fresh.context_data.len()];
    fresh
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__SetDefaultContext(
  this: *mut c_void,
  context: *const Context,
) {
  let iso = creator_iso(this);
  if iso.is_null() {
    return;
  }
  let qctx = ctx_of(context);
  let external_references = crate::quickjs::core::iso_state(iso)
    .external_references
    .clone();
  let mut capture =
    super::snapshot::capture_context(qctx, &external_references, &[]);
  let st = crate::quickjs::core::iso_state(iso);
  omit_unchanged_context_global(st, qctx as usize, &mut capture);
  st.snap_default_context = Some(qctx as usize);
  st.snap_default_context_capture = Some(capture);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__AddContext(
  this: *mut c_void,
  context: *const Context,
) -> usize {
  let iso = creator_iso(this);
  if iso.is_null() {
    return 0;
  }
  let qctx = ctx_of(context);
  let external_references = crate::quickjs::core::iso_state(iso)
    .external_references
    .clone();
  let mut capture =
    super::snapshot::capture_context(qctx, &external_references, &[]);
  let st = crate::quickjs::core::iso_state(iso);
  omit_unchanged_context_global(st, qctx as usize, &mut capture);
  let n = st.snap_contexts.len();
  st.snap_contexts.push(qctx as usize);
  st.snap_context_captures.push(capture);
  st.iso_added_contexts += 1;
  n
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__AddData_to_isolate(
  this: *mut c_void,
  data: *const Data,
) -> usize {
  let iso = creator_iso(this);
  if iso.is_null() || data.is_null() {
    return 0;
  }
  let ctx = current_ctx();
  let external_refs = crate::quickjs::core::iso_state(iso)
    .external_references
    .clone();
  let bytes = if super::function::template_kind(data as *const c_void)
    == Some(super::function::TemplKind::Func)
  {
    super::snapshot::serialize_function_template(
      ctx,
      data as *const FunctionTemplate,
      &external_refs,
    )
  } else {
    super::snapshot::serialize_value_with_refs(
      ctx,
      jsval_of(data),
      &external_refs,
    )
  }
  .unwrap_or_default();
  let st = crate::quickjs::core::iso_state(iso);
  let n = st.snap_isolate_data.len();
  st.snap_isolate_data.push(bytes);
  st.iso_data_count += 1;
  n
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__AddData_to_context(
  this: *mut c_void,
  context: *const Context,
  data: *const Data,
) -> usize {
  let iso = creator_iso(this);
  if iso.is_null() || data.is_null() {
    return 0;
  }
  let ctx = ctx_of(context);
  let external_refs = crate::quickjs::core::iso_state(iso)
    .external_references
    .clone();
  let is_function_template =
    super::function::template_kind(data as *const c_void)
      == Some(super::function::TemplKind::Func);
  let function_template_proto = if is_function_template {
    super::function::snapshot_function_template_info(
      data as *const FunctionTemplate,
    )
    .and_then(|info| info.cached_proto)
  } else {
    None
  };
  let val = (!is_function_template).then(|| jsval_of(data));
  let bytes = if is_function_template {
    super::snapshot::serialize_function_template(
      ctx,
      data as *const FunctionTemplate,
      &external_refs,
    )
  } else {
    super::snapshot::serialize_value_with_refs(
      ctx,
      val.unwrap(),
      &external_refs,
    )
  };
  let graph_value = if let Some(proto) = function_template_proto {
    Some(unsafe { JS_DupValue(ctx, proto) })
  } else {
    val.and_then(|value| {
      super::snapshot::duplicate_graph_serializable_value(
        ctx,
        value,
        &external_refs,
      )
    })
  };
  if std::env::var_os("QJS_DEBUG_SNAPSHOT").is_some() {
    let Some(val) = val else {
      eprintln!(
        "[qjs snapshot] AddData_to_context FunctionTemplate ser={}",
        bytes.is_some(),
      );
      return store_context_snapshot_data(
        iso,
        ctx,
        bytes.unwrap_or_default(),
        graph_value,
      );
    };
    let preview = unsafe {
      let mut l = 0usize;
      let s = JS_ToCStringLen(ctx, &mut l, val);
      let out = if s.is_null() {
        "<unstringifiable>".to_string()
      } else {
        let b = std::slice::from_raw_parts(s as *const u8, l.min(80));
        let c = String::from_utf8_lossy(b).into_owned();
        JS_FreeCString(ctx, s);
        c
      };
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      out
    };
    eprintln!(
      "[qjs snapshot] AddData_to_context tag={} ser={} preview={preview}",
      val.tag,
      bytes.is_some(),
    );
  }
  let bytes = bytes.unwrap_or_default();
  store_context_snapshot_data(iso, ctx, bytes, graph_value)
}

fn store_context_snapshot_data(
  iso: *mut RealIsolate,
  ctx: *mut JSContext,
  bytes: Vec<u8>,
  graph_value: Option<JSValue>,
) -> usize {
  let st = crate::quickjs::core::iso_state(iso);
  let n = st.ctx_data_counts.entry(ctx as usize).or_insert(0);
  let idx = *n;
  *n += 1;
  st.snap_context_data
    .entry(ctx as usize)
    .or_default()
    .push(bytes);
  st.snap_context_data_values
    .entry(ctx as usize)
    .or_default()
    .push(graph_value);
  idx
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StartupData__CanBeRehashed(_this: *const c_void) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StartupData__IsValid(_this: *const c_void) -> bool {
  // No snapshot support on quickjs: never a valid startup blob.
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StartupData__data__DELETE(this: *const c_char) {
  super::snapshot::free_blob(this as *const u8);
}

// ICU common-data loader (vendored rusty_v8 `icu::set_common_data_77`). QuickJS
// brings its own ICU/Intl, so we never actually *load* V8's blob — but we do
// validate its header exactly like ICU's `udata_setCommonData`: a real ICU data
// file (icudtl.dat) begins with `headerSize:u16` followed by the format magic
// bytes 0xDA 0x27. The test harness ships a blob with a valid header, so setup's
// `set_common_data_77(<icudtl.dat>)` returns Ok; a garbage blob — e.g. the
// `[1, 2, 3, 0, …]` from `icu_set_common_data_fail` — has the wrong magic and
// must return `U_INVALID_FORMAT_ERROR`. No length crosses this C ABI, so we read
// only the 4 header bytes every real caller is guaranteed to provide.
#[unsafe(no_mangle)]
pub extern "C" fn udata_setCommonData_78(
  data: *const u8,
  error_code: *mut i32,
) {
  // ICU's UErrorCode for a bad/unrecognized data header.
  const U_INVALID_FORMAT_ERROR: i32 = 3;
  let valid = !data.is_null()
    && unsafe {
      // ICU DataHeader: bytes [2] and [3] are the magic 0xDA 0x27.
      *data.add(2) == 0xDA && *data.add(3) == 0x27
    };
  if !error_code.is_null() {
    unsafe {
      *error_code = if valid { 0 } else { U_INVALID_FORMAT_ERROR };
    }
  }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn v8__Task__Run(task: *mut c_void) {
  unsafe { super::init::run_foreground_task(task) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn v8__Task__DELETE(task: *mut c_void) {
  unsafe { super::init::delete_foreground_task(task) };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__IdleTask__Run(
  _task: *mut c_void,
  _deadline_in_seconds: f64,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__IdleTask__DELETE(_task: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WeakCallbackInfo__GetIsolate(
  this: *const c_void,
) -> *mut c_void {
  if this.is_null() {
    return current_iso() as *mut c_void;
  }
  unsafe { (*(this as *const WeakCallbackInfoShim)).isolate as *mut c_void }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WeakCallbackInfo__GetParameter(
  this: *const c_void,
) -> *mut c_void {
  if this.is_null() {
    return ptr::null_mut();
  }
  unsafe { (*(this as *const WeakCallbackInfoShim)).parameter as *mut c_void }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WeakCallbackInfo__SetSecondPassCallback(
  this: *const c_void,
  callback: unsafe extern "C" fn(*const c_void),
) {
  if this.is_null() {
    return;
  }
  unsafe {
    (*(this as *mut WeakCallbackInfoShim)).second_pass = Some(callback);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__Unpack(
  _isolate: *mut c_void,
  value: *const Value,
  that: *mut c_void,
) {
  super::wasm::streaming_unpack(value, that);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__shared_ptr_DESTRUCT(this: *mut c_void) {
  super::wasm::streaming_shared_ptr_destruct(this);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__OnBytesReceived(
  this: *mut c_void,
  data: *const u8,
  len: usize,
) {
  super::wasm::streaming_on_bytes_received(this, data, len);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__Finish(
  this: *mut c_void,
  callback: Option<unsafe extern "C" fn(*mut c_void)>,
) {
  super::wasm::streaming_finish(this, callback);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__Abort(
  this: *mut c_void,
  exception: *const Value,
) {
  super::wasm::streaming_abort(this, exception);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__SetUrl(
  this: *mut c_void,
  url: *const c_char,
  len: usize,
) {
  super::wasm::streaming_set_url(this, url, len);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleObject__FromCompiledModule(
  isolate: *mut c_void,
  compiled_module: *const c_void,
) -> *const c_void {
  super::wasm::module_object_from_compiled_module(isolate, compiled_module)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleObject__GetCompiledModule(
  this: *const c_void,
) -> *mut c_void {
  super::wasm::module_object_get_compiled_module(this)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleObject__Compile(
  isolate: *mut RealIsolate,
  wire_bytes_data: *const u8,
  length: usize,
) -> *const Object {
  if isolate.is_null() || wire_bytes_data.is_null() {
    return ptr::null();
  }
  let st = iso_state(isolate);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  if ctx.is_null() {
    return ptr::null();
  }
  let bytes = unsafe { std::slice::from_raw_parts(wire_bytes_data, length) };
  let v = unsafe { super::wasm::compile_module_object(ctx, bytes) };
  if v.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  intern::<Object>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CompiledWasmModule__DELETE(this: *mut c_void) {
  super::wasm::compiled_module_delete(this);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__NEW() -> *mut c_void {
  super::wasm::module_compilation_new()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__DELETE(this: *mut c_void) {
  super::wasm::module_compilation_delete(this);
}

unsafe extern "C" {
  fn JS_JSONStringify(
    ctx: *mut JSContext,
    obj: JSValue,
    replacer: JSValue,
    space0: JSValue,
  ) -> JSValue;
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__JSON__Stringify(
  context: *const Context,
  json_object: *const Value,
) -> *const V8String {
  let ctx = ctx_of(context);
  if ctx.is_null() || json_object.is_null() {
    return ptr::null();
  }
  unsafe {
    let s = JS_JSONStringify(
      ctx,
      jsval_of(json_object),
      jsv_undefined(),
      jsv_undefined(),
    );
    if s.tag == JS_TAG_EXCEPTION {
      // Leave the exception PENDING (v8's JSON::Stringify returns Empty and
      // keeps it set) so the caller's TryCatch can read it — e.g. console's
      // `%j` distinguishes circular-reference TypeErrors from real ones.
      // Clearing it here made TryCatch::Exception() return undefined.
      return ptr::null();
    }
    intern::<V8String>(s)
  }
}

unsafe fn call_unary_closure(
  ctx: *mut JSContext,
  src: &[u8],
  obj: JSValue,
) -> JSValue {
  unsafe {
    let f = JS_Eval(
      ctx,
      src.as_ptr() as *const c_char,
      src.len() - 1,
      c"<map-set>".as_ptr(),
      JS_EVAL_TYPE_GLOBAL,
    );
    if f.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return jsv_exception();
    }
    let mut args = [JS_DupValue(ctx, obj)];
    let r = JS_Call(ctx, f, jsv_undefined(), 1, args.as_mut_ptr());
    JS_FreeValue(ctx, f);
    JS_FreeValue(ctx, args[0]);
    r
  }
}

fn map_set_size(v: *const Object) -> usize {
  let ctx = current_ctx();
  if ctx.is_null() || v.is_null() {
    return 0;
  }
  unsafe {
    let sz = JS_GetPropertyStr(ctx, jsval_of(v), c"size".as_ptr());
    if sz.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return 0;
    }
    let mut n: i64 = 0;
    let rc = JS_ToInt64(ctx, &mut n, sz);
    JS_FreeValue(ctx, sz);
    if rc < 0 {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return 0;
    }
    n.max(0) as usize
  }
}

fn mb(b: bool) -> crate::support::MaybeBool {
  if b {
    crate::support::MaybeBool::JustTrue
  } else {
    crate::support::MaybeBool::JustFalse
  }
}

// Invoke `this.<method>(args...)` and return the raw result JSValue (the caller
// must `JS_FreeValue` it) or `jsv_exception()` when the method is missing or
// threw (the pending exception is cleared). Used to back the Map/Set C-ABI on
// top of QuickJS's native Map/Set prototype methods.
unsafe fn call_collection_method(
  ctx: *mut JSContext,
  this: JSValue,
  method: *const c_char,
  args: &mut [JSValue],
) -> JSValue {
  unsafe {
    let f = JS_GetPropertyStr(ctx, this, method);
    if !JS_IsFunction(ctx, f) {
      JS_FreeValue(ctx, f);
      return jsv_exception();
    }
    let r = JS_Call(
      ctx,
      f,
      this,
      args.len() as std::os::raw::c_int,
      args.as_mut_ptr(),
    );
    JS_FreeValue(ctx, f);
    if r.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
    }
    r
  }
}

// `new <ctor_name>()` against the isolate's active context (Map / Set).
fn new_builtin(isolate: *mut RealIsolate, ctor_name: *const c_char) -> JSValue {
  if isolate.is_null() {
    return jsv_exception();
  }
  let st = iso_state(isolate);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  if ctx.is_null() {
    return jsv_exception();
  }
  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let ctor = JS_GetPropertyStr(ctx, global, ctor_name);
    JS_FreeValue(ctx, global);
    if !JS_IsConstructor(ctx, ctor) {
      JS_FreeValue(ctx, ctor);
      return jsv_exception();
    }
    let v = JS_CallConstructor(ctx, ctor, 0, ptr::null_mut());
    JS_FreeValue(ctx, ctor);
    if v.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return jsv_exception();
    }
    v
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Size(map: *const crate::Map) -> usize {
  map_set_size(map as *const Object)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Size(set: *const crate::Set) -> usize {
  map_set_size(set as *const Object)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__As__Array(
  this: *const crate::Map,
) -> *const crate::Array {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }

  const SRC: &[u8] =
        b"(function(m){var r=[];m.forEach(function(v,k){r.push(k);r.push(v);});return r;})\0";
  let arr = unsafe { call_unary_closure(ctx, SRC, jsval_of(this)) };
  if arr.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  intern::<crate::Array>(arr)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__As__Array(
  this: *const crate::Set,
) -> *const crate::Array {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  const SRC: &[u8] =
    b"(function(s){var r=[];s.forEach(function(v){r.push(v);});return r;})\0";
  let arr = unsafe { call_unary_closure(ctx, SRC, jsval_of(this)) };
  if arr.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  intern::<crate::Array>(arr)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__New(isolate: *mut RealIsolate) -> *const crate::Set {
  if isolate.is_null() {
    return ptr::null();
  }
  let st = iso_state(isolate);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  if ctx.is_null() {
    return ptr::null();
  }
  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let ctor = JS_GetPropertyStr(ctx, global, c"Set".as_ptr());
    JS_FreeValue(ctx, global);
    if !JS_IsConstructor(ctx, ctor) {
      JS_FreeValue(ctx, ctor);
      return ptr::null();
    }
    let v = JS_CallConstructor(ctx, ctor, 0, ptr::null_mut());
    JS_FreeValue(ctx, ctor);
    if v.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return ptr::null();
    }
    intern::<crate::Set>(v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Add(
  this: *const crate::Set,
  context: *const Context,
  key: *const Value,
) -> *const crate::Set {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  unsafe {
    let add = JS_GetPropertyStr(ctx, jsval_of(this), c"add".as_ptr());
    if !JS_IsFunction(ctx, add) {
      JS_FreeValue(ctx, add);
      return ptr::null();
    }
    let mut args = [JS_DupValue(ctx, jsval_of(key))];
    let r = JS_Call(ctx, add, jsval_of(this), 1, args.as_mut_ptr());
    JS_FreeValue(ctx, add);
    JS_FreeValue(ctx, args[0]);
    if r.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return ptr::null();
    }
    JS_FreeValue(ctx, r);

    intern::<crate::Set>(JS_DupValue(ctx, jsval_of(this)))
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Date__New(
  context: *const Context,
  value: f64,
) -> *const crate::Date {
  let ctx = ctx_of(context);
  if ctx.is_null() {
    return ptr::null();
  }
  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let ctor = JS_GetPropertyStr(ctx, global, c"Date".as_ptr());
    JS_FreeValue(ctx, global);
    if !JS_IsConstructor(ctx, ctor) {
      JS_FreeValue(ctx, ctor);
      return ptr::null();
    }
    let mut args = [JS_NewFloat64(ctx, value)];
    let v = JS_CallConstructor(ctx, ctor, 1, args.as_mut_ptr());
    JS_FreeValue(ctx, ctor);
    if v.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return ptr::null();
    }
    intern::<crate::Date>(v)
  }
}

/// Calls the zero-argument method `name` on `this` and returns its result
/// (+1), or `None` with the exception cleared.
unsafe fn call_date_method(
  ctx: *mut JSContext,
  this: JSValue,
  name: &std::ffi::CStr,
) -> Option<JSValue> {
  unsafe {
    let method = JS_GetPropertyStr(ctx, this, name.as_ptr());
    if !JS_IsFunction(ctx, method) {
      JS_FreeValue(ctx, method);
      super::exception::clear_pending(ctx);
      return None;
    }
    let r = JS_Call(ctx, method, this, 0, ptr::null_mut());
    JS_FreeValue(ctx, method);
    if r.tag == JS_TAG_EXCEPTION {
      super::exception::clear_pending(ctx);
      return None;
    }
    Some(r)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Date__Parse(
  context: *const Context,
  date_string: *const crate::String,
) -> *const crate::Date {
  let ctx = ctx_of(context);
  if ctx.is_null() || date_string.is_null() {
    return ptr::null();
  }
  let ms = unsafe {
    let global = JS_GetGlobalObject(ctx);
    let ctor = JS_GetPropertyStr(ctx, global, c"Date".as_ptr());
    JS_FreeValue(ctx, global);
    let parse = JS_GetPropertyStr(ctx, ctor, c"parse".as_ptr());
    JS_FreeValue(ctx, ctor);
    if !JS_IsFunction(ctx, parse) {
      JS_FreeValue(ctx, parse);
      super::exception::clear_pending(ctx);
      return ptr::null();
    }
    let mut args = [JS_DupValue(ctx, jsval_of(date_string))];
    let time = JS_Call(ctx, parse, jsv_undefined(), 1, args.as_mut_ptr());
    JS_FreeValue(ctx, parse);
    JS_FreeValue(ctx, args[0]);
    if time.tag == JS_TAG_EXCEPTION {
      // A throwing `toString` on the argument is the only way to get here;
      // leave it pending so the empty handle carries an exception, as in V8.
      return ptr::null();
    }
    let mut ms = f64::NAN;
    JS_ToFloat64(ctx, &mut ms, time);
    JS_FreeValue(ctx, time);
    ms
  };
  v8__Date__New(context, ms)
}

/// Like V8, a date whose time value is NaN stringifies as "Invalid Date"
/// instead of throwing the RangeError that `toISOString()` throws in JS.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Date__ToISOString(
  this: *const crate::Date,
) -> *const crate::String {
  date_to_string(this, c"toISOString")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Date__ToUTCString(
  this: *const crate::Date,
) -> *const crate::String {
  date_to_string(this, c"toUTCString")
}

fn date_to_string(
  this: *const crate::Date,
  method: &std::ffi::CStr,
) -> *const crate::String {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  if v8__Date__ValueOf(this).is_nan() {
    const INVALID: &str = "Invalid Date";
    let s = unsafe {
      JS_NewStringLen(ctx, INVALID.as_ptr() as *const c_char, INVALID.len())
    };
    return intern::<crate::String>(s);
  }
  match unsafe { call_date_method(ctx, jsval_of(this), method) } {
    Some(s) => intern::<crate::String>(s),
    None => ptr::null(),
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Date__ValueOf(this: *const crate::Date) -> f64 {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0.0;
  }
  unsafe {
    let vo = JS_GetPropertyStr(ctx, jsval_of(this), c"valueOf".as_ptr());
    if !JS_IsFunction(ctx, vo) {
      JS_FreeValue(ctx, vo);
      return 0.0;
    }
    let r = JS_Call(ctx, vo, jsval_of(this), 0, ptr::null_mut());
    JS_FreeValue(ctx, vo);
    if r.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return 0.0;
    }
    let mut n: f64 = 0.0;
    JS_ToFloat64(ctx, &mut n, r);
    JS_FreeValue(ctx, r);
    n
  }
}

thread_local! {
    static EMBEDDER_DATA: std::cell::RefCell<HashMap<usize, Vec<JSValue>>> =
        std::cell::RefCell::new(HashMap::new());
    static SECURITY_TOKEN: std::cell::RefCell<HashMap<usize, JSValue>> =
        std::cell::RefCell::new(HashMap::new());
}

unsafe extern "C" {
  fn JS_IsStrictEqual(ctx: *mut JSContext, op1: JSValue, op2: JSValue) -> bool;
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetEmbedderData(
  this: *const Context,
  index: std::os::raw::c_int,
  value: *const Value,
) {
  let ctx = ctx_of(this);
  if ctx.is_null() || index < 0 {
    return;
  }
  let owned = unsafe { JS_DupValue(ctx, jsval_of(value)) };
  EMBEDDER_DATA.with(|m| {
    let mut map = m.borrow_mut();
    let slots = map.entry(ctx as usize).or_default();
    let idx = index as usize;
    while slots.len() <= idx {
      slots.push(jsv_undefined());
    }
    let old = slots[idx];
    if old.tag < 0 {
      unsafe { JS_FreeValue(ctx, old) };
    }
    slots[idx] = owned;
  });
}

/// Snapshot support: borrow every embedder-data value slot of `ctx`.
/// `None` marks unset (undefined) slots.
pub(crate) fn embedder_data_snapshot(
  ctx: *mut JSContext,
) -> Vec<Option<JSValue>> {
  EMBEDDER_DATA.with(|m| {
    m.borrow()
      .get(&(ctx as usize))
      .map(|slots| {
        slots
          .iter()
          .map(|v| if jsv_is_undefined(v) { None } else { Some(*v) })
          .collect()
      })
      .unwrap_or_default()
  })
}

/// Snapshot support: install a replayed value into an embedder-data slot.
/// Dups `value` (caller keeps its refcount).
pub(crate) fn set_embedder_data_raw(
  ctx: *mut JSContext,
  index: usize,
  value: JSValue,
) {
  let owned = unsafe { JS_DupValue(ctx, value) };
  EMBEDDER_DATA.with(|m| {
    let mut map = m.borrow_mut();
    let slots = map.entry(ctx as usize).or_default();
    while slots.len() <= index {
      slots.push(jsv_undefined());
    }
    let old = slots[index];
    if old.tag < 0 {
      unsafe { JS_FreeValue(ctx, old) };
    }
    slots[index] = owned;
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetEmbedderData(
  this: *const Context,
  index: std::os::raw::c_int,
) -> *const Value {
  let ctx = ctx_of(this);
  if ctx.is_null() || index < 0 {
    return ptr::null();
  }
  let found = EMBEDDER_DATA.with(|m| {
    m.borrow()
      .get(&(ctx as usize))
      .and_then(|slots| slots.get(index as usize).copied())
  });
  let h = match found {
    Some(v) => intern_dup::<Value>(ctx, v),
    None => intern::<Value>(jsv_undefined()),
  };
  h
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetSecurityToken(
  this: *const Context,
  value: *const Value,
) {
  let ctx = ctx_of(this);
  if ctx.is_null() {
    return;
  }
  let owned = unsafe { JS_DupValue(ctx, jsval_of(value)) };
  SECURITY_TOKEN.with(|m| {
    if let Some(old) = m.borrow_mut().insert(ctx as usize, owned) {
      if old.tag < 0 {
        unsafe { JS_FreeValue(ctx, old) };
      }
    }
  });
}

fn context_security_token(ctx: *mut JSContext) -> JSValue {
  if ctx.is_null() {
    return jsv_undefined();
  }
  let found = SECURITY_TOKEN.with(|m| m.borrow().get(&(ctx as usize)).copied());
  match found {
    Some(v) => unsafe { JS_DupValue(ctx, v) },
    None => unsafe { JS_GetGlobalObject(ctx) },
  }
}

pub(crate) fn contexts_share_security_token(
  accessing_ctx: *mut JSContext,
  target_ctx: *mut JSContext,
) -> bool {
  if accessing_ctx.is_null() || target_ctx.is_null() {
    return false;
  }
  if accessing_ctx == target_ctx {
    return true;
  }

  let accessing = context_security_token(accessing_ctx);
  let target = context_security_token(target_ctx);
  let matches = unsafe { JS_IsStrictEqual(accessing_ctx, accessing, target) };
  unsafe {
    JS_FreeValue(accessing_ctx, accessing);
    JS_FreeValue(accessing_ctx, target);
  }
  matches
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetSecurityToken(
  this: *const Context,
) -> *const Value {
  let ctx = ctx_of(this);
  if ctx.is_null() {
    return ptr::null();
  }
  intern::<Value>(context_security_token(ctx))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__UseDefaultSecurityToken(this: *const Context) {
  let ctx = ctx_of(this);
  if ctx.is_null() {
    return;
  }
  SECURITY_TOKEN.with(|m| {
    if let Some(old) = m.borrow_mut().remove(&(ctx as usize)) {
      if old.tag < 0 {
        unsafe { JS_FreeValue(ctx, old) };
      }
    }
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__CONSTRUCT(this: *mut *const Data) {
  if !this.is_null() {
    unsafe { this.write_unaligned(ptr::null()) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__DESTRUCT(this: *mut *const Data) {
  v8__Eternal__Clear(this as *mut c_void);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__Get(
  this: *const *const Data,
  _isolate: *mut RealIsolate,
) -> *const Data {
  if this.is_null() {
    return ptr::null();
  }
  let stored = unsafe { this.read_unaligned() };
  if stored.is_null() {
    return ptr::null();
  }

  let ctx = current_ctx();
  if ctx.is_null() {
    return stored;
  }
  intern_dup::<Data>(ctx, jsval_of(stored))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__Set(
  this: *mut *const Data,
  _isolate: *mut RealIsolate,
  data: *mut Data,
) {
  if this.is_null() {
    return;
  }
  let ctx = current_ctx();
  if ctx.is_null() || data.is_null() {
    unsafe { this.write_unaligned(ptr::null()) };
    return;
  }

  let owned = unsafe { JS_DupValue(ctx, jsval_of(data)) };
  let boxed = Box::into_raw(Box::new(owned));
  unsafe { this.write_unaligned(boxed as *const Data) };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddGCPrologueCallback(
  isolate: *mut RealIsolate,
  callback: crate::isolate::GcCallbackWithData,
  data: *mut c_void,
  gc_type_filter: crate::gc::GCType,
) {
  if !isolate.is_null() {
    iso_state(isolate).gc_prologue_callbacks.push(
      crate::quickjs::core::GcCallbackEntry {
        callback,
        data,
        gc_type_filter,
      },
    );
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddGCEpilogueCallback(
  isolate: *mut RealIsolate,
  callback: crate::isolate::GcCallbackWithData,
  data: *mut c_void,
  gc_type_filter: crate::gc::GCType,
) {
  if !isolate.is_null() {
    iso_state(isolate).gc_epilogue_callbacks.push(
      crate::quickjs::core::GcCallbackEntry {
        callback,
        data,
        gc_type_filter,
      },
    );
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddNearHeapLimitCallback(
  isolate: *mut RealIsolate,
  callback: crate::isolate::NearHeapLimitCallback,
  data: *mut c_void,
) {
  crate::quickjs::isolate::set_near_heap_limit_callback(
    isolate, callback, data,
  );
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AdjustAmountOfExternalAllocatedMemory(
  isolate: *mut RealIsolate,
  change_in_bytes: i64,
) -> i64 {
  let isolate = if isolate.is_null() {
    current_iso()
  } else {
    isolate
  };
  if isolate.is_null() {
    return change_in_bytes;
  }
  adjust_external_memory(iso_state(isolate), change_in_bytes)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__LowMemoryNotification(
  isolate: *mut RealIsolate,
) {
  if isolate.is_null() {
    return;
  }
  let rt = iso_state(isolate).rt;
  if !rt.is_null() {
    unsafe { JS_RunGC(rt) };
  }
  super::arraybuffer::release_pending_allocator_buffers(isolate);
  let st = iso_state(isolate);
  release_external_string_memory(st);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__NumberOfHeapSpaces(
  _isolate: *mut RealIsolate,
) -> usize {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetHeapSpaceStatistics(
  _isolate: *mut RealIsolate,
  space_statistics: *mut crate::binding::v8__HeapSpaceStatistics,
  _index: usize,
) -> bool {
  if !space_statistics.is_null() {
    unsafe {
      ptr::write_bytes(
        space_statistics as *mut u8,
        0,
        std::mem::size_of::<crate::binding::v8__HeapSpaceStatistics>(),
      );
    }
  }
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetHeapCodeAndMetadataStatistics(
  isolate: *mut RealIsolate,
  code_statistics: *mut crate::binding::v8__HeapCodeStatistics,
) -> bool {
  if isolate.is_null() {
    return false;
  }
  if !code_statistics.is_null() {
    let bytecode_size = iso_state(isolate)
      .bytecode_and_metadata_size
      .load(Ordering::SeqCst);
    unsafe {
      ptr::write_bytes(
        code_statistics as *mut u8,
        0,
        std::mem::size_of::<crate::binding::v8__HeapCodeStatistics>(),
      );
      (*code_statistics).bytecode_and_metadata_size_ = bytecode_size;
    }
  }
  true
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__DateTimeConfigurationChangeNotification(
  _isolate: *mut RealIsolate,
  _time_zone_detection: crate::isolate::TimeZoneDetection,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetAllowWasmCodeGenerationCallback(
  _isolate: *mut RealIsolate,
  _callback: crate::isolate::AllowWasmCodeGenerationCallback,
) {
}

unsafe fn qjs_value_to_string(
  ctx: *mut JSContext,
  v: JSValue,
) -> Option<String> {
  let mut len = 0usize;
  let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, v) };
  if cstr.is_null() {
    return None;
  }
  let text = unsafe {
    let bytes = std::slice::from_raw_parts(cstr as *const u8, len);
    String::from_utf8_lossy(bytes).into_owned()
  };
  unsafe { JS_FreeCString(ctx, cstr) };
  Some(text)
}

unsafe fn qjs_atom_to_string(
  ctx: *mut JSContext,
  atom: JSAtom,
) -> Option<String> {
  let value = unsafe { JS_AtomToString(ctx, atom) };
  if value.tag == JS_TAG_EXCEPTION {
    return None;
  }
  let text = unsafe { qjs_value_to_string(ctx, value) };
  unsafe { JS_FreeValue(ctx, value) };
  text
}

fn push_snapshot_name(
  names: &mut Vec<String>,
  seen_names: &mut HashSet<String>,
  name: String,
) {
  if name.is_empty() || name == "undefined" || name == "null" {
    return;
  }
  if seen_names.insert(name.clone()) {
    names.push(name);
  }
}

unsafe fn snapshot_data_property(
  ctx: *mut JSContext,
  value: JSValue,
  atom: JSAtom,
) -> Option<JSValue> {
  let mut current = unsafe { JS_DupValue(ctx, value) };
  loop {
    if !jsv_is_object(&current) || unsafe { JS_IsProxy(current) } {
      unsafe { JS_FreeValue(ctx, current) };
      return None;
    }
    let mut desc = JSPropertyDescriptorQjs {
      flags: 0,
      value: jsv_undefined(),
      getter: jsv_undefined(),
      setter: jsv_undefined(),
    };
    let result = unsafe { JS_GetOwnProperty(ctx, &mut desc, current, atom) };
    if result < 0 {
      unsafe {
        JS_FreeValue(ctx, current);
        crate::quickjs::exception::clear_pending(ctx);
      }
      return None;
    }
    if result > 0 {
      unsafe {
        JS_FreeValue(ctx, current);
        JS_FreeValue(ctx, desc.getter);
        JS_FreeValue(ctx, desc.setter);
      }
      if desc.flags & JS_PROP_GETSET_QJS != 0 {
        unsafe { JS_FreeValue(ctx, desc.value) };
        return None;
      }
      return Some(desc.value);
    }
    unsafe {
      JS_FreeValue(ctx, desc.value);
      JS_FreeValue(ctx, desc.getter);
      JS_FreeValue(ctx, desc.setter);
    }
    let prototype = unsafe { JS_GetPrototype(ctx, current) };
    unsafe { JS_FreeValue(ctx, current) };
    if jsv_is_exception(&prototype) {
      unsafe { crate::quickjs::exception::clear_pending(ctx) };
      return None;
    }
    if jsv_is_null(&prototype) {
      unsafe { JS_FreeValue(ctx, prototype) };
      return None;
    }
    current = prototype;
  }
}

unsafe fn snapshot_constructor_name(
  ctx: *mut JSContext,
  value: JSValue,
  constructor_atom: JSAtom,
  name_atom: JSAtom,
) -> Option<String> {
  let constructor =
    unsafe { snapshot_data_property(ctx, value, constructor_atom) }?;
  let name = unsafe { snapshot_data_property(ctx, constructor, name_atom) };
  unsafe { JS_FreeValue(ctx, constructor) };
  let name = name?;
  let text = unsafe { qjs_value_to_string(ctx, name) };
  unsafe { JS_FreeValue(ctx, name) };
  if text.is_none() {
    unsafe { crate::quickjs::exception::clear_pending(ctx) };
  }
  text.filter(|name| !name.is_empty())
}

unsafe fn quickjs_heap_object_names(ctx: *mut JSContext) -> Vec<String> {
  let constructor_atom = unsafe { JS_NewAtom(ctx, c"constructor".as_ptr()) };
  let name_atom = unsafe { JS_NewAtom(ctx, c"name".as_ptr()) };
  let mut count = 0usize;
  let objects = unsafe { v82jsc_heap_objects(ctx, &mut count) };
  let mut names = Vec::with_capacity(count);
  if !objects.is_null() {
    for value in unsafe { std::slice::from_raw_parts(objects, count) } {
      if let Some(name) = unsafe {
        snapshot_constructor_name(ctx, *value, constructor_atom, name_atom)
      } {
        names.push(name);
      }
    }
    unsafe { v82jsc_free_heap_objects(ctx, objects, count) };
  }
  unsafe {
    JS_FreeAtom(ctx, constructor_atom);
    JS_FreeAtom(ctx, name_atom);
  }
  names
}

unsafe fn collect_constructor_name(
  ctx: *mut JSContext,
  value: JSValue,
  names: &mut Vec<String>,
  seen_names: &mut HashSet<String>,
) {
  if !jsv_is_object(&value) {
    return;
  }
  let ctor = unsafe { JS_GetPropertyStr(ctx, value, c"constructor".as_ptr()) };
  if ctor.tag == JS_TAG_EXCEPTION {
    return;
  }
  let name = unsafe { JS_GetPropertyStr(ctx, ctor, c"name".as_ptr()) };
  if name.tag != JS_TAG_EXCEPTION {
    if let Some(text) = unsafe { qjs_value_to_string(ctx, name) } {
      push_snapshot_name(names, seen_names, text);
    }
  }
  unsafe {
    JS_FreeValue(ctx, name);
    JS_FreeValue(ctx, ctor);
  }
}

fn collect_cppgc_snapshot_names(
  names: &mut Vec<String>,
  seen_names: &mut HashSet<String>,
) {
  let objects = CPPGC_OBJECTS.with(|objects| {
    objects
      .borrow()
      .iter()
      .map(|object| object.ptr)
      .collect::<Vec<_>>()
  });
  for object in objects {
    let name = unsafe { rusty_v8_RustObj_get_name(object as *const RustObj) };
    if name.is_null() {
      continue;
    }
    if let Ok(name) = unsafe { CStr::from_ptr(name) }.to_str() {
      push_snapshot_name(names, seen_names, name.to_owned());
    }
  }
}

unsafe fn collect_heap_snapshot_names(
  ctx: *mut JSContext,
  value: JSValue,
  depth: usize,
  seen_objects: &mut HashSet<usize>,
  seen_names: &mut HashSet<String>,
  names: &mut Vec<String>,
) {
  if depth > HEAP_SNAPSHOT_MAX_DEPTH || !jsv_is_object(&value) {
    return;
  }

  let ptr = unsafe { value.u.ptr } as usize;
  if ptr == 0 || !seen_objects.insert(ptr) {
    return;
  }

  unsafe { collect_constructor_name(ctx, value, names, seen_names) };

  if unsafe { JS_IsArray(value) } {
    let len_value =
      unsafe { JS_GetPropertyStr(ctx, value, c"length".as_ptr()) };
    let mut len = 0i32;
    unsafe {
      JS_ToInt32(ctx, &mut len, len_value);
      JS_FreeValue(ctx, len_value);
    }
    for index in 0..(len.max(0) as u32).min(HEAP_SNAPSHOT_ARRAY_SAMPLE) {
      let element = unsafe { JS_GetPropertyUint32(ctx, value, index) };
      unsafe {
        collect_heap_snapshot_names(
          ctx,
          element,
          depth + 1,
          seen_objects,
          seen_names,
          names,
        );
        JS_FreeValue(ctx, element);
      }
    }
  }

  let mut ptab: *mut JSPropertyEnum = ptr::null_mut();
  let mut plen = 0u32;
  let flags = JS_GPN_STRING_MASK | JS_GPN_SYMBOL_MASK | JS_GPN_ENUM_ONLY;
  let rc =
    unsafe { JS_GetOwnPropertyNames(ctx, &mut ptab, &mut plen, value, flags) };
  if rc != 0 || ptab.is_null() {
    return;
  }

  for index in 0..plen as usize {
    let atom = unsafe { (*ptab.add(index)).atom };
    if let Some(name) = unsafe { qjs_atom_to_string(ctx, atom) } {
      push_snapshot_name(names, seen_names, name);
    }
    let property = unsafe { JS_GetProperty(ctx, value, atom) };
    unsafe {
      collect_heap_snapshot_names(
        ctx,
        property,
        depth + 1,
        seen_objects,
        seen_names,
        names,
      );
      JS_FreeValue(ctx, property);
      JS_FreeAtom(ctx, atom);
    }
  }
  unsafe { js_free(ctx, ptab as *mut c_void) };
}

fn push_json_string(out: &mut String, value: &str) {
  out.push('"');
  for ch in value.chars() {
    match ch {
      '"' => out.push_str("\\\""),
      '\\' => out.push_str("\\\\"),
      '\n' => out.push_str("\\n"),
      '\r' => out.push_str("\\r"),
      '\t' => out.push_str("\\t"),
      ch if ch.is_control() => {
        use std::fmt::Write;
        let _ = write!(out, "\\u{:04x}", ch as u32);
      }
      ch => out.push(ch),
    }
  }
  out.push('"');
}

fn quickjs_heap_snapshot_json(isolate: *mut RealIsolate) -> Vec<u8> {
  if isolate.is_null() {
    return br#"{"snapshot":{"meta":{}},"nodes":[],"strings":[]}"#.to_vec();
  }
  let ctx = {
    let current = current_ctx();
    if !current.is_null() {
      current
    } else {
      let st = iso_state(isolate);
      st.contexts.last().copied().unwrap_or(st.ctx)
    }
  };
  if ctx.is_null() {
    return br#"{"snapshot":{"meta":{}},"nodes":[],"strings":[]}"#.to_vec();
  }

  let mut object_names = unsafe { quickjs_heap_object_names(ctx) };
  let mut cppgc_names = Vec::new();
  let mut seen_cppgc_names = HashSet::new();
  collect_cppgc_snapshot_names(&mut cppgc_names, &mut seen_cppgc_names);
  object_names.extend(cppgc_names);

  let mut strings = vec![String::new()];
  let mut string_indices = HashMap::new();
  string_indices.insert(String::new(), 0usize);
  let name_indices = object_names
    .into_iter()
    .map(|name| {
      if let Some(index) = string_indices.get(&name) {
        *index
      } else {
        let index = strings.len();
        string_indices.insert(name.clone(), index);
        strings.push(name);
        index
      }
    })
    .collect::<Vec<_>>();

  let mut json = String::from(
    "{\"snapshot\":{\"meta\":{\"node_fields\":[\"type\",\"name\",\"id\",\"self_size\",\"edge_count\",\"trace_node_id\",\"detachedness\"],\"node_types\":[[\"hidden\",\"array\",\"string\",\"object\",\"code\",\"closure\",\"regexp\",\"number\",\"native\",\"synthetic\",\"concatenated string\",\"sliced string\",\"symbol\",\"bigint\"],\"string\",\"number\",\"number\",\"number\",\"number\",\"number\"],\"edge_fields\":[\"type\",\"name_or_index\",\"to_node\"],\"edge_types\":[[\"context\",\"element\",\"property\",\"internal\",\"hidden\",\"shortcut\",\"weak\"],\"string_or_number\",\"node\"]},\"node_count\":",
  );
  json.push_str(&name_indices.len().to_string());
  json.push_str(",\"edge_count\":0,\"trace_function_count\":0},\"nodes\":[");
  for (index, name_index) in name_indices.iter().enumerate() {
    if index > 0 {
      json.push(',');
    }
    let id = index.saturating_mul(2).saturating_add(1);
    use std::fmt::Write;
    let _ = write!(json, "3,{name_index},{id},0,0,0,0");
  }
  json.push_str("],\"edges\":[],\"trace_function_infos\":[],\"trace_tree\":[],\"samples\":[],\"locations\":[],\"strings\":[");
  for (index, value) in strings.iter().enumerate() {
    if index > 0 {
      json.push(',');
    }
    push_json_string(&mut json, value);
  }
  json.push_str("]}");
  json.into_bytes()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__HeapProfiler__TakeHeapSnapshot(
  isolate: *mut RealIsolate,
  callback: unsafe extern "C" fn(*mut c_void, *const u8, usize) -> bool,
  arg: *mut c_void,
) {
  let snapshot = quickjs_heap_snapshot_json(isolate);
  if snapshot.is_empty() {
    return;
  }
  unsafe {
    callback(arg, snapshot.as_ptr(), snapshot.len());
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CachedDataVersionTag() -> u32 {
  0x5145_4a53
}

// ---------------------------------------------------------------------------
// Link-stubs for v8 C-ABI symbols that `test_api.rs` references but this
// backend doesn't implement yet. Each returns a benign default
// (null / 0 / false / `Nothing`) so the target LINKS and the many tests that
// don't touch these paths run; tests that do exercise them fail gracefully
// without crashing. Promote individual stubs to real implementations over time.
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn icu_set_default_locale(locale: *const c_char) {
  if locale.is_null() {
    return;
  }
  let s = unsafe { std::ffi::CStr::from_ptr(locale) }.to_string_lossy();
  crate::quickjs::cli_extra::set_default_locale_str(&s);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CompiledWasmModule__GetWireBytesRef(
  this: *mut std::os::raw::c_void,
  length: *mut std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  super::wasm::compiled_module_wire_bytes(this, length)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CompiledWasmModule__SourceUrl(
  this: *mut std::os::raw::c_void,
  length: *mut std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  super::wasm::compiled_module_source_url(this, length)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__Clear(this: *mut std::os::raw::c_void) {
  if this.is_null() {
    return;
  }
  let slot = this as *mut *const Data;
  let stored = unsafe { slot.read_unaligned() };
  unsafe { slot.write_unaligned(ptr::null()) };
  if stored.is_null() {
    return;
  }

  let boxed = unsafe { Box::from_raw(stored as *mut JSValue) };
  let ctx = current_ctx();
  if !ctx.is_null() {
    unsafe { JS_FreeValue(ctx, *boxed) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__IsEmpty(
  this: *const std::os::raw::c_void,
) -> bool {
  if this.is_null() {
    return true;
  }
  let slot = this as *const *const Data;
  unsafe { slot.read_unaligned().is_null() }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Clear(this: *const crate::Map) {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return;
  }
  unsafe {
    let r =
      call_collection_method(ctx, jsval_of(this), c"clear".as_ptr(), &mut []);
    JS_FreeValue(ctx, r);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Delete(
  this: *const crate::Map,
  context: *const Context,
  key: *const Value,
) -> crate::support::MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return crate::support::MaybeBool::Nothing;
  }
  unsafe {
    let mut args = [JS_DupValue(ctx, jsval_of(key))];
    let r = call_collection_method(
      ctx,
      jsval_of(this),
      c"delete".as_ptr(),
      &mut args,
    );
    JS_FreeValue(ctx, args[0]);
    if r.tag == JS_TAG_EXCEPTION {
      return crate::support::MaybeBool::Nothing;
    }
    let b = JS_ToBool(ctx, r) != 0;
    JS_FreeValue(ctx, r);
    mb(b)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Get(
  this: *const crate::Map,
  context: *const Context,
  key: *const Value,
) -> *const Value {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut args = [JS_DupValue(ctx, jsval_of(key))];
    let r =
      call_collection_method(ctx, jsval_of(this), c"get".as_ptr(), &mut args);
    JS_FreeValue(ctx, args[0]);
    if r.tag == JS_TAG_EXCEPTION {
      return ptr::null();
    }
    intern::<Value>(r)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Has(
  this: *const crate::Map,
  context: *const Context,
  key: *const Value,
) -> crate::support::MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return crate::support::MaybeBool::Nothing;
  }
  unsafe {
    let mut args = [JS_DupValue(ctx, jsval_of(key))];
    let r =
      call_collection_method(ctx, jsval_of(this), c"has".as_ptr(), &mut args);
    JS_FreeValue(ctx, args[0]);
    if r.tag == JS_TAG_EXCEPTION {
      return crate::support::MaybeBool::Nothing;
    }
    let b = JS_ToBool(ctx, r) != 0;
    JS_FreeValue(ctx, r);
    mb(b)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__New(isolate: *mut RealIsolate) -> *const crate::Map {
  let v = new_builtin(isolate, c"Map".as_ptr());
  if v.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  intern::<crate::Map>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Set(
  this: *const crate::Map,
  context: *const Context,
  key: *const Value,
  value: *const Value,
) -> *const crate::Map {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut args = [
      JS_DupValue(ctx, jsval_of(key)),
      JS_DupValue(ctx, jsval_of(value)),
    ];
    let r =
      call_collection_method(ctx, jsval_of(this), c"set".as_ptr(), &mut args);
    JS_FreeValue(ctx, args[0]);
    JS_FreeValue(ctx, args[1]);
    if r.tag == JS_TAG_EXCEPTION {
      return ptr::null();
    }
    // Map.prototype.set returns the map; return our handle to `this`.
    JS_FreeValue(ctx, r);
    intern::<crate::Map>(JS_DupValue(ctx, jsval_of(this)))
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__IsRevoked(
  this: *const std::os::raw::c_void,
) -> bool {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return false;
  }
  let proxy = jsval_of(this as *const Value);
  if let Some(revoked) = proxy_revoked_from_table(proxy) {
    return revoked;
  }
  let target = unsafe { JS_GetProxyTarget(ctx, proxy) };
  if target.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return true;
  }
  unsafe { JS_FreeValue(ctx, target) };
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__New(
  context: *const std::os::raw::c_void,
  target: *const std::os::raw::c_void,
  handler: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  if context.is_null() || target.is_null() || handler.is_null() {
    return ptr::null();
  }
  let ctx = ctx_of(context as *const Context);
  if ctx.is_null() {
    return ptr::null();
  }

  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let proxy_ctor = JS_GetPropertyStr(ctx, global, c"Proxy".as_ptr());
    JS_FreeValue(ctx, global);
    if !jsv_is_object(&proxy_ctor) {
      JS_FreeValue(ctx, proxy_ctor);
      return ptr::null();
    }
    let revocable = JS_GetPropertyStr(ctx, proxy_ctor, c"revocable".as_ptr());
    if !JS_IsFunction(ctx, revocable) {
      JS_FreeValue(ctx, revocable);
      JS_FreeValue(ctx, proxy_ctor);
      return ptr::null();
    }

    let mut args = [
      jsval_of(target as *const Object),
      jsval_of(handler as *const Object),
    ];
    let result = JS_Call(ctx, revocable, proxy_ctor, 2, args.as_mut_ptr());
    JS_FreeValue(ctx, revocable);
    JS_FreeValue(ctx, proxy_ctor);
    if result.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return ptr::null();
    }

    let proxy = JS_GetPropertyStr(ctx, result, c"proxy".as_ptr());
    let revoke = JS_GetPropertyStr(ctx, result, c"revoke".as_ptr());
    JS_FreeValue(ctx, result);
    if proxy.tag == JS_TAG_EXCEPTION
      || revoke.tag == JS_TAG_EXCEPTION
      || !JS_IsProxy(proxy)
      || !JS_IsFunction(ctx, revoke)
    {
      if proxy.tag != JS_TAG_EXCEPTION {
        JS_FreeValue(ctx, proxy);
      }
      if revoke.tag != JS_TAG_EXCEPTION {
        JS_FreeValue(ctx, revoke);
      }
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return ptr::null();
    }

    register_proxy_revoke(ctx, proxy, revoke);
    intern::<crate::Proxy>(proxy) as *const std::os::raw::c_void
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__Revoke(this: *const std::os::raw::c_void) {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return;
  }
  let proxy = jsval_of(this as *const Value);
  let Some(revoke) = proxy_revoke_function(ctx, proxy) else {
    return;
  };
  unsafe {
    let result = JS_Call(ctx, revoke, jsv_undefined(), 0, ptr::null_mut());
    JS_FreeValue(ctx, revoke);
    if result.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
    } else {
      JS_FreeValue(ctx, result);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Clear(this: *const crate::Set) {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return;
  }
  unsafe {
    let r =
      call_collection_method(ctx, jsval_of(this), c"clear".as_ptr(), &mut []);
    JS_FreeValue(ctx, r);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Delete(
  this: *const crate::Set,
  context: *const Context,
  key: *const Value,
) -> crate::support::MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return crate::support::MaybeBool::Nothing;
  }
  unsafe {
    let mut args = [JS_DupValue(ctx, jsval_of(key))];
    let r = call_collection_method(
      ctx,
      jsval_of(this),
      c"delete".as_ptr(),
      &mut args,
    );
    JS_FreeValue(ctx, args[0]);
    if r.tag == JS_TAG_EXCEPTION {
      return crate::support::MaybeBool::Nothing;
    }
    let b = JS_ToBool(ctx, r) != 0;
    JS_FreeValue(ctx, r);
    mb(b)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Has(
  this: *const crate::Set,
  context: *const Context,
  key: *const Value,
) -> crate::support::MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return crate::support::MaybeBool::Nothing;
  }
  unsafe {
    let mut args = [JS_DupValue(ctx, jsval_of(key))];
    let r =
      call_collection_method(ctx, jsval_of(this), c"has".as_ptr(), &mut args);
    JS_FreeValue(ctx, args[0]);
    if r.tag == JS_TAG_EXCEPTION {
      return crate::support::MaybeBool::Nothing;
    }
    let b = JS_ToBool(ctx, r) != 0;
    JS_FreeValue(ctx, r);
    mb(b)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__Abort(
  this: *mut std::os::raw::c_void,
) {
  super::wasm::module_compilation_abort(this);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__Finish(
  this: *mut std::os::raw::c_void,
  isolate: *mut std::os::raw::c_void,
  caching_callback: *const std::os::raw::c_void,
  resolution_callback: *const std::os::raw::c_void,
  resolution_data: *mut std::os::raw::c_void,
  drop_resolution_data: *const std::os::raw::c_void,
) {
  super::wasm::module_compilation_finish(
    this,
    isolate,
    caching_callback,
    resolution_callback,
    resolution_data,
    drop_resolution_data,
  );
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__OnBytesReceived(
  this: *mut std::os::raw::c_void,
  bytes: *const std::os::raw::c_void,
  size: usize,
) {
  super::wasm::module_compilation_on_bytes_received(this, bytes, size);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__SetUrl(
  this: *mut std::os::raw::c_void,
  url: *const std::os::raw::c_void,
  length: usize,
) {
  super::wasm::module_compilation_set_url(this, url, length);
}
