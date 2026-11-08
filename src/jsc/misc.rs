//! Family "misc": SnapshotCreator / StartupData / CppHeap / cppgc /
//! WeakCallbackInfo / Proxy / JSON / Wasm* / Task / IdleTask / Global /
//! shared_ptr<Platform>.
//!
//! JSC has no equivalent for V8 snapshots, cppgc, the WebAssembly C++ internals
//! or the C++ task abstractions, so most of these are safe inert defaults
//! (see the `TODO(v82jsc)` markers). Global handles, JSON parsing and the
//! shared_ptr<Platform> machinery are implemented for real.
#![allow(non_snake_case, unused)]

use crate::jsc::core::{
  ctx_of, current_ctx, current_iso, intern, intern_ctx, iso_state, jsval,
};
use crate::jsc::jsc_sys::*;
use crate::support::{SharedPtrBase, UniquePtr, long};
use crate::{Context, Data, Object, String as V8String, Value};

use std::os::raw::{c_char, c_void};
use std::ptr;

type PlatformOpaque = c_void;

struct WeakCallbackInfoShim {
  isolate: *mut crate::RealIsolate,
  parameter: *const c_void,
  second_pass: Option<crate::jsc::core::WeakCallback>,
}

fn collect_weak_handles(isolate: *mut crate::RealIsolate) {
  if isolate.is_null() {
    return;
  }

  let weak_handles = {
    let st = crate::jsc::core::iso_state(isolate);
    std::mem::take(&mut st.weak_handles)
  };

  for weak in weak_handles {
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
}

fn run_gc_callbacks(
  isolate: *mut crate::RealIsolate,
  callbacks: &[crate::jsc::core::GcCallbackEntry],
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
}

// cppgc `Member<T>` / `WeakMember<T>` slots. NOTE: the bindgen-derived
// `cppgc__Member_SIZE` is only **4 bytes** (compressed pointer), so we must
// never write a raw 64-bit pointer into a member slot — that overflows the
// inline `[u8; 4]` field and corrupts adjacent memory. We don't run a real
// Oilpan GC, so members are inert: construct/destruct zero the 4-byte slot and
// `Get` returns null (`Set`/`Assign` are no-ops). This keeps `test_cppgc`
// *linking* and non-crashing; the GC-collection assertions in those tests need
// a real cppgc heap and are expected to fail.
#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Member__CONSTRUCT(
  member: *mut c_void,
  _obj: *mut c_void,
) {
  if !member.is_null() {
    unsafe { ptr::write_unaligned(member as *mut u32, 0) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Member__DESTRUCT(member: *mut c_void) {
  if !member.is_null() {
    unsafe { ptr::write_unaligned(member as *mut u32, 0) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Member__Get(_member: *const c_void) -> *mut c_void {
  ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Member__Assign(
  _member: *mut c_void,
  _obj: *mut c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakMember__CONSTRUCT(
  member: *mut c_void,
  _obj: *mut c_void,
) {
  if !member.is_null() {
    unsafe { ptr::write_unaligned(member as *mut u32, 0) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakMember__DESTRUCT(member: *mut c_void) {
  if !member.is_null() {
    unsafe { ptr::write_unaligned(member as *mut u32, 0) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakMember__Get(
  _member: *const c_void,
) -> *mut c_void {
  ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakMember__Assign(
  _member: *mut c_void,
  _obj: *mut c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Visitor__Trace__Member(
  _visitor: *mut c_void,
  _member: *const c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Visitor__Trace__WeakMember(
  _visitor: *mut c_void,
  _member: *const c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__RequestGarbageCollectionForTesting(
  isolate: *mut crate::RealIsolate,
  _type: usize,
) {
  // Best-effort: run JSC's GC. Our cppgc shim doesn't reclaim native wrappers,
  // so the cppgc-specific collection assertions still won't hold, but this
  // satisfies the many `test_api` tests that just need a GC to run.
  let ctx = current_ctx();
  if !ctx.is_null() {
    unsafe { JSGarbageCollect(ctx) };
  }
  let isolate = if isolate.is_null() {
    current_iso()
  } else {
    isolate
  };
  if !isolate.is_null() {
    let st = crate::jsc::core::iso_state(isolate);
    let prologue_callbacks = st.gc_prologue_callbacks.clone();
    run_gc_callbacks(isolate, &prologue_callbacks);
    crate::jsc::core::release_external_string_memory(st);
    let epilogue_callbacks = st.gc_epilogue_callbacks.clone();
    run_gc_callbacks(isolate, &epilogue_callbacks);
    collect_weak_handles(isolate);
  }
}

thread_local! {
    static DUMMY_CPP_HEAP: std::cell::Cell<*mut c_void> =
        const { std::cell::Cell::new(ptr::null_mut()) };
}

pub(crate) fn current_cpp_heap() -> *mut c_void {
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
pub extern "C" fn cppgc__make_garbage_collectable(
  _heap: *mut c_void,
  additional_bytes: usize,
  align: usize,
) -> *mut c_void {
  const RUST_OBJ_SIZE: usize = 8;
  let size = RUST_OBJ_SIZE + additional_bytes;
  let align = align.max(8);
  let layout = match std::alloc::Layout::from_size_align(size, align) {
    Ok(l) => l,
    Err(_) => return ptr::null_mut(),
  };
  unsafe {
    let p = std::alloc::alloc_zeroed(layout);
    p as *mut c_void
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CppHeap__Terminate(_heap: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CppHeap__DELETE(_heap: *mut c_void) {}

thread_local! {
    static GLOBAL_PROTECT: std::cell::RefCell<
        std::collections::HashMap<usize, (JSContextRef, usize)>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

fn stable_protect_ctx() -> JSContextRef {
  let ctx = current_ctx();
  if !ctx.is_null() {
    return ctx;
  }
  let iso = current_iso();
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

fn is_non_value_handle(v: JSValueRef) -> bool {
  crate::jsc::core::is_non_value_handle(current_iso(), v)
}

fn global_protect(v: JSValueRef) {
  if v.is_null() {
    return;
  }

  if is_non_value_handle(v) {
    return;
  }
  GLOBAL_PROTECT.with(|m| {
    let mut map = m.borrow_mut();
    match map.get_mut(&(v as usize)) {
      Some((ctx, count)) => {
        unsafe { JSValueProtect(*ctx, v) };
        *count += 1;
      }
      None => {
        let ctx = stable_protect_ctx();
        if ctx.is_null() {
          return;
        }
        unsafe { JSValueProtect(ctx, v) };
        map.insert(v as usize, (ctx, 1));
      }
    }
  });
}

fn global_unprotect(v: JSValueRef) {
  if v.is_null() {
    return;
  }
  if is_non_value_handle(v) {
    return;
  }
  GLOBAL_PROTECT.with(|m| {
    let mut map = m.borrow_mut();
    if let Some((ctx, count)) = map.get_mut(&(v as usize)) {
      let ctx = *ctx;
      unsafe { JSValueUnprotect(ctx, v) };
      *count -= 1;
      if *count == 0 {
        map.remove(&(v as usize));
      }
    }
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__New(
  _isolate: *mut c_void,
  data: *const Data,
) -> *const Data {
  if data.is_null() {
    return ptr::null();
  }
  global_protect(jsval(data));

  data
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__NewWeak(
  isolate: *mut c_void,
  data: *const Data,
  parameter: *const c_void,
  callback: unsafe extern "C" fn(*const c_void),
) -> *const Data {
  if !data.is_null() && !isolate.is_null() {
    crate::jsc::core::iso_state(isolate as *mut crate::RealIsolate)
      .weak_handles
      .push(crate::jsc::core::WeakHandle {
        handle: data,
        parameter,
        callback,
      });
  }
  data
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__Reset(data: *const Data) {
  if data.is_null() {
    return;
  }
  let iso = current_iso();
  if !iso.is_null() {
    crate::jsc::core::iso_state(iso)
      .weak_handles
      .retain(|weak| weak.handle != data);
  }
  global_unprotect(jsval(data));
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
    let v = this.read_unaligned() as JSValueRef;
    if !v.is_null() {
      global_unprotect(v);
      this.write_unaligned(0);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TracedReference__Reset(
  this: *mut usize,
  _isolate: *mut crate::RealIsolate,
  data: *const Data,
) {
  if this.is_null() {
    return;
  }
  unsafe {
    let old = this.read_unaligned() as JSValueRef;
    if !old.is_null() {
      global_unprotect(old);
    }
    let v = jsval(data);
    if v.is_null() {
      this.write_unaligned(0);
    } else {
      global_protect(v);
      this.write_unaligned(v as usize);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TracedReference__Get(
  this: *const usize,
  _isolate: *mut crate::RealIsolate,
) -> *const Data {
  if this.is_null() {
    return ptr::null();
  }
  unsafe { this.read_unaligned() as *const Data }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__JSON__Parse(
  context: *const Context,
  json_string: *const V8String,
) -> *const Value {
  if context.is_null() || json_string.is_null() {
    return ptr::null();
  }
  let ctx = ctx_of(context) as JSContextRef;
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let s = JSValueToStringCopy(ctx, jsval(json_string), &mut exc);
    if s.is_null() {
      return ptr::null();
    }
    let parsed = JSValueMakeFromJSONString(ctx, s);
    JSStringRelease(s);
    if parsed.is_null() {
      return ptr::null();
    }
    intern_ctx::<Value>(ctx, parsed)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__JSON__Stringify(
  context: *const Context,
  json_object: *const Value,
) -> *const crate::String {
  if context.is_null() {
    return ptr::null();
  }
  let ctx = ctx_of(context) as JSContextRef;
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let s = JSValueCreateJSONString(ctx, jsval(json_object), 0, &mut exc);
    if s.is_null() || !exc.is_null() {
      return ptr::null();
    }

    let v = JSValueMakeString(ctx, s);
    JSStringRelease(s);
    intern_ctx::<crate::String>(ctx, v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Date__New(
  context: *const Context,
  value: f64,
) -> *const crate::Date {
  if context.is_null() {
    return ptr::null();
  }
  let ctx = ctx_of(context) as JSContextRef;
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let arg = JSValueMakeNumber(ctx, value);
    let args = [arg];
    let d = JSObjectMakeDate(ctx, 1, args.as_ptr(), &mut exc);
    if !exc.is_null() || d.is_null() {
      return ptr::null();
    }
    intern_ctx::<crate::Date>(ctx, d as JSValueRef)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Date__ValueOf(this: *const crate::Date) -> f64 {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return f64::NAN;
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();

    let n = JSValueToNumber(ctx, jsval(this), &mut exc);
    if !exc.is_null() {
      return f64::NAN;
    }
    n
  }
}

/// `Date.parse` followed by `new Date`, which is what V8's `Date::Parse` does:
/// an unparsable string yields a Date whose time value is NaN.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Date__Parse(
  context: *const Context,
  date_string: *const V8String,
) -> *const crate::Date {
  if context.is_null() || date_string.is_null() {
    return ptr::null();
  }
  let ctx = ctx_of(context) as JSContextRef;
  match unsafe {
    crate::jsc::core::call_js_fn(
      ctx,
      c"(function(s){return new Date(Date.parse(s))})",
      &[jsval(date_string)],
    )
  } {
    Ok(d) => intern_ctx::<crate::Date>(ctx, d),
    Err(exc) => {
      crate::jsc::core::record_pending_exception(ctx, exc);
      ptr::null()
    }
  }
}

/// Runs one of the Date-to-string conversions V8 exposes as infallible.
/// `src` must not throw for a NaN date, which V8 formats as "Invalid Date".
fn date_to_string(
  this: *const crate::Date,
  src: &std::ffi::CStr,
) -> *const V8String {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  match unsafe { crate::jsc::core::call_js_fn(ctx, src, &[jsval(this)]) } {
    Ok(v) => intern_ctx::<V8String>(ctx, v),
    Err(_) => ptr::null(),
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Date__ToISOString(
  this: *const crate::Date,
) -> *const V8String {
  date_to_string(
    this,
    c"(function(d){var t=Date.prototype.valueOf.call(d);\
       return t!==t?'Invalid Date':Date.prototype.toISOString.call(d)})",
  )
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Date__ToUTCString(
  this: *const crate::Date,
) -> *const V8String {
  date_to_string(
    this,
    c"(function(d){return Date.prototype.toUTCString.call(d)})",
  )
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__GetHandler(this: *const c_void) -> *const Value {
  // Native read of ProxyObject::handler() (introspect.cpp) — JSC's C API has
  // no handler accessor.
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  let v = this as *const Value;
  let handler =
    unsafe { crate::jsc::introspect::v82jsc_proxy_handler(ctx, jsval(v)) };
  if handler.is_null() {
    return ptr::null();
  }
  intern_ctx::<Value>(ctx, handler)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__GetTarget(this: *const c_void) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  let v = this as *const Value;
  let target = unsafe {
    crate::jsc::value::JSObjectGetProxyTarget(jsval(v) as JSObjectRef)
  };
  if target.is_null() {
    return ptr::null();
  }
  intern_ctx::<Value>(ctx, target as JSValueRef)
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawStartupDataAbi {
  data: *const c_char,
  raw_size: std::os::raw::c_int,
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__CONSTRUCT(
  _buf: *mut c_void,
  _params: *const c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__DESTRUCT(_this: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__GetIsolate(
  _this: *const c_void,
) -> *mut c_void {
  current_iso() as *mut c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__CreateBlob(
  _this: *mut c_void,
  _function_code_handling: u32,
) -> RawStartupDataAbi {
  RawStartupDataAbi {
    data: ptr::null(),
    raw_size: 0,
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__SetDefaultContext(
  _this: *mut c_void,
  _context: *const Context,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__AddContext(
  _this: *mut c_void,
  _context: *const Context,
) -> usize {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__AddData_to_context(
  _this: *mut c_void,
  _context: *const Context,
  _data: *const Data,
) -> usize {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StartupData__CanBeRehashed(_this: *const c_void) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StartupData__IsValid(_this: *const c_void) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StartupData__data__DELETE(_this: *const c_char) {}

// ICU common-data loader (vendored rusty_v8 `icu::set_common_data_77`). JSC
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
pub extern "C" fn v8__Task__Run(_task: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Task__DELETE(_task: *mut c_void) {}

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
  _value: *const Value,
  _that: *mut c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__shared_ptr_DESTRUCT(_this: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__OnBytesReceived(
  _this: *mut c_void,
  _data: *const u8,
  _len: usize,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__Finish(
  _this: *mut c_void,
  _callback: Option<unsafe extern "C" fn(*mut c_void)>,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__Abort(
  _this: *mut c_void,
  _exception: *const Value,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__SetUrl(
  _this: *mut c_void,
  _url: *const c_char,
  _len: usize,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleObject__FromCompiledModule(
  _isolate: *mut c_void,
  _compiled_module: *const c_void,
) -> *const c_void {
  ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleObject__GetCompiledModule(
  _this: *const c_void,
) -> *mut c_void {
  ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleObject__Compile(
  _isolate: *mut c_void,
  _wire_bytes_data: *const u8,
  _length: usize,
) -> *mut c_void {
  ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CompiledWasmModule__DELETE(_this: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__NEW() -> *mut c_void {
  ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__DELETE(_this: *mut c_void) {}

#[repr(C)]
struct PlatformSharedRepr {
  platform: *mut c_void,
  refcount: *mut usize,
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__CONVERT__std__unique_ptr(
  unique_ptr: UniquePtr<crate::Platform>,
) -> SharedPtrBase<crate::Platform> {
  let raw = unique_ptr.into_raw() as *mut c_void;
  let repr = if raw.is_null() {
    PlatformSharedRepr {
      platform: ptr::null_mut(),
      refcount: ptr::null_mut(),
    }
  } else {
    PlatformSharedRepr {
      platform: raw,
      refcount: Box::into_raw(Box::new(1usize)),
    }
  };
  unsafe {
    std::mem::transmute::<PlatformSharedRepr, SharedPtrBase<crate::Platform>>(
      repr,
    )
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__get(
  ptr: *const SharedPtrBase<crate::Platform>,
) -> *mut crate::Platform {
  if ptr.is_null() {
    return ptr::null_mut();
  }
  let repr = ptr as *const PlatformSharedRepr;
  unsafe { (*repr).platform as *mut crate::Platform }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__COPY(
  ptr: *const SharedPtrBase<crate::Platform>,
) -> SharedPtrBase<crate::Platform> {
  if ptr.is_null() {
    return SharedPtrBase::default();
  }
  let repr = ptr as *const PlatformSharedRepr;
  let (platform, refcount) = unsafe { ((*repr).platform, (*repr).refcount) };
  if !refcount.is_null() {
    unsafe { *refcount += 1 };
  }
  let copy = PlatformSharedRepr { platform, refcount };
  unsafe {
    std::mem::transmute::<PlatformSharedRepr, SharedPtrBase<crate::Platform>>(
      copy,
    )
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__reset(
  ptr: *mut SharedPtrBase<crate::Platform>,
) {
  if ptr.is_null() {
    return;
  }
  let repr = ptr as *mut PlatformSharedRepr;
  unsafe {
    let refcount = (*repr).refcount;
    if !refcount.is_null() {
      *refcount -= 1;
      if *refcount == 0 {
        drop(Box::from_raw(refcount));
        let p = (*repr).platform as *mut crate::Platform;
        if !p.is_null() {
          drop(Box::from_raw(p));
        }
      }
    }
    (*repr).platform = ptr::null_mut();
    (*repr).refcount = ptr::null_mut();
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__use_count(
  ptr: *const SharedPtrBase<crate::Platform>,
) -> long {
  if ptr.is_null() {
    return 0;
  }
  let repr = ptr as *const PlatformSharedRepr;
  let refcount = unsafe { (*repr).refcount };
  if refcount.is_null() {
    0
  } else {
    unsafe { *refcount as long }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__CONSTRUCT(this: *mut usize) {
  if !this.is_null() {
    unsafe { this.write_unaligned(0) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__DESTRUCT(_this: *mut usize) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__Get(
  this: *const usize,
  _isolate: *mut crate::RealIsolate,
) -> *const Data {
  if this.is_null() {
    return ptr::null();
  }
  unsafe { this.read_unaligned() as *const Data }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__Set(
  this: *mut usize,
  _isolate: *mut crate::RealIsolate,
  data: *mut Data,
) {
  if this.is_null() {
    return;
  }
  let v = jsval(data);
  if v.is_null() {
    unsafe { this.write_unaligned(0) };
    return;
  }
  global_protect(v);
  unsafe { this.write_unaligned(v as usize) };
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Persistent__CONSTRUCT(
  obj: *mut c_void,
) -> *mut c_void {
  Box::into_raw(Box::new(obj)) as *mut c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Persistent__DESTRUCT(this: *mut c_void) {
  if this.is_null() {
    return;
  }

  unsafe { drop(Box::from_raw(this as *mut *mut c_void)) };
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Persistent__Get(this: *const c_void) -> *mut c_void {
  if this.is_null() {
    return ptr::null_mut();
  }
  unsafe { *(this as *const *mut c_void) }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Visitor__Trace__TracedReference(
  _visitor: *mut c_void,
  _reference: *const c_void,
) {
}

// ---------------------------------------------------------------------------
// Link-stubs for v8 C-ABI symbols that `test_api.rs` references but this
// backend doesn't implement yet. Each returns a benign default
// (null / 0 / false / `Nothing`) so the target LINKS and the many tests that
// don't touch these paths run; tests that do exercise them fail gracefully
// without crashing. Promote individual stubs to real implementations over time.
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn icu_set_default_locale(locale: *const std::os::raw::c_char) {
  if locale.is_null() {
    return;
  }
  let s = unsafe { std::ffi::CStr::from_ptr(locale) }.to_string_lossy();
  crate::jsc::cli_extra::set_default_locale_str(&s);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CompiledWasmModule__GetWireBytesRef(
  _this: *mut std::os::raw::c_void,
  _length: *mut std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CompiledWasmModule__SourceUrl(
  _this: *mut std::os::raw::c_void,
  _length: *mut std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__Clear(_this: *mut std::os::raw::c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__IsEmpty(
  _this: *const std::os::raw::c_void,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__IsRevoked(
  _this: *const std::os::raw::c_void,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__New(
  _context: *const std::os::raw::c_void,
  _target: *const std::os::raw::c_void,
  _handler: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__Revoke(_this: *const std::os::raw::c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__AddData_to_isolate(
  _this: *mut std::os::raw::c_void,
  _data: *const std::os::raw::c_void,
) -> usize {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__Abort(
  _this: *mut std::os::raw::c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__Finish(
  _this: *mut std::os::raw::c_void,
  _isolate: *mut std::os::raw::c_void,
  _caching_callback: *const std::os::raw::c_void,
  _resolution_callback: *const std::os::raw::c_void,
  _resolution_data: *mut std::os::raw::c_void,
  _drop_resolution_data: *const std::os::raw::c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__OnBytesReceived(
  _this: *mut std::os::raw::c_void,
  _bytes: *const std::os::raw::c_void,
  _size: usize,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__SetUrl(
  _this: *mut std::os::raw::c_void,
  _url: *const std::os::raw::c_void,
  _length: usize,
) {
}
