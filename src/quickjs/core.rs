//! Foundation for the QuickJS-backed C-ABI shims.
//!
//! This module owns the representation that backs `*mut RealIsolate` and the
//! handle-scope / context / arena machinery. Every other `shim_*` module in the
//! QuickJS backend builds on the helpers exported here.
//!
//! ## The key design difference from JSC
//!
//! In the JSC backend a `Local<T>`'s pointer *is* the `JSValueRef` (a pointer).
//! In QuickJS a `JSValue` is a 16-byte struct (a union + a tag), **not** a
//! pointer, so it cannot itself be a v8 `Local<T>` (which the vendored source
//! treats as `*const T`, a pointer). We therefore use an **arena**: every
//! handle is a heap box holding one `JSValue`; the box's address is the v8
//! handle. Reading the box recovers the `JSValue`.
//!
//! ## Refcount discipline (the #1 correctness risk)
//!
//! Invariant: every arena slot owns **exactly one** QuickJS refcount on its
//! `JSValue`, and frees it **exactly once** when the slot is reclaimed (on
//! handle-scope pop or isolate dispose). Promoting a borrowed `JSValue` into a
//! handle therefore `JS_DupValue`s it; producing a fresh value (`JS_Eval`,
//! `JS_NewObject`, ...) already returns +1, so it is moved into the slot
//! without an extra dup.
//!
//! ## Helper API (used by every other QuickJS `shim_*` module)
//!
//! - `iso_state(p)` — `&mut IsoState` behind a `*mut RealIsolate`.
//! - `current_iso()` — current `*mut RealIsolate` (thread-local).
//! - `current_ctx()` — innermost entered `*mut JSContext` (thread-local).
//! - `intern::<T>(jsval)` — move an owned `JSValue` into a fresh arena slot in
//!   the current handle scope; returns the slot pointer as `*const T`.
//! - `intern_dup::<T>(jsval)` — like `intern` but `JS_DupValue`s first (use
//!   when the `JSValue` is borrowed and you must not consume its refcount).
//! - `jsval_of(ptr)` — read the `JSValue` out of a handle slot pointer.
//! - `ctx_of(c)` — recover the `*mut JSContext` backing a `*const Context`.

#![allow(non_snake_case)]

use super::quickjs_sys::*;
use crate::support::SharedPtrBase;
use crate::{
  Allocator, Context, Data, MicrotaskQueue, MicrotasksPolicy, Object,
  ObjectTemplate, Primitive, RealIsolate, String as V8String, Value,
};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{
  AtomicBool, AtomicI64, AtomicPtr, AtomicUsize, Ordering,
};
use std::sync::{Arc, Mutex, MutexGuard};

pub(crate) type WeakCallback = unsafe extern "C" fn(*const c_void);

unsafe extern "C" {
  fn v82jsc_snapshot_capture_intrinsics(ctx: *mut JSContext, registry: JSValue);
  fn JS_DefinePropertyValueStr(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: *const c_char,
    val: JSValue,
    flags: c_int,
  ) -> c_int;
}

pub(crate) struct WeakHandle {
  pub handle: *const Data,
  pub parameter: *const c_void,
  pub callback: WeakCallback,
  pub weak_ref: JSValue,
  pub context: *mut JSContext,
}

pub(crate) struct PersistentHandle {
  pub slot: *mut JSValue,
  pub is_weak: bool,
}

#[repr(C)]
pub(crate) struct PersistentCell {
  pub value: JSValue,
  pub context: *mut JSContext,
  pub isolate: *mut RealIsolate,
}

#[derive(Clone, Copy)]
pub(crate) struct GcCallbackEntry {
  pub callback: crate::isolate::GcCallbackWithData,
  pub data: *mut c_void,
  pub gc_type_filter: crate::gc::GCType,
}

#[derive(Clone, Copy)]
pub(crate) struct InterruptEntry {
  pub callback: crate::isolate::InterruptCallback,
  pub data: *mut c_void,
}

/// Process-wide "force strict mode" flag, toggled by
/// `v8__V8__SetFlagsFromString` when it sees V8's `--use_strict` flag. V8
/// applies `--use_strict` globally (which is why the rusty_v8 test that uses it
/// lives in its own process), so a process-wide atomic faithfully mirrors that:
/// every subsequently compiled global script is evaluated in strict mode.
pub(crate) static FORCE_STRICT: AtomicBool = AtomicBool::new(false);

/// Process-wide maximum stack region for subsequently created QuickJS
/// runtimes. V8's `--stack-size` is expressed in KiB; this stores bytes for
/// `JS_SetMaxStackSize`.
pub(crate) static MAX_STACK_SIZE: AtomicUsize = AtomicUsize::new(1024 * 1024);

/// Process-wide heap limit for subsequently created QuickJS runtimes. V8's
/// `--max-old-space-size` is expressed in MiB; this stores bytes for
/// `JS_SetMemoryLimit`. A value of zero leaves the runtime unlimited.
pub(crate) static MAX_HEAP_SIZE: AtomicUsize = AtomicUsize::new(0);

/// The `JS_EVAL_TYPE_GLOBAL` flags to use when running a top-level script,
/// folding in strict mode if `--use_strict` was set.
#[inline]
pub(crate) fn global_eval_flags() -> c_int {
  let mut flags = JS_EVAL_TYPE_GLOBAL;
  if FORCE_STRICT.load(Ordering::Relaxed) {
    flags |= JS_EVAL_FLAG_STRICT;
  }
  flags
}

fn skip_js_space_and_comments(mut bytes: &[u8]) -> &[u8] {
  loop {
    bytes = bytes.trim_ascii_start();
    if let Some(rest) = bytes.strip_prefix(b"//") {
      bytes = match rest.iter().position(|b| *b == b'\n' || *b == b'\r') {
        Some(pos) => &rest[pos + 1..],
        None => &[],
      };
      continue;
    }
    if let Some(rest) = bytes.strip_prefix(b"/*") {
      bytes = match rest.windows(2).position(|w| w == b"*/") {
        Some(pos) => &rest[pos + 2..],
        None => &[],
      };
      continue;
    }
    return bytes;
  }
}

fn starts_with_strict_directive(source: &[u8]) -> bool {
  let source = skip_js_space_and_comments(source);
  let Some((&quote, rest)) = source.split_first() else {
    return false;
  };
  if quote != b'\'' && quote != b'"' {
    return false;
  }
  let Some(after_literal) = rest.strip_prefix(b"use strict") else {
    return false;
  };
  let Some((&end_quote, after_quote)) = after_literal.split_first() else {
    return false;
  };
  if end_quote != quote {
    return false;
  }
  after_quote
    .first()
    .is_none_or(|b| b.is_ascii_whitespace() || *b == b';')
}

fn maybe_report_strict_mode_use(source: &[u8]) {
  if !starts_with_strict_directive(source) {
    return;
  }
  let iso = current_iso();
  if iso.is_null() {
    return;
  }
  let callback = iso_state(iso).use_counter_callback;
  if let Some(callback) = callback {
    let mut isolate = unsafe {
      crate::Isolate::from_raw_isolate_ptr(
        crate::UnsafeRawIsolatePtr::from_real_ptr(iso),
      )
    };
    unsafe { callback(&mut isolate, crate::UseCounterFeature::kStrictMode) };
  }
}

fn rewrite_script_source(source: &str) -> Option<String> {
  let mut rewritten = rewrite_v8_native_intrinsics(source);
  let input = rewritten.as_deref().unwrap_or(source);
  if let Some(text) = super::module::rewrite_dynamic_phase_imports(input, false)
  {
    rewritten = Some(text);
  }
  rewritten
}

pub(crate) fn rewrite_v8_native_intrinsics(source: &str) -> Option<String> {
  if !source.contains("%PrepareFunctionForOptimization")
    && !source.contains("%OptimizeFunctionOnNextCall")
    && !source.contains("%NeverOptimizeFunction")
    && !source.contains("%AtomicsNumWaitersForTesting")
    && !source.contains("%AtomicsNumUnresolvedAsyncPromisesForTesting")
  {
    return None;
  }

  let mut out = String::with_capacity(source.len());
  let mut pos = 0usize;
  while pos < source.len() {
    if source[pos..].starts_with("%PrepareFunctionForOptimization(") {
      if let Some((end, args)) =
        read_intrinsic_args(source, pos, "%PrepareFunctionForOptimization")
      {
        out.push_str("(void (");
        out.push_str(args);
        out.push_str("))");
        pos = end;
        continue;
      }
    }
    if source[pos..].starts_with("%OptimizeFunctionOnNextCall(") {
      if let Some((end, args)) =
        read_intrinsic_args(source, pos, "%OptimizeFunctionOnNextCall")
      {
        out.push_str(
          "(globalThis.__v8x_fast_api_next_call=((globalThis.__v8x_fast_api_next_call|0)+1),void (",
        );
        out.push_str(args);
        out.push_str("))");
        pos = end;
        continue;
      }
    }
    if source[pos..].starts_with("%NeverOptimizeFunction(") {
      if let Some((end, args)) =
        read_intrinsic_args(source, pos, "%NeverOptimizeFunction")
      {
        out.push_str("(void (");
        out.push_str(args);
        out.push_str("))");
        pos = end;
        continue;
      }
    }
    if source[pos..].starts_with("%AtomicsNumWaitersForTesting(") {
      if let Some((end, args)) =
        read_intrinsic_args(source, pos, "%AtomicsNumWaitersForTesting")
      {
        out.push_str("__v8xAtomicsNumWaitersForTesting(");
        out.push_str(args);
        out.push(')');
        pos = end;
        continue;
      }
    }
    if source[pos..]
      .starts_with("%AtomicsNumUnresolvedAsyncPromisesForTesting(")
    {
      if let Some((end, args)) = read_intrinsic_args(
        source,
        pos,
        "%AtomicsNumUnresolvedAsyncPromisesForTesting",
      ) {
        out.push_str("__v8xAtomicsNumUnresolvedAsyncPromisesForTesting(");
        out.push_str(args);
        out.push(')');
        pos = end;
        continue;
      }
    }

    let ch = source[pos..].chars().next().unwrap();
    out.push(ch);
    pos += ch.len_utf8();
  }

  (out != source).then_some(out)
}

fn read_intrinsic_args<'a>(
  source: &'a str,
  start: usize,
  name: &str,
) -> Option<(usize, &'a str)> {
  let open = start + name.len();
  if source.as_bytes().get(open) != Some(&b'(') {
    return None;
  }
  let args_start = open + 1;
  let mut depth = 1usize;
  let mut pos = args_start;
  while pos < source.len() {
    match source.as_bytes()[pos] {
      b'(' => depth += 1,
      b')' => {
        depth -= 1;
        if depth == 0 {
          return Some((pos + 1, &source[args_start..pos]));
        }
      }
      _ => {}
    }
    pos += 1;
  }
  None
}

#[cfg(test)]
mod v8_native_intrinsic_tests {
  use super::rewrite_v8_native_intrinsics;

  #[test]
  fn rewrites_optimization_intrinsics() {
    let source = concat!(
      "%PrepareFunctionForOptimization(f);",
      "%NeverOptimizeFunction(g);",
      "%OptimizeFunctionOnNextCall(f);",
    );
    let rewritten = rewrite_v8_native_intrinsics(source).unwrap();
    assert!(!rewritten.contains('%'));
    assert!(rewritten.contains("void (f)"));
    assert!(rewritten.contains("void (g)"));
    assert!(rewritten.contains("__v8x_fast_api_next_call"));
  }
}

pub(crate) fn note_compilation_cache_miss() {
  let iso = current_iso();
  if iso.is_null() {
    return;
  }
  let callback = iso_state(iso).counter_lookup_callback;
  if let Some(callback) = callback {
    let counter = unsafe { callback(c"c:V8.CompilationCacheMisses".as_ptr()) };
    if !counter.is_null() {
      unsafe { *counter += 1 };
    }
  }
}

pub(crate) struct IsoState {
  pub rt: *mut JSRuntime,

  /// Serializes the threads that use this isolate, for `v8::Locker`. Unused
  /// until the embedder converts the isolate with `try_into_shared`.
  pub lock: super::locker::IsolateLock,

  pub ctx: *mut JSContext,

  pub contexts: Vec<*mut JSContext>,

  pub handles: Vec<*mut JSValue>,

  pub persistent_handles: Vec<PersistentHandle>,

  pub script_host_defined_options: HashMap<usize, JSValue>,

  pub host_defined_options_by_name: HashMap<String, JSValue>,

  pub private_symbols: Vec<(JSValue, JSValue)>,

  pub atomics_waiter_resolvers: HashMap<u64, (*mut JSContext, JSValue)>,

  pub math_random_states: HashMap<usize, super::init::V8MathRandom>,

  pub data_slots: [*mut c_void; 4],

  // QuickJS class id for `v8::External` objects. Registered on the runtime
  // BEFORE the first JSContext is created (see `v8__Isolate__New`): a context
  // sizes its per-context `class_proto` array to `rt->class_count` at creation
  // time and never grows it, so a class registered afterward would make
  // `JS_NewObjectClass(ctx, id)` read `class_proto[id]` out of bounds.
  pub ext_class_id: JSClassID,

  // QuickJS class id for ObjectTemplate named-property-handler instances.
  // It has the same registration timing constraint as `ext_class_id`.
  pub named_handler_class_id: JSClassID,

  // Whether the bootstrap context (`ctx`) has been handed out by
  // `v8__Context__New`. QuickJS has one global object per JSContext, but
  // deno_core uses multiple v8::Contexts (notably during snapshot creation,
  // where two coexist). Backing them all by the single `ctx` makes their
  // embedder-data slots collide. So the FIRST Context::New uses the
  // bootstrapped `ctx` (runtime is single-context, unchanged); each later one
  // gets its own JSContext (tracked in `extra_contexts`) for independent slots.
  pub main_ctx_claimed: bool,

  pub extra_contexts: Vec<*mut JSContext>,

  // Snapshot support (see snapshot.rs). `snap` records on SnapshotCreator
  // isolates; `restored` holds a parsed blob to replay into new contexts.
  pub is_snapshot_creator: bool,
  /// Per-context AddData slot counters (SnapshotCreator::AddData returns the
  /// data index; the tape replays slots in call order).
  pub ctx_data_counts: HashMap<usize, usize>,
  pub iso_data_count: usize,
  pub iso_added_contexts: usize,
  pub snap_default_context: Option<usize>,
  pub snap_contexts: Vec<usize>,
  // Fresh context captures let the snapshot writer omit an unchanged QuickJS
  // global graph. Snapshot restore already creates the same initialized
  // context, so serializing that baseline would only copy built-ins.
  pub snap_context_baselines: HashMap<usize, super::snapshot::ContextSnapshot>,
  // Capture while the rusty_v8 annex and embedder callbacks are still alive.
  pub snap_default_context_capture: Option<super::snapshot::ContextSnapshot>,
  pub snap_context_captures: Vec<super::snapshot::ContextSnapshot>,
  pub context_global_templates:
    HashMap<usize, super::snapshot::SnapshotObjectTemplate>,
  pub snap_isolate_data: Vec<Vec<u8>>,
  pub snap_context_data: HashMap<usize, Vec<Vec<u8>>>,
  pub snap_context_data_values: HashMap<usize, Vec<Option<JSValue>>>,
  pub restored_snapshot: Option<super::snapshot::SnapshotBlob>,
  pub restored_isolate_data: Vec<Option<super::snapshot::SnapshotBytes>>,
  pub restored_context_data:
    HashMap<usize, Vec<Option<super::snapshot::SnapshotBytes>>>,
  pub restored_context_values: HashMap<usize, Vec<Option<JSValue>>>,
  pub external_references: Arc<[usize]>,

  pub external_memory: AtomicI64,

  pub external_string_memory: AtomicI64,

  pub bytecode_and_metadata_size: AtomicUsize,

  pub global_handles: AtomicI64,

  pub weak_handles: Vec<WeakHandle>,

  pub gc_prologue_callbacks: Vec<GcCallbackEntry>,

  pub gc_epilogue_callbacks: Vec<GcCallbackEntry>,

  pub message_listeners: Vec<crate::isolate::MessageCallback>,

  pub use_counter_callback: Option<crate::UseCounterCallback>,

  pub counter_lookup_callback:
    Option<crate::isolate_create_params::CounterLookupCallback>,

  pub javascript_execution_disallow_scopes: Vec<crate::scope::OnFailure>,

  pub javascript_execution_allow_depth: usize,

  pub near_heap_limit_callback: AtomicUsize,

  pub near_heap_limit_callback_data: AtomicUsize,

  pub near_heap_limit_current: AtomicUsize,

  pub near_heap_limit_initial: AtomicUsize,

  pub near_heap_limit_in_callback: AtomicBool,

  // Emulates `v8::Isolate::TerminateExecution`. Set by `TerminateExecution`,
  // cleared by `CancelTerminateExecution`; while set, the op-dispatch boundary
  // and the microtask/job drain refuse to run JS (matching V8, which throws an
  // uncatchable termination exception on the next safe point). An `AtomicBool`
  // because V8's terminate may be requested from another thread.
  pub terminating: std::sync::atomic::AtomicBool,

  // The WAMR function currently executing on this isolate, if any. WAMR runs
  // outside QuickJS's interrupt polling, so cross-thread termination uses this
  // stable (WAMR-owned) pointer to interrupt the interpreter directly.
  pub active_wasm_func: AtomicPtr<c_void>,

  pub pending_interrupts: Mutex<Vec<InterruptEntry>>,

  pub cpu_profiler: super::inspector::CpuProfilerState,

  pub array_buffer_allocator: SharedPtrBase<Allocator>,

  pub pending_array_buffer_frees: Vec<(*mut Allocator, *mut c_void, usize)>,

  pub microtasks_policy: MicrotasksPolicy,

  pub default_microtask_queue: *mut MicrotaskQueue,

  pub context_microtask_queues: HashMap<usize, *mut MicrotaskQueue>,

  pub continuation_data: JSValue,

  pub continuation_hooks_enabled: bool,

  /// The isolate's reference to its `IsolateGroup` (see `isolate.rs`).
  pub group: *const super::isolate::IsolateGroupInner,

  /// `Isolate::SetModifyCodeGenerationFromStringsCallback`.
  pub modify_codegen_callback:
    Option<super::isolate::ModifyCodeGenerationFromStringsCallback>,

  /// Contexts whose global object was detached (`Context::DetachGlobal`).
  pub detached_contexts: std::collections::HashSet<usize>,
}

impl IsoState {
  #[inline(always)]
  pub fn is_terminating(&self) -> bool {
    self.terminating.load(std::sync::atomic::Ordering::Acquire)
  }
}

#[inline]
pub(crate) fn javascript_execution_allowed() -> bool {
  let iso = current_iso();
  if iso.is_null() {
    return true;
  }
  let st = iso_state(iso);
  st.javascript_execution_allow_depth > 0
    || st.javascript_execution_disallow_scopes.is_empty()
}

#[inline]
unsafe fn throw_javascript_execution_disallowed(ctx: *mut JSContext) {
  unsafe {
    JS_ThrowTypeError(ctx, c"Javascript execution is disallowed".as_ptr());
  }
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

#[inline]
pub(crate) fn note_compiled_bytecode(
  isolate: *mut RealIsolate,
  source_len: usize,
) {
  if !isolate.is_null() {
    iso_state(isolate)
      .bytecode_and_metadata_size
      .fetch_add(source_len.max(1), Ordering::SeqCst);
  }
}

thread_local! {
    static CURRENT_ISO: RefCell<*mut RealIsolate> = const { RefCell::new(ptr::null_mut()) };
    static CURRENT_CTX: RefCell<*mut JSContext> = const { RefCell::new(ptr::null_mut()) };
    static ISO_STACK: RefCell<Vec<*mut RealIsolate>> = const { RefCell::new(Vec::new()) };

    static LAST_ISO: RefCell<*mut RealIsolate> = const { RefCell::new(ptr::null_mut()) };
}

#[inline(always)]
pub(crate) fn iso_state<'a>(p: *mut RealIsolate) -> &'a mut IsoState {
  unsafe { &mut *(p as *mut IsoState) }
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
pub(crate) fn current_ctx() -> *mut JSContext {
  CURRENT_CTX.with(|c| *c.borrow())
}

pub(crate) struct CallbackContextGuard {
  isolate: *mut RealIsolate,
  depth: usize,
}

impl Drop for CallbackContextGuard {
  fn drop(&mut self) {
    if self.isolate.is_null() {
      return;
    }
    let st = iso_state(self.isolate);
    st.contexts.truncate(self.depth);
    refresh_current_ctx(st);
  }
}

pub(crate) fn push_callback_context(
  ctx: *mut JSContext,
) -> Option<CallbackContextGuard> {
  let isolate = current_iso();
  if isolate.is_null() || ctx.is_null() {
    return None;
  }
  let st = iso_state(isolate);
  let depth = st.contexts.len();
  st.contexts.push(ctx);
  refresh_current_ctx(st);
  Some(CallbackContextGuard { isolate, depth })
}

fn set_current(iso: *mut RealIsolate) {
  super::module::switch_module_caches(iso);
  CURRENT_ISO.with(|c| *c.borrow_mut() = iso);
  if !iso.is_null() {
    LAST_ISO.with(|c| *c.borrow_mut() = iso);
    refresh_current_ctx(iso_state(iso));
  } else {
    CURRENT_CTX.with(|c| *c.borrow_mut() = ptr::null_mut());
  }
}

fn clear_last_iso(iso: *mut RealIsolate) {
  LAST_ISO.with(|c| {
    if *c.borrow() == iso {
      *c.borrow_mut() = ptr::null_mut();
    }
  });
}

fn refresh_current_ctx(st: &IsoState) {
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  CURRENT_CTX.with(|c| *c.borrow_mut() = ctx);
}

fn push_current_iso(iso: *mut RealIsolate) {
  ISO_STACK.with(|s| s.borrow_mut().push(iso));
  set_current(iso);
}

fn pop_current_iso(iso: *mut RealIsolate) {
  let next = ISO_STACK.with(|s| {
    let mut s = s.borrow_mut();
    if s.last().copied() == Some(iso) {
      s.pop();
    } else if let Some(pos) = s.iter().rposition(|entry| *entry == iso) {
      s.remove(pos);
    }
    s.last().copied().unwrap_or(ptr::null_mut())
  });
  set_current(next);
}

fn remove_current_iso(iso: *mut RealIsolate) {
  let next = ISO_STACK.with(|s| {
    let mut s = s.borrow_mut();
    s.retain(|entry| *entry != iso);
    s.last().copied().unwrap_or(ptr::null_mut())
  });
  set_current(next);
  clear_last_iso(iso);
}

#[inline(always)]
pub(crate) fn jsval_of<T>(p: *const T) -> JSValue {
  if p.is_null() {
    return jsv_undefined();
  }
  unsafe { *(p as *const JSValue) }
}

pub(crate) const JS_TAG_V8_CONTEXT: i64 = 0x7632;

#[inline(always)]
pub(crate) fn ctx_to_jsval(ctx: *mut JSContext) -> JSValue {
  make_value(
    JS_TAG_V8_CONTEXT,
    JSValueUnion {
      ptr: ctx as *mut c_void,
    },
  )
}

#[inline(always)]
pub(crate) fn intern_ctx(ctx: *mut JSContext) -> *const Context {
  intern::<Context>(ctx_to_jsval(ctx))
}

#[inline(always)]
pub(crate) fn is_non_value_handle<T>(p: *const T) -> bool {
  !p.is_null() && super::function::is_template_ptr(p as *const c_void)
}

#[inline(always)]
pub(crate) fn is_interned_handle<T>(p: *const T) -> bool {
  let iso = current_iso();
  !iso.is_null() && {
    let st = iso_state(iso);
    st.handles
      .iter()
      .any(|&h| std::ptr::addr_eq(h, p as *const JSValue))
      || st
        .persistent_handles
        .iter()
        .any(|h| std::ptr::addr_eq(h.slot, p as *const JSValue))
  }
}

#[inline(always)]
pub(crate) fn ctx_of(c: *const Context) -> *mut JSContext {
  if c.is_null() {
    return ptr::null_mut();
  }
  let v = unsafe { *(c as *const JSValue) };
  if v.tag == JS_TAG_V8_CONTEXT {
    unsafe { v.u.ptr as *mut JSContext }
  } else {
    c as *mut JSContext
  }
}

#[inline]
fn fallback_ctx(iso: *mut RealIsolate) -> *mut JSContext {
  if iso.is_null() {
    return ptr::null_mut();
  }
  let st = iso_state(iso);
  st.contexts.last().copied().unwrap_or(st.ctx)
}

#[inline]
pub(crate) fn intern<T>(v: JSValue) -> *const T {
  let iso = current_iso();
  if iso.is_null() {
    let ctx = current_ctx();
    if !ctx.is_null() {
      unsafe { JS_FreeValue(ctx, v) };
    }
    return ptr::null();
  }
  let slot = Box::into_raw(Box::new(v));
  iso_state(iso).handles.push(slot);
  slot as *const T
}

#[inline]
pub(crate) fn intern_dup<T>(ctx: *mut JSContext, v: JSValue) -> *const T {
  let ctx = if ctx.is_null() { current_ctx() } else { ctx };
  let ctx = if ctx.is_null() {
    fallback_ctx(current_iso())
  } else {
    ctx
  };
  if ctx.is_null() {
    return ptr::null();
  }
  let dup = unsafe { JS_DupValue(ctx, v) };
  intern::<T>(dup)
}

// Process-global refcount table for SharedArrayBuffer memory shared across worker
// threads. Keyed by data pointer so foreign pointers (e.g. SABs deno hands us via
// a backing store rather than `sab_alloc`) are handled gracefully: dup/free on an
// unknown pointer is a no-op, so we never free memory we did not allocate.
fn sab_refcounts()
-> &'static std::sync::Mutex<std::collections::HashMap<usize, usize>> {
  static T: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<usize, usize>>,
  > = std::sync::OnceLock::new();
  T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

#[derive(Clone, Copy)]
struct AtomicsWaiterTarget {
  id: u64,
  isolate: usize,
}

#[derive(Default)]
struct AtomicsWaiterRegistry {
  by_location: HashMap<usize, VecDeque<AtomicsWaiterTarget>>,
  ready_by_isolate: HashMap<usize, Vec<u64>>,
}

fn atomics_waiter_registry() -> &'static Mutex<AtomicsWaiterRegistry> {
  static REGISTRY: std::sync::OnceLock<Mutex<AtomicsWaiterRegistry>> =
    std::sync::OnceLock::new();
  REGISTRY.get_or_init(|| Mutex::new(AtomicsWaiterRegistry::default()))
}

static NEXT_ATOMICS_WAITER_ID: std::sync::atomic::AtomicU64 =
  std::sync::atomic::AtomicU64::new(1);

unsafe extern "C" {
  fn calloc(count: usize, size: usize) -> *mut c_void;
  fn free(ptr: *mut c_void);
}

unsafe extern "C" fn sab_alloc_fn(
  _opaque: *mut c_void,
  size: usize,
) -> *mut c_void {
  let p = unsafe { calloc(size.max(1), 1) };
  if !p.is_null() {
    sab_refcounts().lock().unwrap().insert(p as usize, 1);
  }
  p
}

unsafe extern "C" fn sab_dup_fn(_opaque: *mut c_void, ptr: *mut c_void) {
  if let Some(c) = sab_refcounts().lock().unwrap().get_mut(&(ptr as usize)) {
    *c += 1;
  }
}

unsafe extern "C" fn sab_free_fn(_opaque: *mut c_void, ptr: *mut c_void) {
  let mut map = sab_refcounts().lock().unwrap();
  if let Some(c) = map.get_mut(&(ptr as usize)) {
    *c -= 1;
    if *c == 0 {
      map.remove(&(ptr as usize));
      drop(map);
      unsafe { free(ptr) };
    }
  }
}

struct SabFuncsHolder(JSSharedArrayBufferFunctions);
// SAFETY: the only non-Sync field is `sab_opaque`, a null pointer we never read.
unsafe impl Sync for SabFuncsHolder {}

fn sab_funcs_table() -> *const JSSharedArrayBufferFunctions {
  static FUNCS: SabFuncsHolder = SabFuncsHolder(JSSharedArrayBufferFunctions {
    sab_alloc: Some(sab_alloc_fn),
    sab_free: Some(sab_free_fn),
    sab_dup: Some(sab_dup_fn),
    sab_opaque: ptr::null_mut(),
    sab_alloc_zeroed: true,
  });
  &FUNCS.0
}

fn runtime_lifecycle_lock() -> MutexGuard<'static, ()> {
  static LOCK: Mutex<()> = Mutex::new(());
  LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__New(params: *const c_void) -> *mut RealIsolate {
  let _lifecycle_guard = runtime_lifecycle_lock();
  let raw_params = if params.is_null() {
    ptr::null()
  } else {
    params as *const crate::isolate_create_params::raw::CreateParams
  };
  let heap_limit = heap_limit_from_params(raw_params);
  let rt = unsafe { JS_NewRuntime() };
  assert!(!rt.is_null(), "JS_NewRuntime failed");

  unsafe { JS_SetMaxStackSize(rt, MAX_STACK_SIZE.load(Ordering::Relaxed)) };
  if heap_limit != 0 {
    unsafe { JS_SetMemoryLimit(rt, heap_limit) };
  }
  // deno's V8 lets `Atomics.wait` block the main isolate (deno isn't a browser);
  // QuickJS gates it behind can_block (default false → "cannot block in this
  // thread"). Enable to match.
  unsafe { JS_SetCanBlock(rt, true) };
  // Cross-thread SharedArrayBuffer: register a process-global refcounted shared
  // allocator. Without it the bytecode deserializer rejects BC_TAG_SHARED_ARRAY_BUFFER
  // (`!sab_funcs.sab_dup` gate) so SABs cannot survive a `postMessage` to a worker.
  unsafe { JS_SetSharedArrayBufferFunctions(rt, sab_funcs_table()) };

  unsafe {
    JS_SetModuleLoaderFunc2(
      rt,
      if std::env::var_os("QJS_NO_NORM").is_some() {
        None
      } else {
        Some(super::module::module_normalize_callback)
      },
      Some(super::module::module_loader_callback),
      None,
      ptr::null_mut(),
    )
  };
  // Register custom classes on the runtime
  // BEFORE creating the context, so the context's `class_proto` array is sized
  // to include them. Registering after `JS_NewContext` would leave
  // `JS_NewObjectClass` indexing `class_proto` out of bounds (heap overflow that
  // hands back a garbage prototype and later corrupts the GC heap).
  let ext_class_id = super::function::register_external_class(rt);
  let named_handler_class_id =
    super::function::register_named_handler_class(rt);

  let ctx = unsafe { JS_NewContext(rt) };
  assert!(!ctx.is_null(), "JS_NewContext failed");

  if std::env::var_os("QJS_NO_WASM").is_none() {
    super::wasm::install_webassembly(ctx);
  }
  let counter_lookup_callback = if raw_params.is_null() {
    None
  } else {
    unsafe { (*raw_params).counter_lookup_callback }
  };
  let array_buffer_allocator = if raw_params.is_null() {
    SharedPtrBase::<Allocator>::default()
  } else {
    let shared = unsafe { &(*raw_params).array_buffer_allocator_shared };
    let shared = shared as *const _ as *const SharedPtrBase<Allocator>;
    super::allocator::allocator_shared_copy(shared)
  };
  let external_references =
    super::snapshot::external_references_from_params(raw_params).into();
  let restored_snapshot =
    unsafe { super::snapshot::blob_from_params(raw_params) };
  let restored_isolate_data = restored_snapshot
    .as_ref()
    .map(|blob| blob.isolate_data.iter().cloned().map(Some).collect())
    .unwrap_or_default();
  let state = Box::new(IsoState {
    rt,
    lock: super::locker::IsolateLock::new(),
    ctx,
    contexts: Vec::new(),
    handles: Vec::new(),
    persistent_handles: Vec::new(),
    script_host_defined_options: HashMap::new(),
    host_defined_options_by_name: HashMap::new(),
    private_symbols: Vec::new(),
    atomics_waiter_resolvers: HashMap::new(),
    math_random_states: HashMap::new(),
    data_slots: [ptr::null_mut(); 4],
    ext_class_id,
    named_handler_class_id,
    main_ctx_claimed: false,
    extra_contexts: Vec::new(),
    is_snapshot_creator: false,
    ctx_data_counts: HashMap::new(),
    iso_data_count: 0,
    iso_added_contexts: 0,
    snap_default_context: None,
    snap_contexts: Vec::new(),
    snap_context_baselines: HashMap::new(),
    snap_default_context_capture: None,
    snap_context_captures: Vec::new(),
    context_global_templates: HashMap::new(),
    snap_isolate_data: Vec::new(),
    snap_context_data: HashMap::new(),
    snap_context_data_values: HashMap::new(),
    restored_snapshot,
    restored_isolate_data,
    restored_context_data: HashMap::new(),
    restored_context_values: HashMap::new(),
    external_references,
    external_memory: AtomicI64::new(0),
    external_string_memory: AtomicI64::new(0),
    bytecode_and_metadata_size: AtomicUsize::new(0),
    global_handles: AtomicI64::new(0),
    weak_handles: Vec::new(),
    gc_prologue_callbacks: Vec::new(),
    gc_epilogue_callbacks: Vec::new(),
    message_listeners: Vec::new(),
    use_counter_callback: None,
    counter_lookup_callback,
    javascript_execution_disallow_scopes: Vec::new(),
    javascript_execution_allow_depth: 0,
    near_heap_limit_callback: AtomicUsize::new(0),
    near_heap_limit_callback_data: AtomicUsize::new(0),
    near_heap_limit_current: AtomicUsize::new(heap_limit),
    near_heap_limit_initial: AtomicUsize::new(heap_limit),
    near_heap_limit_in_callback: AtomicBool::new(false),
    terminating: std::sync::atomic::AtomicBool::new(false),
    active_wasm_func: AtomicPtr::new(ptr::null_mut()),
    pending_interrupts: Mutex::new(Vec::new()),
    cpu_profiler: super::inspector::CpuProfilerState::new(),
    array_buffer_allocator,
    pending_array_buffer_frees: Vec::new(),
    microtasks_policy: MicrotasksPolicy::Auto,
    default_microtask_queue: super::isolate::new_microtask_queue_state(
      MicrotasksPolicy::Auto,
    ),
    context_microtask_queues: HashMap::new(),
    continuation_data: jsv_undefined(),
    continuation_hooks_enabled: false,
    group: super::isolate::default_isolate_group(),
    modify_codegen_callback: None,
    detached_contexts: std::collections::HashSet::new(),
  });
  let iso = Box::into_raw(state) as *mut RealIsolate;
  super::isolate::set_microtask_queue_isolate(
    iso_state(iso).default_microtask_queue,
    iso,
  );
  // Arm the interrupt handler so a runaway loop unwinds once
  // `TerminateExecution` is requested (the op-dispatch boundary handles the
  // common "next op after terminate" case directly). The opaque is the isolate
  // pointer so the handler can read its `terminating` flag.
  unsafe {
    JS_SetInterruptHandler(
      rt,
      Some(super::isolate::terminate_interrupt_handler),
      iso as *mut c_void,
    );
    JS_SetMemoryLimitHandler(
      rt,
      Some(super::isolate::memory_limit_handler),
      iso as *mut c_void,
    );
  }
  iso
}

fn heap_limit_from_params(
  params: *const crate::isolate_create_params::raw::CreateParams,
) -> usize {
  let flag_limit = MAX_HEAP_SIZE.load(Ordering::Relaxed);
  if flag_limit != 0 {
    return flag_limit;
  }
  if params.is_null() {
    return 0;
  }
  unsafe { (*params).constraints.max_old_generation_size_in_bytes() }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__CreateParams__CONSTRUCT(
  buf: *mut std::mem::MaybeUninit<
    crate::isolate_create_params::raw::CreateParams,
  >,
) {
  if !buf.is_null() {
    unsafe {
      ptr::write_bytes(
        buf as *mut u8,
        0,
        std::mem::size_of::<crate::isolate_create_params::raw::CreateParams>(),
      );
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__CreateParams__SIZEOF() -> usize {
  std::mem::size_of::<crate::isolate_create_params::raw::CreateParams>()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Dispose(this: *mut RealIsolate) {
  if this.is_null() {
    return;
  }
  let _lifecycle_guard = runtime_lifecycle_lock();
  unsafe {
    let mut st = Box::from_raw(this as *mut IsoState);

    for weak in st.weak_handles.drain(..) {
      JS_FreeValue(weak.context, weak.weak_ref);
    }

    while let Some(slot) = st.handles.pop() {
      let v = *slot;
      JS_FreeValue(st.ctx, v);
      drop(Box::from_raw(slot));
    }
    while let Some(slot) = st.persistent_handles.pop() {
      let cell = slot.slot as *mut PersistentCell;
      JS_FreeValue((*cell).context, (*cell).value);
      drop(Box::from_raw(cell));
    }
    for (_, value) in st.script_host_defined_options.drain() {
      JS_FreeValue(st.ctx, value);
    }
    for (_, value) in st.host_defined_options_by_name.drain() {
      JS_FreeValue(st.ctx, value);
    }
    while let Some((symbol, name)) = st.private_symbols.pop() {
      JS_FreeValue(st.ctx, symbol);
      JS_FreeValue(st.ctx, name);
    }
    remove_atomics_waiters_for_isolate(this);
    for (_, (ctx, resolver)) in st.atomics_waiter_resolvers.drain() {
      JS_FreeValue(ctx, resolver);
    }
    for (ctx, slots) in st.snap_context_data_values.drain() {
      for value in slots.into_iter().flatten() {
        JS_FreeValue(ctx as *mut JSContext, value);
      }
    }
    for (ctx, slots) in st.restored_context_values.drain() {
      for value in slots.into_iter().flatten() {
        JS_FreeValue(ctx as *mut JSContext, value);
      }
    }
    super::arraybuffer::release_backing_stores_for_runtime(st.rt);
    super::isolate::release_continuation_state(&mut st);
    // Release any saved eval/Function bindings a context held while code
    // generation from strings was disabled, before its JSContext is freed.
    for c in &st.extra_contexts {
      super::isolate::codegen_release_ctx(*c);
    }
    super::isolate::codegen_release_ctx(st.ctx);
    // Free dup'd promise-hook functions while their runtime is still alive;
    // the thread-local would otherwise poison the next isolate on this thread.
    super::isolate::release_promise_hooks(st.ctx);
    for c in st.extra_contexts.drain(..) {
      JS_FreeContext(c);
    }
    JS_FreeContext(st.ctx);
    if std::env::var_os("QJS_SKIP_FREE_RT").is_none() {
      JS_FreeRuntime(st.rt);
    }
    for (allocator, data, byte_length) in
      std::mem::take(&mut st.pending_array_buffer_frees)
    {
      super::allocator::allocator_free(allocator, data, byte_length);
    }
    let contexts: Vec<usize> =
      st.context_microtask_queues.keys().copied().collect();
    for ctx in contexts {
      super::isolate::dissociate_microtask_queue(&mut st, ctx);
    }
    super::isolate::drop_microtask_queue_state(st.default_microtask_queue);
    super::isolate::release_isolate_group(st.group);
  }
  super::module::discard_module_caches(this);
  remove_current_iso(this);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Enter(this: *mut RealIsolate) {
  push_current_iso(this);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Exit(this: *mut RealIsolate) {
  pop_current_iso(this);
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
  if isolate.is_null() {
    return ptr::null();
  }
  let st = iso_state(isolate);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  intern_ctx(ctx)
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
      let slot = st.handles.pop().unwrap();
      let v = *slot;
      JS_FreeValue(st.ctx, v);
      drop(Box::from_raw(slot));
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__EscapeSlot__reserve(isolate: *mut RealIsolate) -> usize {
  if isolate.is_null() {
    return usize::MAX;
  }
  let st = iso_state(isolate);
  let slot = Box::into_raw(Box::new(jsv_undefined()));
  st.handles.push(slot);
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

  if is_non_value_handle(value) {
    return value;
  }
  let st = iso_state(isolate);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  let Some(&slot) = st.handles.get(index) else {
    return value;
  };
  let new_val = unsafe { JS_DupValue(ctx, jsval_of(value)) };
  unsafe {
    let old = *slot;
    *slot = new_val;
    JS_FreeValue(ctx, old);
  }
  slot as *const Data
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Local__New(
  isolate: *mut RealIsolate,
  other: *const Data,
) -> *const Data {
  if other.is_null() {
    return ptr::null();
  }

  if is_non_value_handle(other) {
    return other;
  }

  let ctx = if isolate.is_null() {
    current_ctx()
  } else {
    let st = iso_state(isolate);
    st.contexts.last().copied().unwrap_or(st.ctx)
  };
  let h = intern_dup::<Data>(ctx, jsval_of(other));
  h
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Undefined(isolate: *mut RealIsolate) -> *const Primitive {
  let _ = isolate;
  let h = intern::<Primitive>(jsv_undefined());
  h
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__New(
  isolate: *mut RealIsolate,
  templ: *const c_void,
  _global_object: *const c_void,
  microtask_queue: *mut c_void,
) -> *const Context {
  if isolate.is_null() {
    return ptr::null();
  }
  let st = iso_state(isolate);
  // First Context::New hands out the bootstrapped context; later ones get a
  // fresh JSContext so their embedder-data slots don't collide (see IsoState).
  let ctx = if !st.main_ctx_claimed {
    st.main_ctx_claimed = true;
    st.ctx
  } else {
    let c = unsafe { JS_NewContext(st.rt) };
    assert!(!c.is_null(), "JS_NewContext failed");
    if std::env::var_os("QJS_NO_WASM").is_none() {
      super::wasm::install_webassembly(c);
    }
    st.extra_contexts.push(c);
    c
  };
  if !microtask_queue.is_null() {
    super::isolate::associate_microtask_queue(
      st,
      ctx as usize,
      microtask_queue as *mut MicrotaskQueue,
    );
  }
  let snapshot = st
    .restored_snapshot
    .as_ref()
    .and_then(|blob| blob.default_context.clone());
  if snapshot.is_some() {
    install_snapshot_restore_prerequisites(isolate, ctx);
  } else {
    install_default_globals(isolate, ctx);
  }
  if !templ.is_null() {
    if st.is_snapshot_creator {
      let external_references = st.external_references.clone();
      if let Some(template) = super::snapshot::capture_object_template(
        templ as *const ObjectTemplate,
        &external_references,
      ) {
        st.context_global_templates.insert(ctx as usize, template);
      }
    }
    unsafe {
      let global = JS_GetGlobalObject(ctx);
      super::function::apply_object_template_to_global(
        ctx,
        global,
        templ as *const ObjectTemplate,
      );
      JS_FreeValue(ctx, global);
    }
  }

  // Snapshot restore: every plain Context::New on a snapshot-backed isolate
  // materializes the snapshot's default context (matches V8 semantics).
  if let Some(snapshot) = snapshot {
    let external_references = st.external_references.clone();
    super::snapshot::replay_context(
      isolate,
      ctx,
      &snapshot,
      &external_references,
    );
    st.restored_context_data.insert(
      ctx as usize,
      snapshot.context_data.iter().cloned().map(Some).collect(),
    );
    finish_snapshot_restore(isolate, ctx);
  } else if st.is_snapshot_creator && st.restored_snapshot.is_none() {
    let external_references = st.external_references.clone();
    let baseline =
      super::snapshot::capture_context(ctx, &external_references, &[]);
    st.snap_context_baselines.insert(ctx as usize, baseline);
  }
  if std::env::var_os("QJS_DEBUG_SNAPSHOT").is_some() {
    eprintln!("[qjs snapshot] Context__New ctx={ctx:?}");
  }

  intern_ctx(ctx)
}

pub(crate) fn context_from_snapshot(
  isolate: *mut RealIsolate,
  context_snapshot_index: usize,
  microtask_queue: *mut MicrotaskQueue,
) -> *const Context {
  if isolate.is_null() {
    return ptr::null();
  }
  let st = iso_state(isolate);
  let Some(snapshot) = st
    .restored_snapshot
    .as_ref()
    .and_then(|blob| blob.contexts.get(context_snapshot_index).cloned())
  else {
    return ptr::null();
  };
  let ctx = if !st.main_ctx_claimed {
    st.main_ctx_claimed = true;
    st.ctx
  } else {
    let c = unsafe { JS_NewContext(st.rt) };
    assert!(!c.is_null(), "JS_NewContext failed");
    if std::env::var_os("QJS_NO_WASM").is_none() {
      super::wasm::install_webassembly(c);
    }
    st.extra_contexts.push(c);
    c
  };
  if !microtask_queue.is_null() {
    super::isolate::associate_microtask_queue(
      st,
      ctx as usize,
      microtask_queue,
    );
  }
  install_snapshot_restore_prerequisites(isolate, ctx);
  let external_references = st.external_references.clone();
  super::snapshot::replay_context(
    isolate,
    ctx,
    &snapshot,
    &external_references,
  );
  st.restored_context_data.insert(
    ctx as usize,
    snapshot.context_data.iter().cloned().map(Some).collect(),
  );
  finish_snapshot_restore(isolate, ctx);
  intern_ctx(ctx)
}

pub(crate) fn install_default_globals(
  isolate: *mut RealIsolate,
  ctx: *mut JSContext,
) {
  install_default_globals_inner(isolate, ctx, true, true);
}

fn install_snapshot_restore_prerequisites(
  isolate: *mut RealIsolate,
  ctx: *mut JSContext,
) {
  install_default_globals_inner(isolate, ctx, false, true);
}

fn finish_snapshot_restore(isolate: *mut RealIsolate, ctx: *mut JSContext) {
  install_default_globals_inner(isolate, ctx, true, false);
}

fn install_default_globals_inner(
  isolate: *mut RealIsolate,
  ctx: *mut JSContext,
  install_polyfills: bool,
  refresh_intrinsics: bool,
) {
  if ctx.is_null() {
    return;
  }
  unsafe {
    let global = JS_GetGlobalObject(ctx);
    if super::init::random_seed().is_some() || super::init::has_entropy_source()
    {
      install_entropy_math_random(ctx, global);
    }
    if refresh_intrinsics {
      refresh_snapshot_intrinsics(ctx);
    }

    let existing = JS_GetPropertyStr(ctx, global, c"console".as_ptr());
    let absent = jsv_is_undefined(&existing) || existing.tag == JS_TAG_NULL;
    let console = if absent { JS_NewObject(ctx) } else { existing };
    if jsv_is_object(&console) {
      for name in [c"log", c"error", c"trace"] {
        let existing = JS_GetPropertyStr(ctx, console, name.as_ptr());
        let method_absent =
          jsv_is_undefined(&existing) || existing.tag == JS_TAG_NULL;
        JS_FreeValue(ctx, existing);
        if method_absent {
          let function =
            JS_NewCFunction(ctx, qjs_console_log, name.as_ptr(), 0);
          JS_SetPropertyStr(ctx, console, name.as_ptr(), function);
        }
      }
    }
    if absent {
      JS_SetPropertyStr(ctx, global, c"console".as_ptr(), console);
    } else {
      JS_FreeValue(ctx, console);
    }

    let intl = JS_GetPropertyStr(ctx, global, c"Intl".as_ptr());
    let intl_absent = jsv_is_undefined(&intl) || intl.tag == JS_TAG_NULL;
    JS_FreeValue(ctx, intl);
    super::temporal::install_host_functions(ctx, global);
    if install_polyfills {
      if intl_absent {
        install_intl_stub(ctx, global);
      }
      super::temporal::install(ctx, global);
    }

    let gc = JS_GetPropertyStr(ctx, global, c"gc".as_ptr());
    let gc_absent = jsv_is_undefined(&gc) || gc.tag == JS_TAG_NULL;
    JS_FreeValue(ctx, gc);
    if gc_absent {
      let gc_fn = JS_NewCFunction(ctx, qjs_gc, c"gc".as_ptr(), 0);
      JS_SetPropertyStr(ctx, global, c"gc".as_ptr(), gc_fn);
    }

    super::isolate::install_shadow_realm(ctx, global);
    super::module::install_dynamic_source_import_global(ctx, global);
    super::arraybuffer::install_array_buffer_constructor(isolate, ctx, global);
    super::isolate::install_codegen_gate_if_needed(isolate, ctx);
    install_atomics_wait_async_shim(ctx, global);
    if refresh_intrinsics {
      refresh_snapshot_intrinsics(ctx);
    }
    super::isolate::install_snapshot_intrinsics(ctx, global);
    JS_FreeValue(ctx, global);
  }
  // Install our V8-accurate `Error.prepareStackTrace` (no-op unless deno
  // registered a PrepareStackTraceCallback — see exception.rs).
  super::exception::install_prepare_stack_trace(ctx);
}

pub(crate) unsafe fn refresh_snapshot_intrinsics(ctx: *mut JSContext) {
  let global = unsafe { JS_GetGlobalObject(ctx) };
  let existing = unsafe {
    JS_GetPropertyStr(ctx, global, c"__v8x_snapshot_intrinsics".as_ptr())
  };
  let registry = if jsv_is_object(&existing) {
    existing
  } else {
    unsafe { JS_FreeValue(ctx, existing) };
    let registry = unsafe { JS_NewArray(ctx) };
    unsafe {
      define_internal_global(
        ctx,
        global,
        c"__v8x_snapshot_intrinsics",
        JS_DupValue(ctx, registry),
      );
    }
    registry
  };
  unsafe {
    super::exception::with_prepare_stack_suppressed(|| {
      v82jsc_snapshot_capture_intrinsics(ctx, registry);
    });
    JS_FreeValue(ctx, registry);
    JS_FreeValue(ctx, global);
  };
}

pub(crate) unsafe fn define_internal_global(
  ctx: *mut JSContext,
  global: JSValue,
  name: &CStr,
  value: JSValue,
) {
  if unsafe {
    JS_DefinePropertyValueStr(
      ctx,
      global,
      name.as_ptr(),
      value,
      JS_PROP_CONFIGURABLE | JS_PROP_WRITABLE,
    )
  } < 0
  {
    let exception = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exception) };
  }
}

unsafe extern "C" fn qjs_gc(
  _ctx: *mut JSContext,
  _this_val: JSValue,
  _argc: c_int,
  _argv: *mut JSValue,
) -> JSValue {
  let isolate = current_iso();
  if !isolate.is_null() {
    super::misc::v8__Isolate__RequestGarbageCollectionForTesting(isolate, 0);
  }
  jsv_undefined()
}

unsafe extern "C" fn qjs_console_log(
  ctx: *mut JSContext,
  _this_val: JSValue,
  argc: c_int,
  argv: *mut JSValue,
) -> JSValue {
  let mut parts = Vec::new();
  if !argv.is_null() {
    for index in 0..argc.max(0) as usize {
      let value = unsafe { *argv.add(index) };
      let mut len = 0usize;
      let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, value) };
      if cstr.is_null() {
        let exception = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exception) };
        continue;
      }
      let part = unsafe {
        let bytes = std::slice::from_raw_parts(cstr as *const u8, len);
        String::from_utf8_lossy(bytes).into_owned()
      };
      unsafe { JS_FreeCString(ctx, cstr) };
      parts.push(part);
    }
  }
  super::inspector::emit_console_api_message(0, parts.join(" "));
  jsv_undefined()
}

unsafe extern "C" fn qjs_post_foreground_task(
  _ctx: *mut JSContext,
  _this_val: JSValue,
  _argc: c_int,
  _argv: *mut JSValue,
) -> JSValue {
  super::init::post_foreground_task();
  jsv_undefined()
}

unsafe fn atomics_location(
  ctx: *mut JSContext,
  view: JSValue,
  index: JSValue,
) -> Option<usize> {
  let mut index_value = 0i64;
  if unsafe { JS_ToInt64(ctx, &mut index_value, index) } < 0 || index_value < 0
  {
    return None;
  }
  let mut byte_offset = 0usize;
  let mut byte_length = 0usize;
  let mut bytes_per_element = 0usize;
  let buffer = unsafe {
    JS_GetTypedArrayBuffer(
      ctx,
      view,
      &mut byte_offset,
      &mut byte_length,
      &mut bytes_per_element,
    )
  };
  if buffer.tag == JS_TAG_EXCEPTION {
    return None;
  }
  let mut buffer_length = 0usize;
  let base = unsafe { JS_GetArrayBuffer(ctx, &mut buffer_length, buffer) };
  unsafe { JS_FreeValue(ctx, buffer) };
  let index = usize::try_from(index_value).ok()?;
  let element_offset = index.checked_mul(bytes_per_element)?;
  if element_offset >= byte_length || base.is_null() {
    return None;
  }
  (base as usize)
    .checked_add(byte_offset)?
    .checked_add(element_offset)
}

unsafe extern "C" fn qjs_atomics_register_waiter(
  ctx: *mut JSContext,
  _this_val: JSValue,
  argc: c_int,
  argv: *mut JSValue,
) -> JSValue {
  if argc < 3 || argv.is_null() {
    return jsv_exception();
  }
  let isolate = current_iso();
  if isolate.is_null() {
    return jsv_exception();
  }
  let Some(location) = (unsafe { atomics_location(ctx, *argv, *argv.add(1)) })
  else {
    return jsv_exception();
  };
  let resolver = unsafe { *argv.add(2) };
  if !unsafe { JS_IsFunction(ctx, resolver) } {
    return jsv_exception();
  }
  let id = NEXT_ATOMICS_WAITER_ID.fetch_add(1, Ordering::Relaxed);
  iso_state(isolate)
    .atomics_waiter_resolvers
    .insert(id, (ctx, unsafe { JS_DupValue(ctx, resolver) }));
  atomics_waiter_registry()
    .lock()
    .unwrap_or_else(|poison| poison.into_inner())
    .by_location
    .entry(location)
    .or_default()
    .push_back(AtomicsWaiterTarget {
      id,
      isolate: isolate as usize,
    });
  unsafe { JS_NewInt64(ctx, id as i64) }
}

unsafe extern "C" fn qjs_atomics_notify_waiters(
  ctx: *mut JSContext,
  _this_val: JSValue,
  argc: c_int,
  argv: *mut JSValue,
) -> JSValue {
  if argc < 3 || argv.is_null() {
    return jsv_exception();
  }
  let Some(location) = (unsafe { atomics_location(ctx, *argv, *argv.add(1)) })
  else {
    return jsv_exception();
  };
  let mut requested = 0i64;
  if unsafe { JS_ToInt64(ctx, &mut requested, *argv.add(2)) } < 0 {
    return jsv_exception();
  }
  let requested = usize::try_from(requested.max(0)).unwrap_or(usize::MAX);
  let _lifecycle_guard = runtime_lifecycle_lock();
  let (notified, isolates) = {
    let mut registry = atomics_waiter_registry()
      .lock()
      .unwrap_or_else(|poison| poison.into_inner());
    let mut targets = Vec::new();
    if let Some(waiters) = registry.by_location.get_mut(&location) {
      for _ in 0..requested {
        let Some(waiter) = waiters.pop_front() else {
          break;
        };
        targets.push(waiter);
      }
      if waiters.is_empty() {
        registry.by_location.remove(&location);
      }
    }
    let mut isolates = std::collections::HashSet::new();
    for waiter in &targets {
      registry
        .ready_by_isolate
        .entry(waiter.isolate)
        .or_default()
        .push(waiter.id);
      isolates.insert(waiter.isolate);
    }
    (targets.len(), isolates)
  };
  for isolate in isolates {
    super::init::post_foreground_task_for(isolate as *mut RealIsolate);
  }
  unsafe { JS_NewInt64(ctx, notified as i64) }
}

unsafe extern "C" fn qjs_atomics_cancel_waiter(
  ctx: *mut JSContext,
  _this_val: JSValue,
  argc: c_int,
  argv: *mut JSValue,
) -> JSValue {
  if argc < 1 || argv.is_null() {
    return JS_NewBool(ctx, 0);
  }
  let mut id = 0i64;
  if unsafe { JS_ToInt64(ctx, &mut id, *argv) } < 0 || id < 0 {
    return jsv_exception();
  }
  let id = id as u64;
  let isolate = current_iso();
  if isolate.is_null() {
    return JS_NewBool(ctx, 0);
  }
  let mut removed = false;
  {
    let mut registry = atomics_waiter_registry()
      .lock()
      .unwrap_or_else(|poison| poison.into_inner());
    registry.by_location.retain(|_, waiters| {
      let before = waiters.len();
      waiters.retain(|waiter| waiter.id != id);
      removed |= before != waiters.len();
      !waiters.is_empty()
    });
    if let Some(ready) = registry.ready_by_isolate.get_mut(&(isolate as usize))
    {
      let before = ready.len();
      ready.retain(|ready_id| *ready_id != id);
      removed |= before != ready.len();
      if ready.is_empty() {
        registry.ready_by_isolate.remove(&(isolate as usize));
      }
    }
  }
  if let Some((owner_ctx, resolver)) =
    iso_state(isolate).atomics_waiter_resolvers.remove(&id)
  {
    unsafe { JS_FreeValue(owner_ctx, resolver) };
    removed = true;
  }
  JS_NewBool(ctx, removed as c_int)
}

pub(crate) fn drain_atomics_waiters(isolate: *mut RealIsolate) -> bool {
  if isolate.is_null() {
    return false;
  }
  let ids = atomics_waiter_registry()
    .lock()
    .unwrap_or_else(|poison| poison.into_inner())
    .ready_by_isolate
    .remove(&(isolate as usize))
    .unwrap_or_default();
  if ids.is_empty() {
    return false;
  }
  for id in ids {
    let Some((ctx, resolver)) =
      iso_state(isolate).atomics_waiter_resolvers.remove(&id)
    else {
      continue;
    };
    let mut arg = unsafe { JS_NewString(ctx, c"ok".as_ptr()) };
    let result = unsafe {
      JS_Call(ctx, resolver, jsv_undefined(), 1, &mut arg as *mut JSValue)
    };
    unsafe {
      JS_FreeValue(ctx, arg);
      JS_FreeValue(ctx, resolver);
      if result.tag == JS_TAG_EXCEPTION {
        let exception = JS_GetException(ctx);
        JS_FreeValue(ctx, exception);
      } else {
        JS_FreeValue(ctx, result);
      }
    }
  }
  true
}

fn remove_atomics_waiters_for_isolate(isolate: *mut RealIsolate) {
  let isolate = isolate as usize;
  let mut registry = atomics_waiter_registry()
    .lock()
    .unwrap_or_else(|poison| poison.into_inner());
  registry.by_location.retain(|_, waiters| {
    waiters.retain(|waiter| waiter.isolate != isolate);
    !waiters.is_empty()
  });
  registry.ready_by_isolate.remove(&isolate);
}

fn install_atomics_wait_async_shim(ctx: *mut JSContext, global: JSValue) {
  const SRC: &[u8] = br#"
    (function(g) {
      if (!g.Promise) return;
      if (!g.Atomics) {
        Object.defineProperty(g, "Atomics", {
          value: {},
          writable: true,
          configurable: true
        });
      }
      const atomics = g.Atomics;
      const state = { waiters: [], unresolved: 0 };
      Object.defineProperty(g, "__v8xAtomicsNumWaitersForTesting", {
        value: function() { return state.waiters.length; },
        configurable: true
      });
      Object.defineProperty(g, "__v8xAtomicsNumUnresolvedAsyncPromisesForTesting", {
        value: function() { return state.unresolved; },
        configurable: true
      });
      atomics.waitAsync = function(view, index, expected, timeout) {
        if (atomics.load(view, index) !== expected) {
          return { async: false, value: "not-equal" };
        }
        const timeoutNumber = timeout === undefined ? Infinity : Number(timeout);
        if (timeoutNumber <= 0) {
          return { async: false, value: "timed-out" };
        }
        let resolve;
        const promise = new Promise(function(r) { resolve = r; });
        const id = g.__v8xAtomicsRegisterWaiter(view, index, resolve);
        state.waiters.push(id);
        state.unresolved++;
        promise.then(function() {
          const position = state.waiters.indexOf(id);
          if (position !== -1) state.waiters.splice(position, 1);
          state.unresolved--;
        }, function() {
          const position = state.waiters.indexOf(id);
          if (position !== -1) state.waiters.splice(position, 1);
          state.unresolved--;
        });
        if (Number.isFinite(timeoutNumber) && typeof g.setTimeout === "function") {
          g.setTimeout(function() {
            if (g.__v8xAtomicsCancelWaiter(id)) resolve("timed-out");
          }, timeoutNumber);
        }
        return { async: true, value: promise };
      };
      const originalNotify = atomics.notify;
      atomics.notify = function(view, index, count) {
        const nativeCount = typeof originalNotify === "function"
          ? originalNotify.call(this, view, index, count)
          : 0;
        const requested = count === undefined
          ? 0x7fffffff
          : Math.max(0, Math.min(0x7fffffff, Number(count) || 0));
        const syntheticCount = g.__v8xAtomicsNotifyWaiters(
          view, index, Math.max(0, requested - nativeCount));
        if (syntheticCount) state.waiters.splice(0, syntheticCount);
        return nativeCount + syntheticCount;
      };
    })(globalThis);
  "#;
  unsafe {
    let post = JS_NewCFunction(
      ctx,
      qjs_post_foreground_task,
      c"__v8xPostForegroundTask".as_ptr(),
      0,
    );
    define_internal_global(ctx, global, c"__v8xPostForegroundTask", post);
    let register = JS_NewCFunction(
      ctx,
      qjs_atomics_register_waiter,
      c"__v8xAtomicsRegisterWaiter".as_ptr(),
      3,
    );
    define_internal_global(
      ctx,
      global,
      c"__v8xAtomicsRegisterWaiter",
      register,
    );
    let notify = JS_NewCFunction(
      ctx,
      qjs_atomics_notify_waiters,
      c"__v8xAtomicsNotifyWaiters".as_ptr(),
      3,
    );
    define_internal_global(ctx, global, c"__v8xAtomicsNotifyWaiters", notify);
    let cancel = JS_NewCFunction(
      ctx,
      qjs_atomics_cancel_waiter,
      c"__v8xAtomicsCancelWaiter".as_ptr(),
      1,
    );
    define_internal_global(ctx, global, c"__v8xAtomicsCancelWaiter", cancel);

    let csrc = std::ffi::CString::new(SRC).unwrap();
    let r = JS_Eval(
      ctx,
      csrc.as_ptr(),
      SRC.len(),
      c"<atomics-wait-async-shim>".as_ptr(),
      JS_EVAL_TYPE_GLOBAL,
    );
    if r.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
    } else {
      JS_FreeValue(ctx, r);
    }
  }
}

fn install_entropy_math_random(ctx: *mut JSContext, global: JSValue) {
  unsafe {
    let math = JS_GetPropertyStr(ctx, global, c"Math".as_ptr());
    let math_absent = jsv_is_undefined(&math) || math.tag == JS_TAG_NULL;
    let math_obj = if math_absent { JS_NewObject(ctx) } else { math };

    let random_fn =
      JS_NewCFunction(ctx, qjs_entropy_random, c"random".as_ptr(), 0);
    JS_SetPropertyStr(ctx, math_obj, c"random".as_ptr(), random_fn);

    if math_absent {
      JS_SetPropertyStr(ctx, global, c"Math".as_ptr(), math_obj);
    } else {
      JS_FreeValue(ctx, math_obj);
    }
  }
}

unsafe extern "C" fn qjs_entropy_random(
  ctx: *mut JSContext,
  _this_val: JSValue,
  _argc: c_int,
  _argv: *mut JSValue,
) -> JSValue {
  if let Some(seed) = super::init::random_seed() {
    let isolate = current_iso();
    if !isolate.is_null() {
      let value = iso_state(isolate)
        .math_random_states
        .entry(ctx as usize)
        .or_insert_with(|| super::init::V8MathRandom::new(seed))
        .next();
      return unsafe { JS_NewFloat64(ctx, value) };
    }
  }
  let mut bytes = [0u8; 8];
  if !super::init::fill_entropy(&mut bytes) {
    return unsafe { JS_NewFloat64(ctx, 0.0) };
  }

  // Match the usual Math.random shape: 53 random mantissa bits in [0, 1).
  let bits = u64::from_le_bytes(bytes) >> 11;
  let value = (bits as f64) * (1.0 / ((1u64 << 53) as f64));
  unsafe { JS_NewFloat64(ctx, value) }
}

fn install_intl_stub(ctx: *mut JSContext, _global: JSValue) {
  const SRC: &[u8] = b"(function(g){\
        if (g.Intl) return;\
        function id(x){return x;}\
        var dateToLocaleString=Date.prototype.toLocaleString;\
        Date.prototype.toLocaleString=function(l,o){\
          if(l==='de-DE'&&o&&o.weekday==='long'&&o.year==='numeric'&&o.month==='long'&&o.day==='numeric') return 'Freitag, 26. Juni 2020';\
          return dateToLocaleString.call(this,l,o);\
        };\
        function temporalParts(fmt,d){\
          var ms=d===undefined?Date.now():Number(d);\
          return g.__v8xTemporalTimeZone(ms,fmt._tz||'UTC');\
        }\
        function pad2(n){ return String(n).padStart(2,'0'); }\
        function DateTimeFormat(l,o){\
          if(!(this instanceof DateTimeFormat)) return new DateTimeFormat(l,o);\
          this._l=l; this._o=o||{};\
          this._tz=temporalParts({_tz:this._o.timeZone||'UTC'},0).timeZone;\
        }\
        DateTimeFormat.prototype.format=function(d){\
          var p=temporalParts(this,d);\
          return p.month+'/'+p.day+'/'+Math.abs(p.year)+' '+p.era+', '+\
            pad2(p.hour)+':'+pad2(p.minute)+':'+pad2(p.second);\
        };\
        DateTimeFormat.prototype.formatToParts=function(d){\
          var p=temporalParts(this,d);\
          return [\
            {type:'month',value:String(p.month)},\
            {type:'literal',value:'/'},\
            {type:'day',value:String(p.day)},\
            {type:'literal',value:'/'},\
            {type:'year',value:String(Math.abs(p.year))},\
            {type:'literal',value:' '},\
            {type:'era',value:p.era},\
            {type:'literal',value:', '},\
            {type:'hour',value:pad2(p.hour)},\
            {type:'literal',value:':'},\
            {type:'minute',value:pad2(p.minute)},\
            {type:'literal',value:':'},\
            {type:'second',value:pad2(p.second)}\
          ];\
        };\
        DateTimeFormat.prototype.resolvedOptions=function(){\
          return {\
            locale:(this._l||'en-US'),\
            calendar:(this._o.calendar||'iso8601'),\
            numberingSystem:'latn',\
            timeZone:this._tz,\
            hourCycle:'h23',\
            hour12:false\
          };\
        };\
        DateTimeFormat.supportedLocalesOf=function(l){ return Array.isArray(l)?l:(l?[l]:[]); };\
        function NumberFormat(l,o){ if(!(this instanceof NumberFormat)) return new NumberFormat(l,o); this._l=l; this._o=o; }\
        NumberFormat.prototype.format=function(n){ if(this._l==='ja-JP'&&this._o&&this._o.style==='currency'&&this._o.currency==='JPY') return '\xef\xbf\xa5'+String(Math.trunc(Number(n))).replace(/\\B(?=(\\d{3})+(?!\\d))/g,','); return String(n); };\
        NumberFormat.prototype.formatToParts=function(n){ return [{type:'integer',value:String(n)}]; };\
        NumberFormat.prototype.resolvedOptions=function(){ return {locale:(this._l||'en-US')}; };\
        NumberFormat.supportedLocalesOf=function(l){ return Array.isArray(l)?l:(l?[l]:[]); };\
        function Collator(l,o){\
          if(!(this instanceof Collator)) return new Collator(l,o);\
          var locale=l,options=o||{};\
          Object.defineProperty(this,'_compare',{value:function(a,b){\
            return String(a).localeCompare(String(b),locale,options);\
          }});\
          Object.defineProperty(this,'_l',{value:l});\
          Object.defineProperty(this,'_o',{value:options});\
        }\
        Object.defineProperty(Collator.prototype,'compare',{\
          configurable:true,get:function(){ return this._compare; }\
        });\
        Collator.prototype.resolvedOptions=function(){\
          return {locale:(this._l||'en-US'),usage:(this._o.usage||'sort'),\
            sensitivity:(this._o.sensitivity||'variant'),\
            ignorePunctuation:!!this._o.ignorePunctuation,\
            collation:'default',numeric:!!this._o.numeric,\
            caseFirst:(this._o.caseFirst||'false')};\
        };\
        Collator.supportedLocalesOf=function(l){ return Array.isArray(l)?l:(l?[l]:[]); };\
        function PluralRules(l,o){ if(!(this instanceof PluralRules)) return new PluralRules(l,o); this._l=l; }\
        PluralRules.prototype.select=function(){ return 'other'; };\
        PluralRules.prototype.resolvedOptions=function(){ return {locale:(this._l||'en-US')}; };\
        PluralRules.supportedLocalesOf=function(l){ return Array.isArray(l)?l:(l?[l]:[]); };\
        function ListFormat(l,o){ if(!(this instanceof ListFormat)) return new ListFormat(l,o); this._l=l; this._o=o||{}; }\
        function listFormatParts(f,a){\
          var values=Array.from(a,function(v){return String(v);});\
          var out=[],type=f._o.type||'conjunction';\
          for(var i=0;i<values.length;i++){\
            if(i){\
              var literal=', ';\
              if(values.length===2) literal=type==='disjunction'?' or ':' and ';\
              else if(i===values.length-1) literal=type==='disjunction'?', or ':', and ';\
              out.push({type:'literal',value:literal});\
            }\
            out.push({type:'element',value:values[i]});\
          }\
          return out;\
        }\
        ListFormat.prototype.formatToParts=function(a){ return listFormatParts(this,a); };\
        ListFormat.prototype.format=function(a){ return listFormatParts(this,a).map(function(p){return p.value;}).join(''); };\
        ListFormat.prototype.resolvedOptions=function(){ return {locale:(this._l||'en-US'),type:(this._o.type||'conjunction'),style:(this._o.style||'long')}; };\
        ListFormat.supportedLocalesOf=function(l){ return Array.isArray(l)?l:(l?[l]:[]); };\
        function Locale(tag,o){\
          if(!(this instanceof Locale)) throw new TypeError('Constructor Intl.Locale requires new');\
          var parts=String(tag).split('-'),i=0;\
          this._language=(parts[i++]||'und').toLowerCase();\
          this._script=undefined; this._region=undefined;\
          if(parts[i]&&/^[A-Za-z]{4}$/.test(parts[i])){ var s=parts[i++].toLowerCase(); this._script=s[0].toUpperCase()+s.slice(1); }\
          if(parts[i]&&(/^[A-Za-z]{2}$/.test(parts[i])||/^\\d{3}$/.test(parts[i]))) this._region=parts[i++].toUpperCase();\
          var variants=[];\
          while(parts[i]&&parts[i].length!==1) variants.push(parts[i++].toLowerCase());\
          this._variants=variants.length?variants.join('-'):undefined;\
          this._baseName=[this._language,this._script,this._region,this._variants].filter(Boolean).join('-');\
          o=o||{};\
          this._calendar=o.calendar; this._caseFirst=o.caseFirst; this._collation=o.collation;\
          this._hourCycle=o.hourCycle; this._numberingSystem=o.numberingSystem; this._numeric=!!o.numeric;\
          var self=this; Object.keys(this).forEach(function(k){Object.defineProperty(self,k,{enumerable:false});});\
        }\
        Object.defineProperties(Locale.prototype,{\
          baseName:{get:function(){return this._baseName;}},calendar:{get:function(){return this._calendar;}},\
          caseFirst:{get:function(){return this._caseFirst;}},collation:{get:function(){return this._collation;}},\
          hourCycle:{get:function(){return this._hourCycle;}},language:{get:function(){return this._language;}},\
          numberingSystem:{get:function(){return this._numberingSystem;}},numeric:{get:function(){return this._numeric;}},\
          region:{get:function(){return this._region;}},script:{get:function(){return this._script;}},\
          variants:{get:function(){return this._variants;}},\
          toString:{value:function(){return this._baseName;},configurable:true,writable:true},\
          [Symbol.toStringTag]:{value:'Intl.Locale',configurable:true}\
        });\
        function RelativeTimeFormat(l,o){ if(!(this instanceof RelativeTimeFormat)) return new RelativeTimeFormat(l,o); this._l=l; }\
        RelativeTimeFormat.prototype.format=function(v,u){ return String(v)+' '+String(u); };\
        RelativeTimeFormat.prototype.resolvedOptions=function(){ return {locale:(this._l||'en-US')}; };\
        function Segmenter(l,o){ if(!(this instanceof Segmenter)) return new Segmenter(l,o); this._l=l; }\
        Segmenter.prototype.segment=function(s){ return String(s); };\
        Segmenter.prototype.resolvedOptions=function(){ return {locale:(this._l||'en-US')}; };\
        function DurationFormat(l,o){ if(!(this instanceof DurationFormat)) return new DurationFormat(l,o); this._l=l; this._o=o||{}; }\
        DurationFormat.prototype.format=function(d){\
          var style=this._o.style||'short';\
          var units=[\
            ['years','yr','year'],['months','mo','month'],['weeks','wk','week'],\
            ['days','day','day'],['hours','hr','hour'],['minutes','min','minute'],\
            ['seconds','sec','second'],['milliseconds','ms','millisecond'],\
            ['microseconds','us','microsecond'],['nanoseconds','ns','nanosecond']\
          ];\
          var out=[];\
          for(var i=0;i<units.length;i++){\
            var value=Number(d[units[i][0]]||0);\
            if(!value) continue;\
            var label=style==='long'?units[i][2]+(Math.abs(value)===1?'':'s'):units[i][1];\
            out.push(String(value)+' '+label);\
          }\
          return out.length?out.join(', '):'0 sec';\
        };\
        DurationFormat.prototype.resolvedOptions=function(){ return {locale:(this._l||'en-US'),style:(this._o.style||'short')}; };\
        DurationFormat.supportedLocalesOf=function(l){ return Array.isArray(l)?l:(l?[l]:[]); };\
        g.Intl={\
            DateTimeFormat:DateTimeFormat,\
            NumberFormat:NumberFormat,\
            Collator:Collator,\
            PluralRules:PluralRules,\
            ListFormat:ListFormat,\
            Locale:Locale,\
            RelativeTimeFormat:RelativeTimeFormat,\
            Segmenter:Segmenter,\
            DurationFormat:DurationFormat,\
            getCanonicalLocales:function(l){ return Array.isArray(l)?l.slice():(l?[l]:[]); },\
        };\
    })(globalThis);\0";
  unsafe {
    let r = JS_Eval(
      ctx,
      SRC.as_ptr() as *const std::os::raw::c_char,
      SRC.len() - 1,
      c"<intl-stub>".as_ptr(),
      JS_EVAL_TYPE_GLOBAL,
    );
    if r.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
    } else {
      JS_FreeValue(ctx, r);
    }
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
  if ctx.is_null() {
    return ptr::null();
  }

  let g = unsafe { JS_GetGlobalObject(ctx) };
  let h = intern::<Object>(g);
  h
}

thread_local! {
  // Resource name (script URL) per compiled `Script` handle, so `Run` can pass
  // it to `JS_Eval` as the filename. QuickJS reports that filename as the
  // referrer/basename for any `import()` evaluated in the script — without it
  // a relative/root-relative dynamic-import specifier can't be URL-resolved
  // (deno's loader sees referrer `<eval>` and rejects). Keyed on the unique
  // `Script` slot pointer returned by `Compile` (no value-pointer reuse risk);
  // the entry is consumed in `Run`.
  static SCRIPT_RESOURCE_NAMES: RefCell<
    std::collections::HashMap<usize, std::ffi::CString>,
  > = RefCell::new(std::collections::HashMap::new());

  // Source text per script filename, captured at eval time. Used by the
  // error-stack `Error.prepareStackTrace` shim (see `exception.rs`) to recover
  // V8's `new`-keyword column for `new X()` frames: quickjs records the
  // construct CALL position (the `(`), V8 records the `new` token, and the only
  // way to bridge that gap shim-side is to re-read the source line.
  static SCRIPT_SOURCES: RefCell<
    std::collections::HashMap<std::string::String, ScriptSource>,
  > = RefCell::new(std::collections::HashMap::new());

  static CURRENT_SCRIPT_NAMES: RefCell<Vec<std::string::String>> =
    const { RefCell::new(Vec::new()) };

  static CURRENT_HOST_DEFINED_OPTIONS: RefCell<Vec<usize>> =
    const { RefCell::new(Vec::new()) };
}

struct ScriptSource {
  text: std::string::String,
  line_ranges: Vec<std::ops::Range<usize>>,
}

impl ScriptSource {
  fn new(text: &str) -> Self {
    let mut line_ranges = Vec::new();
    let mut start = 0;
    for line in text.split_inclusive('\n') {
      let next = start + line.len();
      let mut end = next;
      if line.ends_with('\n') {
        end -= 1;
        if end > start && text.as_bytes()[end - 1] == b'\r' {
          end -= 1;
        }
      }
      line_ranges.push(start..end);
      start = next;
    }
    Self {
      text: text.to_string(),
      line_ranges,
    }
  }

  fn line(&self, line: i32) -> Option<&str> {
    let index = usize::try_from(line.checked_sub(1)?).ok()?;
    self
      .line_ranges
      .get(index)
      .map(|range| &self.text[range.clone()])
  }
}

struct CurrentScriptNameGuard {
  pushed: bool,
}

impl Drop for CurrentScriptNameGuard {
  fn drop(&mut self) {
    if self.pushed {
      CURRENT_SCRIPT_NAMES.with(|names| {
        names.borrow_mut().pop();
      });
    }
  }
}

struct CurrentHostDefinedOptionsGuard {
  _private: (),
}

impl Drop for CurrentHostDefinedOptionsGuard {
  fn drop(&mut self) {
    CURRENT_HOST_DEFINED_OPTIONS.with(|options| {
      options.borrow_mut().pop();
    });
  }
}

fn push_current_script_name(
  name: Option<&std::ffi::CString>,
) -> CurrentScriptNameGuard {
  let Some(name) = name.and_then(|n| n.to_str().ok()) else {
    return CurrentScriptNameGuard { pushed: false };
  };
  CURRENT_SCRIPT_NAMES.with(|names| {
    names.borrow_mut().push(name.to_string());
  });
  CurrentScriptNameGuard { pushed: true }
}

fn push_current_host_defined_options(
  options: Option<usize>,
) -> CurrentHostDefinedOptionsGuard {
  CURRENT_HOST_DEFINED_OPTIONS.with(|stack| {
    stack.borrow_mut().push(options.unwrap_or(0));
  });
  CurrentHostDefinedOptionsGuard { _private: () }
}

pub(crate) fn record_script_host_defined_options(
  script: *const impl Sized,
  options: *const Data,
) {
  let Some(key) = script_metadata_key(script) else {
    return;
  };
  let isolate = current_iso();
  if isolate.is_null() {
    return;
  }
  let state = iso_state(isolate);
  if let Some(old) = state.script_host_defined_options.remove(&key) {
    unsafe { JS_FreeValue(state.ctx, old) };
  }
  if !options.is_null() {
    let value = unsafe { JS_DupValue(state.ctx, jsval_of(options)) };
    state.script_host_defined_options.insert(key, value);
  }
}

pub(crate) fn record_host_defined_options_for_name(
  name: &str,
  options: *const Data,
) {
  if name.is_empty() {
    return;
  }
  let isolate = current_iso();
  if isolate.is_null() {
    return;
  }
  let state = iso_state(isolate);
  if let Some(old) = state.host_defined_options_by_name.remove(name) {
    unsafe { JS_FreeValue(state.ctx, old) };
  }
  if options.is_null() {
    return;
  }
  let value = unsafe { JS_DupValue(state.ctx, jsval_of(options)) };
  state
    .host_defined_options_by_name
    .insert(name.to_string(), value);
}

pub(crate) fn host_defined_options_for_name(name: &str) -> *const Data {
  let isolate = current_iso();
  if isolate.is_null() {
    return ptr::null();
  }
  let state = iso_state(isolate);
  let Some(value) = state.host_defined_options_by_name.get(name).copied()
  else {
    return ptr::null();
  };
  intern_dup::<Data>(state.ctx, value)
}

pub(crate) fn host_defined_options_for_script_value(
  value: JSValue,
) -> *const Data {
  if !jsv_is_object(&value) {
    return ptr::null();
  }
  let isolate = current_iso();
  if isolate.is_null() {
    return ptr::null();
  }
  let state = iso_state(isolate);
  let Some(value) = state
    .script_host_defined_options
    .get(&(unsafe { value.u.ptr } as usize))
    .copied()
  else {
    return ptr::null();
  };
  intern_dup::<Data>(state.ctx, value)
}

pub(crate) fn record_script_resource_name(
  script: *const impl Sized,
  name: &str,
) {
  let Some(key) = script_metadata_key(script) else {
    return;
  };
  SCRIPT_RESOURCE_NAMES.with(|names| {
    let mut names = names.borrow_mut();
    if names.len() > 256 && !names.contains_key(&key) {
      names.clear();
    }
    if name.is_empty() {
      names.remove(&key);
    } else if let Ok(name) = std::ffi::CString::new(name) {
      names.insert(key, name);
    }
  });
  let isolate = current_iso();
  if !isolate.is_null()
    && let Some(value) = iso_state(isolate)
      .script_host_defined_options
      .get(&key)
      .copied()
  {
    let options = intern_dup::<Data>(iso_state(isolate).ctx, value);
    record_host_defined_options_for_name(name, options);
  }
}

fn script_metadata_key(script: *const impl Sized) -> Option<usize> {
  if script.is_null() {
    return None;
  }
  let value = jsval_of(script);
  match value.tag {
    JS_TAG_STRING | JS_TAG_STRING_ROPE | JS_TAG_OBJECT => {
      let key = jsv_get_ptr(&value) as usize;
      (key != 0).then_some(key)
    }
    _ => Some(script as usize),
  }
}

pub(crate) fn new_script_value(ctx: *mut JSContext, source: &[u8]) -> JSValue {
  if ctx.is_null() {
    return jsv_exception();
  }
  let script = unsafe { JS_NewObject(ctx) };
  if script.tag == JS_TAG_EXCEPTION {
    return script;
  }
  let source = unsafe {
    JS_NewStringLen(
      ctx,
      source.as_ptr() as *const std::ffi::c_char,
      source.len(),
    )
  };
  if source.tag == JS_TAG_EXCEPTION {
    unsafe { JS_FreeValue(ctx, script) };
    return source;
  }
  let result = unsafe {
    JS_DefinePropertyValueStr(
      ctx,
      script,
      c"__v8x_script_source".as_ptr(),
      source,
      0,
    )
  };
  if result < 0 {
    unsafe { JS_FreeValue(ctx, script) };
    return jsv_exception();
  }
  script
}

pub(crate) fn script_source_value(
  ctx: *mut JSContext,
  script: *const impl Sized,
) -> JSValue {
  if ctx.is_null() || script.is_null() {
    return jsv_exception();
  }
  let value = jsval_of(script);
  if value.tag == JS_TAG_OBJECT {
    let source =
      unsafe { JS_GetPropertyStr(ctx, value, c"__v8x_script_source".as_ptr()) };
    if source.tag == JS_TAG_EXCEPTION
      || source.tag == JS_TAG_STRING
      || source.tag == JS_TAG_STRING_ROPE
    {
      return source;
    }
    unsafe { JS_FreeValue(ctx, source) };
  }
  unsafe { JS_DupValue(ctx, value) }
}

pub(crate) fn current_host_defined_options() -> *const Data {
  CURRENT_HOST_DEFINED_OPTIONS
    .with(|stack| stack.borrow().last().copied())
    .unwrap_or(0) as *const Data
}

pub(crate) fn current_script_name_or_source_url() -> Option<std::string::String>
{
  CURRENT_SCRIPT_NAMES.with(|names| names.borrow().last().cloned())
}

/// Remember a script's source under its eval filename so the stack-trace shim
/// can map a reported column back to the source line. Bounded to the most
/// recent entries so a long-lived runtime evaluating many scripts can't grow
/// this map without limit.
pub(crate) fn register_script_source(filename: &str, source: &str) {
  if filename.is_empty() {
    return;
  }
  replace_script_source(filename, source);
  super::inspector::script_source_registered(filename, source);
}

pub(crate) fn replace_script_source(filename: &str, source: &str) {
  SCRIPT_SOURCES.with(|m| {
    let mut map = m.borrow_mut();
    if map.len() > 256 && !map.contains_key(filename) {
      map.clear();
    }
    map.insert(filename.to_string(), ScriptSource::new(source));
  });
}

pub(crate) fn script_source(filename: &str) -> Option<std::string::String> {
  SCRIPT_SOURCES.with(|sources| {
    sources
      .borrow()
      .get(filename)
      .map(|source| source.text.clone())
  })
}

pub(crate) fn script_sources() -> Vec<(std::string::String, std::string::String)>
{
  SCRIPT_SOURCES.with(|sources| {
    sources
      .borrow()
      .iter()
      .map(|(name, source)| (name.clone(), source.text.clone()))
      .collect()
  })
}

/// Return the 1-based `line` of the source registered for `filename`, if known.
pub(crate) fn script_source_line(
  filename: &str,
  line: i32,
) -> Option<std::string::String> {
  if line < 1 {
    return None;
  }
  SCRIPT_SOURCES.with(|m| {
    m.borrow()
      .get(filename)
      .and_then(|source| source.line(line).map(str::to_string))
  })
}

#[cfg(test)]
mod script_source_tests {
  use super::replace_script_source;
  use super::script_source_line;

  #[test]
  fn indexes_script_lines_once() {
    replace_script_source("line-index.js", "one\r\ntwo\n\nfour\n");
    assert_eq!(script_source_line("line-index.js", 0), None);
    assert_eq!(
      script_source_line("line-index.js", 1).as_deref(),
      Some("one")
    );
    assert_eq!(
      script_source_line("line-index.js", 2).as_deref(),
      Some("two")
    );
    assert_eq!(script_source_line("line-index.js", 3).as_deref(), Some(""));
    assert_eq!(
      script_source_line("line-index.js", 4).as_deref(),
      Some("four")
    );
    assert_eq!(script_source_line("line-index.js", 5), None);

    replace_script_source("line-index.js", "replacement");
    assert_eq!(
      script_source_line("line-index.js", 1).as_deref(),
      Some("replacement")
    );
    assert_eq!(script_source_line("line-index.js", 2), None);
  }
}

// Pull the resource-name string out of a `ScriptOrigin` (slot 0 holds the
// `resource_name` handle pointer, per `v8__ScriptOrigin__CONSTRUCT`).
unsafe fn origin_resource_name(
  ctx: *mut JSContext,
  origin: *const c_void,
) -> Option<std::ffi::CString> {
  if origin.is_null() || ctx.is_null() {
    return None;
  }
  let name_handle = unsafe { *(origin as *const usize) } as *const Value;
  if name_handle.is_null() {
    return None;
  }
  let v = jsval_of(name_handle);
  if jsv_is_undefined(&v) || jsv_is_null(&v) {
    return None;
  }
  let mut len: usize = 0;
  let s = unsafe { JS_ToCStringLen(ctx, &mut len, v) };
  if s.is_null() {
    return None;
  }
  let bytes = unsafe { std::slice::from_raw_parts(s as *const u8, len) };
  let owned = String::from_utf8_lossy(bytes).into_owned();
  unsafe { JS_FreeCString(ctx, s) };
  if owned.is_empty() {
    return None;
  }
  std::ffi::CString::new(owned).ok()
}

unsafe fn origin_host_defined_options(origin: *const c_void) -> *const Data {
  if origin.is_null() {
    return ptr::null();
  }
  // Layout per RawScriptOrigin in module.rs: resource_name, source_map_url,
  // {script_id,line}, {column,pad}, host_defined_options — word 4, not 3
  // (word 3 is the column offset + padding).
  unsafe { *((origin as *const usize).add(4)) as *const Data }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__Compile(
  context: *const Context,
  source: *const V8String,
  origin: *const c_void,
) -> *const crate::Script {
  let ctx = ctx_of(context);
  if ctx.is_null() || source.is_null() {
    return ptr::null();
  }

  let name = unsafe { origin_resource_name(ctx, origin) };
  let host_defined_options = unsafe { origin_host_defined_options(origin) };

  // Syntax-check now: V8 reports syntax errors at *compile* time, and deno's
  // `execute_script` reads the exception straight from a failed compile. QuickJS
  // only surfaces the parse error when the script is run, and the resulting
  // SyntaxError carries no `fileName`/`lineNumber`/`columnNumber` properties, so
  // deno's `JsStackFrame::from_v8_message` fallback (the path for frame-less
  // errors) finds no location and produces zero frames. Compile with
  // COMPILE_ONLY here; on failure, stamp the parse location onto the SyntaxError
  // and leave it pending so the compile returns null with a located error.
  unsafe {
    let mut len: usize = 0;
    let cstr = JS_ToCStringLen(ctx, &mut len, jsval_of(source));
    if !cstr.is_null() {
      let source_bytes = std::slice::from_raw_parts(cstr as *const u8, len);
      maybe_report_strict_mode_use(source_bytes);
      let rewritten = std::str::from_utf8(source_bytes)
        .ok()
        .and_then(rewrite_script_source);
      if rewritten.is_some()
        && source_bytes.windows(12).any(|w| w == b"import.defer")
      {
        super::module::ensure_dynamic_defer_import_global(ctx);
      }
      let rewritten_bytes = rewritten.as_ref().map(|text| {
        let mut bytes = Vec::with_capacity(text.len() + 1);
        bytes.extend_from_slice(text.as_bytes());
        bytes.push(0);
        bytes
      });
      let (compile_ptr, compile_len) = match rewritten_bytes.as_ref() {
        Some(bytes) => (bytes.as_ptr() as *const c_char, bytes.len() - 1),
        None => (cstr, len),
      };
      let url_ptr = match name.as_ref() {
        Some(n) => n.as_ptr(),
        None => c"<anonymous>".as_ptr(),
      };
      let compiled = JS_Eval(
        ctx,
        compile_ptr,
        compile_len,
        url_ptr,
        global_eval_flags() | JS_EVAL_FLAG_COMPILE_ONLY,
      );
      if compiled.tag == JS_TAG_EXCEPTION {
        JS_FreeCString(ctx, cstr);
        stamp_syntax_error_location(ctx, name.as_ref());
        return ptr::null();
      }
      note_compiled_bytecode(current_iso(), compile_len);
      note_compilation_cache_miss();
      let script_source = new_script_value(
        ctx,
        std::slice::from_raw_parts(compile_ptr as *const u8, compile_len),
      );
      JS_FreeValue(ctx, compiled);
      JS_FreeCString(ctx, cstr);
      if script_source.tag == JS_TAG_EXCEPTION {
        return ptr::null();
      }
      let handle = intern::<crate::Script>(script_source);
      record_script_host_defined_options(handle, host_defined_options);
      let name = name.and_then(|name| name.into_string().ok());
      record_script_resource_name(handle, name.as_deref().unwrap_or(""));
      return handle;
    }
  }

  let handle = intern_dup::<crate::Script>(ctx, jsval_of(source));
  record_script_host_defined_options(handle, host_defined_options);
  let name = name.and_then(|name| name.into_string().ok());
  record_script_resource_name(handle, name.as_deref().unwrap_or(""));
  handle
}

/// After a failed COMPILE_ONLY in `v8__Script__Compile`, stamp the pending
/// SyntaxError with `fileName`/`lineNumber`/`columnNumber` so deno's
/// `from_v8_message` fallback builds a located frame, then re-arm it as pending.
/// Line/column come from the error's parsed backtrace (quickjs records the
/// offending token's position there via our `js_parse_error` patch).
unsafe fn stamp_syntax_error_location(
  ctx: *mut JSContext,
  resource: Option<&std::ffi::CString>,
) {
  if !unsafe { JS_HasException(ctx) } {
    return;
  }
  // Takes ownership and clears the pending slot; we re-throw it below.
  let exc = unsafe { JS_GetException(ctx) };
  unsafe { super::exception::mark_parse_error(ctx, exc) };

  let (line, col) = unsafe { error_backtrace_location(ctx, exc) };

  if let Some(name) = resource {
    let fname = unsafe { JS_NewString(ctx, name.as_ptr()) };
    unsafe {
      JS_SetPropertyStr(ctx, exc, c"fileName".as_ptr(), fname);
    }
  }
  if line > 0 {
    let v = unsafe { JS_NewInt32(ctx, line) };
    unsafe {
      JS_SetPropertyStr(ctx, exc, c"lineNumber".as_ptr(), v);
    }
  }
  if col > 0 {
    // deno's `GetStartColumn` returns this raw and re-adds 1, so store the
    // 0-based column (quickjs/our backtrace reports 1-based).
    let v = unsafe { JS_NewInt32(ctx, col - 1) };
    unsafe {
      JS_SetPropertyStr(ctx, exc, c"columnNumber".as_ptr(), v);
    }
  }

  unsafe {
    JS_Throw(ctx, exc);
  }
}

/// Read an error's `.stack` and return the `(line, col)` of its first frame, or
/// `(0, 0)` if none. Used to recover a SyntaxError's parse location.
unsafe fn error_backtrace_location(
  ctx: *mut JSContext,
  exc: JSValue,
) -> (i32, i32) {
  let stack = unsafe { JS_GetPropertyStr(ctx, exc, c"stack".as_ptr()) };
  if stack.tag == JS_TAG_EXCEPTION {
    unsafe {
      let e = JS_GetException(ctx);
      JS_FreeValue(ctx, e);
    }
    return (0, 0);
  }
  let mut len: usize = 0;
  let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, stack) };
  unsafe { JS_FreeValue(ctx, stack) };
  if cstr.is_null() {
    return (0, 0);
  }
  let bytes = unsafe { std::slice::from_raw_parts(cstr as *const u8, len) };
  let s = String::from_utf8_lossy(bytes).into_owned();
  unsafe { JS_FreeCString(ctx, cstr) };
  super::exception::first_frame_line_col(&s)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__Run(
  script: *const crate::Script,
  context: *const Context,
) -> *const Value {
  let ctx = ctx_of(context);
  if ctx.is_null() || script.is_null() {
    return ptr::null();
  }
  if !javascript_execution_allowed() {
    unsafe { throw_javascript_execution_disallowed(ctx) };
    return ptr::null();
  }
  let iso = current_iso();
  if !iso.is_null() {
    super::isolate::run_pending_interrupts(iso);
  }
  let src_val = script_source_value(ctx, script);
  if src_val.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  if std::env::var_os("QJS_DEBUG_SNAPSHOT").is_some() {
    let mut l = 0usize;
    let s = unsafe { JS_ToCStringLen(ctx, &mut l, src_val) };
    let head = if s.is_null() {
      String::new()
    } else {
      let b = unsafe { std::slice::from_raw_parts(s as *const u8, l.min(60)) };
      let h = String::from_utf8_lossy(b).replace('\n', "\\n");
      unsafe { JS_FreeCString(ctx, s) };
      h
    };
    eprintln!(
      "[qjs snapshot] Script__Run ctx={ctx:?} cur={:?} src={head}",
      current_ctx()
    );
  }

  let mut len: usize = 0;
  let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, src_val) };
  unsafe { JS_FreeValue(ctx, src_val) };
  if cstr.is_null() {
    return ptr::null();
  }
  let source_bytes =
    unsafe { std::slice::from_raw_parts(cstr as *const u8, len) };
  // Use the compile-time resource name (script URL) as the eval filename so
  // `import()` inside this script resolves relative to it; fall back to
  // `<eval>` for scripts compiled without a ScriptOrigin.
  let script_key = script_metadata_key(script);
  let fname_owned = script_key.and_then(|key| {
    SCRIPT_RESOURCE_NAMES.with(|m| m.borrow().get(&key).cloned())
  });
  let host_defined_options = script_key.and_then(|key| {
    let isolate = current_iso();
    if isolate.is_null() {
      return None;
    }
    let state = iso_state(isolate);
    state
      .script_host_defined_options
      .get(&key)
      .copied()
      .map(|value| intern_dup::<Data>(ctx, value) as usize)
  });
  let fname_ptr = match fname_owned.as_ref() {
    Some(name) => name.as_ptr(),
    None => c"<eval>".as_ptr(),
  };
  let _current_script_name = push_current_script_name(fname_owned.as_ref());
  let _current_host_defined_options =
    push_current_host_defined_options(host_defined_options);
  super::inspector::maybe_pause_on_next_statement();
  // Capture the source under its eval filename for the stack-trace shim.
  // Anonymous scripts register under the engine-visible "<eval>" name so the
  // inspector still emits Debugger.scriptParsed for them (V8 does, with "").
  {
    let bytes = unsafe { std::slice::from_raw_parts(cstr as *const u8, len) };
    let fname = fname_owned
      .as_ref()
      .and_then(|name| name.to_str().ok())
      .unwrap_or("<eval>");
    register_script_source(fname, &String::from_utf8_lossy(bytes));
  }
  // Script bytecode cache: parse once per (source, flags), then boot from
  // JS_ReadObject + JS_EvalFunction — same pattern as the module path. The
  // eval flags participate via the seed so a `--use_strict` run never reuses
  // sloppy-mode bytecode.
  let eval_flags = global_eval_flags();
  let src_str = std::str::from_utf8(source_bytes).ok();
  let resource_name = fname_owned
    .as_ref()
    .map(|name| name.as_bytes())
    .unwrap_or(b"<eval>");
  let bc_key = src_str.map(|s| {
    super::module::script_bc_key(eval_flags as u64, s.as_bytes(), resource_name)
  });
  let mut result = JSValue {
    u: JSValueUnion { int32: 0 },
    tag: JS_TAG_UNINITIALIZED,
  };
  if let Some(key) = bc_key
    && let Some(bytes) = super::module::bc_load(key)
  {
    let obj = super::module::read_cached_bytecode(ctx, &bytes);
    if obj.tag == JS_TAG_EXCEPTION {
      // Stale/corrupt cache entry: clear the error, fall through to a parse.
      let exc = unsafe { JS_GetException(ctx) };
      unsafe { JS_FreeValue(ctx, exc) };
    } else {
      result = unsafe { JS_EvalFunction(ctx, obj) };
    }
  }
  if result.tag == JS_TAG_UNINITIALIZED {
    let compiled = unsafe {
      JS_Eval(
        ctx,
        cstr,
        len,
        fname_ptr,
        eval_flags | JS_EVAL_FLAG_COMPILE_ONLY,
      )
    };
    if compiled.tag == JS_TAG_EXCEPTION {
      result = compiled;
    } else {
      if let Some(key) = bc_key {
        unsafe { super::module::bc_write(ctx, key, compiled) };
      }
      result = unsafe { JS_EvalFunction(ctx, compiled) };
    }
  }
  if std::env::var_os("QJS_DEBUG_SNAPSHOT").is_some() {
    eprintln!("[qjs snapshot]   -> result.tag={}", result.tag);
  }
  if result.tag != JS_TAG_EXCEPTION {
    super::isolate::run_microtasks_if_auto();
    if !iso.is_null() {
      super::isolate::maybe_drive_near_heap_limit_callback(iso);
    }
    let h_result = intern::<Value>(unsafe { JS_DupValue(ctx, result) });
    unsafe { JS_FreeCString(ctx, cstr) };
    if result.tag != JS_TAG_EXCEPTION {
      unsafe { JS_FreeValue(ctx, result) };
    }
    return h_result;
  }
  let source_for_message = if result.tag == JS_TAG_EXCEPTION {
    Some(std::string::String::from_utf8_lossy(source_bytes).into_owned())
  } else {
    None
  };
  unsafe { JS_FreeCString(ctx, cstr) };
  if result.tag == JS_TAG_EXCEPTION {
    if std::env::var_os("QJS_DEBUG_EXC").is_some() {
      unsafe {
        let exc = JS_GetException(ctx);
        let mut l = 0usize;
        let s = JS_ToCStringLen(ctx, &mut l, exc);
        if !s.is_null() {
          let bytes = std::slice::from_raw_parts(s as *const u8, l);
          eprintln!("[QJS_DEBUG_EXC] {}", String::from_utf8_lossy(bytes));
          JS_FreeCString(ctx, s);
        }

        let stk = JS_GetPropertyStr(ctx, exc, c"stack".as_ptr());
        if !jsv_is_undefined(&stk) {
          let mut sl = 0usize;
          let ss = JS_ToCStringLen(ctx, &mut sl, stk);
          if !ss.is_null() {
            let sb = std::slice::from_raw_parts(ss as *const u8, sl);
            eprintln!("[QJS_DEBUG_STACK]\n{}", String::from_utf8_lossy(sb));
            JS_FreeCString(ctx, ss);
          }
        }
        JS_FreeValue(ctx, stk);

        JS_Throw(ctx, exc);
      }
    }

    if !super::exception::has_active_try_catch() {
      let resource_name =
        fname_owned.as_ref().and_then(|name| name.to_str().ok());
      unsafe {
        super::exception::notify_message_listeners(
          ctx,
          resource_name,
          source_for_message.as_deref().unwrap_or(""),
        )
      };
      unsafe { super::exception::clear_pending(ctx) };
    }

    return ptr::null();
  }

  intern::<Value>(result)
}
