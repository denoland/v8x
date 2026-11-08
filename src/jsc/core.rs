//! Foundation for the JSC-backed C-ABI shims.
//!
//! This module owns the representation that backs `*mut RealIsolate` and the
//! handle-scope / context machinery. Every other `shim_*` module builds on the
//! helpers exported here:
//!
//! - `iso_state(p)` — `&mut IsoState` behind a `*mut RealIsolate`.
//! - `current_ctx()` — innermost entered `JSContextRef` (thread-local).
//! - `current_iso()` — current `*mut RealIsolate` (thread-local).
//! - `intern::<T>(jsval)` — protect `jsval` against the current context, record
//!   it in the current handle scope, return it as a `*const T`. The pointer of
//!   a `Local<T>` *is* the JSC `JSValueRef`.
//! - `jsval(p)` / `ctx_of(c)` — reinterpret a handle / context pointer.
#![allow(non_snake_case)]

use crate::jsc::jsc_sys::*;
use crate::{Context, Data, Object, Primitive, RealIsolate};
use std::cell::RefCell;
use std::os::raw::c_void;
use std::ptr;
use std::sync::atomic::{AtomicI64, Ordering};

pub(crate) type WeakCallback = unsafe extern "C" fn(*const c_void);

pub(crate) struct WeakHandle {
  pub handle: *const Data,
  pub parameter: *const c_void,
  pub callback: WeakCallback,
}

#[derive(Clone, Copy)]
pub(crate) struct GcCallbackEntry {
  pub callback: crate::isolate::GcCallbackWithData,
  pub data: *mut c_void,
  pub gc_type_filter: crate::gc::GCType,
}

pub(crate) struct IsoState {
  pub group: JSContextGroupRef,

  /// Serializes the threads that use this isolate, for `v8::Locker`. Unused
  /// until the embedder converts the isolate with `try_into_shared`.
  pub lock: super::locker::IsolateLock,

  pub contexts: Vec<JSGlobalContextRef>,

  pub owned_contexts: Vec<JSGlobalContextRef>,

  pub handles: Vec<(JSContextRef, JSValueRef)>,

  pub data_slots: [*mut c_void; 4],

  pub pending_exception: Option<(JSContextRef, JSValueRef)>,

  pub import_meta_cb:
    Option<crate::isolate::HostInitializeImportMetaObjectCallback>,

  pub cpp_heap: *mut c_void,

  /// Lazily-created fallback context used when a value must be materialised
  /// but no `Context` has been entered (e.g. `v8::External::new` called on a
  /// bare `HandleScope`, as in rusty_v8's `test_simple_external`). V8 lets you
  /// create primitives/externals without an active context; JSC has no such
  /// notion, so we keep one throwaway global context per isolate to host them.
  /// Released in `Isolate::Dispose`.
  pub base_ctx: JSGlobalContextRef,

  pub external_memory: AtomicI64,

  pub external_string_memory: AtomicI64,

  pub weak_handles: Vec<WeakHandle>,

  pub gc_prologue_callbacks: Vec<GcCallbackEntry>,

  pub gc_epilogue_callbacks: Vec<GcCallbackEntry>,

  /// The canonical string for each distinct contents passed to
  /// `String::InternalizeString`, protected until the isolate is disposed so
  /// that internalizing equal strings always yields the same handle.
  pub internalized: std::collections::HashMap<Vec<u16>, JSValueRef>,
}

thread_local! {
    static CURRENT_ISO: RefCell<*mut RealIsolate> = const { RefCell::new(ptr::null_mut()) };
    static CURRENT_CTX: RefCell<JSContextRef> = const { RefCell::new(ptr::null()) };

    static LAST_ISO: RefCell<*mut RealIsolate> = const { RefCell::new(ptr::null_mut()) };
}

#[inline(always)]
pub(crate) fn iso_state<'a>(p: *mut RealIsolate) -> &'a mut IsoState {
  unsafe { &mut *(p as *mut IsoState) }
}

#[inline]
fn adjust_i64(counter: &AtomicI64, delta: i64) -> i64 {
  let mut current = counter.load(Ordering::SeqCst);
  loop {
    let next = current.saturating_add(delta);
    match counter.compare_exchange(
      current,
      next,
      Ordering::SeqCst,
      Ordering::SeqCst,
    ) {
      Ok(_) => return next,
      Err(actual) => current = actual,
    }
  }
}

#[inline]
pub(crate) fn adjust_external_memory(st: &IsoState, delta: i64) -> i64 {
  adjust_i64(&st.external_memory, delta)
}

#[inline]
pub(crate) fn adjust_external_string_memory(st: &IsoState, delta: i64) -> i64 {
  adjust_i64(&st.external_string_memory, delta)
}

#[inline]
pub(crate) fn release_external_string_memory(st: &IsoState) -> i64 {
  let released = st.external_string_memory.swap(0, Ordering::SeqCst);
  if released > 0 {
    adjust_external_memory(st, -released);
  }
  released
}

#[inline(always)]
pub(crate) fn current_iso() -> *mut RealIsolate {
  let cur = CURRENT_ISO.with(|c| *c.borrow());
  if !cur.is_null() {
    return cur;
  }

  LAST_ISO.with(|c| *c.borrow())
}

#[inline(always)]
pub(crate) fn current_ctx() -> JSContextRef {
  CURRENT_CTX.with(|c| *c.borrow())
}

fn set_current(iso: *mut RealIsolate) {
  CURRENT_ISO.with(|c| *c.borrow_mut() = iso);
  if !iso.is_null() {
    LAST_ISO.with(|c| *c.borrow_mut() = iso);
  }
}

pub(crate) fn clear_last_iso(iso: *mut RealIsolate) {
  LAST_ISO.with(|c| {
    if *c.borrow() == iso {
      *c.borrow_mut() = ptr::null_mut();
    }
  });
}

pub(crate) fn restore_current(iso: *mut RealIsolate) {
  if iso.is_null() {
    return;
  }
  set_current(iso);
  refresh_current_ctx(iso_state(iso));
}

fn refresh_current_ctx(st: &IsoState) {
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  CURRENT_CTX.with(|c| *c.borrow_mut() = ctx);
}

/// Run a JSC→Rust callback body, converting any Rust panic into `fallback`
/// instead of letting it unwind across JSC's C frame.
///
/// JSC invokes our trampolines / finalizers / deallocators through plain
/// `extern "C"` function pointers. A Rust panic that tries to unwind through
/// the intervening C (interpreter / JIT / GC) frame is undefined behaviour and
/// the runtime turns it into an immediate `abort()` ("panic in a function that
/// cannot unwind"). Because GC finalizers and promise/microtask callbacks run
/// at non-deterministic times, such a panic shows up as a non-deterministic
/// SIGABRT that truncates the whole test binary (denoland/divybot#653).
///
/// Catching the unwind at the boundary keeps the process alive: the offending
/// operation fails locally (the default panic hook still prints the panic
/// message + location to stderr, so CI logs pinpoint the culprit) but the
/// binary runs to completion deterministically. `AssertUnwindSafe` is sound
/// here: we never observe broken invariants of a poisoned value across the
/// boundary — on panic we discard the result and return a fresh fallback.
#[inline]
pub(crate) fn ffi_guard<R>(
  body: impl FnOnce() -> R,
  fallback: impl FnOnce() -> R,
) -> R {
  match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
    Ok(r) => r,
    Err(_) => fallback(),
  }
}

#[inline(always)]
pub(crate) fn jsval<T>(p: *const T) -> JSValueRef {
  p as JSValueRef
}

#[inline(always)]
pub(crate) fn ctx_of(c: *const Context) -> JSGlobalContextRef {
  c as JSGlobalContextRef
}

#[inline]
fn fallback_ctx(iso: *mut RealIsolate) -> JSContextRef {
  if iso.is_null() {
    return ptr::null();
  }
  let st = iso_state(iso);
  st.contexts
    .last()
    .or_else(|| st.owned_contexts.last())
    .copied()
    .unwrap_or(ptr::null_mut()) as JSContextRef
}

/// A context guaranteed to be non-null (as long as the isolate is live):
/// the entered/owned context if there is one, otherwise the isolate's lazily
/// created base context. Use this when a value MUST be materialised even
/// though the embedder hasn't entered a `Context` (V8 allows this; JSC needs
/// a concrete `JSContextRef`). See [`IsoState::base_ctx`].
#[inline]
pub(crate) fn ensure_ctx(iso: *mut RealIsolate) -> JSContextRef {
  if iso.is_null() {
    return ptr::null();
  }
  let ctx = fallback_ctx(iso);
  if !ctx.is_null() {
    return ctx;
  }
  let st = iso_state(iso);
  if st.base_ctx.is_null() {
    st.base_ctx =
      unsafe { JSGlobalContextCreateInGroup(st.group, ptr::null_mut()) };
  }
  st.base_ctx as JSContextRef
}

#[inline]
pub(crate) fn is_non_value_handle(
  iso: *mut RealIsolate,
  v: JSValueRef,
) -> bool {
  if crate::jsc::function::is_template_ptr(v as *const c_void) {
    return true;
  }
  if iso.is_null() {
    return false;
  }
  let st = iso_state(iso);
  let p = v as JSGlobalContextRef;
  st.owned_contexts.contains(&p) || st.contexts.contains(&p)
}

#[inline]
pub(crate) fn intern_ctx<T>(ctx: JSContextRef, v: JSValueRef) -> *const T {
  if v.is_null() {
    return ptr::null();
  }
  let iso = current_iso();

  if is_non_value_handle(iso, v) {
    return v as *const T;
  }

  let mut ctx = if ctx.is_null() { current_ctx() } else { ctx };
  if ctx.is_null() {
    ctx = fallback_ctx(iso);
  }
  if !iso.is_null() && !ctx.is_null() {
    unsafe {
      JSValueProtect(ctx, v);
      iso_state(iso).handles.push((ctx, v));
    }
  }
  v as *const T
}

#[inline]
pub(crate) fn intern<T>(v: JSValueRef) -> *const T {
  intern_ctx(current_ctx(), v)
}

pub(crate) fn record_pending_exception(ctx: JSContextRef, exc: JSValueRef) {
  if exc.is_null() {
    return;
  }
  let iso = current_iso();
  if iso.is_null() {
    return;
  }
  let ctx = if ctx.is_null() { current_ctx() } else { ctx };
  let ctx = if ctx.is_null() {
    fallback_ctx(iso)
  } else {
    ctx
  };
  if ctx.is_null() {
    return;
  }
  let st = iso_state(iso);

  if let Some((octx, ov)) = st.pending_exception.take() {
    if !octx.is_null() && !ov.is_null() {
      unsafe { JSValueUnprotect(octx, ov) };
    }
  }
  unsafe { JSValueProtect(ctx, exc) };
  st.pending_exception = Some((ctx, exc));
}

pub(crate) fn clear_pending_exception(iso: *mut RealIsolate) {
  if iso.is_null() {
    return;
  }
  let st = iso_state(iso);
  if let Some((ctx, v)) = st.pending_exception.take() {
    if !ctx.is_null() && !v.is_null() {
      unsafe { JSValueUnprotect(ctx, v) };
    }
  }
}

pub(crate) fn peek_pending_exception(iso: *mut RealIsolate) -> JSValueRef {
  if iso.is_null() {
    return ptr::null();
  }
  let st = iso_state(iso);
  st.pending_exception.map(|(_, v)| v).unwrap_or(ptr::null())
}

/// Calls the JS function that `src` evaluates to with `args` in `ctx`.
///
/// Returns `Err(exception)` if evaluating `src` or the call throws (the
/// exception may be null if JSC reported none), so callers can choose whether
/// to surface it as the isolate's pending exception.
pub(crate) unsafe fn call_js_fn(
  ctx: JSContextRef,
  src: &std::ffi::CStr,
  args: &[JSValueRef],
) -> Result<JSValueRef, JSValueRef> {
  if ctx.is_null() {
    return Err(ptr::null());
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let s = JSStringCreateWithUTF8CString(src.as_ptr());
    let f =
      JSEvaluateScript(ctx, s, ptr::null_mut(), ptr::null_mut(), 1, &mut exc);
    JSStringRelease(s);
    if !exc.is_null() || f.is_null() || !JSValueIsObject(ctx, f) {
      return Err(exc);
    }
    let r = JSObjectCallAsFunction(
      ctx,
      f as JSObjectRef,
      ptr::null_mut(),
      args.len(),
      if args.is_empty() {
        ptr::null()
      } else {
        args.as_ptr()
      },
      &mut exc,
    );
    if !exc.is_null() || r.is_null() {
      return Err(exc);
    }
    Ok(r)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__New(params: *const c_void) -> *mut RealIsolate {
  let group = unsafe { JSContextGroupCreate() };
  let state = Box::new(IsoState {
    group,
    lock: super::locker::IsolateLock::new(),
    contexts: Vec::new(),
    owned_contexts: Vec::new(),
    handles: Vec::new(),
    data_slots: [ptr::null_mut(); 4],
    pending_exception: None,
    import_meta_cb: None,
    cpp_heap: crate::jsc::misc::current_cpp_heap(),
    base_ctx: ptr::null_mut(),
    external_memory: AtomicI64::new(0),
    external_string_memory: AtomicI64::new(0),
    weak_handles: Vec::new(),
    gc_prologue_callbacks: Vec::new(),
    gc_epilogue_callbacks: Vec::new(),
    internalized: std::collections::HashMap::new(),
  });
  let iso = Box::into_raw(state) as *mut RealIsolate;
  // Arm the execution-time-limit watchdog so `TerminateExecution` and the
  // near-heap-limit callback have a JSC-side hook to fire through.
  crate::jsc::terminate::install(iso, group, heap_limit_from_params(params));
  iso
}

// Isolate groups. V8 partitions isolates into groups that each own a pointer
// compression cage; JSC has no such cage, and every isolate here already owns
// its own `JSContextGroupRef`. So there is exactly one process-wide group, the
// default one, and V8's answer for a build without multi-cage pointer
// compression applies: `CanCreateNewGroups` is false. The `IsolateGroup`
// handle Rust holds is that group's address; it carries no reference count,
// so cloning and destroying it are free.

static DEFAULT_ISOLATE_GROUP: u8 = 0;

#[inline]
fn default_isolate_group() -> *const c_void {
  &DEFAULT_ISOLATE_GROUP as *const u8 as *const c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__IsolateGroup__GetDefault() -> *const c_void {
  default_isolate_group()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__IsolateGroup__CanCreateNewGroups() -> bool {
  false
}

/// Unreachable through rusty_v8, which checks `CanCreateNewGroups` first
/// because V8 aborts here. Answer with the only group there is.
#[unsafe(no_mangle)]
pub extern "C" fn v8__IsolateGroup__Create() -> *const c_void {
  default_isolate_group()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__IsolateGroup__CLONE(
  this: *const *const c_void,
) -> *const c_void {
  if this.is_null() {
    return default_isolate_group();
  }
  unsafe { *this }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__IsolateGroup__DESTRUCT(_this: *mut *const c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__IsolateGroup__EQ(
  this: *const *const c_void,
  other: *const *const c_void,
) -> bool {
  if this.is_null() || other.is_null() {
    return this == other;
  }
  unsafe { *this == *other }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetGroup(
  _isolate: *const RealIsolate,
) -> *const c_void {
  default_isolate_group()
}

/// Every isolate is in the default group, so this is `Isolate::New`.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__NewInGroup(
  _group: *const *const c_void,
  params: *const c_void,
) -> *mut RealIsolate {
  v8__Isolate__New(params)
}

/// Pull the configured max heap size out of a raw `CreateParams` pointer, if
/// any. Layout (see `isolate_create_params::raw`): word 0 is the
/// `code_event_handler` pointer, then `ResourceConstraints` whose second word
/// (params word 2) is `max_old_generation_size_` — what `heap_limits(_, max)`
/// stores. 0 means no limit was set.
fn heap_limit_from_params(params: *const c_void) -> usize {
  if params.is_null() {
    return 0;
  }
  unsafe { *(params as *const usize).add(2) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetCppHeap(
  isolate: *mut RealIsolate,
) -> *mut c_void {
  if isolate.is_null() {
    return crate::jsc::misc::current_cpp_heap();
  }
  let st = iso_state(isolate);
  if st.cpp_heap.is_null() {
    st.cpp_heap = crate::jsc::misc::current_cpp_heap();
  }
  st.cpp_heap
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Dispose(this: *mut RealIsolate) {
  if this.is_null() {
    return;
  }
  // Disarm the watchdog (and drop its state) before the group is released.
  crate::jsc::terminate::uninstall(this);
  unsafe {
    let mut st = Box::from_raw(this as *mut IsoState);
    st.weak_handles.clear();
    if let Some((ctx, v)) = st.pending_exception.take() {
      if !ctx.is_null() && !v.is_null() {
        JSValueUnprotect(ctx, v);
      }
    }
    while let Some((ctx, v)) = st.handles.pop() {
      if !ctx.is_null() && !v.is_null() {
        JSValueUnprotect(ctx, v);
      }
    }
    if !st.base_ctx.is_null() {
      for (_, v) in st.internalized.drain() {
        JSValueUnprotect(st.base_ctx, v);
      }
    }
    for ctx in st.owned_contexts.drain(..) {
      JSGlobalContextRelease(ctx);
    }
    if !st.base_ctx.is_null() {
      JSGlobalContextRelease(st.base_ctx);
      st.base_ctx = ptr::null_mut();
    }
    JSContextGroupRelease(st.group);
  }
  set_current(ptr::null_mut());
  clear_last_iso(this);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Enter(this: *mut RealIsolate) {
  set_current(this);
  refresh_current_ctx(iso_state(this));
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Exit(_this: *mut RealIsolate) {
  set_current(ptr::null_mut());
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetCurrent() -> *mut RealIsolate {
  current_iso()
}

/// Unlike `GetCurrent`, this reports only an isolate that is actually entered.
/// `current_iso` falls back to the last isolate this thread ran, which is what
/// callbacks arriving outside an Enter/Exit pair need, but `v8::Locker` uses
/// this to assert that no isolate is entered before it takes the lock, and
/// that fallback would make the assert fire on a thread that had merely used
/// an isolate earlier.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__TryGetCurrent() -> *mut RealIsolate {
  CURRENT_ISO.with(|c| *c.borrow())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetNumberOfDataSlots(
  _this: *const RealIsolate,
) -> u32 {
  4
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetData(
  isolate: *const RealIsolate,
  slot: u32,
) -> *mut c_void {
  let st = iso_state(isolate as *mut RealIsolate);
  *st.data_slots.get(slot as usize).unwrap_or(&ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetData(
  isolate: *const RealIsolate,
  slot: u32,
  data: *mut c_void,
) {
  let st = iso_state(isolate as *mut RealIsolate);
  if let Some(s) = st.data_slots.get_mut(slot as usize) {
    *s = data;
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetCurrentContext(
  isolate: *mut RealIsolate,
) -> *const Context {
  let st = iso_state(isolate);
  st.contexts.last().copied().unwrap_or(ptr::null_mut()) as *const Context
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__HandleScope__CONSTRUCT(
  buf: *mut usize,
  isolate: *mut RealIsolate,
) {
  set_current(isolate);
  let st = iso_state(isolate);
  refresh_current_ctx(st);
  unsafe {
    *buf.offset(0) = isolate as usize;
    *buf.offset(1) = st.handles.len();
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__HandleScope__DESTRUCT(this: *mut usize) {
  unsafe {
    let isolate = *this.offset(0) as *mut RealIsolate;
    let saved_depth = *this.offset(1);
    if isolate.is_null() {
      return;
    }
    let st = iso_state(isolate);
    while st.handles.len() > saved_depth {
      let (ctx, v) = st.handles.pop().unwrap();
      if !ctx.is_null() && !v.is_null() {
        JSValueUnprotect(ctx, v);
      }
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Local__New(
  isolate: *mut RealIsolate,
  other: *const Data,
) -> *const Data {
  let _ = isolate;

  intern::<Data>(jsval(other))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__EscapeSlot__reserve(isolate: *mut RealIsolate) -> usize {
  if isolate.is_null() {
    return usize::MAX;
  }
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() {
    return usize::MAX;
  }
  let v = unsafe { JSValueMakeUndefined(ctx) };
  unsafe {
    JSValueProtect(ctx, v);
    st.handles.push((ctx, v));
  }
  st.handles.len() - 1
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__EscapeSlot__escape(
  isolate: *mut RealIsolate,
  index: usize,
  value: *const Data,
) -> *const Data {
  if isolate.is_null() || index == usize::MAX || value.is_null() {
    return value;
  }
  let st = iso_state(isolate);
  let new_val = jsval(value);
  let Some(slot) = st.handles.get_mut(index) else {
    return value;
  };
  let (ctx, old) = *slot;
  if ctx.is_null() {
    return value;
  }
  unsafe {
    JSValueProtect(ctx, new_val);
    if !old.is_null() {
      JSValueUnprotect(ctx, old);
    }
  }
  *slot = (ctx, new_val);
  value
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Undefined(isolate: *mut RealIsolate) -> *const Primitive {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  let v = unsafe { JSValueMakeUndefined(ctx) };
  intern_ctx::<Primitive>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__New(
  isolate: *mut RealIsolate,
  _templ: *const c_void,
  _global_object: *const c_void,
  _microtask_queue: *mut c_void,
) -> *const Context {
  let st = iso_state(isolate);
  let ctx = unsafe { JSGlobalContextCreateInGroup(st.group, ptr::null_mut()) };
  st.owned_contexts.push(ctx);
  unsafe { install_context_compat_shims(ctx) };
  unsafe { install_stacktrace_shim(ctx) };

  unsafe { crate::jsc::isolate::install_unhandled_rejection_bridge(ctx) };
  unsafe { crate::jsc::module::install_dynamic_import_global(ctx) };
  if prepare_stack_trace_cb_is_set() {
    unsafe { install_prepare_stack_trace_bridge(ctx) };
  }
  ctx as *const Context
}

/// V8-compatible structured stack traces. JSC's `Error.captureStackTrace` writes
/// `.stack` as a STRING, ignoring `Error.prepareStackTrace`; many npm packages
/// (express's `depd`, stack-trace, ...) do `Error.captureStackTrace(obj);
/// obj.stack` and expect `prepareStackTrace(obj, callSites)` to run, then call
/// `callSite.getFileName()` etc. Override `captureStackTrace` to install a lazy
/// `stack` accessor that, when `prepareStackTrace` is set, parses JSC's native
/// stack string into V8-shaped CallSite objects and passes them through.
unsafe fn install_stacktrace_shim(ctx: JSGlobalContextRef) {
  if ctx.is_null() {
    return;
  }
  const SRC: &[u8] = b"(function(){\
    'use strict';\
    var OrigError = Error;\
    var nativeCapture = OrigError.captureStackTrace;\
    function header(e){\
      var n, m;\
      try { n = e.name; } catch(_){ n = undefined; }\
      try { m = e.message; } catch(_){ m = undefined; }\
      n = n === undefined ? 'Error' : String(n);\
      m = m === undefined ? '' : String(m);\
      if (n === '') return m;\
      if (m === '') return n;\
      return n + ': ' + m;\
    }\
    function parseFrames(s){\
      if (typeof s !== 'string' || !s) return [];\
      var out = [];\
      var lines = s.split('\\n');\
      for (var i=0;i<lines.length;i++){\
        var ln = lines[i].trim();\
        if (!ln) continue;\
        if (ln.slice(0,3) === 'at ') ln = ln.slice(3);\
        var fn='', loc=ln;\
        var at = ln.lastIndexOf('@');\
        if (at >= 0){ fn = ln.slice(0,at); loc = ln.slice(at+1); }\
        var file=loc, line=0, col=0;\
        var m = /^(.*):(\\d+):(\\d+)$/.exec(loc);\
        if (m){ file=m[1]; line=+m[2]; col=+m[3]; }\
        out.push({fn:fn, file:file, line:line, col:col, raw:lines[i]});\
      }\
      return out;\
    }\
    function makeCallSite(f){\
      return {\
        getFileName:function(){ return f.file || undefined; },\
        getScriptNameOrSourceURL:function(){ return f.file || undefined; },\
        getLineNumber:function(){ return f.line || undefined; },\
        getColumnNumber:function(){ return f.col || undefined; },\
        getFunctionName:function(){ return f.fn || null; },\
        getMethodName:function(){ return f.fn || null; },\
        getTypeName:function(){ return null; },\
        getThis:function(){ return undefined; },\
        getFunction:function(){ return undefined; },\
        getEvalOrigin:function(){ return undefined; },\
        isToplevel:function(){ return true; },\
        isEval:function(){ return false; },\
        isNative:function(){ return f.file === '[native code]'; },\
        isConstructor:function(){ return false; },\
        isAsync:function(){ return false; },\
        isPromiseAll:function(){ return false; },\
        getPromiseIndex:function(){ return null; },\
        toString:function(){ return f.raw; }\
      };\
    }\
    function toV8Frames(raw){\
      var fs = parseFrames(raw);\
      var out = [];\
      for (var i=0;i<fs.length;i++){\
        var f = fs[i];\
        var loc = f.line ? (f.file + ':' + f.line + ':' + f.col) : f.file;\
        out.push('    at ' + (f.fn ? (f.fn + ' (' + loc + ')') : loc));\
      }\
      return out.join('\\n');\
    }\
    function formatStack(target, raw){\
      var prep = globalThis.Error.prepareStackTrace;\
      if (typeof prep === 'function'){\
        try { return prep(target, parseFrames(raw).map(makeCallSite)); } catch(e){}\
      }\
      var h = header(target);\
      var frames = raw ? toV8Frames(raw) : '';\
      return frames ? (h + '\\n' + frames) : (h + '\\n');\
    }\
    function installLazy(target, raw){\
      if (typeof raw !== 'string') raw = '';\
      Object.defineProperty(target, 'stack', {\
        configurable: true, enumerable: false,\
        get: function(){ return formatStack(this, raw); },\
        set: function(v){ Object.defineProperty(this, 'stack', {value:v, writable:true, configurable:true}); }\
      });\
    }\
    function captureStackTrace(target, ctorOpt){\
      var holder = {};\
      try { if (nativeCapture) nativeCapture.call(OrigError, holder, ctorOpt); } catch(e){}\
      var raw = holder.stack;\
      installLazy(target, typeof raw === 'string' ? raw : '');\
    }\
    var names = ['Error','EvalError','RangeError','ReferenceError','SyntaxError','TypeError','URIError','AggregateError'];\
    for (var i=0;i<names.length;i++){\
      (function(nm){\
        var Orig = globalThis[nm];\
        if (typeof Orig !== 'function') return;\
        function trap(e){\
          var raw; try { raw = e.stack; } catch(_){ raw = ''; }\
          installLazy(e, typeof raw === 'string' ? raw : '');\
          return e;\
        }\
        var h = Object.create(null);\
        h.construct = function(t, args, nt){ return trap(Reflect.construct(t, args, nt)); };\
        h.apply = function(t, thisArg, args){ return trap(Reflect.construct(t, args)); };\
        globalThis[nm] = new Proxy(Orig, h);\
      })(names[i]);\
    }\
    globalThis.Error.captureStackTrace = captureStackTrace;\
  })()\0";
  unsafe {
    let js = JSStringCreateWithUTF8CString(
      SRC.as_ptr() as *const std::os::raw::c_char
    );
    let mut exc: JSValueRef = ptr::null();
    JSEvaluateScript(ctx, js, ptr::null_mut(), ptr::null_mut(), 1, &mut exc);
    JSStringRelease(js);
  }
}

thread_local! {
  // deno's native PrepareStackTraceCallback (set via
  // v8__Isolate__SetPrepareStackTraceCallback). When `error.stack` is read,
  // V8 calls this *instead of* a JS `Error.prepareStackTrace`. JSC has no such
  // engine hook, so we bridge through a JS `Error.prepareStackTrace` that
  // forwards into this callback (see install_prepare_stack_trace_bridge).
  static PREPARE_STACK_TRACE_CB: std::cell::Cell<
    Option<crate::isolate::PrepareStackTraceCallback<'static>>,
  > = const { std::cell::Cell::new(None) };
}

unsafe extern "C" fn prepare_stack_trace_trampoline(
  ctx: JSContextRef,
  _function: JSObjectRef,
  _this: JSObjectRef,
  argc: usize,
  argv: *const JSValueRef,
  _exception: *mut JSValueRef,
) -> JSValueRef {
  ffi_guard(
    || unsafe { prepare_stack_trace_impl(ctx, argc, argv) },
    || unsafe { JSValueMakeUndefined(ctx) },
  )
}

unsafe fn prepare_stack_trace_impl(
  ctx: JSContextRef,
  argc: usize,
  argv: *const JSValueRef,
) -> JSValueRef {
  let Some(cb) = PREPARE_STACK_TRACE_CB.with(|c| c.get()) else {
    return unsafe { JSValueMakeUndefined(ctx) };
  };
  if argc < 2 || argv.is_null() {
    return unsafe { JSValueMakeUndefined(ctx) };
  }
  unsafe {
    restore_current(current_iso());
    let error = *argv;
    let sites = *argv.add(1);
    let context = ctx as *const Context;
    let err_h = intern_ctx::<crate::Value>(ctx, error);
    let sites_h = intern_ctx::<crate::Array>(ctx, sites);
    let (Some(c_l), Some(e_l), Some(s_l)) = (
      crate::Local::from_raw(context),
      crate::Local::from_raw(err_h),
      crate::Local::from_raw(sites_h),
    ) else {
      return JSValueMakeUndefined(ctx);
    };
    let ret = cb(c_l, e_l, s_l);
    // PrepareStackTraceCallbackRet is repr(C)(*const Value); its field is
    // private, so transmute to read the returned (source-mapped) stack string.
    let v: *const crate::Value = std::mem::transmute(ret);
    if v.is_null() {
      return JSValueMakeUndefined(ctx);
    }
    jsval(v)
  }
}

/// Wire `Error.prepareStackTrace` to deno's native callback. The
/// `install_stacktrace_shim` lazy `.stack` getter already builds V8-shaped
/// CallSite objects and calls `Error.prepareStackTrace(error, sites)` when one
/// is set — but deno registers a *native* callback, not a JS one. Bridge them:
/// a JS wrapper forwards `(error, sites)` into the native trampoline, clearing
/// the hook for the duration so deno's own re-dispatch to
/// `Error.prepareStackTrace` (it calls the user hook if present) can't recurse.
pub(crate) unsafe fn install_prepare_stack_trace_bridge(
  ctx: JSGlobalContextRef,
) {
  if ctx.is_null() {
    return;
  }
  unsafe {
    let name =
      JSStringCreateWithUTF8CString(c"__v8jsc_denoPrepareNative".as_ptr());
    let f = JSObjectMakeFunctionWithCallback(
      ctx,
      name,
      Some(prepare_stack_trace_trampoline),
    );
    if !f.is_null() {
      let global = JSContextGetGlobalObject(ctx);
      JSObjectSetProperty(
        ctx,
        global,
        name,
        f as JSValueRef,
        0,
        ptr::null_mut(),
      );
    }
    JSStringRelease(name);
    if f.is_null() {
      return;
    }
    const SRC: &[u8] = b"(function(){\
      var native = globalThis.__v8jsc_denoPrepareNative;\
      try { delete globalThis.__v8jsc_denoPrepareNative; } catch(e){}\
      Error.prepareStackTrace = function(error, sites){\
        var saved = Error.prepareStackTrace;\
        Error.prepareStackTrace = undefined;\
        try { return native(error, sites); }\
        finally { Error.prepareStackTrace = saved; }\
      };\
    })()\0";
    let js = JSStringCreateWithUTF8CString(
      SRC.as_ptr() as *const std::os::raw::c_char
    );
    let mut exc: JSValueRef = ptr::null();
    JSEvaluateScript(ctx, js, ptr::null_mut(), ptr::null_mut(), 1, &mut exc);
    JSStringRelease(js);
  }
}

/// Store deno's PrepareStackTraceCallback and install the JS bridge on every
/// known context of `iso` (mirrors SetPromiseRejectCallback's fan-out).
pub(crate) fn set_prepare_stack_trace_cb(
  iso: *mut RealIsolate,
  cb: crate::isolate::PrepareStackTraceCallback<'static>,
) {
  PREPARE_STACK_TRACE_CB.with(|c| c.set(Some(cb)));
  let iso = if iso.is_null() { current_iso() } else { iso };
  if iso.is_null() {
    return;
  }
  let st = iso_state(iso);
  let ctxs: Vec<JSGlobalContextRef> = st
    .owned_contexts
    .iter()
    .chain(st.contexts.iter())
    .copied()
    .collect();
  for gctx in ctxs {
    if !gctx.is_null() {
      unsafe { install_prepare_stack_trace_bridge(gctx) };
    }
  }
}

pub(crate) fn prepare_stack_trace_cb_is_set() -> bool {
  PREPARE_STACK_TRACE_CB.with(|c| {
    let v = c.get();
    let set = v.is_some();
    c.set(v);
    set
  })
}

unsafe fn install_context_compat_shims(ctx: JSGlobalContextRef) {
  if ctx.is_null() {
    return;
  }

  const SRC: &[u8] = b"(function(){\
        'use strict';\
        var realO = Object.setPrototypeOf;\
        var realR = Reflect.setPrototypeOf;\
        var g = globalThis;\
        var gdop = Object.getOwnPropertyDescriptor;\
        var gopn = Object.getOwnPropertyNames;\
        var gops = Object.getOwnPropertySymbols;\
        var dp = Object.defineProperty;\
        var getProto = Object.getPrototypeOf;\
        var realIsProto = Object.prototype.isPrototypeOf;\
        var virtualChain = [];\
        function recordChain(p){\
            var cur = p;\
            while (cur && cur !== Object.prototype) {\
                if (virtualChain.indexOf(cur) === -1) virtualChain.push(cur);\
                cur = getProto(cur);\
            }\
        }\
        function flatten(t, p){\
            var chain = [];\
            var cur = p;\
            while (cur && cur !== Object.prototype && cur !== Function.prototype) {\
                chain.push(cur); cur = getProto(cur);\
            }\
            for (var i = chain.length - 1; i >= 0; i--) {\
                var proto = chain[i];\
                var keys = gopn(proto).concat(gops(proto));\
                for (var j = 0; j < keys.length; j++) {\
                    var k = keys[j];\
                    if (k === 'constructor') continue;\
                    if (Object.prototype.hasOwnProperty.call(t, k)) continue;\
                    var d = gdop(proto, k);\
                    if (!d) continue;\
                    try { dp(t, k, d); } catch(e) {}\
                }\
            }\
        }\
        function onGlobalProto(p){ flatten(g, p); recordChain(p); }\
        Object.setPrototypeOf = function(t, p){\
            if (t === g) { try { return realO(t, p); } catch(e) { onGlobalProto(p); return t; } }\
            return realO(t, p);\
        };\
        Reflect.setPrototypeOf = function(t, p){\
            if (t === g) { try { return realR(t, p); } catch(e) { onGlobalProto(p); return true; } }\
            return realR(t, p);\
        };\
        dp(Object.prototype, 'isPrototypeOf', { value: function(o){\
            if (o === g && virtualChain.indexOf(this) !== -1) return true;\
            return realIsProto.call(this, o);\
        }, writable: true, enumerable: false, configurable: true });\
    })()\0";
  unsafe {
    let js = JSStringCreateWithUTF8CString(
      SRC.as_ptr() as *const std::os::raw::c_char
    );
    let mut exc: JSValueRef = ptr::null();
    JSEvaluateScript(ctx, js, ptr::null_mut(), ptr::null_mut(), 1, &mut exc);
    JSStringRelease(js);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__Enter(this: *const Context) {
  let iso = current_iso();
  if iso.is_null() {
    return;
  }
  let st = iso_state(iso);
  st.contexts.push(ctx_of(this));
  refresh_current_ctx(st);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__Exit(_this: *const Context) {
  let iso = current_iso();
  if iso.is_null() {
    return;
  }
  let st = iso_state(iso);
  st.contexts.pop();
  refresh_current_ctx(st);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__Global(this: *const Context) -> *const Object {
  let ctx = ctx_of(this);
  let global = unsafe { JSContextGetGlobalObject(ctx) };
  intern_ctx::<Object>(ctx, global as JSValueRef)
}
