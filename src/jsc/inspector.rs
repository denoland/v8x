#![allow(non_snake_case, unused)]

use crate::Context;
use crate::StackTrace;
use crate::Value;
use crate::isolate::RealIsolate;
use crate::support::Opaque;
use crate::support::UniquePtr;
use crate::support::int;

use std::ffi::c_void;
use std::mem::MaybeUninit;

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
  _session: *mut RawV8InspectorSession,
  _break_reason: StringView,
  _break_details: StringView,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__cancelPauseOnNextStatement(
  _session: *mut RawV8InspectorSession,
) {
}

/// An owned `Runtime.RemoteObject`. V8 hands back an opaque C++ object that
/// serializes on demand; this backend serializes eagerly and carries the bytes,
/// which is all `toBytes` needs.
pub(crate) struct RemoteObjectBytes(Vec<u8>);

/// Wraps `value` in a `Runtime.RemoteObject`. Objects get an `objectId` the
/// session can resolve again with `unwrapObject`, and stay alive until their
/// object group is released, as V8's injected script keeps them.
#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8InspectorSession__wrapObject(
  session: *mut RawV8InspectorSession,
  context: *const Context,
  value: *const Value,
  object_group: StringView,
  _generate_preview: bool,
) -> *mut RemoteObjectBytes {
  use crate::jsc::jsc_sys::*;
  if value.is_null() {
    return std::ptr::null_mut();
  }
  let ctx = if context.is_null() {
    crate::jsc::core::current_ctx()
  } else {
    crate::jsc::core::ctx_of(context) as JSContextRef
  };
  let v = crate::jsc::core::jsval(value);
  let ty = if ctx.is_null() {
    "object"
  } else {
    match unsafe { JSValueGetType(ctx, v) } {
      t if t == kJSTypeUndefined => "undefined",
      t if t == kJSTypeBoolean => "boolean",
      t if t == kJSTypeNumber => "number",
      t if t == kJSTypeString => "string",
      t if t == kJSTypeSymbol => "symbol",
      t if t == kJSTypeBigInt => "bigint",
      t if t == kJSTypeObject
        && unsafe { JSObjectIsFunction(ctx, v as JSObjectRef) } =>
      {
        "function"
      }
      _ => "object",
    }
  };
  let is_object = !ctx.is_null()
    && unsafe { JSValueIsObject(ctx, v) }
    && !unsafe { JSValueIsNull(ctx, v) };
  let json = if is_object && !session.is_null() {
    let sess = unsafe { &mut *(session.cast::<cdp::CdpSession>()) };
    unsafe { JSValueProtect(ctx, v) };
    let id = sess.add_remote_object(cdp::RemoteRef {
      value: v,
      context,
      ctx,
      group: string_view_to_utf16(&object_group),
    });
    format!(r#"{{"type":"{ty}","objectId":"{id}"}}"#)
  } else {
    format!(r#"{{"type":"{ty}"}}"#)
  };
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
    sess
      .remote_object(&id)
      .map(|r| (r.value, r.context, r.ctx, r.group.clone()))
  };
  let Some((v, ctx_handle, ctx, group)) = found else {
    if !error.is_null() {
      let units: Vec<u16> = "Invalid remote object id".encode_utf16().collect();
      unsafe { *error = RealStringBuffer::boxed_from_utf16(units) };
    }
    return false;
  };
  unsafe {
    if !value.is_null() {
      *value = crate::jsc::core::intern_ctx::<Value>(ctx, v);
    }
    if !context.is_null() {
      *context = ctx_handle;
    }
    if !object_group.is_null() {
      *object_group = RealStringBuffer::boxed_from_utf16(group);
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
  for r in sess.release_group(&string_view_to_utf16(&object_group)) {
    unsafe { crate::jsc::jsc_sys::JSValueUnprotect(r.ctx, r.value) };
  }
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

/// A real `StringBuffer` backing store. V8's `StringBuffer::create` copies the
/// source `StringView` into an owned buffer that outlives it; the previous stub
/// dropped the contents, so `string()` reported an empty view (rusty_v8's
/// `inspector_string_buffer` test). Mirrors the quickjs backend.
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

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__DELETE(this: *mut RawV8Inspector) {
  unsafe { free_inert(this) };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__create(
  _isolate: *mut RealIsolate,
  _client: *mut RawV8InspectorClient,
) -> *mut RawV8Inspector {
  alloc_inert::<RawV8Inspector>()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__connect(
  _inspector: *mut RawV8Inspector,
  _context_group_id: int,
  channel: *mut RawChannel,
  _state: StringView,
  _client_trust_level: V8InspectorClientTrustLevel,
) -> *mut RawV8InspectorSession {
  Box::into_raw(Box::new(cdp::CdpSession::new(channel)))
    .cast::<RawV8InspectorSession>()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__contextCreated(
  _this: *mut RawV8Inspector,
  _context: *const Context,
  _contextGroupId: int,
  _humanReadableName: StringView,
  _auxData: StringView,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8_inspector__V8Inspector__contextDestroyed(
  _this: *mut RawV8Inspector,
  _context: *const Context,
) {
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
  _this: *mut RawV8Inspector,
  _context: *const Context,
  _message: StringView,
  _exception: *const Value,
  _detailed_message: StringView,
  _url: StringView,
  _line_number: u32,
  _column_number: u32,
  _stack_trace: *mut V8StackTrace,
  _script_id: int,
) -> u32 {
  0
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

mod cdp {
  use super::RawChannel;
  use super::RealStringBuffer;
  use crate::inspector::StringBuffer;
  use crate::support::UniquePtr;
  use crate::support::int;

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

  /// An object handed out by `wrapObject`, protected until its group is
  /// released. Dropping the session does not unprotect: it may outlive the
  /// context, and the values go away with the isolate's VM regardless.
  pub struct RemoteRef {
    pub value: crate::jsc::jsc_sys::JSValueRef,
    pub context: *const crate::Context,
    pub ctx: crate::jsc::jsc_sys::JSContextRef,
    pub group: Vec<u16>,
  }

  pub struct CdpSession {
    channel: *mut RawChannel,
    remote_objects: std::collections::HashMap<String, RemoteRef>,
    next_object_id: u64,
  }

  impl CdpSession {
    pub fn new(channel: *mut RawChannel) -> Self {
      CdpSession {
        channel,
        remote_objects: std::collections::HashMap::new(),
        next_object_id: 1,
      }
    }

    /// Registers `r` and returns its `objectId`. The format follows V8's
    /// `<injected script>.<context>.<ordinal>`.
    pub fn add_remote_object(&mut self, r: RemoteRef) -> String {
      let id = format!("1.1.{}", self.next_object_id);
      self.next_object_id += 1;
      self.remote_objects.insert(id.clone(), r);
      id
    }

    pub fn remote_object(&self, id: &str) -> Option<&RemoteRef> {
      self.remote_objects.get(id)
    }

    pub fn release_group(&mut self, group: &[u16]) -> Vec<RemoteRef> {
      let ids: Vec<String> = self
        .remote_objects
        .iter()
        .filter(|(_, r)| r.group == group)
        .map(|(id, _)| id.clone())
        .collect();
      ids
        .iter()
        .filter_map(|id| self.remote_objects.remove(id))
        .collect()
    }
  }

  fn json_string_value(message: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let mut rest = message.split_once(&needle)?.1;
    rest = rest.split_once(':')?.1.trim_start();
    if !rest.starts_with('"') {
      return None;
    }
    let mut escaped = false;
    let mut out = String::new();
    for ch in rest[1..].chars() {
      if escaped {
        out.push(match ch {
          '"' => '"',
          '\\' => '\\',
          '/' => '/',
          'b' => '\u{0008}',
          'f' => '\u{000C}',
          'n' => '\n',
          'r' => '\r',
          't' => '\t',
          other => other,
        });
        escaped = false;
      } else if ch == '\\' {
        escaped = true;
      } else if ch == '"' {
        return Some(out);
      } else {
        out.push(ch);
      }
    }
    None
  }

  fn json_i32_value(message: &str, key: &str) -> Option<i32> {
    let needle = format!("\"{key}\"");
    let rest = message.split_once(&needle)?.1;
    let rest = rest.split_once(':')?.1.trim_start();
    let end = rest
      .find(|ch: char| !ch.is_ascii_digit() && ch != '-')
      .unwrap_or(rest.len());
    rest[..end].parse().ok()
  }

  fn send(channel: *mut RawChannel, json: &str, call_id: Option<i32>) {
    if channel.is_null() {
      return;
    }
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

  pub fn dispatch(sess: &mut CdpSession, message: &str) {
    let call_id = json_i32_value(message, "id").unwrap_or(0);
    let method = json_string_value(message, "method").unwrap_or_default();
    send(
      sess.channel,
      &format!("{{\"id\":{call_id},\"result\":{{}}}}"),
      Some(call_id),
    );

    if method == "Runtime.enable" {
      send(
        sess.channel,
        r#"{"method":"Runtime.executionContextCreated","params":{"context":{"id":1,"origin":"","name":"repl","uniqueId":"1","auxData":{"isDefault":true}}}}"#,
        None,
      );
    }
  }
}
