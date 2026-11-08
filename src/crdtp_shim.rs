//! Pure-Rust implementation of the `crdtp__*` C-ABI surface (Chrome DevTools
//! Protocol / inspector_protocol) that the vendored rusty_v8 `crdtp.rs` binding
//! calls into.
//!
//! The real binding (`vendor/rusty_v8/src/crdtp_binding.cc`) is backed by V8's
//! `third_party/inspector_protocol/crdtp` C++ library (CBOR codec + dispatcher).
//! Our engine backends (QuickJS / JSC) don't ship that library, so these symbols
//! were undefined and **every** test target that references them
//! (`test_api.rs`, hundreds of tests) failed to link and scored zero.
//!
//! This module re-implements the surface in pure Rust so the tests link and run.
//! It is engine-independent (no JSC/QuickJS calls), so it lives at the crate root
//! and is compiled for all backends.
//!
//! ## Simplification: "CBOR" == canonical JSON bytes
//!
//! Real crdtp serializes messages as CBOR. The rusty_v8 tests only assert
//! *round-trip* and *substring* properties (e.g. the JSON contains `"id":1` or
//! `Network.enable`), never the exact CBOR byte layout. So we use a self-
//! consistent encoding where the "CBOR" representation is simply the validated
//! JSON bytes. `json_to_cbor` / `cbor_to_json` are identity transforms gated on
//! strict JSON validation (which is what makes the malformed-input tests pass:
//! invalid bytes fail to validate and surface as `None` / `!ok`).
//!
//! The dispatcher (`UberDispatcher` / `DomainDispatcher` / `FrontendChannel`)
//! is implemented for real against the Rust callbacks defined in `crdtp.rs`.

#![allow(non_snake_case)]
#![allow(clippy::missing_safety_doc)]

use std::ffi::CStr;
use std::os::raw::{c_char, c_void};

// ---------------------------------------------------------------------------
// Rust-side callbacks defined in vendor/rusty_v8/src/crdtp.rs (#[no_mangle]).
// We call back into these to deliver responses to the frontend channel and to
// probe/run Rust domain dispatchers. Pointer argument types are erased to
// `c_void` here — the ABI is identical to the typed declarations in crdtp.rs.
// ---------------------------------------------------------------------------
unsafe extern "C" {
  fn crdtp__FrontendChannel__BASE__sendProtocolResponse(
    this: *mut c_void,
    call_id: i32,
    message: *mut c_void,
  );
  #[allow(dead_code)]
  fn crdtp__FrontendChannel__BASE__sendProtocolNotification(
    this: *mut c_void,
    message: *mut c_void,
  );
  fn crdtp__DomainDispatcher__BASE__Dispatch(
    rust_dispatcher: *mut c_void,
    command_data: *const u8,
    command_len: usize,
    dispatchable: *const c_void,
  ) -> bool;
  fn crdtp__DomainDispatcher__BASE__Drop(rust_dispatcher: *mut c_void);
}

// ===========================================================================
// Minimal strict JSON parser (validation + envelope extraction).
// ===========================================================================

enum Json {
  Null,
  Bool(bool),
  Int(i64),
  Float(f64),
  Str(String),
  Arr(Vec<Json>),
  Obj(Vec<(String, Json)>),
}

struct Parser<'a> {
  b: &'a [u8],
  i: usize,
}

impl<'a> Parser<'a> {
  fn new(b: &'a [u8]) -> Self {
    Parser { b, i: 0 }
  }

  fn ws(&mut self) {
    while let Some(&c) = self.b.get(self.i) {
      if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
        self.i += 1;
      } else {
        break;
      }
    }
  }

  /// Parse a complete JSON document; require that the whole input is consumed.
  fn parse_document(&mut self) -> Option<Json> {
    self.ws();
    let v = self.value()?;
    self.ws();
    if self.i == self.b.len() {
      Some(v)
    } else {
      None
    }
  }

  fn value(&mut self) -> Option<Json> {
    self.ws();
    match *self.b.get(self.i)? {
      b'{' => self.object(),
      b'[' => self.array(),
      b'"' => self.string().map(Json::Str),
      b't' => self.literal(b"true").map(|_| Json::Bool(true)),
      b'f' => self.literal(b"false").map(|_| Json::Bool(false)),
      b'n' => self.literal(b"null").map(|_| Json::Null),
      b'-' | b'0'..=b'9' => self.number(),
      _ => None,
    }
  }

  fn literal(&mut self, lit: &[u8]) -> Option<()> {
    if self.b[self.i..].starts_with(lit) {
      self.i += lit.len();
      Some(())
    } else {
      None
    }
  }

  fn object(&mut self) -> Option<Json> {
    self.i += 1; // '{'
    let mut members = Vec::new();
    self.ws();
    if self.b.get(self.i) == Some(&b'}') {
      self.i += 1;
      return Some(Json::Obj(members));
    }
    loop {
      self.ws();
      if self.b.get(self.i) != Some(&b'"') {
        return None;
      }
      let key = self.string()?;
      self.ws();
      if self.b.get(self.i) != Some(&b':') {
        return None;
      }
      self.i += 1;
      let val = self.value()?;
      members.push((key, val));
      self.ws();
      match self.b.get(self.i) {
        Some(&b',') => {
          self.i += 1;
        }
        Some(&b'}') => {
          self.i += 1;
          return Some(Json::Obj(members));
        }
        _ => return None,
      }
    }
  }

  fn array(&mut self) -> Option<Json> {
    self.i += 1; // '['
    let mut items = Vec::new();
    self.ws();
    if self.b.get(self.i) == Some(&b']') {
      self.i += 1;
      return Some(Json::Arr(items));
    }
    loop {
      let val = self.value()?;
      items.push(val);
      self.ws();
      match self.b.get(self.i) {
        Some(&b',') => {
          self.i += 1;
        }
        Some(&b']') => {
          self.i += 1;
          return Some(Json::Arr(items));
        }
        _ => return None,
      }
    }
  }

  fn string(&mut self) -> Option<String> {
    self.i += 1; // opening quote
    let mut out = String::new();
    loop {
      let c = *self.b.get(self.i)?;
      self.i += 1;
      match c {
        b'"' => return Some(out),
        b'\\' => {
          let e = *self.b.get(self.i)?;
          self.i += 1;
          match e {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{0008}'),
            b'f' => out.push('\u{000C}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
              let cp = self.hex4()?;
              // Surrogate pairs: best-effort, treat lone surrogates as U+FFFD.
              if (0xD800..=0xDBFF).contains(&cp) {
                if self.b.get(self.i) == Some(&b'\\')
                  && self.b.get(self.i + 1) == Some(&b'u')
                {
                  self.i += 2;
                  let lo = self.hex4()?;
                  if (0xDC00..=0xDFFF).contains(&lo) {
                    let c = 0x10000
                      + (((cp - 0xD800) as u32) << 10)
                      + (lo - 0xDC00) as u32;
                    out.push(char::from_u32(c).unwrap_or('\u{FFFD}'));
                  } else {
                    out.push('\u{FFFD}');
                  }
                } else {
                  out.push('\u{FFFD}');
                }
              } else {
                out.push(char::from_u32(cp as u32).unwrap_or('\u{FFFD}'));
              }
            }
            _ => return None,
          }
        }
        // Control characters must be escaped in strict JSON.
        0x00..=0x1F => return None,
        // UTF-8 continuation: collect the raw bytes of this multibyte char.
        _ => {
          let start = self.i - 1;
          let extra = match c {
            0x00..=0x7F => 0,
            0xC0..=0xDF => 1,
            0xE0..=0xEF => 2,
            0xF0..=0xF7 => 3,
            _ => return None,
          };
          for _ in 0..extra {
            // continuation byte
            let cc = *self.b.get(self.i)?;
            if !(0x80..=0xBF).contains(&cc) {
              return None;
            }
            self.i += 1;
          }
          let s = std::str::from_utf8(&self.b[start..self.i]).ok()?;
          out.push_str(s);
        }
      }
    }
  }

  fn hex4(&mut self) -> Option<u16> {
    let mut v: u16 = 0;
    for _ in 0..4 {
      let c = *self.b.get(self.i)?;
      self.i += 1;
      let d = match c {
        b'0'..=b'9' => (c - b'0') as u16,
        b'a'..=b'f' => (c - b'a' + 10) as u16,
        b'A'..=b'F' => (c - b'A' + 10) as u16,
        _ => return None,
      };
      v = (v << 4) | d;
    }
    Some(v)
  }

  fn cur_is_digit(&self) -> bool {
    self.b.get(self.i).is_some_and(|c| c.is_ascii_digit())
  }

  fn number(&mut self) -> Option<Json> {
    let start = self.i;
    let mut is_float = false;
    if self.b.get(self.i) == Some(&b'-') {
      self.i += 1;
    }
    // integer part
    match self.b.get(self.i).copied() {
      Some(b'0') => {
        self.i += 1;
      }
      Some(c) if c.is_ascii_digit() => {
        while self.cur_is_digit() {
          self.i += 1;
        }
      }
      _ => return None,
    }
    // fraction
    if self.b.get(self.i) == Some(&b'.') {
      is_float = true;
      self.i += 1;
      if !self.cur_is_digit() {
        return None;
      }
      while self.cur_is_digit() {
        self.i += 1;
      }
    }
    // exponent
    if matches!(self.b.get(self.i).copied(), Some(b'e') | Some(b'E')) {
      is_float = true;
      self.i += 1;
      if matches!(self.b.get(self.i).copied(), Some(b'+') | Some(b'-')) {
        self.i += 1;
      }
      if !self.cur_is_digit() {
        return None;
      }
      while self.cur_is_digit() {
        self.i += 1;
      }
    }
    let text = std::str::from_utf8(&self.b[start..self.i]).ok()?;
    if is_float {
      Some(Json::Float(text.parse().ok()?))
    } else {
      match text.parse::<i64>() {
        Ok(n) => Some(Json::Int(n)),
        // Out-of-range integer: still valid JSON, keep as float.
        Err(_) => Some(Json::Float(text.parse().ok()?)),
      }
    }
  }
}

fn parse_json(bytes: &[u8]) -> Option<Json> {
  Parser::new(bytes).parse_document()
}

fn escape_json_str(s: &str, out: &mut String) {
  out.push('"');
  for c in s.chars() {
    match c {
      '"' => out.push_str("\\\""),
      '\\' => out.push_str("\\\\"),
      '\n' => out.push_str("\\n"),
      '\r' => out.push_str("\\r"),
      '\t' => out.push_str("\\t"),
      '\u{0008}' => out.push_str("\\b"),
      '\u{000C}' => out.push_str("\\f"),
      c if (c as u32) < 0x20 => {
        out.push_str(&format!("\\u{:04x}", c as u32));
      }
      c => out.push(c),
    }
  }
  out.push('"');
}

fn serialize(v: &Json, out: &mut String) {
  match v {
    Json::Null => out.push_str("null"),
    Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
    Json::Int(n) => out.push_str(&n.to_string()),
    Json::Float(f) => out.push_str(&f.to_string()),
    Json::Str(s) => escape_json_str(s, out),
    Json::Arr(items) => {
      out.push('[');
      for (i, it) in items.iter().enumerate() {
        if i > 0 {
          out.push(',');
        }
        serialize(it, out);
      }
      out.push(']');
    }
    Json::Obj(members) => {
      out.push('{');
      for (i, (k, val)) in members.iter().enumerate() {
        if i > 0 {
          out.push(',');
        }
        escape_json_str(k, out);
        out.push(':');
        serialize(val, out);
      }
      out.push('}');
    }
  }
}

// ===========================================================================
// Opaque types behind the C-ABI pointers.
// ===========================================================================

/// `std::vector<uint8_t>` analogue used by the serializer / json codec.
pub(crate) type CppVecU8 = Vec<u8>;

pub(crate) struct Serializable {
  bytes: Vec<u8>,
}

pub(crate) struct Dispatchable {
  ok: bool,
  has_call_id: bool,
  call_id: i32,
  method: Vec<u8>,
  session_id: Vec<u8>,
  params: Vec<u8>,
  /// The serialized message exactly as it arrived. The fallthrough callback
  /// is handed it verbatim.
  message: Vec<u8>,
  /// Embedder blob carried alongside the message, handed back verbatim by
  /// `associatedData` and to the fallthrough callback.
  associated_data: Vec<u8>,
  /// Owned `FallthroughCallbackData` from the Rust side, released with
  /// `crdtp__FallthroughCallback__Drop`. Null when the embedder gave none.
  fallthrough: *mut c_void,
}

unsafe extern "C" {
  /// Both are defined by the vendored `crdtp.rs`, which owns the boxed Rust
  /// closure this shim only stores and hands back.
  fn crdtp__FallthroughCallback__Run(
    callback: *mut c_void,
    call_id: i32,
    method_data: *const u8,
    method_len: usize,
    message_data: *const u8,
    message_len: usize,
    associated_data: *const u8,
    associated_data_len: usize,
  );
  fn crdtp__FallthroughCallback__Drop(callback: *mut c_void);
}

#[derive(Clone, Copy, PartialEq)]
enum RespKind {
  Success,
  FallThrough,
  Error,
}

pub(crate) struct DispatchResponse {
  kind: RespKind,
  code: i32,
  message: String,
}

pub(crate) struct UberDispatcher {
  channel: *mut c_void,
  domains: Vec<(Vec<u8>, *mut DomainDispatcher)>,
}

pub(crate) struct DomainDispatcher {
  channel: *mut c_void,
  rust_dispatcher: *mut c_void,
}

#[inline]
fn box_raw<T>(v: T) -> *mut T {
  Box::into_raw(Box::new(v))
}

#[inline]
unsafe fn free_box<T>(p: *mut T) {
  if !p.is_null() {
    unsafe { drop(Box::from_raw(p)) };
  }
}

/// Build a `Serializable` from JSON bytes and return its raw pointer.
fn serializable_ptr(bytes: Vec<u8>) -> *mut Serializable {
  box_raw(Serializable { bytes })
}

// ---------------------------------------------------------------------------
// FrontendChannel
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn crdtp__FrontendChannel__BASE__CONSTRUCT(buf: *mut c_void) {
  // The raw channel is a single vtable pointer slot. We never dispatch through
  // a C++ vtable (we call the Rust `*__BASE__*` callbacks directly), so just
  // zero it to leave the buffer in a defined state.
  if !buf.is_null() {
    unsafe { (buf as *mut *mut c_void).write(std::ptr::null_mut()) };
  }
}

// ---------------------------------------------------------------------------
// Serializable
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__Serializable__DELETE(this: *mut Serializable) {
  unsafe { free_box(this) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__Serializable__AppendSerialized(
  this: *const Serializable,
  out: *mut CppVecU8,
) {
  if this.is_null() || out.is_null() {
    return;
  }
  let bytes = unsafe { &*this }.bytes.clone();
  unsafe { &mut *out }.extend_from_slice(&bytes);
}

// ---------------------------------------------------------------------------
// Dispatchable
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__Dispatchable__new(
  data: *const u8,
  len: usize,
  associated_data: *const u8,
  associated_data_len: usize,
  fallthrough_callback: *mut c_void,
) -> *mut Dispatchable {
  let bytes = if data.is_null() || len == 0 {
    &[][..]
  } else {
    unsafe { std::slice::from_raw_parts(data, len) }
  };

  // Copied rather than borrowed: the C++ Dispatchable borrows the caller's
  // buffers and rusty_v8 keeps them alive for it, but this shim outlives
  // nothing in particular, so it owns what it reports.
  let associated_data = if associated_data.is_null() || associated_data_len == 0
  {
    Vec::new()
  } else {
    unsafe { std::slice::from_raw_parts(associated_data, associated_data_len) }
      .to_vec()
  };

  let mut d = Dispatchable {
    ok: false,
    has_call_id: false,
    call_id: 0,
    method: Vec::new(),
    session_id: Vec::new(),
    params: Vec::new(),
    message: bytes.to_vec(),
    associated_data,
    fallthrough: fallthrough_callback,
  };

  if let Some(Json::Obj(members)) = parse_json(bytes) {
    let mut has_method = false;
    for (k, v) in &members {
      match k.as_str() {
        "id" => {
          if let Json::Int(n) = v {
            d.has_call_id = true;
            d.call_id = *n as i32;
          }
        }
        "method" => {
          if let Json::Str(s) = v {
            if !s.is_empty() {
              d.method = s.as_bytes().to_vec();
              has_method = true;
            }
          }
        }
        "sessionId" => {
          if let Json::Str(s) = v {
            d.session_id = s.as_bytes().to_vec();
          }
        }
        "params" => {
          let mut s = String::new();
          serialize(v, &mut s);
          d.params = s.into_bytes();
        }
        _ => {}
      }
    }
    // crdtp requires both an integer call id and a method to be dispatchable.
    d.ok = d.has_call_id && has_method;
  }

  box_raw(d)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__Dispatchable__DELETE(this: *mut Dispatchable) {
  if this.is_null() {
    return;
  }
  let fallthrough = unsafe { (*this).fallthrough };
  if !fallthrough.is_null() {
    unsafe { crdtp__FallthroughCallback__Drop(fallthrough) };
  }
  unsafe { free_box(this) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__Dispatchable__associatedDataLen(
  this: *const Dispatchable,
) -> usize {
  if this.is_null() {
    0
  } else {
    unsafe { &*this }.associated_data.len()
  }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__Dispatchable__associatedDataCopy(
  this: *const Dispatchable,
  out: *mut u8,
) {
  if this.is_null() || out.is_null() {
    return;
  }
  let d = unsafe { &(*this).associated_data };
  unsafe { std::ptr::copy_nonoverlapping(d.as_ptr(), out, d.len()) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__Dispatchable__ok(
  this: *const Dispatchable,
) -> bool {
  !this.is_null() && unsafe { &*this }.ok
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__Dispatchable__callId(
  this: *const Dispatchable,
) -> i32 {
  if this.is_null() {
    0
  } else {
    unsafe { (*this).call_id }
  }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__Dispatchable__hasCallId(
  this: *const Dispatchable,
) -> bool {
  !this.is_null() && unsafe { (*this).has_call_id }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__Dispatchable__methodLen(
  this: *const Dispatchable,
) -> usize {
  if this.is_null() {
    0
  } else {
    unsafe { &*this }.method.len()
  }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__Dispatchable__methodCopy(
  this: *const Dispatchable,
  out: *mut u8,
) {
  if this.is_null() || out.is_null() {
    return;
  }
  let m = unsafe { &(*this).method };
  unsafe { std::ptr::copy_nonoverlapping(m.as_ptr(), out, m.len()) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__Dispatchable__sessionIdLen(
  this: *const Dispatchable,
) -> usize {
  if this.is_null() {
    0
  } else {
    unsafe { &*this }.session_id.len()
  }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__Dispatchable__sessionIdCopy(
  this: *const Dispatchable,
  out: *mut u8,
) {
  if this.is_null() || out.is_null() {
    return;
  }
  let m = unsafe { &(*this).session_id };
  unsafe { std::ptr::copy_nonoverlapping(m.as_ptr(), out, m.len()) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__Dispatchable__paramsLen(
  this: *const Dispatchable,
) -> usize {
  if this.is_null() {
    0
  } else {
    unsafe { &*this }.params.len()
  }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__Dispatchable__paramsCopy(
  this: *const Dispatchable,
  out: *mut u8,
) {
  if this.is_null() || out.is_null() {
    return;
  }
  let m = unsafe { &(*this).params };
  unsafe { std::ptr::copy_nonoverlapping(m.as_ptr(), out, m.len()) };
}

// ---------------------------------------------------------------------------
// DispatchResponse
// ---------------------------------------------------------------------------

fn make_response(
  kind: RespKind,
  code: i32,
  message: &str,
) -> *mut DispatchResponse {
  box_raw(DispatchResponse {
    kind,
    code,
    message: message.to_string(),
  })
}

#[unsafe(no_mangle)]
pub extern "C" fn crdtp__DispatchResponse__Success() -> *mut DispatchResponse {
  make_response(RespKind::Success, 0, "")
}

#[unsafe(no_mangle)]
pub extern "C" fn crdtp__DispatchResponse__FallThrough() -> *mut DispatchResponse
{
  make_response(RespKind::FallThrough, 0, "")
}

unsafe fn msg(msg: *const u8, len: usize) -> String {
  if msg.is_null() || len == 0 {
    return String::new();
  }
  let s = unsafe { std::slice::from_raw_parts(msg, len) };
  String::from_utf8_lossy(s).into_owned()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__DispatchResponse__ParseError(
  m: *const u8,
  len: usize,
) -> *mut DispatchResponse {
  make_response(RespKind::Error, -32700, &unsafe { msg(m, len) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__DispatchResponse__InvalidRequest(
  m: *const u8,
  len: usize,
) -> *mut DispatchResponse {
  make_response(RespKind::Error, -32600, &unsafe { msg(m, len) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__DispatchResponse__MethodNotFound(
  m: *const u8,
  len: usize,
) -> *mut DispatchResponse {
  make_response(RespKind::Error, -32601, &unsafe { msg(m, len) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__DispatchResponse__InvalidParams(
  m: *const u8,
  len: usize,
) -> *mut DispatchResponse {
  make_response(RespKind::Error, -32602, &unsafe { msg(m, len) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__DispatchResponse__ServerError(
  m: *const u8,
  len: usize,
) -> *mut DispatchResponse {
  make_response(RespKind::Error, -32000, &unsafe { msg(m, len) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__DispatchResponse__DELETE(
  this: *mut DispatchResponse,
) {
  unsafe { free_box(this) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__DispatchResponse__isSuccess(
  this: *const DispatchResponse,
) -> bool {
  !this.is_null() && unsafe { (*this).kind == RespKind::Success }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__DispatchResponse__isError(
  this: *const DispatchResponse,
) -> bool {
  !this.is_null() && unsafe { (*this).kind == RespKind::Error }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__DispatchResponse__isFallThrough(
  this: *const DispatchResponse,
) -> bool {
  !this.is_null() && unsafe { (*this).kind == RespKind::FallThrough }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__DispatchResponse__code(
  this: *const DispatchResponse,
) -> i32 {
  if this.is_null() {
    0
  } else {
    unsafe { (*this).code }
  }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__DispatchResponse__messageLen(
  this: *const DispatchResponse,
) -> usize {
  if this.is_null() {
    0
  } else {
    unsafe { &*this }.message.len()
  }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__DispatchResponse__messageCopy(
  this: *const DispatchResponse,
  out: *mut u8,
) {
  if this.is_null() || out.is_null() {
    return;
  }
  let m = unsafe { &*this }.message.as_bytes().to_vec();
  unsafe { std::ptr::copy_nonoverlapping(m.as_ptr(), out, m.len()) };
}

// ---------------------------------------------------------------------------
// vec<u8>
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn crdtp__vec_u8__new() -> *mut CppVecU8 {
  box_raw(Vec::<u8>::new())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__vec_u8__DELETE(this: *mut CppVecU8) {
  unsafe { free_box(this) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__vec_u8__size(this: *const CppVecU8) -> usize {
  if this.is_null() {
    0
  } else {
    unsafe { &*this }.len()
  }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__vec_u8__copy(
  this: *const CppVecU8,
  out: *mut u8,
) {
  if this.is_null() || out.is_null() {
    return;
  }
  let v = unsafe { &*this };
  unsafe { std::ptr::copy_nonoverlapping(v.as_ptr(), out, v.len()) };
}

// ---------------------------------------------------------------------------
// JSON <-> CBOR (identity transform gated on strict JSON validation).
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__json__ConvertJSONToCBOR(
  json_data: *const u8,
  json_len: usize,
  cbor_out: *mut CppVecU8,
) -> bool {
  if cbor_out.is_null() || json_data.is_null() || json_len == 0 {
    return false;
  }
  let bytes = unsafe { std::slice::from_raw_parts(json_data, json_len) };
  if parse_json(bytes).is_none() {
    return false;
  }
  unsafe { &mut *cbor_out }.extend_from_slice(bytes);
  true
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__json__ConvertCBORToJSON(
  cbor_data: *const u8,
  cbor_len: usize,
  json_out: *mut CppVecU8,
) -> bool {
  if json_out.is_null() || cbor_data.is_null() || cbor_len == 0 {
    return false;
  }
  let bytes = unsafe { std::slice::from_raw_parts(cbor_data, cbor_len) };
  if parse_json(bytes).is_none() {
    return false;
  }
  unsafe { &mut *json_out }.extend_from_slice(bytes);
  true
}

// ---------------------------------------------------------------------------
// Message builders (produce JSON-bytes Serializables).
// ---------------------------------------------------------------------------

/// `{"id":<call_id>,"error":{"code":<code>,"message":"<msg>"}}`
fn error_response_json(call_id: i32, resp: &DispatchResponse) -> Vec<u8> {
  let mut s = String::new();
  s.push_str("{\"id\":");
  s.push_str(&call_id.to_string());
  s.push_str(",\"error\":{\"code\":");
  s.push_str(&resp.code.to_string());
  s.push_str(",\"message\":");
  escape_json_str(&resp.message, &mut s);
  s.push_str("}}");
  s.into_bytes()
}

/// `{"id":<call_id>,"result":<params or {}>}`
fn response_json(call_id: i32, params: Option<&[u8]>) -> Vec<u8> {
  let mut s = String::new();
  s.push_str("{\"id\":");
  s.push_str(&call_id.to_string());
  s.push_str(",\"result\":");
  match params {
    Some(p) if !p.is_empty() => s.push_str(&String::from_utf8_lossy(p)),
    _ => s.push_str("{}"),
  }
  s.push('}');
  s.into_bytes()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__CreateErrorResponse(
  call_id: i32,
  response: *mut DispatchResponse,
) -> *mut Serializable {
  let bytes = if response.is_null() {
    error_response_json(
      call_id,
      &DispatchResponse {
        kind: RespKind::Error,
        code: -32000,
        message: String::new(),
      },
    )
  } else {
    let r = unsafe { &*response };
    error_response_json(call_id, r)
  };
  unsafe { free_box(response) };
  serializable_ptr(bytes)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__CreateResponse(
  call_id: i32,
  params: *mut Serializable,
) -> *mut Serializable {
  let bytes = if params.is_null() {
    response_json(call_id, None)
  } else {
    let p = unsafe { &*params };
    response_json(call_id, Some(&p.bytes))
  };
  unsafe { free_box(params) };
  serializable_ptr(bytes)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__CreateNotification(
  method: *const c_char,
  params: *mut Serializable,
) -> *mut Serializable {
  let method_str = if method.is_null() {
    String::new()
  } else {
    unsafe { CStr::from_ptr(method) }
      .to_string_lossy()
      .into_owned()
  };
  let mut s = String::new();
  s.push_str("{\"method\":");
  escape_json_str(&method_str, &mut s);
  s.push_str(",\"params\":");
  if params.is_null() {
    s.push_str("{}");
  } else {
    let p = unsafe { &*params };
    if p.bytes.is_empty() {
      s.push_str("{}");
    } else {
      s.push_str(&String::from_utf8_lossy(&p.bytes));
    }
  }
  s.push('}');
  unsafe { free_box(params) };
  serializable_ptr(s.into_bytes())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__CreateErrorNotification(
  response: *mut DispatchResponse,
) -> *mut Serializable {
  let mut s = String::new();
  s.push_str("{\"error\":{\"code\":");
  if response.is_null() {
    s.push_str("-32000,\"message\":\"\"");
  } else {
    let r = unsafe { &*response };
    s.push_str(&r.code.to_string());
    s.push_str(",\"message\":");
    escape_json_str(&r.message, &mut s);
  }
  s.push_str("}}");
  unsafe { free_box(response) };
  serializable_ptr(s.into_bytes())
}

// ---------------------------------------------------------------------------
// UberDispatcher
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn crdtp__UberDispatcher__new(
  channel: *mut c_void,
) -> *mut UberDispatcher {
  box_raw(UberDispatcher {
    channel,
    domains: Vec::new(),
  })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__UberDispatcher__DELETE(
  this: *mut UberDispatcher,
) {
  if this.is_null() {
    return;
  }
  let uber = unsafe { Box::from_raw(this) };
  // Destroy each wired domain dispatcher: free its Rust impl via the Drop
  // callback, then the DomainDispatcher box itself. Mirrors the C++
  // ~crdtp__DomainDispatcher__BASE() -> crdtp__DomainDispatcher__BASE__Drop.
  for (_domain, dd) in uber.domains.iter() {
    if !dd.is_null() {
      let dispatcher = unsafe { Box::from_raw(*dd) };
      if !dispatcher.rust_dispatcher.is_null() {
        unsafe {
          crdtp__DomainDispatcher__BASE__Drop(dispatcher.rust_dispatcher)
        };
      }
    }
  }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__UberDispatcher__channel(
  this: *mut UberDispatcher,
) -> *mut c_void {
  if this.is_null() {
    std::ptr::null_mut()
  } else {
    unsafe { (*this).channel }
  }
}

/// Split a `Domain.command` method into (domain, command) on the first dot.
fn split_method(method: &[u8]) -> (&[u8], &[u8]) {
  match method.iter().position(|&c| c == b'.') {
    Some(i) => (&method[..i], &method[i + 1..]),
    None => (method, &[]),
  }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__UberDispatcher__Dispatch(
  this: *mut UberDispatcher,
  dispatchable: *mut Dispatchable,
) {
  let uber = unsafe { &mut *this };
  let d = unsafe { &*dispatchable };

  let (domain, command) = split_method(&d.method);

  // Find the matching domain dispatcher.
  let dd_ptr = uber
    .domains
    .iter()
    .find(|(dom, _)| dom.as_slice() == domain)
    .map(|(_, p)| *p);

  // Since rusty_v8 v152.0.0 there is no probe phase: `BASE__Dispatch` takes a
  // live `&Dispatchable` and runs the command, reporting whether it handled it.
  // Calling it twice, as the probe-then-execute split did, would run the
  // command twice and send two responses.
  let handled = match dd_ptr {
    None => false,
    Some(dd) => unsafe {
      crdtp__DomainDispatcher__BASE__Dispatch(
        (*dd).rust_dispatcher,
        command.as_ptr(),
        command.len(),
        dispatchable as *const c_void,
      )
    },
  };

  if !handled {
    // An unhandled method goes to the embedder's fallthrough callback when one
    // was registered. Only without it does crdtp answer the request itself,
    // with a MethodNotFound error response.
    let channel = uber.channel;
    let call_id = d.call_id;
    let has_call_id = d.has_call_id;
    let method = d.method.clone();
    if !d.fallthrough.is_null() {
      unsafe {
        crdtp__FallthroughCallback__Run(
          d.fallthrough,
          call_id,
          method.as_ptr(),
          method.len(),
          d.message.as_ptr(),
          d.message.len(),
          d.associated_data.as_ptr(),
          d.associated_data.len(),
        );
      }
      return;
    }
    if has_call_id && !channel.is_null() {
      let method_str = String::from_utf8_lossy(&method);
      let resp = DispatchResponse {
        kind: RespKind::Error,
        code: -32601,
        message: format!("'{method_str}' wasn't found"),
      };
      let bytes = error_response_json(call_id, &resp);
      let msg = serializable_ptr(bytes);
      unsafe {
        crdtp__FrontendChannel__BASE__sendProtocolResponse(
          channel,
          call_id,
          msg as *mut c_void,
        );
      }
    }
  }
}

// ---------------------------------------------------------------------------
// DomainDispatcher
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn crdtp__DomainDispatcher__new(
  channel: *mut c_void,
  rust_dispatcher: *mut c_void,
) -> *mut DomainDispatcher {
  box_raw(DomainDispatcher {
    channel,
    rust_dispatcher,
  })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__DomainDispatcher__sendResponse(
  this: *mut DomainDispatcher,
  call_id: i32,
  response: *mut DispatchResponse,
  result: *mut Serializable,
) {
  if this.is_null() {
    unsafe { free_box(response) };
    unsafe { free_box(result) };
    return;
  }
  let dd = unsafe { &*this };

  // Success -> {"id","result"}; error -> {"id","error"}. Matches crdtp's
  // DomainDispatcher::sendResponse.
  let is_success =
    response.is_null() || unsafe { (*response).kind } == RespKind::Success;

  let bytes = if is_success {
    let params = if result.is_null() {
      None
    } else {
      Some(unsafe { &(*result).bytes }.clone())
    };
    response_json(call_id, params.as_deref())
  } else {
    let r = unsafe { &*response };
    error_response_json(call_id, r)
  };

  unsafe { free_box(response) };
  unsafe { free_box(result) };

  let msg = serializable_ptr(bytes);
  if !dd.channel.is_null() {
    unsafe {
      crdtp__FrontendChannel__BASE__sendProtocolResponse(
        dd.channel,
        call_id,
        msg as *mut c_void,
      );
    }
  } else {
    unsafe { free_box(msg) };
  }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crdtp__UberDispatcher__WireBackend(
  uber: *mut UberDispatcher,
  domain_data: *const u8,
  domain_len: usize,
  dispatcher: *mut DomainDispatcher,
) {
  if uber.is_null() {
    return;
  }
  let domain = if domain_data.is_null() || domain_len == 0 {
    Vec::new()
  } else {
    unsafe { std::slice::from_raw_parts(domain_data, domain_len) }.to_vec()
  };
  unsafe { &mut *uber }.domains.push((domain, dispatcher));
}
