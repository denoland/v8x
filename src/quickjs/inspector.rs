#![allow(non_snake_case, unused)]

use crate::Context;
use crate::StackTrace;
use crate::Value;
use crate::isolate::RealIsolate;
use crate::quickjs::quickjs_sys::*;
use crate::support::Opaque;
use crate::support::UniquePtr;
use crate::support::int;

use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CpuProfileFrame {
  function_name: String,
  url: String,
  line_number: i32,
  column_number: i32,
}

#[derive(Clone, Debug)]
pub(crate) struct CpuProfileSample {
  frames: Vec<CpuProfileFrame>,
  timestamp_us: u64,
}

#[derive(Debug)]
pub(crate) struct CompletedCpuProfile {
  start_time_us: u64,
  end_time_us: u64,
  samples: Vec<CpuProfileSample>,
}

#[derive(Debug)]
pub(crate) struct CpuProfilerState {
  active: bool,
  is_idle: bool,
  interval: Duration,
  started_at: Option<Instant>,
  idle_since: Option<Instant>,
  start_time_us: u64,
  last_sample_at: Option<Instant>,
  samples: Vec<CpuProfileSample>,
}

impl CpuProfilerState {
  pub(crate) fn new() -> Self {
    Self {
      active: false,
      is_idle: false,
      interval: Duration::from_micros(1_000),
      started_at: None,
      idle_since: None,
      start_time_us: 0,
      last_sample_at: None,
      samples: Vec::new(),
    }
  }

  fn set_interval(&mut self, interval_us: u64) {
    if !self.active {
      self.interval = Duration::from_micros(interval_us.max(1));
    }
  }

  fn start(&mut self) {
    let now = Instant::now();
    self.active = true;
    self.started_at = Some(now);
    self.idle_since = self.is_idle.then_some(now);
    self.start_time_us = SystemTime::now()
      .duration_since(SystemTime::UNIX_EPOCH)
      .unwrap_or_default()
      .as_micros()
      .min(u64::MAX as u128) as u64;
    self.last_sample_at = None;
    self.samples.clear();
  }

  fn finish(&mut self) -> CompletedCpuProfile {
    self.record_idle_until(Instant::now());
    let elapsed_us = self
      .started_at
      .map(|start| start.elapsed().as_micros())
      .unwrap_or_default()
      .min(u64::MAX as u128) as u64;
    self.active = false;
    self.started_at = None;
    self.idle_since = None;
    self.last_sample_at = None;
    CompletedCpuProfile {
      start_time_us: self.start_time_us,
      end_time_us: self.start_time_us.saturating_add(elapsed_us),
      samples: std::mem::take(&mut self.samples),
    }
  }

  pub(crate) fn set_idle(&mut self, is_idle: bool) {
    if self.is_idle == is_idle {
      return;
    }
    let now = Instant::now();
    if is_idle {
      self.is_idle = true;
      if self.active {
        self.idle_since = Some(now);
      }
    } else {
      self.record_idle_until(now);
      self.is_idle = false;
      self.idle_since = None;
    }
  }

  fn record_idle_until(&mut self, now: Instant) {
    if !self.active {
      return;
    }
    let (Some(started_at), Some(idle_since)) =
      (self.started_at, self.idle_since)
    else {
      return;
    };
    let interval_us = self.interval.as_micros().max(1);
    let start_us = idle_since.saturating_duration_since(started_at).as_micros();
    let end_us = now.saturating_duration_since(started_at).as_micros();
    let first_sample_us = start_us.saturating_add(interval_us);
    if first_sample_us > end_us {
      return;
    }

    let idle_frame = CpuProfileFrame {
      function_name: "(idle)".to_string(),
      url: String::new(),
      line_number: -1,
      column_number: -1,
    };
    let sample_count = ((end_us - first_sample_us) / interval_us) + 1;
    self
      .samples
      .reserve(sample_count.min(usize::MAX as u128) as usize);
    for index in 0..sample_count {
      let timestamp_us = first_sample_us.saturating_add(index * interval_us);
      self.samples.push(CpuProfileSample {
        frames: vec![idle_frame.clone()],
        timestamp_us: timestamp_us.min(u64::MAX as u128) as u64,
      });
    }
    self.last_sample_at = Some(now);
    self.idle_since = Some(now);
  }
}

#[derive(Clone, Debug)]
struct CoverageHit {
  line_number: i32,
  column_number: i32,
  count: u64,
}

#[derive(Clone, Debug)]
struct PreciseCoverageFunction {
  function_name: String,
  url: String,
  source: String,
  start_line: i32,
  start_column: i32,
  call_count: u64,
  locations: HashSet<(i32, i32)>,
  hits: HashMap<u32, CoverageHit>,
  ranges: HashMap<(u32, u32), u64>,
}

#[derive(Default)]
struct PreciseCoverageState {
  functions: HashMap<usize, PreciseCoverageFunction>,
}

thread_local! {
  static PRECISE_COVERAGE: std::cell::RefCell<PreciseCoverageState> =
    std::cell::RefCell::new(PreciseCoverageState::default());
}

unsafe fn borrowed_utf8(ptr: *const std::ffi::c_char, len: usize) -> String {
  if ptr.is_null() || len == 0 {
    return String::new();
  }
  String::from_utf8_lossy(unsafe {
    std::slice::from_raw_parts(ptr.cast::<u8>(), len)
  })
  .into_owned()
}

unsafe fn borrowed_cstr(ptr: *const std::ffi::c_char) -> String {
  if ptr.is_null() {
    return String::new();
  }
  unsafe { std::ffi::CStr::from_ptr(ptr) }
    .to_string_lossy()
    .into_owned()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn v82jsc_coverage_function(
  key: *const c_void,
  function_name: *const std::ffi::c_char,
  filename: *const std::ffi::c_char,
  source: *const std::ffi::c_char,
  source_len: usize,
  line_number: i32,
  column_number: i32,
) {
  let key = key as usize;
  let function_name = unsafe { borrowed_cstr(function_name) };
  let url = unsafe { borrowed_cstr(filename) };
  let source = unsafe { borrowed_utf8(source, source_len) };
  PRECISE_COVERAGE.with(|state| {
    let mut state = state.borrow_mut();
    let function =
      state
        .functions
        .entry(key)
        .or_insert_with(|| PreciseCoverageFunction {
          function_name: function_name.clone(),
          url: url.clone(),
          source: source.clone(),
          start_line: line_number,
          start_column: column_number,
          call_count: 0,
          locations: HashSet::new(),
          hits: HashMap::new(),
          ranges: HashMap::new(),
        });
    if function.url != url
      || function.start_line != line_number
      || function.start_column != column_number
      || function.source != source
    {
      *function = PreciseCoverageFunction {
        function_name,
        url,
        source,
        start_line: line_number,
        start_column: column_number,
        call_count: 0,
        locations: HashSet::new(),
        hits: HashMap::new(),
        ranges: HashMap::new(),
      };
    }
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v82jsc_coverage_function_hit(key: *const c_void) {
  PRECISE_COVERAGE.with(|state| {
    if let Some(function) =
      state.borrow_mut().functions.get_mut(&(key as usize))
    {
      function.call_count = function.call_count.saturating_add(1);
    }
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v82jsc_coverage_location(
  key: *const c_void,
  line_number: i32,
  column_number: i32,
) {
  PRECISE_COVERAGE.with(|state| {
    if let Some(function) =
      state.borrow_mut().functions.get_mut(&(key as usize))
    {
      function.locations.insert((line_number, column_number));
    }
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v82jsc_coverage_hit(
  key: *const c_void,
  pc: u32,
  line_number: i32,
  column_number: i32,
) {
  PRECISE_COVERAGE.with(|state| {
    if let Some(function) =
      state.borrow_mut().functions.get_mut(&(key as usize))
    {
      let hit = function.hits.entry(pc).or_insert(CoverageHit {
        line_number,
        column_number,
        count: 0,
      });
      hit.count = hit.count.saturating_add(1);
    }
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v82jsc_coverage_range(
  key: *const c_void,
  start: u32,
  end: u32,
) {
  PRECISE_COVERAGE.with(|state| {
    if let Some(function) =
      state.borrow_mut().functions.get_mut(&(key as usize))
    {
      function.ranges.entry((start, end)).or_insert(0);
    }
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v82jsc_coverage_range_hit(
  key: *const c_void,
  start: u32,
  end: u32,
) {
  PRECISE_COVERAGE.with(|state| {
    if let Some(function) =
      state.borrow_mut().functions.get_mut(&(key as usize))
    {
      let count = function.ranges.entry((start, end)).or_insert(0);
      *count = count.saturating_add(1);
    }
  });
}

fn start_precise_coverage(ctx: *mut JSContext) {
  PRECISE_COVERAGE.with(|state| state.borrow_mut().functions.clear());
  unsafe { JS_SetPreciseCoverageEnabled(ctx, true) };
}

fn stop_precise_coverage(ctx: *mut JSContext) {
  unsafe { JS_SetPreciseCoverageEnabled(ctx, false) };
  PRECISE_COVERAGE.with(|state| state.borrow_mut().functions.clear());
}

fn take_precise_coverage() -> Vec<PreciseCoverageFunction> {
  PRECISE_COVERAGE.with(|state| {
    std::mem::take(&mut state.borrow_mut().functions)
      .into_values()
      .collect()
  })
}

#[cfg(all(test, feature = "link_quickjs"))]
mod coverage_dispatch_tests {
  use super::*;

  #[test]
  fn instrumented_dispatch_records_coverage() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      start_precise_coverage(ctx);
      let source = c"function add(a, b) { return a + b; } add(1, 2);";
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"coverage-dispatch.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert_ne!(result.tag, JS_TAG_EXCEPTION);
      JS_FreeValue(ctx, result);

      let functions = take_precise_coverage();
      assert!(functions.iter().any(|function| {
        function.url == "coverage-dispatch.js"
          && function.call_count > 0
          && function.hits.values().any(|hit| hit.count > 0)
      }));

      stop_precise_coverage(ctx);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }
}

unsafe extern "C" fn collect_cpu_profile_frame(
  opaque: *mut c_void,
  function_name: *const std::ffi::c_char,
  filename: *const std::ffi::c_char,
  line_num: i32,
  column_num: i32,
) {
  let frames = unsafe { &mut *opaque.cast::<Vec<CpuProfileFrame>>() };
  let function_name = if function_name.is_null() {
    String::new()
  } else {
    unsafe { std::ffi::CStr::from_ptr(function_name) }
      .to_string_lossy()
      .into_owned()
  };
  let url = if filename.is_null() {
    String::new()
  } else {
    unsafe { std::ffi::CStr::from_ptr(filename) }
      .to_string_lossy()
      .into_owned()
  };
  frames.push(CpuProfileFrame {
    function_name,
    url,
    line_number: line_num.saturating_sub(1),
    column_number: column_num.saturating_sub(1),
  });
}

pub(crate) unsafe fn maybe_collect_cpu_profile_sample(
  isolate: *mut RealIsolate,
  runtime: *mut JSRuntime,
) {
  if isolate.is_null() || runtime.is_null() {
    return;
  }
  let now = Instant::now();
  let should_sample = {
    let profiler = &crate::quickjs::core::iso_state(isolate).cpu_profiler;
    profiler.active
      && profiler
        .last_sample_at
        .is_none_or(|last| now.duration_since(last) >= profiler.interval)
  };
  if !should_sample {
    return;
  }

  let mut frames = Vec::new();
  unsafe {
    JS_VisitStackFrames(
      runtime,
      Some(collect_cpu_profile_frame),
      (&mut frames as *mut Vec<CpuProfileFrame>).cast(),
    );
  }
  if frames.is_empty() {
    return;
  }

  let profiler = &mut crate::quickjs::core::iso_state(isolate).cpu_profiler;
  let Some(started_at) = profiler.started_at else {
    return;
  };
  profiler.last_sample_at = Some(now);
  profiler.samples.push(CpuProfileSample {
    frames,
    timestamp_us: now
      .duration_since(started_at)
      .as_micros()
      .min(u64::MAX as u128) as u64,
  });
}

use crate::inspector::RawV8Inspector;
use crate::inspector::RawV8InspectorClient;
use crate::inspector::RawV8InspectorSession;
use crate::inspector::StringBuffer;
use crate::inspector::StringView;
use crate::inspector::V8InspectorClientTrustLevel;
use crate::inspector::V8StackTrace;

#[repr(C)]
pub struct RawChannel {
  _cxx_vtable: *const Opaque,
}

#[repr(C)]
struct InertObj {
  _cxx_vtable: *const Opaque,
}

#[repr(C)]
struct InspectorState {
  _cxx_vtable: *const Opaque,
  client: *mut RawV8InspectorClient,
  context_group_id: int,
  channel: *mut RawChannel,
  context_name: String,
  context_aux_data: String,
  /// Trust level of the session last connected on `channel`. Since V8 15.8
  /// remote object ids handed to untrusted sessions carry isolate id 0.
  session_trusted: bool,
}

#[derive(Clone, Copy)]
struct ScheduledPause {
  client: *mut RawV8InspectorClient,
  context_group_id: int,
  channel: *mut RawChannel,
  scheduled: bool,
}

thread_local! {
  static ACTIVE_INSPECTOR_CLIENT:
    std::cell::RefCell<Option<(*mut RawV8InspectorClient, int)>> =
      const { std::cell::RefCell::new(None) };
  static ACTIVE_RUNTIME_CHANNEL: std::cell::RefCell<Option<*mut RawChannel>> =
    const { std::cell::RefCell::new(None) };
  static SCHEDULED_PAUSE: std::cell::RefCell<Option<ScheduledPause>> =
    const { std::cell::RefCell::new(None) };
}

unsafe extern "C" {
  fn v8_inspector__V8InspectorClient__BASE__generateUniqueId(
    this: *mut RawV8InspectorClient,
  ) -> i64;
  fn v8_inspector__V8InspectorClient__BASE__runMessageLoopOnPause(
    this: *mut RawV8InspectorClient,
    context_group_id: int,
  );
  fn v8_inspector__V8InspectorClient__BASE__quitMessageLoopOnPause(
    this: *mut RawV8InspectorClient,
  );
  fn v8_inspector__V8InspectorClient__BASE__consoleAPIMessage(
    this: *mut RawV8InspectorClient,
    context_group_id: int,
    level: int,
    message: &StringView,
    url: &StringView,
    line_number: u32,
    column_number: u32,
    stack_trace: &mut V8StackTrace,
  );
  fn v8_inspector__V8Inspector__Channel__BASE__sendNotification(
    this: *mut RawChannel,
    message: UniquePtr<StringBuffer>,
  );
  fn v8_inspector__V8Inspector__Channel__BASE__flushProtocolNotifications(
    this: *mut RawChannel,
  );
  fn JS_DefinePropertyValueStr(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: *const std::ffi::c_char,
    val: JSValue,
    flags: std::ffi::c_int,
  ) -> std::ffi::c_int;
  fn JS_TriggerInterrupt(ctx: *mut JSContext);
}

#[inline]
fn alloc_inert<T>() -> *mut T {
  Box::into_raw(Box::new(InertObj {
    _cxx_vtable: std::ptr::null(),
  }))
  .cast::<T>()
}

#[inline]
unsafe fn free_inert<T>(p: *mut T) {
  if !p.is_null() {
    unsafe { drop(Box::from_raw(p.cast::<InertObj>())) };
  }
}

pub(crate) fn emit_console_api_message(level: int, message: String) {
  let Some((client, context_group_id)) =
    ACTIVE_INSPECTOR_CLIENT.with(|slot| *slot.borrow())
  else {
    return;
  };
  if client.is_null() {
    return;
  }

  let message_bytes = message.into_bytes();
  let message_view = StringView::from(message_bytes.as_slice());
  let url_bytes: [u8; 0] = [];
  let url_view = StringView::from(&url_bytes[..]);
  let stack_trace = alloc_inert::<V8StackTrace>();
  if stack_trace.is_null() {
    return;
  }
  unsafe {
    v8_inspector__V8InspectorClient__BASE__consoleAPIMessage(
      client,
      context_group_id,
      level,
      &message_view,
      &url_view,
      0,
      0,
      &mut *stack_trace,
    );
    free_inert(stack_trace);
  }
}

fn json_string(value: &str) -> String {
  let mut out = String::with_capacity(value.len() + 2);
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
  out
}

unsafe fn js_value_to_string(
  ctx: *mut JSContext,
  value: JSValue,
) -> Option<String> {
  let mut len = 0usize;
  let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, value) };
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

unsafe fn js_exception_description(
  ctx: *mut JSContext,
  value: JSValue,
) -> String {
  let stack = unsafe { JS_GetPropertyStr(ctx, value, c"stack".as_ptr()) };
  let description = if stack.tag == JS_TAG_EXCEPTION {
    let exception = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exception) };
    None
  } else {
    unsafe { js_value_to_string(ctx, stack) }
  };
  unsafe { JS_FreeValue(ctx, stack) };
  description
    .or_else(|| unsafe { js_value_to_string(ctx, value) })
    .unwrap_or_else(|| "Error".to_string())
}

unsafe fn exception_description_and_message(
  exception: *const Value,
) -> (String, String) {
  let ctx = crate::quickjs::core::current_ctx();
  if ctx.is_null() || exception.is_null() {
    return ("Error".to_string(), String::new());
  }
  let value = crate::quickjs::core::jsval_of(exception);
  let description = unsafe { js_exception_description(ctx, value) };
  let message_value =
    unsafe { JS_GetPropertyStr(ctx, value, c"message".as_ptr()) };
  let message = if message_value.tag == JS_TAG_EXCEPTION {
    String::new()
  } else {
    unsafe { js_value_to_string(ctx, message_value) }.unwrap_or_default()
  };
  unsafe { JS_FreeValue(ctx, message_value) };
  (description, message)
}

fn send_channel_notification(channel: *mut RawChannel, json: String) {
  if channel.is_null() {
    return;
  }
  let units: Vec<u16> = json.encode_utf16().collect();
  let buf = RealStringBuffer::boxed_from_utf16(units);
  let unique = unsafe { UniquePtr::from_raw(buf) };
  unsafe {
    v8_inspector__V8Inspector__Channel__BASE__sendNotification(channel, unique)
  };
}

fn activate_runtime_session(channel: *mut RawChannel) {
  if !channel.is_null() {
    ACTIVE_RUNTIME_CHANNEL.with(|slot| *slot.borrow_mut() = Some(channel));
  }
}

pub(crate) fn script_source_registered(url: &str, source: &str) {
  cdp::script_source_registered(url, source);
}

fn deactivate_runtime_session(channel: *mut RawChannel) {
  ACTIVE_RUNTIME_CHANNEL.with(|slot| {
    let mut slot = slot.borrow_mut();
    if *slot == Some(channel) {
      *slot = None;
    }
  });
}

unsafe fn console_remote_object_json(
  ctx: *mut JSContext,
  value: JSValue,
) -> String {
  if jsv_is_undefined(&value) {
    return r#"{"type":"undefined"}"#.to_string();
  }
  if jsv_is_null(&value) {
    return r#"{"type":"object","subtype":"null","value":null}"#.to_string();
  }
  if jsv_is_bool(&value) {
    return format!(
      r#"{{"type":"boolean","value":{}}}"#,
      unsafe { JS_ToBool(ctx, value) } != 0
    );
  }
  if jsv_is_number(&value) {
    let mut number = 0.0;
    if unsafe { JS_ToFloat64(ctx, &mut number, value) } == 0 {
      if number.is_finite() {
        return format!(r#"{{"type":"number","value":{number}}}"#);
      }
      let value = if number.is_nan() {
        "NaN"
      } else if number.is_sign_negative() {
        "-Infinity"
      } else {
        "Infinity"
      };
      return format!(
        r#"{{"type":"number","unserializableValue":{}}}"#,
        json_string(value)
      );
    }
  }
  if jsv_is_string(&value) {
    let value = unsafe { js_value_to_string(ctx, value) }.unwrap_or_default();
    return format!(r#"{{"type":"string","value":{}}}"#, json_string(&value));
  }
  if jsv_is_bigint(&value) {
    let value = unsafe { js_value_to_string(ctx, value) }.unwrap_or_default();
    return format!(
      r#"{{"type":"bigint","unserializableValue":{},"description":{}}}"#,
      json_string(&format!("{value}n")),
      json_string(&format!("{value}n"))
    );
  }
  if jsv_is_symbol(&value) {
    return r#"{"type":"symbol","description":"Symbol()"}"#.to_string();
  }
  if unsafe { JS_IsFunction(ctx, value) } {
    return r#"{"type":"function","className":"Function","description":"Function"}"#.to_string();
  }
  r#"{"type":"object","className":"Object","description":"Object"}"#.to_string()
}

fn console_event_type(magic: std::ffi::c_int) -> &'static str {
  match magic {
    1 => "debug",
    2 => "info",
    3 => "warning",
    4 => "error",
    5 => "dir",
    6 => "dirxml",
    7 => "table",
    8 => "trace",
    9 => "startGroup",
    10 => "startGroupCollapsed",
    11 => "endGroup",
    12 => "clear",
    13 => "assert",
    _ => "log",
  }
}

unsafe fn emit_console_api_called(
  ctx: *mut JSContext,
  magic: std::ffi::c_int,
  argc: std::ffi::c_int,
  argv: *mut JSValue,
) {
  let Some(channel) = ACTIVE_RUNTIME_CHANNEL.with(|slot| *slot.borrow()) else {
    return;
  };
  if channel.is_null() {
    return;
  }

  let mut args = Vec::with_capacity(argc.max(1) as usize);
  if !argv.is_null() {
    for index in 0..argc.max(0) as usize {
      args.push(unsafe { console_remote_object_json(ctx, *argv.add(index)) });
    }
  }
  if magic == 11 && args.is_empty() {
    args.push(r#"{"type":"string","value":"console.groupEnd"}"#.to_string());
  }
  let timestamp = SystemTime::now()
    .duration_since(SystemTime::UNIX_EPOCH)
    .unwrap_or_default()
    .as_secs_f64()
    * 1_000.0;
  send_channel_notification(
    channel,
    format!(
      r#"{{"method":"Runtime.consoleAPICalled","params":{{"type":{},"args":[{}],"executionContextId":1,"timestamp":{timestamp}}}}}"#,
      json_string(console_event_type(magic)),
      args.join(",")
    ),
  );
  unsafe {
    v8_inspector__V8Inspector__Channel__BASE__flushProtocolNotifications(
      channel,
    );
  }
}

unsafe extern "C" fn inspector_console_wrapper(
  ctx: *mut JSContext,
  this_val: JSValue,
  argc: std::ffi::c_int,
  argv: *mut JSValue,
  magic: std::ffi::c_int,
  func_data: *mut JSValue,
) -> JSValue {
  unsafe { emit_console_api_called(ctx, magic, argc, argv) };
  if func_data.is_null() {
    return jsv_undefined();
  }
  unsafe { JS_Call(ctx, *func_data, this_val, argc, argv) }
}

unsafe fn install_console_api_wrappers(ctx: *mut JSContext) {
  let global = unsafe { JS_GetGlobalObject(ctx) };
  let console = unsafe { JS_GetPropertyStr(ctx, global, c"console".as_ptr()) };
  unsafe { JS_FreeValue(ctx, global) };
  if !jsv_is_object(&console) {
    unsafe { JS_FreeValue(ctx, console) };
    return;
  }

  let marker = unsafe {
    JS_GetPropertyStr(ctx, console, c"__v8x_inspector_wrapped__".as_ptr())
  };
  let already_wrapped = unsafe { JS_ToBool(ctx, marker) } != 0;
  unsafe { JS_FreeValue(ctx, marker) };
  if already_wrapped {
    unsafe { JS_FreeValue(ctx, console) };
    return;
  }

  for (name, magic) in [
    (c"log", 0),
    (c"debug", 1),
    (c"info", 2),
    (c"warn", 3),
    (c"error", 4),
    (c"dir", 5),
    (c"dirxml", 6),
    (c"table", 7),
    (c"trace", 8),
    (c"group", 9),
    (c"groupCollapsed", 10),
    (c"groupEnd", 11),
    (c"clear", 12),
    (c"assert", 13),
  ] {
    let original = unsafe { JS_GetPropertyStr(ctx, console, name.as_ptr()) };
    if unsafe { JS_IsFunction(ctx, original) } {
      let mut data = [original];
      let wrapper = unsafe {
        JS_NewCFunctionData(
          ctx,
          inspector_console_wrapper,
          0,
          magic,
          data.len() as std::ffi::c_int,
          data.as_mut_ptr(),
        )
      };
      if !jsv_is_exception(&wrapper) {
        unsafe { JS_SetPropertyStr(ctx, console, name.as_ptr(), wrapper) };
      }
    }
    unsafe { JS_FreeValue(ctx, original) };
  }
  unsafe {
    JS_DefinePropertyValueStr(
      ctx,
      console,
      c"__v8x_inspector_wrapped__".as_ptr(),
      JS_NewBool(ctx, 1),
      JS_PROP_CONFIGURABLE,
    );
    JS_FreeValue(ctx, console);
  }
}

fn schedule_pause_on_next_statement(
  client: *mut RawV8InspectorClient,
  context_group_id: int,
  channel: *mut RawChannel,
) {
  if client.is_null() || channel.is_null() {
    return;
  }
  SCHEDULED_PAUSE.with(|slot| {
    *slot.borrow_mut() = Some(ScheduledPause {
      client,
      context_group_id,
      channel,
      scheduled: true,
    });
  });
  let ctx = crate::quickjs::core::current_ctx();
  if !ctx.is_null() {
    unsafe { JS_TriggerInterrupt(ctx) };
  }
}

fn cancel_pause_on_next_statement(
  client: *mut RawV8InspectorClient,
  channel: *mut RawChannel,
) {
  SCHEDULED_PAUSE.with(|slot| {
    let mut slot = slot.borrow_mut();
    if matches!(
      *slot,
      Some(ScheduledPause {
        client: scheduled_client,
        channel: scheduled_channel,
        ..
      }) if scheduled_client == client && scheduled_channel == channel
    ) {
      if let Some(state) = slot.as_mut() {
        state.scheduled = false;
      }
    }
  });
}

fn activate_debugger_session(
  client: *mut RawV8InspectorClient,
  context_group_id: int,
  channel: *mut RawChannel,
) {
  if client.is_null() || channel.is_null() {
    return;
  }
  SCHEDULED_PAUSE.with(|slot| {
    *slot.borrow_mut() = Some(ScheduledPause {
      client,
      context_group_id,
      channel,
      scheduled: false,
    });
  });
}

fn deactivate_debugger_session(
  client: *mut RawV8InspectorClient,
  channel: *mut RawChannel,
) {
  SCHEDULED_PAUSE.with(|slot| {
    let mut slot = slot.borrow_mut();
    if matches!(
      *slot,
      Some(ScheduledPause {
        client: active_client,
        channel: active_channel,
        ..
      }) if active_client == client && active_channel == channel
    ) {
      *slot = None;
    }
  });
}

thread_local! {
  static PAUSE_RESUMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn note_pause_resumed() {
  PAUSE_RESUMED.with(|c| c.set(true));
}

fn pause_inspector(state: ScheduledPause) {
  if state.client.is_null() || state.channel.is_null() {
    return;
  }

  unsafe {
    let _ =
      v8_inspector__V8InspectorClient__BASE__generateUniqueId(state.client);
  }
  send_channel_notification(
    state.channel,
    r#"{"method":"Debugger.paused","params":{"callFrames":[],"reason":"other","hitBreakpoints":[]}}"#.to_string(),
  );
  PAUSE_RESUMED.with(|c| c.set(false));
  unsafe {
    v8_inspector__V8Inspector__Channel__BASE__flushProtocolNotifications(
      state.channel,
    );
    v8_inspector__V8InspectorClient__BASE__runMessageLoopOnPause(
      state.client,
      state.context_group_id,
    );
  }
  // When the embedder's message loop returns without an explicit
  // Debugger.resume, V8 still reports the pause as over.
  if !PAUSE_RESUMED.with(|c| c.get()) {
    send_channel_notification(
      state.channel,
      r#"{"method":"Debugger.resumed","params":{}}"#.to_string(),
    );
  }
}

pub(crate) fn maybe_pause_on_next_statement() {
  let Some(state) = SCHEDULED_PAUSE.with(|slot| {
    let mut slot = slot.borrow_mut();
    let state = slot.as_mut()?;
    if !state.scheduled {
      return None;
    }
    state.scheduled = false;
    Some(*state)
  }) else {
    return;
  };

  pause_inspector(state);
}

#[unsafe(no_mangle)]
pub extern "C" fn v82jsc_debugger_statement() {
  let Some(state) = SCHEDULED_PAUSE.with(|slot| *slot.borrow()) else {
    return;
  };
  pause_inspector(state);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__Channel__BASE__CONSTRUCT(
  buf: *mut MaybeUninit<RawChannel>,
) {
  if !buf.is_null() {
    unsafe {
      (*buf).write(RawChannel {
        _cxx_vtable: std::ptr::null(),
      });
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__Channel__sendResponse(
  _this: *mut RawChannel,
  _call_id: int,
  _message: UniquePtr<StringBuffer>,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__Channel__sendNotification(
  _this: *mut RawChannel,
  _message: UniquePtr<StringBuffer>,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__Channel__flushProtocolNotifications(
  _this: *mut RawChannel,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__BASE__CONSTRUCT(
  buf: *mut MaybeUninit<RawV8InspectorClient>,
) {
  if !buf.is_null() {
    unsafe {
      std::ptr::write_bytes(buf, 0, 1);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__generateUniqueId(
  _this: *mut RawV8InspectorClient,
) -> i64 {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__runMessageLoopOnPause(
  _this: *mut RawV8InspectorClient,
  _context_group_id: int,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__quitMessageLoopOnPause(
  _this: *mut RawV8InspectorClient,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__runIfWaitingForDebugger(
  _this: *mut RawV8InspectorClient,
  _context_group_id: int,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorClient__consoleAPIMessage(
  _this: *mut RawV8InspectorClient,
  _context_group_id: int,
  _level: int,
  _message: &StringView,
  _url: &StringView,
  _line_number: u32,
  _column_number: u32,
  _stack_trace: &mut V8StackTrace,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__DELETE(
  this: *mut RawV8InspectorSession,
) {
  if !this.is_null() {
    unsafe { drop(Box::from_raw(this.cast::<cdp::CdpSession>())) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__dispatchProtocolMessage(
  session: *mut RawV8InspectorSession,
  message: StringView,
) {
  if session.is_null() {
    return;
  }
  let sess = unsafe { &mut *(session.cast::<cdp::CdpSession>()) };
  let msg = string_view_to_string(&message);
  cdp::dispatch(sess, &msg);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__schedulePauseOnNextStatement(
  session: *mut RawV8InspectorSession,
  _break_reason: StringView,
  _break_details: StringView,
) {
  if session.is_null() {
    return;
  }
  let sess = unsafe { &*(session.cast::<cdp::CdpSession>()) };
  sess.schedule_pause_on_next_statement();
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__cancelPauseOnNextStatement(
  session: *mut RawV8InspectorSession,
) {
  if session.is_null() {
    return;
  }
  let sess = unsafe { &*(session.cast::<cdp::CdpSession>()) };
  sess.cancel_pause_on_next_statement();
}

/// An owned `Runtime.RemoteObject`. V8 hands back an opaque C++ object that
/// serializes on demand; this backend serializes eagerly and carries the bytes,
/// which is all `toBytes` needs.
pub(crate) struct RemoteObjectBytes(Vec<u8>);

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__wrapObject(
  session: *mut RawV8InspectorSession,
  context: *const Context,
  value: *const Value,
  object_group: StringView,
  _generate_preview: bool,
) -> *mut RemoteObjectBytes {
  // Values that are not primitives get an `objectId`, under which the session
  // keeps them alive until their object group is released, so that
  // `unwrapObject` can hand them back. The preview flag is not honoured: the
  // description is all this backend reports either way.
  let ctx = crate::quickjs::core::ctx_of(context);
  if ctx.is_null() || value.is_null() {
    return std::ptr::null_mut();
  }
  let v = crate::quickjs::core::jsval_of(value);
  let mut json = unsafe { console_remote_object_json(ctx, v) };
  let by_reference = v.tag == JS_TAG_OBJECT || jsv_is_symbol(&v);
  if by_reference && !session.is_null() && json.starts_with('{') {
    let sess = unsafe { &mut *(session.cast::<cdp::CdpSession>()) };
    let group = string_view_to_string(&object_group);
    let id = sess.retain_wrapped(ctx, v, group);
    json.insert_str(1, &format!(r#""objectId":"{id}","#));
  }
  Box::into_raw(Box::new(RemoteObjectBytes(json.into_bytes())))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__unwrapObject(
  session: *mut RawV8InspectorSession,
  error: *mut *mut StringBuffer,
  object_id: StringView,
  value: *mut *const Value,
  context: *mut *const Context,
  object_group: *mut *mut StringBuffer,
) -> bool {
  let id = string_view_to_string(&object_id);
  let found = if session.is_null() {
    None
  } else {
    let sess = unsafe { &*(session.cast::<cdp::CdpSession>()) };
    sess.unwrap(&id)
  };
  let Some((v, ctx, group)) = found else {
    if !error.is_null() {
      let units = "Invalid remote object id".encode_utf16().collect();
      unsafe { *error = RealStringBuffer::boxed_from_utf16(units) };
    }
    return false;
  };
  unsafe {
    if !value.is_null() {
      *value = crate::quickjs::core::intern_dup::<Value>(ctx, v);
    }
    if !context.is_null() {
      *context = crate::quickjs::core::intern_ctx(ctx);
    }
    if !object_group.is_null() {
      let units = group.encode_utf16().collect();
      *object_group = RealStringBuffer::boxed_from_utf16(units);
    }
  }
  true
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__RemoteObject__DELETE(
  this: *mut RemoteObjectBytes,
) {
  if !this.is_null() {
    drop(unsafe { Box::from_raw(this) });
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__RemoteObject__toBytes(
  this: *const RemoteObjectBytes,
) -> *mut crate::crdtp_shim::CppVecU8 {
  let bytes = if this.is_null() {
    Vec::new()
  } else {
    unsafe { &*this }.0.clone()
  };
  Box::into_raw(Box::new(bytes))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__releaseObjectGroup(
  session: *mut RawV8InspectorSession,
  object_group: StringView,
) {
  if session.is_null() {
    return;
  }
  let sess = unsafe { &mut *(session.cast::<cdp::CdpSession>()) };
  sess.release_group(&string_view_to_string(&object_group));
}

unsafe extern "C" {
  /// Defined by the vendored `inspector.rs`; releases the boxed Rust closure
  /// an `Inspectable` was built from.
  fn v8_inspector__V8InspectorSession__Inspectable__BASE__DROP(
    rust_impl: *mut c_void,
  );
}

/// Owns the Rust closure behind an `Inspectable` until it is deleted.
pub(crate) struct InspectableHandle(*mut c_void);

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__Inspectable__NEW(
  rust_impl: *mut c_void,
) -> *mut InspectableHandle {
  Box::into_raw(Box::new(InspectableHandle(rust_impl)))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__Inspectable__DELETE(
  inspectable: *mut InspectableHandle,
) {
  if inspectable.is_null() {
    return;
  }
  let handle = unsafe { Box::from_raw(inspectable) };
  if !handle.0.is_null() {
    unsafe {
      v8_inspector__V8InspectorSession__Inspectable__BASE__DROP(handle.0);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__addInspectedObject(
  _session: *mut RawV8InspectorSession,
  inspectable: *mut InspectableHandle,
) {
  // V8 keeps the object so the console can reach it through `$0` to `$4`. This
  // backend serves no console history, so nothing can ever dereference it.
  // Ownership still transfers here, so release it rather than leak it.
  v8_inspector__V8InspectorSession__Inspectable__DELETE(inspectable);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__canDispatchMethod(
  method: StringView,
) -> bool {
  // A method "Domain.command" is dispatchable iff its domain is one of the
  // protocol domains V8InspectorSession serves built-in. Mirror V8's set.
  let s: String = if let Some(b) = method.characters8() {
    b.iter().map(|&c| c as char).collect()
  } else if let Some(u) = method.characters16() {
    String::from_utf16_lossy(u)
  } else {
    String::new()
  };
  let domain = s.split('.').next().unwrap_or("");
  matches!(
    domain,
    "Runtime" | "Debugger" | "Profiler" | "HeapProfiler" | "Console" | "Schema"
  )
}

#[repr(C)]
struct RealStringBuffer {
  _vtable: *const Opaque,
  units: Vec<u16>,
}

impl RealStringBuffer {
  fn boxed_from_utf16(units: Vec<u16>) -> *mut StringBuffer {
    Box::into_raw(Box::new(RealStringBuffer {
      _vtable: std::ptr::null(),
      units,
    }))
    .cast::<StringBuffer>()
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__StringBuffer__DELETE(this: *mut StringBuffer) {
  if !this.is_null() {
    unsafe { drop(Box::from_raw(this.cast::<RealStringBuffer>())) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__StringBuffer__string(
  this: &StringBuffer,
) -> StringView<'_> {
  let rb =
    unsafe { &*(this as *const StringBuffer as *const RealStringBuffer) };
  StringView::from(rb.units.as_slice())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__StringBuffer__create(
  source: StringView,
) -> UniquePtr<StringBuffer> {
  let units = string_view_to_utf16(&source);
  unsafe { UniquePtr::from_raw(RealStringBuffer::boxed_from_utf16(units)) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__DELETE(this: *mut RawV8Inspector) {
  if !this.is_null() {
    unsafe { drop(Box::from_raw(this.cast::<InspectorState>())) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__create(
  _isolate: *mut RealIsolate,
  client: *mut RawV8InspectorClient,
) -> *mut RawV8Inspector {
  Box::into_raw(Box::new(InspectorState {
    _cxx_vtable: std::ptr::null(),
    client,
    context_group_id: 1,
    channel: std::ptr::null_mut(),
    context_name: "main realm".to_string(),
    context_aux_data: r#"{"isDefault":true,"type":"default"}"#.to_string(),
    session_trusted: true,
  }))
  .cast::<RawV8Inspector>()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__connect(
  inspector: *mut RawV8Inspector,
  _context_group_id: int,
  channel: *mut RawChannel,
  _state: StringView,
  _client_trust_level: V8InspectorClientTrustLevel,
) -> *mut RawV8InspectorSession {
  let mut client = std::ptr::null_mut();
  let mut context_name = String::new();
  let mut context_aux_data = String::new();
  if !inspector.is_null() {
    let state = unsafe { &mut *inspector.cast::<InspectorState>() };
    state.channel = channel;
    state.session_trusted = matches!(
      _client_trust_level,
      V8InspectorClientTrustLevel::FullyTrusted
    );
    client = state.client;
    context_name.clone_from(&state.context_name);
    context_aux_data.clone_from(&state.context_aux_data);
  }
  let mut sess = Box::new(cdp::CdpSession::new(
    channel,
    client,
    _context_group_id,
    context_name,
    context_aux_data,
  ));
  cdp::register_session(&mut *sess);
  Box::into_raw(sess).cast::<RawV8InspectorSession>()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__contextCreated(
  this: *mut RawV8Inspector,
  _context: *const Context,
  contextGroupId: int,
  humanReadableName: StringView,
  auxData: StringView,
) {
  if this.is_null() {
    return;
  }
  let state = unsafe { &mut *this.cast::<InspectorState>() };
  state.context_group_id = contextGroupId;
  state.context_name =
    String::from_utf16_lossy(&string_view_to_utf16(&humanReadableName));
  state.context_aux_data =
    String::from_utf16_lossy(&string_view_to_utf16(&auxData));
  ACTIVE_INSPECTOR_CLIENT.with(|slot| {
    *slot.borrow_mut() = Some((state.client, contextGroupId));
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__contextDestroyed(
  this: *mut RawV8Inspector,
  _context: *const Context,
) {
  if this.is_null() {
    return;
  }
  let state = unsafe { &*this.cast::<InspectorState>() };
  // V8 only emits Runtime.executionContextDestroyed to sessions that enabled
  // the Runtime domain; an unenabled session gets no event (and may not even
  // be re-entrant, e.g. a test holding a borrow across contextDestroyed).
  let runtime_active =
    ACTIVE_RUNTIME_CHANNEL.with(|slot| *slot.borrow() == Some(state.channel));
  if !state.channel.is_null() && runtime_active {
    send_channel_notification(
      state.channel,
      r#"{"method":"Runtime.executionContextDestroyed","params":{"executionContextId":1,"executionContextUniqueId":"1"}}"#.to_string(),
    );
    unsafe {
      v8_inspector__V8Inspector__Channel__BASE__flushProtocolNotifications(
        state.channel,
      );
    }
  }
  deactivate_runtime_session(state.channel);
  ACTIVE_INSPECTOR_CLIENT.with(|slot| {
    let mut slot = slot.borrow_mut();
    if matches!(*slot, Some((client, _)) if client == state.client) {
      *slot = None;
    }
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__idleStarted(
  _this: *mut RawV8Inspector,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__idleFinished(
  _this: *mut RawV8Inspector,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__asyncTaskScheduled(
  _this: *mut RawV8Inspector,
  _task_name: StringView,
  _task: *const c_void,
  _recurring: bool,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__asyncTaskCanceled(
  _this: *mut RawV8Inspector,
  _task: *const c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__asyncTaskStarted(
  _this: *mut RawV8Inspector,
  _task: *const c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__asyncTaskFinished(
  _this: *mut RawV8Inspector,
  _task: *const c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__allAsyncTasksCanceled(
  _this: *mut RawV8Inspector,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__exceptionThrown(
  this: *mut RawV8Inspector,
  _context: *const Context,
  message: StringView,
  _exception: *const Value,
  _detailed_message: StringView,
  url: StringView,
  _line_number: u32,
  _column_number: u32,
  _stack_trace: *mut V8StackTrace,
  _script_id: int,
) -> u32 {
  if this.is_null() {
    return 0;
  }
  let state = unsafe { &*this.cast::<InspectorState>() };
  let text = string_view_to_string(&message);
  let url = string_view_to_string(&url);
  let (description, exception_message) =
    unsafe { exception_description_and_message(_exception) };
  let json = format!(
    "{{\"method\":\"Runtime.exceptionThrown\",\"params\":{{\"timestamp\":0,\"exceptionDetails\":{{\"exceptionId\":1,\"text\":{},\"lineNumber\":0,\"columnNumber\":0,\"scriptId\":\"1\",\"url\":{},\"exception\":{{\"type\":\"object\",\"subtype\":\"error\",\"className\":\"Error\",\"description\":{},\"objectId\":\"{}.1.1\",\"preview\":{{\"type\":\"object\",\"subtype\":\"error\",\"description\":{},\"overflow\":false,\"properties\":[{{\"name\":\"stack\",\"type\":\"string\",\"value\":{}}},{{\"name\":\"message\",\"type\":\"string\",\"value\":{}}}]}}}},\"executionContextId\":1}}}}}}",
    json_string(&text),
    json_string(&url),
    json_string(&description),
    if state.session_trusted { 1 } else { 0 },
    json_string(&description),
    json_string(&description),
    json_string(&exception_message),
  );
  send_channel_notification(state.channel, json);
  1
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__createStackTrace(
  _this: *mut RawV8Inspector,
  _stack_trace: *const StackTrace,
) -> *mut V8StackTrace {
  std::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8StackTrace__DELETE(this: *mut V8StackTrace) {
  unsafe { free_inert(this) };
}

pub(crate) mod cdp {
  use super::CompletedCpuProfile;
  use super::CpuProfileFrame;
  use super::PreciseCoverageFunction;
  use super::RawChannel;
  use super::RawV8InspectorClient;
  use super::RealStringBuffer;
  use crate::inspector::StringBuffer;
  use crate::quickjs::core::current_ctx;
  use crate::quickjs::core::current_iso;
  use crate::quickjs::core::iso_state;
  use crate::quickjs::core::register_script_source;
  use crate::quickjs::core::replace_script_source;
  use crate::quickjs::core::script_source;
  use crate::quickjs::core::script_sources;
  use crate::quickjs::quickjs_sys::*;
  use crate::support::UniquePtr;
  use crate::support::int;
  use std::collections::BTreeMap;
  use std::collections::HashMap;
  use std::collections::HashSet;
  use std::ffi::{CStr, CString};
  use std::os::raw::c_char;
  use std::sync::atomic::Ordering;

  #[repr(C)]
  struct JSPropertyEnum {
    is_enumerable: bool,
    atom: JSAtom,
  }

  const JS_GPN_STRING_MASK: i32 = 1 << 0;

  struct TerminationSuppression {
    isolate: *mut crate::isolate::RealIsolate,
    restore: bool,
  }

  impl TerminationSuppression {
    fn new() -> Self {
      let isolate = current_iso();
      let restore = !isolate.is_null()
        && iso_state(isolate).terminating.swap(false, Ordering::AcqRel);
      Self { isolate, restore }
    }

    fn restore(&mut self) {
      if self.restore {
        iso_state(self.isolate)
          .terminating
          .store(true, Ordering::Release);
        self.restore = false;
      }
    }
  }

  impl Drop for TerminationSuppression {
    fn drop(&mut self) {
      self.restore();
    }
  }

  unsafe extern "C" {
    fn JS_GetOwnPropertyNames(
      ctx: *mut JSContext,
      ptab: *mut *mut JSPropertyEnum,
      plen: *mut u32,
      obj: JSValue,
      flags: i32,
    ) -> i32;
    fn JS_FreePropertyEnum(
      ctx: *mut JSContext,
      tab: *mut JSPropertyEnum,
      len: u32,
    );
    fn JS_AtomToCStringLen(
      ctx: *mut JSContext,
      plen: *mut usize,
      atom: JSAtom,
    ) -> *const c_char;
  }

  thread_local! {
    static ACTIVE_SESSIONS: std::cell::RefCell<Vec<*mut CdpSession>> =
      const { std::cell::RefCell::new(Vec::new()) };
  }

  unsafe extern "C" {
    fn v8_inspector__V8Inspector__Channel__BASE__sendResponse(
      this: *mut RawChannel,
      call_id: int,
      message: UniquePtr<StringBuffer>,
    );
    fn v8_inspector__V8Inspector__Channel__BASE__sendNotification(
      this: *mut RawChannel,
      message: UniquePtr<StringBuffer>,
    );
  }

  pub struct CdpSession {
    channel: *mut RawChannel,
    client: *mut RawV8InspectorClient,
    context_group_id: int,
    context_name: String,
    context_aux_data: String,
    next_obj_id: u64,
    next_script_id: u64,
    debugger_enabled: bool,

    objects: HashMap<u64, JSValue>,
    /// For objects handed out by `wrapObject`: the context they were wrapped
    /// in and their object group, which `unwrapObject` reports back.
    wrapped: HashMap<u64, (usize, String)>,
    pending_evaluations: Vec<PendingEvaluation>,
    scripts: HashMap<String, String>,
    script_ids_by_url: HashMap<String, String>,
  }

  struct PendingEvaluation {
    call_id: i32,
    kind: PendingEvaluationKind,
    promise: JSValue,
  }

  #[derive(Clone, Copy)]
  enum PendingEvaluationKind {
    AsyncEval,
    Promise,
  }

  impl CdpSession {
    pub fn new(
      channel: *mut RawChannel,
      client: *mut RawV8InspectorClient,
      context_group_id: int,
      context_name: String,
      context_aux_data: String,
    ) -> Self {
      CdpSession {
        channel,
        client,
        context_group_id,
        context_name,
        context_aux_data,
        next_obj_id: 1,
        next_script_id: 1,
        debugger_enabled: false,
        objects: HashMap::new(),
        wrapped: HashMap::new(),
        pending_evaluations: Vec::new(),
        scripts: HashMap::new(),
        script_ids_by_url: HashMap::new(),
      }
    }
    pub fn schedule_pause_on_next_statement(&self) {
      super::schedule_pause_on_next_statement(
        self.client,
        self.context_group_id,
        self.channel,
      );
    }
    pub fn cancel_pause_on_next_statement(&self) {
      super::cancel_pause_on_next_statement(self.client, self.channel);
    }
    fn resume(&self) {
      super::note_pause_resumed();
      super::send_channel_notification(
        self.channel,
        r#"{"method":"Debugger.resumed","params":{}}"#.to_string(),
      );
      unsafe {
        super::v8_inspector__V8Inspector__Channel__BASE__flushProtocolNotifications(
          self.channel,
        );
        super::v8_inspector__V8InspectorClient__BASE__quitMessageLoopOnPause(
          self.client,
        );
      }
    }
    fn step_over(&self) {
      self.schedule_pause_on_next_statement();
      self.resume();
    }
    fn register_script(&mut self, url: &str, source: &str) -> bool {
      if let Some(script_id) = self.script_ids_by_url.get(url) {
        self.scripts.insert(script_id.clone(), source.to_string());
        return false;
      }
      let script_id = self.next_script_id.to_string();
      self.next_script_id += 1;
      self
        .script_ids_by_url
        .insert(url.to_string(), script_id.clone());
      self.scripts.insert(script_id.clone(), source.to_string());
      if !self.debugger_enabled {
        return false;
      }
      let (end_line, end_column) = source
        .rsplit_once('\n')
        .map(|(lines, last)| {
          (
            lines.bytes().filter(|b| *b == b'\n').count() + 1,
            last.len(),
          )
        })
        .unwrap_or((0, source.len()));
      super::send_channel_notification(
        self.channel,
        format!(
          r#"{{"method":"Debugger.scriptParsed","params":{{"scriptId":{},"url":{},"startLine":0,"startColumn":0,"endLine":{end_line},"endColumn":{end_column},"executionContextId":1,"hash":""}}}}"#,
          super::json_string(&script_id),
          super::json_string(url),
        ),
      );
      true
    }
    fn enable_debugger(&mut self) {
      super::activate_debugger_session(
        self.client,
        self.context_group_id,
        self.channel,
      );
      self.debugger_enabled = true;
      let mut sources = script_sources();
      sources.sort_by(|(left, _), (right, _)| left.cmp(right));
      let mut notified = false;
      for (url, source) in sources {
        notified |= self.register_script(&url, &source);
      }
      // Flush only when scriptParsed notifications were actually queued —
      // V8 sends nothing on Debugger.enable in a fresh isolate.
      if notified {
        unsafe {
          super::v8_inspector__V8Inspector__Channel__BASE__flushProtocolNotifications(
            self.channel,
          );
        }
      }
    }
    fn disable_debugger(&mut self) {
      self.debugger_enabled = false;
      super::deactivate_debugger_session(self.client, self.channel);
    }
    fn enable_runtime(&self) {
      super::activate_runtime_session(self.channel);
    }
    fn retain(&mut self, ctx: *mut JSContext, v: JSValue) -> u64 {
      let id = self.next_obj_id;
      self.next_obj_id += 1;
      self.objects.insert(id, unsafe { JS_DupValue(ctx, v) });
      id
    }

    /// Keeps `v` reachable under a fresh remote object id, for `wrapObject`.
    pub fn retain_wrapped(
      &mut self,
      ctx: *mut JSContext,
      v: JSValue,
      group: String,
    ) -> u64 {
      let id = self.retain(ctx, v);
      self.wrapped.insert(id, (ctx as usize, group));
      id
    }

    /// The value, context and object group behind a `wrapObject` id.
    pub fn unwrap(
      &self,
      object_id: &str,
    ) -> Option<(JSValue, *mut JSContext, String)> {
      let id = object_id.parse::<u64>().ok()?;
      let value = *self.objects.get(&id)?;
      let (ctx, group) = self.wrapped.get(&id)?;
      Some((value, *ctx as *mut JSContext, group.clone()))
    }

    /// Drops every remote object wrapped under `group`.
    pub fn release_group(&mut self, group: &str) {
      let ids: Vec<u64> = self
        .wrapped
        .iter()
        .filter(|(_, (_, g))| g == group)
        .map(|(&id, _)| id)
        .collect();
      for id in ids {
        let Some((ctx, _)) = self.wrapped.remove(&id) else {
          continue;
        };
        if let Some(v) = self.objects.remove(&id) {
          unsafe { JS_FreeValue(ctx as *mut JSContext, v) };
        }
      }
    }
  }

  impl Drop for CdpSession {
    fn drop(&mut self) {
      unregister_session(self);
      super::deactivate_debugger_session(self.client, self.channel);
      super::deactivate_runtime_session(self.channel);
      let ctx = current_ctx();
      if !ctx.is_null() {
        for (_, v) in self.objects.drain() {
          unsafe { JS_FreeValue(ctx, v) };
        }
        for pending in self.pending_evaluations.drain(..) {
          unsafe { JS_FreeValue(ctx, pending.promise) };
        }
      }
    }
  }

  pub fn register_session(session: *mut CdpSession) {
    ACTIVE_SESSIONS.with(|sessions| sessions.borrow_mut().push(session));
  }

  fn unregister_session(session: *mut CdpSession) {
    ACTIVE_SESSIONS.with(|sessions| {
      sessions.borrow_mut().retain(|active| *active != session);
    });
  }

  pub fn script_source_registered(url: &str, source: &str) {
    ACTIVE_SESSIONS.with(|sessions| {
      for session in sessions.borrow().iter().copied() {
        let Some(session) = (unsafe { session.as_mut() }) else {
          continue;
        };
        if !session.debugger_enabled {
          continue;
        }
        // No flush here: V8 batches scriptParsed events into the next
        // protocol flush (e.g. the pause sequence) instead of flushing per
        // compiled script; the notification itself is already delivered.
        session.register_script(url, source);
      }
    });
  }

  unsafe fn set_str(ctx: *mut JSContext, obj: JSValue, key: &CStr, val: &str) {
    let v =
      unsafe { JS_NewStringLen(ctx, val.as_ptr() as *const c_char, val.len()) };
    unsafe { JS_SetPropertyStr(ctx, obj, key.as_ptr(), v) };
  }
  unsafe fn set_val(
    ctx: *mut JSContext,
    obj: JSValue,
    key: &CStr,
    val: JSValue,
  ) {
    unsafe { JS_SetPropertyStr(ctx, obj, key.as_ptr(), val) };
  }
  unsafe fn set_bool(ctx: *mut JSContext, obj: JSValue, key: &CStr, b: bool) {
    unsafe {
      JS_SetPropertyStr(ctx, obj, key.as_ptr(), JS_NewBool(ctx, b as i32))
    };
  }

  unsafe fn get_str(
    ctx: *mut JSContext,
    obj: JSValue,
    key: &CStr,
  ) -> Option<String> {
    let v = unsafe { JS_GetPropertyStr(ctx, obj, key.as_ptr()) };
    if jsv_is_exception(&v) {
      unsafe { drain_exc(ctx) };
      return None;
    }
    if !jsv_is_string(&v) {
      unsafe { JS_FreeValue(ctx, v) };
      return None;
    }
    let s = unsafe { cstr_value(ctx, v) };
    unsafe { JS_FreeValue(ctx, v) };
    s
  }

  unsafe fn get_int(
    ctx: *mut JSContext,
    obj: JSValue,
    key: &CStr,
  ) -> Option<i64> {
    let v = unsafe { JS_GetPropertyStr(ctx, obj, key.as_ptr()) };
    if jsv_is_exception(&v) {
      unsafe { drain_exc(ctx) };
      return None;
    }
    if jsv_is_undefined(&v) || jsv_is_null(&v) {
      unsafe { JS_FreeValue(ctx, v) };
      return None;
    }
    let mut out: i32 = 0;
    let rc = unsafe { JS_ToInt32(ctx, &mut out, v) };
    unsafe { JS_FreeValue(ctx, v) };
    if rc < 0 {
      unsafe { drain_exc(ctx) };
      None
    } else {
      Some(out as i64)
    }
  }

  unsafe fn get_bool(
    ctx: *mut JSContext,
    obj: JSValue,
    key: &CStr,
  ) -> Option<bool> {
    let v = unsafe { JS_GetPropertyStr(ctx, obj, key.as_ptr()) };
    if jsv_is_exception(&v) {
      unsafe { drain_exc(ctx) };
      return None;
    }
    let result = unsafe { JS_ToBool(ctx, v) };
    unsafe { JS_FreeValue(ctx, v) };
    (result >= 0).then_some(result != 0)
  }

  unsafe fn cstr_value(ctx: *mut JSContext, v: JSValue) -> Option<String> {
    let p = unsafe { JS_ToCString(ctx, v) };
    if p.is_null() {
      unsafe { drain_exc(ctx) };
      return None;
    }
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    unsafe { JS_FreeCString(ctx, p) };
    Some(s)
  }

  unsafe fn drain_exc(ctx: *mut JSContext) {
    if unsafe { JS_HasException(ctx) } {
      let e = unsafe { JS_GetException(ctx) };
      unsafe { JS_FreeValue(ctx, e) };
    }
  }

  unsafe fn type_of(ctx: *mut JSContext, v: JSValue) -> &'static str {
    if jsv_is_undefined(&v) {
      "undefined"
    } else if jsv_is_null(&v) {
      "object"
    } else if jsv_is_bool(&v) {
      "boolean"
    } else if jsv_is_number(&v) {
      "number"
    } else if jsv_is_string(&v) {
      "string"
    } else if jsv_is_bigint(&v) {
      "bigint"
    } else if jsv_is_symbol(&v) {
      "symbol"
    } else if unsafe { JS_IsFunction(ctx, v) } {
      "function"
    } else {
      "object"
    }
  }

  unsafe fn remote_object(
    sess: &mut CdpSession,
    ctx: *mut JSContext,
    val: JSValue,
  ) -> JSValue {
    let ro = unsafe { JS_NewObject(ctx) };
    let t = unsafe { type_of(ctx, val) };
    unsafe { set_str(ctx, ro, c"type", t) };
    match t {
      "undefined" => {}
      "boolean" => unsafe {
        set_val(ctx, ro, c"value", JS_DupValue(ctx, val));
      },
      "number" => unsafe {
        set_val(ctx, ro, c"value", JS_DupValue(ctx, val));
        if let Some(d) = cstr_value(ctx, val) {
          set_str(ctx, ro, c"description", &d);
        }
      },
      "string" => unsafe {
        set_val(ctx, ro, c"value", JS_DupValue(ctx, val));
      },
      "bigint" => unsafe {
        if let Some(d) = cstr_value(ctx, val) {
          let disp = format!("{d}n");
          set_str(ctx, ro, c"unserializableValue", &disp);
          set_str(ctx, ro, c"description", &disp);
        }
      },
      "symbol" => unsafe {
        if let Some(d) = cstr_value(ctx, val) {
          set_str(ctx, ro, c"description", &d);
        }
        let id = sess.retain(ctx, val);
        set_str(ctx, ro, c"objectId", &id.to_string());
      },
      "function" => unsafe {
        set_str(ctx, ro, c"className", "Function");
        if let Some(d) = cstr_value(ctx, val) {
          set_str(ctx, ro, c"description", &d);
        }
        let id = sess.retain(ctx, val);
        set_str(ctx, ro, c"objectId", &id.to_string());
      },
      _ => unsafe {
        if jsv_is_null(&val) {
          set_str(ctx, ro, c"subtype", "null");
          set_val(ctx, ro, c"value", jsv_null());
        } else {
          let cname = ctor_name(ctx, val);
          set_str(ctx, ro, c"className", &cname);

          let is_err = is_error(ctx, val);
          let desc = if is_err {
            set_str(ctx, ro, c"subtype", "error");
            super::js_exception_description(ctx, val)
          } else {
            object_description(ctx, val, &cname)
          };
          set_str(ctx, ro, c"description", &desc);
          let id = sess.retain(ctx, val);
          set_str(ctx, ro, c"objectId", &id.to_string());
        }
      },
    }
    ro
  }

  unsafe fn is_error(ctx: *mut JSContext, val: JSValue) -> bool {
    let stack = unsafe { JS_GetPropertyStr(ctx, val, c"stack".as_ptr()) };
    let is = jsv_is_string(&stack);
    if jsv_is_exception(&stack) {
      unsafe { drain_exc(ctx) };
    }
    unsafe { JS_FreeValue(ctx, stack) };
    is
  }

  unsafe fn ctor_name(ctx: *mut JSContext, val: JSValue) -> String {
    let ctor = unsafe { JS_GetPropertyStr(ctx, val, c"constructor".as_ptr()) };
    if jsv_is_exception(&ctor) {
      unsafe { drain_exc(ctx) };
      return "Object".to_string();
    }
    if !jsv_is_object(&ctor) {
      unsafe { JS_FreeValue(ctx, ctor) };
      return "Object".to_string();
    }
    let name = unsafe { JS_GetPropertyStr(ctx, ctor, c"name".as_ptr()) };
    unsafe { JS_FreeValue(ctx, ctor) };
    if jsv_is_string(&name) {
      let s = unsafe { cstr_value(ctx, name) }
        .unwrap_or_else(|| "Object".to_string());
      unsafe { JS_FreeValue(ctx, name) };
      if s.is_empty() {
        "Object".to_string()
      } else {
        s
      }
    } else {
      unsafe { JS_FreeValue(ctx, name) };
      "Object".to_string()
    }
  }

  unsafe fn object_description(
    ctx: *mut JSContext,
    val: JSValue,
    cname: &str,
  ) -> String {
    let len = unsafe { JS_GetPropertyStr(ctx, val, c"length".as_ptr()) };
    let is_arrayish = jsv_is_number(&len);
    let mut n = 0i32;
    if is_arrayish {
      unsafe { JS_ToInt32(ctx, &mut n, len) };
    }
    unsafe { JS_FreeValue(ctx, len) };
    if cname == "Array" {
      format!("Array({n})")
    } else {
      cname.to_string()
    }
  }

  unsafe fn drain_jobs(ctx: *mut JSContext) {
    let rt = unsafe { JS_GetRuntime(ctx) };
    if rt.is_null() {
      return;
    }
    let mut pctx: *mut JSContext = std::ptr::null_mut();
    loop {
      let r = unsafe { JS_ExecutePendingJob(rt, &mut pctx) };
      if r == 0 {
        break;
      }
      if r < 0 {
        unsafe { drain_exc(pctx) };
      }
    }
  }

  unsafe fn poll_async_eval(
    ctx: *mut JSContext,
    promise: JSValue,
    kind: PendingEvaluationKind,
  ) -> Option<JSValue> {
    debug_assert!(jsv_is_object(&promise) && unsafe { JS_IsPromise(promise) });
    unsafe { drain_jobs(ctx) };
    let state = unsafe { JS_PromiseState(ctx, promise) };
    if state == 0 {
      return None;
    }

    let result = unsafe { JS_PromiseResult(ctx, promise) };
    unsafe { JS_FreeValue(ctx, promise) };
    if state == 2 {
      unsafe { JS_Throw(ctx, JS_DupValue(ctx, result)) };
      unsafe { JS_FreeValue(ctx, result) };
      return Some(jsv_exception());
    }

    if !matches!(kind, PendingEvaluationKind::AsyncEval)
      || !jsv_is_object(&result)
    {
      return Some(result);
    }
    let value = unsafe { JS_GetPropertyStr(ctx, result, c"value".as_ptr()) };
    if jsv_is_exception(&value) {
      unsafe { drain_exc(ctx) };
      return Some(result);
    }
    unsafe { JS_FreeValue(ctx, result) };
    Some(value)
  }

  pub(crate) fn poll_pending_evaluations(ctx: *mut JSContext) {
    if ctx.is_null() {
      return;
    }
    ACTIVE_SESSIONS.with(|sessions| {
      for session in sessions.borrow().iter().copied() {
        let Some(session) = (unsafe { session.as_mut() }) else {
          continue;
        };
        let pending = std::mem::take(&mut session.pending_evaluations);
        for evaluation in pending {
          match unsafe {
            poll_async_eval(ctx, evaluation.promise, evaluation.kind)
          } {
            Some(result) => unsafe {
              send_eval_result(session, ctx, result, evaluation.call_id)
            },
            None => session.pending_evaluations.push(evaluation),
          }
        }
      }
    });
  }

  fn send(channel: *mut RawChannel, json: &str, call_id: Option<i32>) {
    let units: Vec<u16> = json.encode_utf16().collect();
    let buf = RealStringBuffer::boxed_from_utf16(units);
    let up = unsafe { UniquePtr::from_raw(buf) };
    unsafe {
      match call_id {
        Some(id) => v8_inspector__V8Inspector__Channel__BASE__sendResponse(
          channel, id, up,
        ),
        None => v8_inspector__V8Inspector__Channel__BASE__sendNotification(
          channel, up,
        ),
      }
    }
  }

  unsafe fn send_obj(
    sess: &CdpSession,
    ctx: *mut JSContext,
    obj: JSValue,
    call_id: Option<i32>,
  ) {
    let to_send = match call_id {
      Some(id) => {
        let env = unsafe { JS_NewObject(ctx) };
        unsafe { set_val(ctx, env, c"id", JS_NewInt32(ctx, id)) };
        unsafe { set_val(ctx, env, c"result", obj) };
        env
      }
      None => obj,
    };
    let s = unsafe {
      JS_JSONStringify(ctx, to_send, jsv_undefined(), jsv_undefined())
    };
    let json = if jsv_is_string(&s) {
      unsafe { cstr_value(ctx, s) }.unwrap_or_else(|| "{}".to_string())
    } else {
      unsafe { drain_exc(ctx) };
      "{}".to_string()
    };
    unsafe { JS_FreeValue(ctx, s) };
    unsafe { JS_FreeValue(ctx, to_send) };
    send(sess.channel, &json, call_id);
  }

  unsafe fn ack(sess: &CdpSession, ctx: *mut JSContext, call_id: i32) {
    let o = unsafe { JS_NewObject(ctx) };
    unsafe { send_obj(sess, ctx, o, Some(call_id)) };
  }

  unsafe fn send_eval_result(
    sess: &mut CdpSession,
    ctx: *mut JSContext,
    outcome: JSValue,
    call_id: i32,
  ) {
    let resp = unsafe { JS_NewObject(ctx) };
    if jsv_is_exception(&outcome) {
      let exc = unsafe { JS_GetException(ctx) };
      let ro = unsafe { remote_object(sess, ctx, exc) };
      unsafe { set_val(ctx, resp, c"result", ro) };
      let ed = unsafe { JS_NewObject(ctx) };
      let text = unsafe { cstr_value(ctx, exc) }
        .unwrap_or_else(|| "Uncaught".to_string());
      unsafe { set_str(ctx, ed, c"text", "Uncaught") };
      unsafe { set_val(ctx, ed, c"exceptionId", JS_NewInt32(ctx, 1)) };
      unsafe { set_val(ctx, ed, c"lineNumber", JS_NewInt32(ctx, 0)) };
      unsafe { set_val(ctx, ed, c"columnNumber", JS_NewInt32(ctx, 0)) };
      let exc_ro = unsafe { remote_object(sess, ctx, exc) };
      unsafe { set_val(ctx, ed, c"exception", exc_ro) };
      let _ = text;
      unsafe { set_val(ctx, resp, c"exceptionDetails", ed) };
      unsafe { JS_FreeValue(ctx, exc) };
    } else {
      let ro = unsafe { remote_object(sess, ctx, outcome) };
      unsafe { set_val(ctx, resp, c"result", ro) };
      unsafe { JS_FreeValue(ctx, outcome) };
    }
    unsafe { send_obj(sess, ctx, resp, Some(call_id)) };
  }

  unsafe fn handle_runtime_enable(
    sess: &CdpSession,
    ctx: *mut JSContext,
    call_id: i32,
  ) {
    sess.enable_runtime();
    unsafe { super::install_console_api_wrappers(ctx) };
    unsafe { ack(sess, ctx, call_id) };
    let notif = unsafe { JS_NewObject(ctx) };
    unsafe {
      set_str(ctx, notif, c"method", "Runtime.executionContextCreated")
    };
    let params = unsafe { JS_NewObject(ctx) };
    let context = unsafe { JS_NewObject(ctx) };
    unsafe { set_val(ctx, context, c"id", JS_NewInt32(ctx, 1)) };
    unsafe { set_str(ctx, context, c"origin", "") };
    unsafe { set_str(ctx, context, c"name", &sess.context_name) };
    unsafe { set_str(ctx, context, c"uniqueId", "1") };
    let aux = CString::new(sess.context_aux_data.as_str())
      .ok()
      .map(|json| unsafe {
        JS_ParseJSON(
          ctx,
          json.as_ptr(),
          sess.context_aux_data.len(),
          c"<context-aux-data>".as_ptr(),
        )
      })
      .filter(|value| !jsv_is_exception(value))
      .unwrap_or_else(|| {
        unsafe { drain_exc(ctx) };
        let aux = unsafe { JS_NewObject(ctx) };
        unsafe { set_bool(ctx, aux, c"isDefault", true) };
        aux
      });
    unsafe { set_val(ctx, context, c"auxData", aux) };
    unsafe { set_val(ctx, params, c"context", context) };
    unsafe { set_val(ctx, notif, c"params", params) };
    unsafe { send_obj(sess, ctx, notif, None) };
  }

  unsafe fn handle_runtime_get_heap_usage(
    sess: &CdpSession,
    ctx: *mut JSContext,
    call_id: i32,
  ) {
    let mut usage = JSMemoryUsage::default();
    let runtime = unsafe { JS_GetRuntime(ctx) };
    if !runtime.is_null() {
      unsafe { JS_ComputeMemoryUsage(runtime, &mut usage) };
    }
    let used_size = usage.malloc_size.max(usage.memory_used_size).max(0);
    let total_size = usage.malloc_limit.max(used_size).max(0);
    let response = unsafe { JS_NewObject(ctx) };
    unsafe {
      set_val(
        ctx,
        response,
        c"usedSize",
        JS_NewFloat64(ctx, used_size as f64),
      );
      set_val(
        ctx,
        response,
        c"totalSize",
        JS_NewFloat64(ctx, total_size as f64),
      );
      send_obj(sess, ctx, response, Some(call_id));
    }
  }

  unsafe fn handle_debugger_get_script_source(
    sess: &CdpSession,
    ctx: *mut JSContext,
    params: JSValue,
    call_id: i32,
  ) {
    let script_id = unsafe { get_str(ctx, params, c"scriptId") };
    let source = script_id
      .as_ref()
      .and_then(|script_id| sess.scripts.get(script_id))
      .cloned()
      .unwrap_or_default();
    let response = unsafe { JS_NewObject(ctx) };
    unsafe {
      set_str(ctx, response, c"scriptSource", &source);
      send_obj(sess, ctx, response, Some(call_id));
    }
  }

  unsafe fn handle_debugger_set_script_source(
    sess: &mut CdpSession,
    ctx: *mut JSContext,
    params: JSValue,
    call_id: i32,
  ) {
    let script_id = unsafe { get_str(ctx, params, c"scriptId") };
    let source = unsafe { get_str(ctx, params, c"scriptSource") };
    let response = unsafe { JS_NewObject(ctx) };

    let Some((script_id, source)) = script_id.zip(source) else {
      unsafe {
        set_str(ctx, response, c"status", "BlockedByTopLevelEsModuleChange");
        send_obj(sess, ctx, response, Some(call_id));
      }
      return;
    };
    if !sess.scripts.contains_key(&script_id) {
      unsafe {
        set_str(ctx, response, c"status", "BlockedByTopLevelEsModuleChange");
        send_obj(sess, ctx, response, Some(call_id));
      }
      return;
    }

    let Some(url) = sess
      .script_ids_by_url
      .iter()
      .find_map(|(url, id)| (id == &script_id).then(|| url.clone()))
    else {
      unsafe {
        set_str(ctx, response, c"status", "BlockedByTopLevelEsModuleChange");
        send_obj(sess, ctx, response, Some(call_id));
      }
      return;
    };

    let source_c = CString::new(source.as_str())
      .unwrap_or_else(|_| CString::new("").expect("empty CString"));
    let url_c = CString::new(url.as_str())
      .unwrap_or_else(|_| CString::new("<live-edit>").expect("static CString"));
    let compiled = unsafe {
      JS_Eval(
        ctx,
        source_c.as_ptr(),
        source.len(),
        url_c.as_ptr(),
        JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY,
      )
    };
    if jsv_is_exception(&compiled) {
      let exception = unsafe { JS_GetException(ctx) };
      let description = unsafe { cstr_value(ctx, exception) }
        .unwrap_or_else(|| "SyntaxError".to_string());
      let details = unsafe { JS_NewObject(ctx) };
      unsafe {
        set_str(ctx, response, c"status", "CompileError");
        set_str(ctx, details, c"text", &format!("Uncaught {description}"));
        set_val(ctx, response, c"exceptionDetails", details);
        JS_FreeValue(ctx, exception);
      }
    } else {
      let status =
        unsafe { v82jsc_live_edit_module(ctx, url_c.as_ptr(), compiled) };
      unsafe {
        JS_FreeValue(ctx, compiled);
        set_str(
          ctx,
          response,
          c"status",
          match status {
            0 => "Ok",
            1 => "BlockedByActiveFunction",
            _ => "BlockedByTopLevelEsModuleChange",
          },
        );
      }
      if status == 0 {
        sess.scripts.insert(script_id, source.clone());
        replace_script_source(&url, &source);
      }
    }
    unsafe { send_obj(sess, ctx, response, Some(call_id)) };
  }

  unsafe fn handle_evaluate(
    sess: &mut CdpSession,
    ctx: *mut JSContext,
    params: JSValue,
    call_id: i32,
  ) {
    let expr =
      unsafe { get_str(ctx, params, c"expression") }.unwrap_or_default();
    register_script_source("<anonymous>", &expr);
    let throw_on_side_effect =
      unsafe { get_bool(ctx, params, c"throwOnSideEffect") }.unwrap_or(false);
    let repl_mode =
      unsafe { get_bool(ctx, params, c"replMode") }.unwrap_or(false);
    let cexpr = CString::new(expr.as_str())
      .unwrap_or_else(|_| CString::new("undefined").unwrap());
    let mut val = if throw_on_side_effect {
      unsafe {
        v82jsc_eval_no_side_effect(
          ctx,
          cexpr.as_ptr(),
          expr.len(),
          c"<anonymous>".as_ptr(),
        )
      }
    } else if repl_mode {
      unsafe {
        v82jsc_eval_repl(
          ctx,
          cexpr.as_ptr(),
          expr.len(),
          c"<anonymous>".as_ptr(),
          JS_EVAL_TYPE_GLOBAL,
        )
      }
    } else {
      unsafe {
        JS_Eval(
          ctx,
          cexpr.as_ptr(),
          expr.len(),
          c"<anonymous>".as_ptr(),
          JS_EVAL_TYPE_GLOBAL,
        )
      }
    };

    let mut was_async = false;
    if !throw_on_side_effect && jsv_is_exception(&val) && expr.contains("await")
    {
      unsafe { drain_exc(ctx) };

      was_async = true;
      val = if repl_mode {
        unsafe {
          v82jsc_eval_repl(
            ctx,
            cexpr.as_ptr(),
            expr.len(),
            c"<anonymous>".as_ptr(),
            JS_EVAL_TYPE_GLOBAL | JS_EVAL_FLAG_ASYNC,
          )
        }
      } else {
        unsafe {
          JS_Eval(
            ctx,
            cexpr.as_ptr(),
            expr.len(),
            c"<anonymous>".as_ptr(),
            JS_EVAL_TYPE_GLOBAL | JS_EVAL_FLAG_ASYNC,
          )
        }
      };
    }
    if was_async && !jsv_is_exception(&val) && unsafe { JS_IsPromise(val) } {
      if let Some(result) =
        unsafe { poll_async_eval(ctx, val, PendingEvaluationKind::AsyncEval) }
      {
        unsafe { send_eval_result(sess, ctx, result, call_id) };
      } else {
        sess.pending_evaluations.push(PendingEvaluation {
          call_id,
          kind: PendingEvaluationKind::AsyncEval,
          promise: val,
        });
      }
      return;
    }
    unsafe { send_eval_result(sess, ctx, val, call_id) };
  }

  fn is_indexed_property(name: &str) -> bool {
    name
      .parse::<u32>()
      .ok()
      .is_some_and(|index| index != u32::MAX && index.to_string() == name)
  }

  unsafe fn collect_property_names(
    ctx: *mut JSContext,
    value: JSValue,
    include_prototypes: bool,
    non_indexed_only: bool,
  ) -> Vec<String> {
    if !jsv_is_object(&value) {
      return Vec::new();
    }

    let mut names = Vec::new();
    let mut seen_names = HashSet::new();
    let mut seen_objects = HashSet::new();
    let mut current = unsafe { JS_DupValue(ctx, value) };

    for _ in 0..64 {
      let object_key = unsafe { current.u.ptr } as usize;
      if object_key == 0 || !seen_objects.insert(object_key) {
        break;
      }

      let mut properties = std::ptr::null_mut();
      let mut property_count = 0u32;
      let result = unsafe {
        JS_GetOwnPropertyNames(
          ctx,
          &mut properties,
          &mut property_count,
          current,
          JS_GPN_STRING_MASK,
        )
      };
      if result < 0 {
        unsafe { drain_exc(ctx) };
        break;
      }
      for index in 0..property_count as usize {
        let atom = unsafe { (*properties.add(index)).atom };
        let mut text_len = 0usize;
        let text = unsafe { JS_AtomToCStringLen(ctx, &mut text_len, atom) };
        if text.is_null() {
          unsafe { drain_exc(ctx) };
          continue;
        }
        let name = String::from_utf8_lossy(unsafe {
          std::slice::from_raw_parts(text as *const u8, text_len)
        })
        .into_owned();
        unsafe { JS_FreeCString(ctx, text) };
        if (!non_indexed_only || !is_indexed_property(&name))
          && seen_names.insert(name.clone())
        {
          names.push(name);
        }
      }
      unsafe { JS_FreePropertyEnum(ctx, properties, property_count) };

      if !include_prototypes {
        break;
      }
      let prototype = unsafe { JS_GetPrototype(ctx, current) };
      unsafe { JS_FreeValue(ctx, current) };
      if jsv_is_exception(&prototype) {
        unsafe { drain_exc(ctx) };
        return names;
      }
      if jsv_is_null(&prototype) {
        unsafe { JS_FreeValue(ctx, prototype) };
        return names;
      }
      current = prototype;
    }

    unsafe { JS_FreeValue(ctx, current) };
    names
  }

  unsafe fn handle_get_properties(
    sess: &CdpSession,
    ctx: *mut JSContext,
    params: JSValue,
    call_id: i32,
  ) {
    let object_id = unsafe { get_str(ctx, params, c"objectId") }
      .and_then(|id| id.parse::<u64>().ok());
    let own_properties =
      unsafe { get_bool(ctx, params, c"ownProperties") }.unwrap_or(false);
    let non_indexed_only =
      unsafe { get_bool(ctx, params, c"nonIndexedPropertiesOnly") }
        .unwrap_or(false);
    let names = object_id
      .and_then(|id| sess.objects.get(&id).copied())
      .map(|value| unsafe {
        collect_property_names(ctx, value, !own_properties, non_indexed_only)
      })
      .unwrap_or_default();

    let descriptors = unsafe { JS_NewArray(ctx) };
    for (index, name) in names.iter().enumerate() {
      let descriptor = unsafe { JS_NewObject(ctx) };
      unsafe {
        set_str(ctx, descriptor, c"name", name);
        JS_SetPropertyUint32(ctx, descriptors, index as u32, descriptor);
      }
    }
    let response = unsafe { JS_NewObject(ctx) };
    unsafe {
      set_val(ctx, response, c"result", descriptors);
      send_obj(sess, ctx, response, Some(call_id));
    }
  }

  unsafe fn handle_global_lexical_scope_names(
    sess: &CdpSession,
    ctx: *mut JSContext,
    call_id: i32,
  ) {
    let lexical = unsafe { v82jsc_global_var_obj(ctx) };
    let names = if jsv_is_exception(&lexical) {
      unsafe { drain_exc(ctx) };
      Vec::new()
    } else {
      let names = unsafe { collect_property_names(ctx, lexical, false, false) };
      unsafe { JS_FreeValue(ctx, lexical) };
      names
    };
    let values = unsafe { JS_NewArray(ctx) };
    for (index, name) in names.iter().enumerate() {
      let value = unsafe {
        JS_NewStringLen(ctx, name.as_ptr() as *const c_char, name.len())
      };
      unsafe { JS_SetPropertyUint32(ctx, values, index as u32, value) };
    }
    let response = unsafe { JS_NewObject(ctx) };
    unsafe {
      set_val(ctx, response, c"names", values);
      send_obj(sess, ctx, response, Some(call_id));
    }
  }

  unsafe fn handle_call_function_on(
    sess: &mut CdpSession,
    ctx: *mut JSContext,
    params: JSValue,
    call_id: i32,
  ) {
    let decl = unsafe { get_str(ctx, params, c"functionDeclaration") }
      .unwrap_or_default();
    let this_id = unsafe { get_str(ctx, params, c"objectId") }
      .and_then(|s| s.parse::<u64>().ok());
    let await_promise =
      unsafe { get_bool(ctx, params, c"awaitPromise") }.unwrap_or(false);
    let this_val = match this_id.and_then(|id| sess.objects.get(&id).copied()) {
      Some(v) => v,
      None => jsv_undefined(),
    };

    let src = format!("({decl})");
    let csrc = CString::new(src.as_str())
      .unwrap_or_else(|_| CString::new("(()=>{})").unwrap());
    let func = unsafe {
      JS_Eval(
        ctx,
        csrc.as_ptr(),
        src.len(),
        c"<repl-call>".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      )
    };
    if jsv_is_exception(&func) {
      unsafe { send_eval_result(sess, ctx, jsv_exception(), call_id) };
      return;
    }

    let mut args: Vec<JSValue> = Vec::new();
    let arr = unsafe { JS_GetPropertyStr(ctx, params, c"arguments".as_ptr()) };
    if jsv_is_object(&arr) {
      let len = unsafe { JS_GetPropertyStr(ctx, arr, c"length".as_ptr()) };
      let mut n = 0i32;
      unsafe { JS_ToInt32(ctx, &mut n, len) };
      unsafe { JS_FreeValue(ctx, len) };
      for i in 0..n.max(0) as u32 {
        let item = unsafe { JS_GetPropertyUint32(ctx, arr, i) };
        args.push(unsafe { call_arg_to_value(sess, ctx, item) });
        unsafe { JS_FreeValue(ctx, item) };
      }
    }
    unsafe { JS_FreeValue(ctx, arr) };
    let ret = unsafe {
      JS_Call(ctx, func, this_val, args.len() as i32, args.as_mut_ptr())
    };
    for a in &args {
      unsafe { JS_FreeValue(ctx, *a) };
    }
    unsafe { JS_FreeValue(ctx, func) };
    if await_promise && !jsv_is_exception(&ret) && unsafe { JS_IsPromise(ret) }
    {
      if let Some(result) =
        unsafe { poll_async_eval(ctx, ret, PendingEvaluationKind::Promise) }
      {
        unsafe { send_eval_result(sess, ctx, result, call_id) };
      } else {
        sess.pending_evaluations.push(PendingEvaluation {
          call_id,
          kind: PendingEvaluationKind::Promise,
          promise: ret,
        });
      }
      return;
    }
    unsafe { send_eval_result(sess, ctx, ret, call_id) };
  }

  unsafe fn call_arg_to_value(
    sess: &CdpSession,
    ctx: *mut JSContext,
    arg: JSValue,
  ) -> JSValue {
    if let Some(oid) = unsafe { get_str(ctx, arg, c"objectId") }
      .and_then(|s| s.parse::<u64>().ok())
    {
      if let Some(v) = sess.objects.get(&oid) {
        return unsafe { JS_DupValue(ctx, *v) };
      }
    }
    if let Some(u) = unsafe { get_str(ctx, arg, c"unserializableValue") } {
      let cu = CString::new(u.as_str())
        .unwrap_or_else(|_| CString::new("undefined").unwrap());
      let v = unsafe {
        JS_Eval(
          ctx,
          cu.as_ptr(),
          u.len(),
          c"<arg>".as_ptr(),
          JS_EVAL_TYPE_GLOBAL,
        )
      };
      if jsv_is_exception(&v) {
        unsafe { drain_exc(ctx) };
        return jsv_undefined();
      }
      return v;
    }

    let v = unsafe { JS_GetPropertyStr(ctx, arg, c"value".as_ptr()) };
    if jsv_is_exception(&v) {
      unsafe { drain_exc(ctx) };
      return jsv_undefined();
    }
    v
  }

  unsafe fn handle_compile_script(
    sess: &CdpSession,
    ctx: *mut JSContext,
    params: JSValue,
    call_id: i32,
  ) {
    let expr =
      unsafe { get_str(ctx, params, c"expression") }.unwrap_or_default();
    let cexpr =
      CString::new(expr.as_str()).unwrap_or_else(|_| CString::new("").unwrap());
    let r = unsafe {
      JS_Eval(
        ctx,
        cexpr.as_ptr(),
        expr.len(),
        c"<compile>".as_ptr(),
        JS_EVAL_TYPE_GLOBAL | JS_EVAL_FLAG_COMPILE_ONLY,
      )
    };
    let resp = unsafe { JS_NewObject(ctx) };
    if jsv_is_exception(&r) {
      let exc = unsafe { JS_GetException(ctx) };
      let ed = unsafe { JS_NewObject(ctx) };
      unsafe { set_str(ctx, ed, c"text", "SyntaxError") };
      unsafe { set_val(ctx, ed, c"exceptionId", JS_NewInt32(ctx, 1)) };
      unsafe { set_val(ctx, ed, c"lineNumber", JS_NewInt32(ctx, 0)) };
      unsafe { set_val(ctx, ed, c"columnNumber", JS_NewInt32(ctx, 0)) };
      unsafe { JS_FreeValue(ctx, exc) };
      unsafe { set_val(ctx, resp, c"exceptionDetails", ed) };
    } else {
      unsafe { JS_FreeValue(ctx, r) };
    }
    unsafe { send_obj(sess, ctx, resp, Some(call_id)) };
  }

  #[derive(Debug)]
  struct ProfileNode {
    frame: CpuProfileFrame,
    children: Vec<usize>,
    hit_count: u32,
  }

  unsafe fn profile_node_to_value(
    ctx: *mut JSContext,
    node: &ProfileNode,
    id: usize,
    script_id: &str,
  ) -> JSValue {
    let value = unsafe { JS_NewObject(ctx) };
    unsafe { set_val(ctx, value, c"id", JS_NewInt32(ctx, id as i32)) };
    let call_frame = unsafe { JS_NewObject(ctx) };
    unsafe {
      set_str(ctx, call_frame, c"functionName", &node.frame.function_name);
      set_str(ctx, call_frame, c"scriptId", script_id);
      set_str(ctx, call_frame, c"url", &node.frame.url);
      set_val(
        ctx,
        call_frame,
        c"lineNumber",
        JS_NewInt32(ctx, node.frame.line_number),
      );
      set_val(
        ctx,
        call_frame,
        c"columnNumber",
        JS_NewInt32(ctx, node.frame.column_number),
      );
      set_val(ctx, value, c"callFrame", call_frame);
      set_val(ctx, value, c"hitCount", JS_NewUint32(ctx, node.hit_count));
    }
    if !node.children.is_empty() {
      let children = unsafe { JS_NewArray(ctx) };
      for (index, child) in node.children.iter().enumerate() {
        unsafe {
          JS_SetPropertyUint32(
            ctx,
            children,
            index as u32,
            JS_NewInt32(ctx, (*child + 1) as i32),
          )
        };
      }
      unsafe { set_val(ctx, value, c"children", children) };
    }
    value
  }

  unsafe fn cpu_profile_to_value(
    ctx: *mut JSContext,
    profile: CompletedCpuProfile,
  ) -> JSValue {
    let root_frame = CpuProfileFrame {
      function_name: "(root)".to_string(),
      url: String::new(),
      line_number: -1,
      column_number: -1,
    };
    let mut nodes = vec![ProfileNode {
      frame: root_frame,
      children: Vec::new(),
      hit_count: 0,
    }];
    let mut edges: HashMap<(usize, CpuProfileFrame), usize> = HashMap::new();
    let mut sample_node_ids = Vec::with_capacity(profile.samples.len());
    let mut time_deltas = Vec::with_capacity(profile.samples.len());
    let mut previous_timestamp = 0u64;

    for sample in &profile.samples {
      let mut parent = 0usize;
      for frame in sample.frames.iter().rev() {
        let key = (parent, frame.clone());
        let node_id = if let Some(node_id) = edges.get(&key) {
          *node_id
        } else {
          let node_id = nodes.len();
          nodes.push(ProfileNode {
            frame: frame.clone(),
            children: Vec::new(),
            hit_count: 0,
          });
          nodes[parent].children.push(node_id);
          edges.insert(key, node_id);
          node_id
        };
        parent = node_id;
      }
      nodes[parent].hit_count = nodes[parent].hit_count.saturating_add(1);
      sample_node_ids.push(parent + 1);
      time_deltas.push(sample.timestamp_us.saturating_sub(previous_timestamp));
      previous_timestamp = sample.timestamp_us;
    }

    let mut script_ids = HashMap::<String, String>::new();
    let mut next_script_id = 1usize;
    let nodes_value = unsafe { JS_NewArray(ctx) };
    for (index, node) in nodes.iter().enumerate() {
      let script_id = if node.frame.url.is_empty() {
        "0".to_string()
      } else if let Some(script_id) = script_ids.get(&node.frame.url) {
        script_id.clone()
      } else {
        let script_id = next_script_id.to_string();
        next_script_id += 1;
        script_ids.insert(node.frame.url.clone(), script_id.clone());
        script_id
      };
      let node_value =
        unsafe { profile_node_to_value(ctx, node, index + 1, &script_id) };
      unsafe {
        JS_SetPropertyUint32(ctx, nodes_value, index as u32, node_value)
      };
    }

    let samples_value = unsafe { JS_NewArray(ctx) };
    let deltas_value = unsafe { JS_NewArray(ctx) };
    for (index, node_id) in sample_node_ids.into_iter().enumerate() {
      unsafe {
        JS_SetPropertyUint32(
          ctx,
          samples_value,
          index as u32,
          JS_NewInt32(ctx, node_id as i32),
        );
        JS_SetPropertyUint32(
          ctx,
          deltas_value,
          index as u32,
          JS_NewInt64(ctx, time_deltas[index].min(i64::MAX as u64) as i64),
        );
      }
    }

    let value = unsafe { JS_NewObject(ctx) };
    unsafe {
      set_val(ctx, value, c"nodes", nodes_value);
      set_val(
        ctx,
        value,
        c"startTime",
        JS_NewFloat64(ctx, profile.start_time_us as f64),
      );
      set_val(
        ctx,
        value,
        c"endTime",
        JS_NewFloat64(ctx, profile.end_time_us as f64),
      );
      set_val(ctx, value, c"samples", samples_value);
      set_val(ctx, value, c"timeDeltas", deltas_value);
    }
    value
  }

  #[derive(Debug)]
  struct CoverageRangeData {
    start_offset: usize,
    end_offset: usize,
    count: u64,
  }

  #[derive(Debug)]
  struct FunctionCoverageData {
    function_name: String,
    ranges: Vec<CoverageRangeData>,
  }

  struct SourceOffsets {
    source: String,
    utf16_by_byte: Vec<usize>,
    line_starts: Vec<usize>,
    utf16_len: usize,
  }

  impl SourceOffsets {
    fn new(source: String) -> Self {
      let mut utf16_by_byte = vec![0; source.len() + 1];
      let mut line_starts = vec![0];
      let mut utf16_len = 0;
      for (byte_offset, ch) in source.char_indices() {
        let next_byte_offset = byte_offset + ch.len_utf8();
        utf16_by_byte[byte_offset..next_byte_offset].fill(utf16_len);
        utf16_len += ch.len_utf16();
        utf16_by_byte[next_byte_offset] = utf16_len;
        if ch == '\n' {
          line_starts.push(next_byte_offset);
        }
      }
      Self {
        source,
        utf16_by_byte,
        line_starts,
        utf16_len,
      }
    }

    fn line_column(&self, line_number: i32, column_number: i32) -> usize {
      let line_index = line_number.max(1) as usize - 1;
      let Some(&line_start) = self.line_starts.get(line_index) else {
        return self.utf16_len;
      };
      let line_end = self
        .line_starts
        .get(line_index + 1)
        .copied()
        .unwrap_or(self.source.len());
      let column = column_number.max(0) as usize;
      let byte_offset = self.source[line_start..line_end]
        .char_indices()
        .nth(column)
        .map(|(offset, _)| line_start + offset)
        .unwrap_or(line_end);
      self.utf16_by_byte[byte_offset]
    }

    fn byte_offset(&self, byte_offset: usize) -> usize {
      self.utf16_by_byte[byte_offset.min(self.source.len())]
    }
  }

  #[cfg(test)]
  mod source_offsets_tests {
    use super::SourceOffsets;

    #[test]
    fn maps_lines_columns_and_utf8_bytes_to_utf16() {
      let offsets = SourceOffsets::new("a😀\nβ".to_string());

      assert_eq!(offsets.line_column(1, 0), 0);
      assert_eq!(offsets.line_column(1, 1), 1);
      assert_eq!(offsets.line_column(1, 2), 3);
      assert_eq!(offsets.line_column(2, 0), 4);
      assert_eq!(offsets.line_column(2, 1), 5);
      assert_eq!(offsets.byte_offset(2), 1);
      assert_eq!(offsets.byte_offset(5), 3);
      assert_eq!(offsets.byte_offset(8), 5);
    }
  }

  fn source_for_url(url: &str) -> Option<String> {
    script_source(url).or_else(|| {
      let (base, suffix) = url.rsplit_once("#v8x:")?;
      suffix.parse::<u32>().ok()?;
      script_source(base)
    })
  }

  fn inspector_script_url(url: &str) -> String {
    if url.starts_with('/') {
      format!("file://{url}")
    } else {
      url.to_string()
    }
  }

  fn function_coverage_data(
    function: PreciseCoverageFunction,
    script: &SourceOffsets,
  ) -> Option<FunctionCoverageData> {
    let is_script_root = function.source.is_empty();
    let start_offset = if is_script_root {
      0
    } else {
      script
        .line_column(function.start_line, function.start_column)
        .min(script.utf16_len)
    };
    let source_len = function.source.encode_utf16().count();
    let end_offset = if source_len == 0 {
      script.utf16_len
    } else {
      start_offset
        .saturating_add(source_len)
        .min(script.utf16_len)
    };
    if end_offset <= start_offset {
      return None;
    }

    let mut range_counts = function
      .ranges
      .into_iter()
      .filter_map(|((start, end), count)| {
        let start = script.byte_offset(start as usize);
        let end = script.byte_offset(end as usize);
        (start < end && start >= start_offset && end <= end_offset)
          .then_some(((start, end), count))
      })
      .collect::<BTreeMap<_, _>>();
    let call_count = if is_script_root {
      function.call_count.min(1)
    } else {
      function.call_count
    };
    let mut location_counts = BTreeMap::<usize, u64>::new();
    if !is_script_root && range_counts.is_empty() {
      // Function entry has no executable QuickJS bytecode, so it never gets a
      // location hit of its own. Seed it from the invocation count to avoid
      // reporting the prologue of an executed function as an uncovered block.
      location_counts.insert(start_offset, call_count);
      for (line, column) in function.locations {
        let offset = script.line_column(line, column);
        if offset >= start_offset && offset < end_offset {
          location_counts.entry(offset).or_insert(0);
        }
      }
      for hit in function.hits.into_values() {
        let offset = script.line_column(hit.line_number, hit.column_number);
        if offset >= start_offset && offset < end_offset {
          location_counts
            .entry(offset)
            .and_modify(|count| *count = (*count).max(hit.count))
            .or_insert(hit.count);
        }
      }
    }
    let mut ranges = vec![CoverageRangeData {
      start_offset,
      end_offset,
      count: call_count,
    }];
    for ((range_start, range_end), count) in std::mem::take(&mut range_counts) {
      if count == call_count {
        continue;
      }
      if let Some(previous) = ranges.last_mut()
        && range_start <= previous.end_offset
        && previous.count == count
        && previous.start_offset != start_offset
      {
        previous.end_offset = previous.end_offset.max(range_end);
      } else {
        ranges.push(CoverageRangeData {
          start_offset: range_start,
          end_offset: range_end,
          count,
        });
      }
    }
    let points = location_counts.into_iter().collect::<Vec<_>>();
    for (index, (range_start, count)) in points.iter().copied().enumerate() {
      let range_end = points
        .get(index + 1)
        .map(|(offset, _)| *offset)
        .unwrap_or(end_offset);
      if range_start >= range_end
        || count == call_count
        || (range_start == start_offset && range_end == end_offset)
      {
        continue;
      }
      if let Some(previous) = ranges.last_mut()
        && range_start <= previous.end_offset
        && previous.count == count
        && previous.start_offset != start_offset
      {
        previous.end_offset = previous.end_offset.max(range_end);
      } else {
        ranges.push(CoverageRangeData {
          start_offset: range_start,
          end_offset: range_end,
          count,
        });
      }
    }

    Some(FunctionCoverageData {
      function_name: if function.function_name == "<eval>" {
        String::new()
      } else {
        function.function_name
      },
      ranges,
    })
  }

  fn precise_coverage_data(
    functions: Vec<PreciseCoverageFunction>,
  ) -> Vec<(String, Vec<FunctionCoverageData>)> {
    let mut scripts = BTreeMap::<String, Vec<FunctionCoverageData>>::new();
    let mut functions_by_url =
      BTreeMap::<String, Vec<PreciseCoverageFunction>>::new();
    for function in functions {
      if !function.url.is_empty() {
        functions_by_url
          .entry(function.url.clone())
          .or_default()
          .push(function);
      }
    }
    for (source_url, functions) in functions_by_url {
      let Some(script) = source_for_url(&source_url) else {
        continue;
      };
      let script = SourceOffsets::new(script);
      let url = inspector_script_url(&source_url);
      let script_functions = scripts.entry(url).or_default();
      for function in functions {
        if let Some(function) = function_coverage_data(function, &script) {
          script_functions.push(function);
        }
      }
    }

    scripts
      .into_iter()
      .map(|(url, mut functions)| {
        functions.sort_by_key(|function| {
          function
            .ranges
            .first()
            .map(|range| range.start_offset)
            .unwrap_or_default()
        });
        let mut index = 0;
        while index < functions.len() {
          if functions[index].function_name != "<static_initializer>" {
            index += 1;
            continue;
          }
          let Some(root) = functions[index].ranges.first() else {
            index += 1;
            continue;
          };
          let mut cursor = root.end_offset;
          let mut next = index + 1;
          while next < functions.len() {
            let Some(candidate_root) = functions[next].ranges.first() else {
              next += 1;
              continue;
            };
            let candidate_start = candidate_root.start_offset;
            let candidate_end = candidate_root.end_offset;
            let candidate_count = candidate_root.count;
            if candidate_start > cursor.saturating_add(16) {
              break;
            }
            cursor = cursor.max(candidate_end);
            if functions[next].function_name == "<static_initializer>" {
              functions[index].ranges[0].end_offset = cursor;
              functions[index].ranges[0].count =
                functions[index].ranges[0].count.min(candidate_count);
              functions[index].ranges.truncate(1);
              functions.remove(next);
              continue;
            }
            next += 1;
          }
          index += 1;
        }
        (url, functions)
      })
      .collect()
  }

  unsafe fn coverage_range_to_value(
    ctx: *mut JSContext,
    range: CoverageRangeData,
  ) -> JSValue {
    let value = unsafe { JS_NewObject(ctx) };
    unsafe {
      set_val(
        ctx,
        value,
        c"startOffset",
        JS_NewInt64(ctx, range.start_offset.min(i64::MAX as usize) as i64),
      );
      set_val(
        ctx,
        value,
        c"endOffset",
        JS_NewInt64(ctx, range.end_offset.min(i64::MAX as usize) as i64),
      );
      set_val(
        ctx,
        value,
        c"count",
        JS_NewInt64(ctx, range.count.min(i64::MAX as u64) as i64),
      );
    }
    value
  }

  unsafe fn function_coverage_to_value(
    ctx: *mut JSContext,
    function: FunctionCoverageData,
  ) -> JSValue {
    let value = unsafe { JS_NewObject(ctx) };
    let ranges = unsafe { JS_NewArray(ctx) };
    for (index, range) in function.ranges.into_iter().enumerate() {
      unsafe {
        JS_SetPropertyUint32(
          ctx,
          ranges,
          index as u32,
          coverage_range_to_value(ctx, range),
        )
      };
    }
    unsafe {
      set_str(ctx, value, c"functionName", &function.function_name);
      set_val(ctx, value, c"ranges", ranges);
      set_val(ctx, value, c"isBlockCoverage", JS_NewBool(ctx, 1));
    }
    value
  }

  unsafe fn precise_coverage_to_value(
    ctx: *mut JSContext,
    functions: Vec<PreciseCoverageFunction>,
  ) -> JSValue {
    let scripts = precise_coverage_data(functions);
    let result = unsafe { JS_NewArray(ctx) };
    for (script_index, (url, functions)) in scripts.into_iter().enumerate() {
      let script = unsafe { JS_NewObject(ctx) };
      let function_values = unsafe { JS_NewArray(ctx) };
      for (index, function) in functions.into_iter().enumerate() {
        unsafe {
          JS_SetPropertyUint32(
            ctx,
            function_values,
            index as u32,
            function_coverage_to_value(ctx, function),
          )
        };
      }
      unsafe {
        set_str(ctx, script, c"scriptId", &(script_index + 1).to_string());
        set_str(ctx, script, c"url", &url);
        set_val(ctx, script, c"functions", function_values);
        JS_SetPropertyUint32(ctx, result, script_index as u32, script);
      }
    }
    result
  }

  unsafe fn handle_take_precise_coverage(
    sess: &CdpSession,
    ctx: *mut JSContext,
    call_id: i32,
  ) {
    let response = unsafe { JS_NewObject(ctx) };
    let result =
      unsafe { precise_coverage_to_value(ctx, super::take_precise_coverage()) };
    let timestamp = std::time::SystemTime::now()
      .duration_since(std::time::SystemTime::UNIX_EPOCH)
      .unwrap_or_default()
      .as_secs_f64();
    unsafe {
      set_val(ctx, response, c"result", result);
      set_val(ctx, response, c"timestamp", JS_NewFloat64(ctx, timestamp));
      send_obj(sess, ctx, response, Some(call_id));
    }
  }

  unsafe fn handle_start_precise_coverage(
    sess: &CdpSession,
    ctx: *mut JSContext,
    call_id: i32,
  ) {
    super::start_precise_coverage(ctx);
    unsafe { ack(sess, ctx, call_id) };
  }

  unsafe fn handle_profiler_set_sampling_interval(
    sess: &CdpSession,
    ctx: *mut JSContext,
    params: JSValue,
    call_id: i32,
  ) {
    if let Some(interval) = unsafe { get_int(ctx, params, c"interval") } {
      let isolate = current_iso();
      if !isolate.is_null() {
        iso_state(isolate)
          .cpu_profiler
          .set_interval(interval.max(1) as u64);
      }
    }
    unsafe { ack(sess, ctx, call_id) };
  }

  unsafe fn handle_profiler_start(
    sess: &CdpSession,
    ctx: *mut JSContext,
    call_id: i32,
  ) {
    let isolate = current_iso();
    if !isolate.is_null() {
      iso_state(isolate).cpu_profiler.start();
    }
    unsafe { ack(sess, ctx, call_id) };
  }

  unsafe fn handle_profiler_stop(
    sess: &CdpSession,
    ctx: *mut JSContext,
    call_id: i32,
  ) {
    let isolate = current_iso();
    let profile = if isolate.is_null() {
      CompletedCpuProfile {
        start_time_us: 0,
        end_time_us: 0,
        samples: Vec::new(),
      }
    } else {
      iso_state(isolate).cpu_profiler.finish()
    };
    let result = unsafe { JS_NewObject(ctx) };
    let profile = unsafe { cpu_profile_to_value(ctx, profile) };
    unsafe {
      set_val(ctx, result, c"profile", profile);
      send_obj(sess, ctx, result, Some(call_id));
    }
  }

  pub fn dispatch(sess: &mut CdpSession, message: &str) {
    // V8's inspector remains usable while execution termination is pending.
    // QuickJS applies its interrupt flag to the JSON work used by this shim,
    // so clear it while decoding the method and for non-executing profiler
    // commands. Restore it before dispatching every other protocol domain.
    let mut termination = TerminationSuppression::new();
    let ctx = current_ctx();
    if ctx.is_null() {
      return;
    }
    let cmsg = match CString::new(message) {
      Ok(c) => c,
      Err(_) => return,
    };
    let parsed = unsafe {
      JS_ParseJSON(ctx, cmsg.as_ptr(), message.len(), c"<cdp>".as_ptr())
    };
    if jsv_is_exception(&parsed) {
      unsafe { drain_exc(ctx) };
      return;
    }
    let id = unsafe { get_int(ctx, parsed, c"id") }.unwrap_or(0) as i32;
    let method = unsafe { get_str(ctx, parsed, c"method") }.unwrap_or_default();
    let params = unsafe { JS_GetPropertyStr(ctx, parsed, c"params".as_ptr()) };
    if !method.starts_with("Profiler.") {
      termination.restore();
    }

    match method.as_str() {
      "Debugger.enable" => unsafe {
        sess.enable_debugger();
        let response = JS_NewObject(ctx);
        set_str(ctx, response, c"debuggerId", "1");
        send_obj(sess, ctx, response, Some(id));
      },
      "Debugger.disable" => unsafe {
        sess.disable_debugger();
        ack(sess, ctx, id);
      },
      "Debugger.resume" => unsafe {
        ack(sess, ctx, id);
        sess.resume();
      },
      "Debugger.stepOver" => unsafe {
        ack(sess, ctx, id);
        sess.step_over();
      },
      "Debugger.getScriptSource" => unsafe {
        handle_debugger_get_script_source(sess, ctx, params, id)
      },
      "Debugger.setScriptSource" => unsafe {
        handle_debugger_set_script_source(sess, ctx, params, id)
      },
      "Profiler.enable" => unsafe { ack(sess, ctx, id) },
      "Profiler.disable" | "Profiler.stopPreciseCoverage" => unsafe {
        super::stop_precise_coverage(ctx);
        ack(sess, ctx, id)
      },
      "Profiler.startPreciseCoverage" => unsafe {
        handle_start_precise_coverage(sess, ctx, id)
      },
      "Profiler.takePreciseCoverage" => unsafe {
        handle_take_precise_coverage(sess, ctx, id)
      },
      "Profiler.setSamplingInterval" => unsafe {
        handle_profiler_set_sampling_interval(sess, ctx, params, id)
      },
      "Profiler.start" => unsafe { handle_profiler_start(sess, ctx, id) },
      "Profiler.stop" => unsafe { handle_profiler_stop(sess, ctx, id) },
      "Runtime.enable" => unsafe { handle_runtime_enable(sess, ctx, id) },
      "Runtime.getHeapUsage" => unsafe {
        handle_runtime_get_heap_usage(sess, ctx, id)
      },
      "Runtime.evaluate" => unsafe { handle_evaluate(sess, ctx, params, id) },
      "Runtime.getProperties" => unsafe {
        handle_get_properties(sess, ctx, params, id)
      },
      "Runtime.callFunctionOn" => unsafe {
        handle_call_function_on(sess, ctx, params, id)
      },
      "Runtime.compileScript" => unsafe {
        handle_compile_script(sess, ctx, params, id)
      },
      "Runtime.globalLexicalScopeNames" => unsafe {
        handle_global_lexical_scope_names(sess, ctx, id)
      },
      "Runtime.releaseObject" => unsafe {
        if let Some(oid) =
          get_str(ctx, params, c"objectId").and_then(|s| s.parse::<u64>().ok())
        {
          if let Some(v) = sess.objects.remove(&oid) {
            JS_FreeValue(ctx, v);
          }
        }
        ack(sess, ctx, id);
      },

      _ => unsafe { ack(sess, ctx, id) },
    }

    unsafe { JS_FreeValue(ctx, params) };
    unsafe { JS_FreeValue(ctx, parsed) };
  }
}

fn string_view_to_utf16(sv: &StringView<'_>) -> Vec<u16> {
  if let Some(s) = sv.characters16() {
    s.to_vec()
  } else if let Some(s) = sv.characters8() {
    s.iter().map(|&b| b as u16).collect()
  } else {
    Vec::new()
  }
}

fn string_view_to_string(sv: &StringView<'_>) -> String {
  if let Some(s) = sv.characters16() {
    String::from_utf16_lossy(s)
  } else if let Some(s) = sv.characters8() {
    s.iter().map(|&b| b as char).collect()
  } else {
    String::new()
  }
}
