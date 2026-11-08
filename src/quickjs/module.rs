//! QuickJS-ng-backed definitions for the "module" family:
//! Module / ModuleRequest / Script / ScriptCompiler / UnboundScript /
//! UnboundModuleScript / FixedArray / ScriptOrigin.
//!
//! Ported from the deno PR's `reference/qjs_v8_compat/src/module.rs` (which is
//! the primary source for the QuickJS module-loader logic) and shaped to the
//! C-ABI of the JSC backend's `src/module.rs`.
//!
//! QuickJS-ng has REAL ES modules, so most of this is implemented for real:
//!   * `ScriptCompiler::CompileModule` compiles via
//!     `JS_Eval(JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY)`, which yields
//!     a `JS_TAG_MODULE` bytecode value whose `u.ptr` is the `JSModuleDef`.
//!   * `Module::Evaluate` runs that bytecode with `JS_EvalFunction` (which
//!     evaluates statically-imported dependencies transitively) and drains
//!     pending jobs, then hands back a resolved promise.
//!   * `Module::GetModuleNamespace` returns the real `JS_GetModuleNamespace`.
//!   * Synthetic modules use `JS_NewCModule` + `JS_AddModuleExport`, with
//!     exports populated lazily by an init callback.
//!
//! Per-module state can't live on the `JSValue` (it's a 16-byte tagged union,
//! not a pointer), so — exactly like the PR — we keep thread-local side tables
//! keyed by the module handle's pointer payload (`JSValue.u.ptr as usize`).
//!
//! Refcount discipline: every handle we return is routed through
//! `intern`/`intern_dup`; every `JSValue` we create and don't keep is
//! `JS_FreeValue`d. The bytecode `JSValue` (owned at +1 from the COMPILE_ONLY
//! eval) is stored in a side table and freed on isolate drop is *not* attempted
//! (QuickJS frees module bytecode through the module def); we dup before handing
//! it to `JS_EvalFunction` (which consumes one ref).
#![allow(non_snake_case, unused)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::mem::MaybeUninit;
use std::ptr;

use crate::quickjs::core::{
  ctx_of, current_ctx, current_host_defined_options, current_iso,
  current_script_name_or_source_url, host_defined_options_for_name,
  host_defined_options_for_script_value, intern, intern_ctx, intern_dup,
  iso_state, jsval_of, new_script_value, note_compilation_cache_miss,
  note_compiled_bytecode, record_host_defined_options_for_name,
  record_script_host_defined_options, record_script_resource_name,
  script_source_value,
};
use crate::quickjs::quickjs_sys::*;
use crate::{
  Context, Data, FixedArray, Function, Message, Module, ModuleRequest, Object,
  RealIsolate, Script, String as V8String, UnboundModuleScript, UnboundScript,
  Value,
};

use crate::isolate::ModuleImportPhase;
use crate::module::{
  Location, ModuleStatus, ResolveModuleCallback, ResolveSourceCallback,
  StalledTopLevelAwaitMessage, SyntheticModuleEvaluationSteps,
};
use crate::script::ScriptOrigin;
use crate::script_compiler::{
  CachedData, CompileOptions, NoCacheReason, Source,
};
use crate::support::{MaybeBool, int};

unsafe extern "C" {
  fn JS_GetProperty(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: JSAtom,
  ) -> JSValue;
  fn JS_GetOwnPropertyNames(
    ctx: *mut JSContext,
    ptab: *mut *mut ModulePropertyEnum,
    plen: *mut u32,
    obj: JSValue,
    flags: int,
  ) -> int;
  fn JS_PreventExtensions(ctx: *mut JSContext, obj: JSValue) -> int;
  fn v82jsc_link_module(ctx: *mut JSContext, module: *mut JSModuleDef) -> int;
  fn v82jsc_set_module_error_backtrace(
    ctx: *mut JSContext,
    error: JSValue,
    filename: *const std::os::raw::c_char,
    line: int,
    column: int,
  );
  fn v82jsc_module_stalled_location(
    module: *mut JSModuleDef,
    line: *mut int,
    column: *mut int,
  ) -> int;
}

#[repr(C)]
struct ModulePropertyEnum {
  is_enumerable: bool,
  atom: JSAtom,
}

const JS_GPN_STRING_MASK: int = 1 << 0;
const JS_GPN_ENUM_ONLY: int = 1 << 4;

const JS_WRITE_OBJ_BYTECODE: int = 1 << 0;
const JS_READ_OBJ_BYTECODE: int = 1 << 0;

const BC_MAGIC: u32 = 0x5142_4306;

unsafe extern "C" {
  fn v82jsc_mark_skip_next_async_frame(
    ctx: *mut JSContext,
    error: JSValue,
    filename: JSValue,
  );
}

unsafe fn new_synthetic_namespace(ctx: *mut JSContext) -> JSValue {
  unsafe { v82jsc_new_module_namespace(ctx) }
}

unsafe fn add_synthetic_namespace_export(
  ctx: *mut JSContext,
  namespace: JSValue,
  name: &CString,
  value: JSValue,
) -> bool {
  unsafe {
    v82jsc_module_namespace_set(ctx, namespace, name.as_ptr(), value) == 0
  }
}

unsafe fn finish_synthetic_namespace(
  ctx: *mut JSContext,
  namespace: JSValue,
) -> Option<JSValue> {
  if unsafe { JS_PreventExtensions(ctx, namespace) } < 0 {
    let exception = unsafe { JS_GetException(ctx) };
    unsafe {
      JS_FreeValue(ctx, exception);
      JS_FreeValue(ctx, namespace);
    }
    None
  } else {
    Some(namespace)
  }
}

fn bc_cache_dir() -> Option<std::path::PathBuf> {
  use std::sync::OnceLock;
  static DIR: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
  DIR
    .get_or_init(|| {
      if std::env::var_os("V82JSC_NO_BC_CACHE").is_some() {
        return None;
      }
      let dir = std::env::var_os("V82JSC_BC_CACHE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_bc_cache_dir);
      std::fs::create_dir_all(&dir).ok()?;
      Some(dir)
    })
    .clone()
}

fn default_bc_cache_dir() -> std::path::PathBuf {
  #[cfg(target_os = "macos")]
  let base = std::env::var_os("HOME")
    .map(std::path::PathBuf::from)
    .map(|home| home.join("Library/Caches"));

  #[cfg(target_os = "windows")]
  let base = std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from);

  #[cfg(not(any(target_os = "macos", target_os = "windows")))]
  let base = std::env::var_os("XDG_CACHE_HOME")
    .map(std::path::PathBuf::from)
    .or_else(|| {
      std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .map(|home| home.join(".cache"))
    });

  base
    .unwrap_or_else(std::env::temp_dir)
    .join("v8x/quickjs-bytecode")
}

/// Fast non-cryptographic hash over 8-byte lanes. Cache keys hash multiple MB
/// of extension source on every boot, so avoid SipHash's per-byte overhead.
pub(crate) fn fast_content_hash(seed: u64, bytes: &[u8]) -> u64 {
  const MIX_1: u64 = 0xa076_1d64_78bd_642f;
  const MIX_2: u64 = 0xe703_7ed1_a0b4_28db;
  const MIX_3: u64 = 0x8ebc_6af0_9c88_c6e3;

  #[inline]
  fn mix(a: u64, b: u64) -> u64 {
    let product = (a as u128).wrapping_mul(b as u128);
    product as u64 ^ (product >> 64) as u64
  }

  let mut h = seed ^ MIX_1 ^ bytes.len() as u64;
  let mut chunks = bytes.chunks_exact(8);
  for c in &mut chunks {
    h = mix(h ^ u64::from_le_bytes(c.try_into().unwrap()), MIX_2);
  }
  let mut tail = 0u64;
  for (shift, &byte) in chunks.remainder().iter().enumerate() {
    tail |= (byte as u64) << (shift * 8);
  }
  mix(mix(h ^ tail, MIX_3), MIX_1)
}

fn versioned_bc_seed(seed: u64) -> u64 {
  let cache_tag = u64::from_str_radix(env!("V82JSC_QUICKJS_CACHE_TAG"), 16)
    .expect("invalid QuickJS cache tag");
  seed ^ cache_tag
}

pub(crate) fn bc_key(source: &str, module_name: &str) -> u64 {
  let seed = fast_content_hash(
    versioned_bc_seed(BC_MAGIC as u64),
    module_name.as_bytes(),
  );
  fast_content_hash(seed, source.as_bytes())
}

pub(crate) fn script_bc_key(
  eval_flags: u64,
  source: &[u8],
  resource_name: &[u8],
) -> u64 {
  let seed = fast_content_hash(
    versioned_bc_seed(0x5343_5249 ^ eval_flags),
    resource_name,
  );
  fast_content_hash(seed, source)
}

#[cfg(test)]
mod cache_key_tests {
  use super::*;

  #[test]
  fn module_bytecode_cache_key_includes_name() {
    assert_ne!(
      bc_key("export const value = 1;", "file:///a.ts"),
      bc_key("export const value = 1;", "file:///b.ts")
    );
  }

  #[test]
  fn module_bytecode_cache_keys_do_not_alias_correlated_sources_and_names() {
    let mut keys = std::collections::HashSet::new();
    for i in 0..200 {
      let source = format!("export const value = {i};\n");
      let name = format!("file:///tmp/modules/mod_{i}.ts");
      assert!(keys.insert(bc_key(&source, &name)), "collision at {i}");
    }
  }

  #[test]
  fn script_bytecode_cache_key_includes_name() {
    assert_ne!(
      script_bc_key(0, b"1 + 1", b"file:///a.ts"),
      script_bc_key(0, b"1 + 1", b"file:///b.ts")
    );
  }

  #[test]
  fn bytecode_cache_seed_includes_compiler_tag() {
    assert_ne!(versioned_bc_seed(0), 0);
  }

  #[test]
  fn import_attribute_keys_are_canonical() {
    let first = import_attribute_key(vec![
      ("type".to_string(), "bytes".to_string()),
      ("mode".to_string(), "strict".to_string()),
    ]);
    let reordered = import_attribute_key(vec![
      ("mode".to_string(), "strict".to_string()),
      ("type".to_string(), "bytes".to_string()),
    ]);
    let different = import_attribute_key(vec![
      ("mode".to_string(), "strict".to_string()),
      ("type".to_string(), "text".to_string()),
    ]);

    assert_eq!(first, reordered);
    assert_ne!(first, different);
  }

  #[test]
  fn parses_import_specifier_with_semicolon() {
    let source = r#"import * as value from "data:application/typescript;base64,ZXhwb3J0";"#;
    assert_eq!(
      parse_import_specifiers(source),
      vec![(
        "data:application/typescript;base64,ZXhwb3J0".to_string(),
        None
      )]
    );
  }

  #[test]
  fn module_caches_follow_nested_isolates() {
    let outer = 1usize as *mut RealIsolate;
    let inner = 2usize as *mut RealIsolate;

    switch_module_caches(outer);
    register_module_source("node:module", "outer");

    switch_module_caches(inner);
    assert_eq!(lookup_module_source_by_name("node:module"), None);
    register_module_source("node:module", "inner");

    switch_module_caches(outer);
    assert_eq!(
      lookup_module_source_by_name("node:module").as_deref(),
      Some("outer")
    );

    discard_module_caches(inner);
    discard_module_caches(outer);
    switch_module_caches(ptr::null_mut());
  }

  #[test]
  fn missing_export_location_points_to_import_name() {
    let source = "import { add } from \"./add.js\";\nconsole.log(add);";
    let message = "SyntaxError: The requested module './add.js' does not provide an export named 'add'";
    assert_eq!(
      missing_export_location(source, message),
      Some((1, 9, Some("import { add } from \"./add.js\";".to_string())))
    );
  }

  #[test]
  fn missing_export_location_points_to_reexport_name() {
    let source =
      "export { AnInterface, isAnInterface } from \"./type_and_code.ts\";";
    let message = "SyntaxError: The requested module './type_and_code.ts' does not provide an export named 'AnInterface'";
    assert_eq!(
      missing_export_location(source, message),
      Some((
        1,
        9,
        Some(
          "export { AnInterface, isAnInterface } from \"./type_and_code.ts\";"
            .to_string()
        )
      ))
    );
  }

  #[test]
  fn missing_export_location_falls_back_for_non_text_modules() {
    let message = "SyntaxError: The requested module './math.ts' does not provide an export named 'add'";
    assert_eq!(missing_export_location("", message), Some((1, 0, None)));
  }
}

/// QuickJS takes source as a pointer plus an explicit length, but its parser
/// also reads a sentinel byte for lookahead. Keep embedded NULs intact while
/// providing that trailing sentinel.
fn eval_source_buffer(source: &str) -> Vec<u8> {
  let mut buffer = Vec::with_capacity(source.len() + 1);
  buffer.extend_from_slice(source.as_bytes());
  buffer.push(0);
  buffer
}

fn bc_path(key: u64) -> Option<std::path::PathBuf> {
  Some(bc_cache_dir()?.join(format!("{key:016x}.qbc")))
}

// Build-time embedded bytecode blob (empty unless V82JSC_BC_BLOB was set at
// build). Defines `EMBEDDED_BC: &[u8]`. Blob format: [count u32 LE] then
// `count` entries of [key u64 LE][len u32 LE][bytes]. Lets a shipped binary
// carry precompiled module bytecode instead of warming an on-disk cache.
include!(concat!(env!("OUT_DIR"), "/bc_embed.rs"));

fn embedded_bc() -> &'static HashMap<u64, &'static [u8]> {
  use std::sync::OnceLock;
  static M: OnceLock<HashMap<u64, &'static [u8]>> = OnceLock::new();
  M.get_or_init(|| {
    let mut m = HashMap::new();
    let b = EMBEDDED_BC;
    if b.len() >= 4 {
      let count = u32::from_le_bytes(b[0..4].try_into().unwrap()) as usize;
      let mut off = 4usize;
      for _ in 0..count {
        if off + 12 > b.len() {
          break;
        }
        let key = u64::from_le_bytes(b[off..off + 8].try_into().unwrap());
        let len =
          u32::from_le_bytes(b[off + 8..off + 12].try_into().unwrap()) as usize;
        off += 12;
        if off + len > b.len() {
          break;
        }
        m.insert(key, &b[off..off + len]);
        off += len;
      }
    }
    m
  })
}

pub(crate) fn bc_load(key: u64) -> Option<Vec<u8>> {
  if let Some(&bytes) = embedded_bc().get(&key) {
    return Some(bytes.to_vec());
  }
  let p = bc_path(key)?;
  std::fs::read(p).ok().filter(|b| !b.is_empty())
}

pub(crate) fn read_cached_bytecode(
  ctx: *mut JSContext,
  bytes: &[u8],
) -> JSValue {
  super::exception::with_prepare_stack_suppressed(|| unsafe {
    JS_ReadObject(ctx, bytes.as_ptr(), bytes.len(), JS_READ_OBJ_BYTECODE)
  })
}

fn bc_store(key: u64, bytes: &[u8]) {
  if bytes.is_empty() {
    return;
  }
  if let Some(p) = bc_path(key) {
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
      let _ = std::fs::rename(&tmp, &p);
    }
  }
}

pub(crate) unsafe fn bc_write(ctx: *mut JSContext, key: u64, obj: JSValue) {
  if bc_cache_dir().is_none() {
    return;
  }
  let mut size: usize = 0;
  let buf =
    unsafe { JS_WriteObject(ctx, &mut size, obj, JS_WRITE_OBJ_BYTECODE) };
  if !buf.is_null() && size > 0 {
    let slice = unsafe { std::slice::from_raw_parts(buf, size) };
    bc_store(key, slice);
  }
  if !buf.is_null() {
    unsafe { js_free(ctx, buf as *mut std::os::raw::c_void) };
  }
}

type ImportAttribute = (std::string::String, std::string::String, i32);

#[derive(Clone)]
struct DeferredImport {
  id: u64,
  specifier: std::string::String,
  source_offset: i32,
  attributes: Vec<ImportAttribute>,
}

struct ModuleState {
  context: *mut JSContext,
  status: ModuleStatus,

  module_def: *mut JSModuleDef,

  bytecode: Option<JSValue>,

  import_specifiers: Vec<(std::string::String, Option<std::string::String>)>,

  // Parallel to `import_specifiers`: byte offset of each specifier literal's
  // opening quote in the module source (deno's `referrer_source_offset`) and
  // the full `with { ... }` attribute key/value set for that import.
  import_offsets: Vec<i32>,
  import_attributes: Vec<Vec<ImportAttribute>>,

  source_imports: Vec<(u64, std::string::String)>,
  deferred_imports: Vec<DeferredImport>,

  synthetic: bool,
  engine_synthetic: bool,

  is_async: bool,

  source_text: std::string::String,
  source_name: std::string::String,
  module_name: std::string::String,
  script_id: int,

  // The `//# sourceMappingURL=` magic-comment value (if any), extracted from the
  // module source at compile time. deno reads it via
  // UnboundModuleScript::GetSourceMappingURL to register native source maps
  // (inline `data:` payloads or external `.map` files).
  source_map_url: Option<std::string::String>,
}

thread_local! {
    static MODULE_STATE: RefCell<HashMap<usize, ModuleState>> =
        RefCell::new(HashMap::new());

    static MODULE_SOURCES_BY_NAME: RefCell<HashMap<std::string::String, std::string::String>> =
        RefCell::new(HashMap::new());

    static MODULE_DEF_CACHE: RefCell<HashMap<std::string::String, usize>> =
        RefCell::new(HashMap::new());

    static RESOLVED_MODULE_TARGETS: RefCell<HashMap<(std::string::String, std::string::String), usize>> =
        RefCell::new(HashMap::new());

    static ATTRIBUTED_MODULE_DEFS: RefCell<std::collections::HashSet<usize>> =
        RefCell::new(std::collections::HashSet::new());

    static SYNTHETIC_EXPORTS: RefCell<HashMap<usize, Vec<(std::string::String, JSValue)>>> =
        RefCell::new(HashMap::new());

    // Persistent (dup'd) copy of a synthetic module's exports, keyed by def ptr,
    // kept so `GetModuleNamespace` can build the namespace object — the values in
    // SYNTHETIC_EXPORTS are consumed by JS_SetModuleExport during init.
    static SYNTHETIC_NS_EXPORTS: RefCell<HashMap<usize, Vec<(std::string::String, JSValue)>>> =
        RefCell::new(HashMap::new());

    static SYNTHETIC_DEFS: RefCell<HashMap<usize, usize>> = RefCell::new(HashMap::new());

    static SYNTHETIC_EXPORT_NAMES: RefCell<HashMap<usize, std::collections::HashSet<std::string::String>>> =
        RefCell::new(HashMap::new());

    static SYNTHETIC_EVAL_STEPS: RefCell<HashMap<usize, (SyntheticModuleEvaluationSteps<'static>, JSValue)>> =
        RefCell::new(HashMap::new());

    static AFTER_FIRST_EVAL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };

    static MODULE_EVAL_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };

    static RESOLVED_SPECIFIERS: RefCell<HashMap<(std::string::String, std::string::String), std::string::String>> =
        RefCell::new(HashMap::new());

    static SCRIPT_SOURCE_MAP_URLS: RefCell<HashMap<usize, std::string::String>> =
        RefCell::new(HashMap::new());

    static MODULE_SCRIPT_IDS_BY_NAME: RefCell<HashMap<std::string::String, int>> =
        RefCell::new(HashMap::new());

    static NEXT_MODULE_SCRIPT_ID: std::cell::Cell<int> = const { std::cell::Cell::new(1) };

    static SCRIPT_IDS: RefCell<HashMap<usize, int>> = RefCell::new(HashMap::new());
}

#[repr(C)]
struct RawScriptOrigin {
  resource_name: usize,
  source_map_url: usize,
  script_id: int,
  resource_line_offset: int,
  resource_column_offset: int,
  host_defined_options: usize,
}

#[repr(C)]
struct RawSource {
  source_string: usize,
  resource_name: usize,
  resource_line_offset: int,
  resource_column_offset: int,
  resource_options: int,
  source_map_url: usize,
  host_defined_options: usize,
  cached_data: usize,
}

fn next_module_script_id() -> int {
  NEXT_MODULE_SCRIPT_ID.with(|next| {
    let id = next.get();
    next.set(id.saturating_add(1).max(1));
    id
  })
}

fn assign_module_script_id(name: &str) -> int {
  let id = next_module_script_id();
  if !name.is_empty() {
    MODULE_SCRIPT_IDS_BY_NAME.with(|m| {
      m.borrow_mut().insert(name.to_string(), id);
    });
  }
  id
}

fn module_script_id_for_name(name: &str) -> int {
  if name.is_empty() {
    return assign_module_script_id(name);
  }
  MODULE_SCRIPT_IDS_BY_NAME
    .with(|m| m.borrow().get(name).copied())
    .unwrap_or_else(|| assign_module_script_id(name))
}

thread_local! {

    static IMPORT_META_ENABLED: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };

    // deno's real HostInitializeImportMetaObjectCallback. When set we route
    // import.meta population through it (so url/main/resolve/filename/dirname all
    // match deno exactly — notably `import.meta.resolve` uses the embedder's
    // loader). Only the native fallback below runs when this is None.
    static IMPORT_META_CB: std::cell::Cell<
        Option<crate::isolate::HostInitializeImportMetaObjectCallback>,
    > = const { std::cell::Cell::new(None) };

    // Maps a module's source name -> the raw `JSValue` of its deno Module
    // WRAPPER (the handle CompileModule returned). deno's import_meta callback
    // looks the module up by Global identity (`get_name_by_module`), so we must
    // hand it a handle naming the EXACT object deno registered. We store the raw
    // JSValue (not the interned handle pointer) because the handle's heap box is
    // freed when its HandleScope unwinds — the object itself stays alive via
    // deno's Global, so the JSValue payload (`u.ptr`) remains a valid identity.
    static MODULE_WRAPPER_BY_NAME: RefCell<HashMap<std::string::String, JSValue>> =
        RefCell::new(HashMap::new());

    // URL of the main module — the first user (file://http(s)) module deno
    // compiles. Used to set `import.meta.main` (deno's host callback, which we
    // bypass, would normally do this).
    static MAIN_MODULE_URL: std::cell::RefCell<Option<std::string::String>> =
        const { std::cell::RefCell::new(None) };
}

fn record_module_wrapper(name: &str, wrapper: *const Module) {
  if name.is_empty() || wrapper.is_null() {
    return;
  }
  let v = jsval_of(wrapper);
  MODULE_WRAPPER_BY_NAME.with(|t| {
    t.borrow_mut().insert(name.to_string(), v);
  });
}

fn lookup_module_wrapper(name: &str) -> Option<JSValue> {
  MODULE_WRAPPER_BY_NAME.with(|t| t.borrow().get(name).copied())
}

fn note_main_module(name: &str) {
  if !(name.starts_with("file://")
    || name.starts_with("http://")
    || name.starts_with("https://"))
  {
    return;
  }
  MAIN_MODULE_URL.with(|c| {
    let mut b = c.borrow_mut();
    if b.is_none() {
      *b = Some(name.to_string());
    }
  });
}

fn is_main_module(name: &str) -> bool {
  MAIN_MODULE_URL.with(|c| c.borrow().as_deref() == Some(name))
}

pub(crate) fn set_import_meta_callback(
  cb: crate::isolate::HostInitializeImportMetaObjectCallback,
) {
  IMPORT_META_ENABLED.with(|c| c.set(true));
  IMPORT_META_CB.with(|c| c.set(Some(cb)));
  unsafe { JS_SetImportMetaHook(Some(import_meta_hook)) };
}

unsafe extern "C" fn import_meta_hook(
  ctx: *mut JSContext,
  def: *mut JSModuleDef,
  meta: JSValue,
) -> int {
  if !IMPORT_META_ENABLED.with(|enabled| enabled.get()) {
    return 0;
  }
  let name = MODULE_STATE.with(|modules| {
    modules
      .borrow()
      .values()
      .find(|module| module.context == ctx && module.module_def == def)
      .map(|module| module.module_name.clone())
  });
  let Some(name) = name else {
    return 0;
  };
  let Some(callback) = IMPORT_META_CB.with(|callback| callback.get()) else {
    return 0;
  };
  let Some(wrapper_value) = lookup_module_wrapper(&name) else {
    return 0;
  };
  let wrapper = intern_dup::<Module>(ctx, wrapper_value);
  let meta = intern_dup::<Object>(ctx, meta);
  let context = intern_ctx(ctx);
  let (Some(context), Some(module), Some(meta)) = (
    unsafe { crate::Local::from_raw(context) },
    unsafe { crate::Local::from_raw(wrapper) },
    unsafe { crate::Local::from_raw(meta) },
  ) else {
    return 0;
  };
  unsafe { callback(context, module, meta) };
  if unsafe { JS_HasException(ctx) } {
    -1
  } else {
    0
  }
}

thread_local! {

    static DYN_IMPORT_CB: std::cell::Cell<
        Option<crate::isolate::RawHostImportModuleDynamicallyCallback>,
    > = const { std::cell::Cell::new(None) };

    static DYN_IMPORT_PHASE_CB: std::cell::Cell<
        Option<crate::isolate::RawHostImportModuleWithPhaseDynamicallyCallback>,
    > = const { std::cell::Cell::new(None) };

    static DYN_IMPORT_CHAIN: std::cell::Cell<Option<JSValue>> =
        const { std::cell::Cell::new(None) };

    static SOURCE_CB: std::cell::Cell<Option<ResolveSourceCallback<'static>>> =
        const { std::cell::Cell::new(None) };

    static ACTIVE_MODULE_ISOLATE: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };

    static MODULE_CACHE_STATES: RefCell<HashMap<usize, ModuleCacheState>> =
        RefCell::new(HashMap::new());

}

#[derive(Default)]
struct ModuleCacheState {
  module_state: HashMap<usize, ModuleState>,
  module_sources_by_name: HashMap<std::string::String, std::string::String>,
  module_def_cache: HashMap<std::string::String, usize>,
  resolved_module_targets:
    HashMap<(std::string::String, std::string::String), usize>,
  attributed_module_defs: std::collections::HashSet<usize>,
  synthetic_exports: HashMap<usize, Vec<(std::string::String, JSValue)>>,
  synthetic_ns_exports: HashMap<usize, Vec<(std::string::String, JSValue)>>,
  synthetic_defs: HashMap<usize, usize>,
  synthetic_export_names:
    HashMap<usize, std::collections::HashSet<std::string::String>>,
  synthetic_eval_steps:
    HashMap<usize, (SyntheticModuleEvaluationSteps<'static>, JSValue)>,
  after_first_eval: bool,
  module_eval_depth: u32,
  resolved_specifiers:
    HashMap<(std::string::String, std::string::String), std::string::String>,
  script_source_map_urls: HashMap<usize, std::string::String>,
  module_script_ids_by_name: HashMap<std::string::String, int>,
  next_module_script_id: int,
  import_meta_enabled: bool,
  import_meta_cb:
    Option<crate::isolate::HostInitializeImportMetaObjectCallback>,
  module_wrapper_by_name: HashMap<std::string::String, JSValue>,
  main_module_url: Option<std::string::String>,
  dyn_import_cb: Option<crate::isolate::RawHostImportModuleDynamicallyCallback>,
  dyn_import_phase_cb:
    Option<crate::isolate::RawHostImportModuleWithPhaseDynamicallyCallback>,
  dyn_import_chain: Option<JSValue>,
  source_cb: Option<ResolveSourceCallback<'static>>,
  src_phase_counter: u64,
}

impl ModuleCacheState {
  fn empty() -> Self {
    Self {
      next_module_script_id: 1,
      src_phase_counter: 1,
      ..Self::default()
    }
  }
}

fn take_module_cache_state() -> ModuleCacheState {
  ModuleCacheState {
    module_state: MODULE_STATE.with(|v| std::mem::take(&mut *v.borrow_mut())),
    module_sources_by_name: MODULE_SOURCES_BY_NAME
      .with(|v| std::mem::take(&mut *v.borrow_mut())),
    module_def_cache: MODULE_DEF_CACHE
      .with(|v| std::mem::take(&mut *v.borrow_mut())),
    resolved_module_targets: RESOLVED_MODULE_TARGETS
      .with(|v| std::mem::take(&mut *v.borrow_mut())),
    attributed_module_defs: ATTRIBUTED_MODULE_DEFS
      .with(|v| std::mem::take(&mut *v.borrow_mut())),
    synthetic_exports: SYNTHETIC_EXPORTS
      .with(|v| std::mem::take(&mut *v.borrow_mut())),
    synthetic_ns_exports: SYNTHETIC_NS_EXPORTS
      .with(|v| std::mem::take(&mut *v.borrow_mut())),
    synthetic_defs: SYNTHETIC_DEFS
      .with(|v| std::mem::take(&mut *v.borrow_mut())),
    synthetic_export_names: SYNTHETIC_EXPORT_NAMES
      .with(|v| std::mem::take(&mut *v.borrow_mut())),
    synthetic_eval_steps: SYNTHETIC_EVAL_STEPS
      .with(|v| std::mem::take(&mut *v.borrow_mut())),
    after_first_eval: AFTER_FIRST_EVAL.with(|v| v.replace(false)),
    module_eval_depth: MODULE_EVAL_DEPTH.with(|v| v.replace(0)),
    resolved_specifiers: RESOLVED_SPECIFIERS
      .with(|v| std::mem::take(&mut *v.borrow_mut())),
    script_source_map_urls: SCRIPT_SOURCE_MAP_URLS
      .with(|v| std::mem::take(&mut *v.borrow_mut())),
    module_script_ids_by_name: MODULE_SCRIPT_IDS_BY_NAME
      .with(|v| std::mem::take(&mut *v.borrow_mut())),
    next_module_script_id: NEXT_MODULE_SCRIPT_ID.with(|v| v.replace(1)),
    import_meta_enabled: IMPORT_META_ENABLED.with(|v| v.replace(false)),
    import_meta_cb: IMPORT_META_CB.with(|v| v.replace(None)),
    module_wrapper_by_name: MODULE_WRAPPER_BY_NAME
      .with(|v| std::mem::take(&mut *v.borrow_mut())),
    main_module_url: MAIN_MODULE_URL.with(|v| v.borrow_mut().take()),
    dyn_import_cb: DYN_IMPORT_CB.with(|v| v.replace(None)),
    dyn_import_phase_cb: DYN_IMPORT_PHASE_CB.with(|v| v.replace(None)),
    dyn_import_chain: DYN_IMPORT_CHAIN.with(|v| v.replace(None)),
    source_cb: SOURCE_CB.with(|v| v.replace(None)),
    src_phase_counter: SRC_PHASE_COUNTER.with(|v| v.replace(1)),
  }
}

fn restore_module_cache_state(state: ModuleCacheState) {
  MODULE_STATE.with(|v| *v.borrow_mut() = state.module_state);
  MODULE_SOURCES_BY_NAME
    .with(|v| *v.borrow_mut() = state.module_sources_by_name);
  MODULE_DEF_CACHE.with(|v| *v.borrow_mut() = state.module_def_cache);
  RESOLVED_MODULE_TARGETS
    .with(|v| *v.borrow_mut() = state.resolved_module_targets);
  ATTRIBUTED_MODULE_DEFS
    .with(|v| *v.borrow_mut() = state.attributed_module_defs);
  SYNTHETIC_EXPORTS.with(|v| *v.borrow_mut() = state.synthetic_exports);
  SYNTHETIC_NS_EXPORTS.with(|v| *v.borrow_mut() = state.synthetic_ns_exports);
  SYNTHETIC_DEFS.with(|v| *v.borrow_mut() = state.synthetic_defs);
  SYNTHETIC_EXPORT_NAMES
    .with(|v| *v.borrow_mut() = state.synthetic_export_names);
  SYNTHETIC_EVAL_STEPS.with(|v| *v.borrow_mut() = state.synthetic_eval_steps);
  AFTER_FIRST_EVAL.with(|v| v.set(state.after_first_eval));
  MODULE_EVAL_DEPTH.with(|v| v.set(state.module_eval_depth));
  RESOLVED_SPECIFIERS.with(|v| *v.borrow_mut() = state.resolved_specifiers);
  SCRIPT_SOURCE_MAP_URLS
    .with(|v| *v.borrow_mut() = state.script_source_map_urls);
  MODULE_SCRIPT_IDS_BY_NAME
    .with(|v| *v.borrow_mut() = state.module_script_ids_by_name);
  NEXT_MODULE_SCRIPT_ID.with(|v| v.set(state.next_module_script_id));
  IMPORT_META_ENABLED.with(|v| v.set(state.import_meta_enabled));
  IMPORT_META_CB.with(|v| v.set(state.import_meta_cb));
  MODULE_WRAPPER_BY_NAME
    .with(|v| *v.borrow_mut() = state.module_wrapper_by_name);
  MAIN_MODULE_URL.with(|v| *v.borrow_mut() = state.main_module_url);
  DYN_IMPORT_CB.with(|v| v.set(state.dyn_import_cb));
  DYN_IMPORT_PHASE_CB.with(|v| v.set(state.dyn_import_phase_cb));
  DYN_IMPORT_CHAIN.with(|v| v.set(state.dyn_import_chain));
  SOURCE_CB.with(|v| v.set(state.source_cb));
  SRC_PHASE_COUNTER.with(|v| v.set(state.src_phase_counter));
}

pub(crate) fn switch_module_caches(isolate: *mut RealIsolate) {
  let next = isolate as usize;
  ACTIVE_MODULE_ISOLATE.with(|active| {
    let current = active.get();
    if current == next {
      return;
    }
    if current != 0 {
      let state = take_module_cache_state();
      MODULE_CACHE_STATES.with(|states| {
        states.borrow_mut().insert(current, state);
      });
    }
    let state = if next == 0 {
      ModuleCacheState::empty()
    } else {
      MODULE_CACHE_STATES
        .with(|states| states.borrow_mut().remove(&next))
        .unwrap_or_else(ModuleCacheState::empty)
    };
    restore_module_cache_state(state);
    active.set(next);
  });
}

pub(crate) fn discard_module_caches(isolate: *mut RealIsolate) {
  let key = isolate as usize;
  ACTIVE_MODULE_ISOLATE.with(|active| {
    if active.get() == key {
      drop(take_module_cache_state());
      active.set(0);
    }
  });
  MODULE_CACHE_STATES.with(|states| {
    states.borrow_mut().remove(&key);
  });
}

pub(crate) fn set_dynamic_import_callback(
  cb: crate::isolate::RawHostImportModuleDynamicallyCallback,
) {
  DYN_IMPORT_CB.with(|c| c.set(Some(cb)));
  unsafe { JS_SetDynamicImportHook(dynamic_import_hook) };
}

pub(crate) fn set_dynamic_import_with_phase_callback(
  cb: crate::isolate::RawHostImportModuleWithPhaseDynamicallyCallback,
) {
  DYN_IMPORT_PHASE_CB.with(|c| c.set(Some(cb)));
}

pub(crate) unsafe fn install_dynamic_source_import_global(
  ctx: *mut JSContext,
  global: JSValue,
) {
  let import_source = unsafe {
    JS_NewCFunction(
      ctx,
      dynamic_source_import_js_cb,
      c"__v8x_import_source".as_ptr(),
      1,
    )
  };
  unsafe {
    super::core::define_internal_global(
      ctx,
      global,
      c"__v8x_import_source",
      import_source,
    );
  }
}

fn empty_host_defined_options(ctx: *mut JSContext) -> *const Data {
  intern::<Data>(unsafe { JS_NewArray(ctx) })
}

pub(crate) unsafe fn ensure_dynamic_defer_import_global(ctx: *mut JSContext) {
  let global = unsafe { JS_GetGlobalObject(ctx) };
  let existing =
    unsafe { JS_GetPropertyStr(ctx, global, c"__v8x_import_defer".as_ptr()) };
  let absent = jsv_is_undefined(&existing) || jsv_is_null(&existing);
  unsafe { JS_FreeValue(ctx, existing) };
  if absent {
    let import_defer = unsafe {
      JS_NewCFunction(
        ctx,
        dynamic_defer_import_js_cb,
        c"__v8x_import_defer".as_ptr(),
        1,
      )
    };
    unsafe {
      super::core::define_internal_global(
        ctx,
        global,
        c"__v8x_import_defer",
        import_defer,
      );
    }
  }
  unsafe { JS_FreeValue(ctx, global) };
}

unsafe extern "C" fn dynamic_source_import_js_cb(
  ctx: *mut JSContext,
  _this_val: JSValue,
  argc: int,
  argv: *mut JSValue,
) -> JSValue {
  unsafe {
    dynamic_phase_import_js_cb(ctx, argc, argv, ModuleImportPhase::kSource)
  }
}

unsafe extern "C" fn dynamic_defer_import_js_cb(
  ctx: *mut JSContext,
  _this_val: JSValue,
  argc: int,
  argv: *mut JSValue,
) -> JSValue {
  unsafe {
    dynamic_phase_import_js_cb(ctx, argc, argv, ModuleImportPhase::kDefer)
  }
}

unsafe fn dynamic_phase_import_js_cb(
  ctx: *mut JSContext,
  argc: int,
  argv: *mut JSValue,
  phase: ModuleImportPhase,
) -> JSValue {
  let Some(cb) = DYN_IMPORT_PHASE_CB.with(|c| c.get()) else {
    return unsafe {
      JS_ThrowTypeError(
        ctx,
        c"dynamic source import: no host callback".as_ptr(),
      )
    };
  };
  if ctx.is_null() || argc < 1 || argv.is_null() {
    return unsafe {
      JS_ThrowTypeError(
        ctx,
        c"dynamic source import: missing specifier".as_ptr(),
      )
    };
  }

  let has_explicit_referrer = argc >= 2;
  let spec_index = usize::from(has_explicit_referrer);
  let spec = unsafe { jsval_to_rust(ctx, *argv.add(spec_index)) };
  let Ok(cspec) = CString::new(spec.as_str()) else {
    return unsafe {
      JS_ThrowTypeError(
        ctx,
        c"dynamic source import: invalid specifier".as_ptr(),
      )
    };
  };
  let spec_handle =
    intern::<V8String>(unsafe { JS_NewString(ctx, cspec.as_ptr()) });

  let host_opts = current_host_defined_options();
  let host_opts = if host_opts.is_null() {
    empty_host_defined_options(ctx)
  } else {
    host_opts
  };

  let referrer_name = if has_explicit_referrer {
    unsafe { jsval_to_rust(ctx, *argv) }
  } else {
    current_script_name_or_source_url().unwrap_or_default()
  };
  let referrer = if referrer_name == "<eval>" || referrer_name == "<anonymous>"
  {
    unsafe { JS_NewString(ctx, c"".as_ptr()) }
  } else if let Ok(cname) = CString::new(referrer_name.as_str()) {
    unsafe { JS_NewString(ctx, cname.as_ptr()) }
  } else {
    unsafe { JS_NewString(ctx, c"".as_ptr()) }
  };
  let referrer = intern::<Value>(referrer);
  let attrs_handle = intern::<FixedArray>(unsafe { JS_NewArray(ctx) });
  let context = intern_ctx(ctx);

  let (Some(ctx_l), Some(ho_l), Some(ref_l), Some(spec_l), Some(attr_l)) = (
    unsafe { crate::Local::from_raw(context) },
    unsafe { crate::Local::from_raw(host_opts) },
    unsafe { crate::Local::from_raw(referrer) },
    unsafe { crate::Local::from_raw(spec_handle) },
    unsafe { crate::Local::from_raw(attrs_handle) },
  ) else {
    return unsafe {
      JS_ThrowTypeError(
        ctx,
        c"dynamic source import: handle alloc failed".as_ptr(),
      )
    };
  };

  // Windows ABI: leading return-slot out-param (see run_synthetic_eval_steps).
  #[cfg(not(target_os = "windows"))]
  let promise_ptr = unsafe { cb(ctx_l, ho_l, ref_l, spec_l, phase, attr_l) };
  #[cfg(target_os = "windows")]
  let promise_ptr = unsafe {
    let mut ret = ptr::null_mut();
    cb(&mut ret, ctx_l, ho_l, ref_l, spec_l, phase, attr_l);
    ret
  };
  if promise_ptr.is_null() {
    if unsafe { JS_HasException(ctx) } {
      return jsv_exception();
    }
    return unsafe {
      JS_ThrowTypeError(
        ctx,
        c"dynamic source import: host callback returned null".as_ptr(),
      )
    };
  }

  unsafe { JS_DupValue(ctx, jsval_of(promise_ptr as *const Value)) }
}

pub(crate) fn rewrite_dynamic_phase_imports(
  body: &str,
  module: bool,
) -> Option<std::string::String> {
  if !body.contains("import.source") && !body.contains("import.defer") {
    return None;
  }

  let bytes = body.as_bytes();
  let mut out: Vec<u8> = Vec::with_capacity(body.len());
  let mut i = 0;
  let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
  let mut changed = false;
  while i < bytes.len() {
    let replacement = if bytes[i] == b'i' {
      if body[i..].starts_with("import.source") {
        Some((
          "import.source",
          if module {
            "globalThis.__v8x_import_source(import.meta.url,"
          } else {
            "globalThis.__v8x_import_source("
          },
        ))
      } else if body[i..].starts_with("import.defer") {
        Some((
          "import.defer",
          if module {
            "globalThis.__v8x_import_defer(import.meta.url,"
          } else {
            "globalThis.__v8x_import_defer("
          },
        ))
      } else {
        None
      }
    } else {
      None
    };
    if bytes[i] == b'i'
      && (i == 0 || !is_ident(bytes[i - 1]))
      && let Some((syntax, replacement)) = replacement
    {
      let mut j = i + syntax.len();
      while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
        j += 1;
      }
      if j < bytes.len() && bytes[j] == b'(' {
        out.extend_from_slice(replacement.as_bytes());
        i = j + 1;
        changed = true;
        continue;
      }
    }
    out.push(bytes[i]);
    i += 1;
  }

  if changed {
    Some(
      std::string::String::from_utf8(out).unwrap_or_else(|_| body.to_string()),
    )
  } else {
    None
  }
}

// Flatten QuickJS's null-proto `with` attributes object (e.g. {type:"json"})
// into deno's host-callback FixedArray shape [key1, val1, key2, val2, ...].
unsafe fn build_dyn_attrs(ctx: *mut JSContext, attributes: JSValue) -> JSValue {
  if !jsv_is_object(&attributes) {
    return unsafe { JS_NewArray(ctx) };
  }
  let src =
    c"(o)=>{const a=[];for(const k of Object.keys(o)){a.push(k,String(o[k]));}return a;}";
  let f = unsafe {
    JS_Eval(
      ctx,
      src.as_ptr(),
      src.to_bytes().len(),
      c"<dynimport-attrs>".as_ptr(),
      JS_EVAL_TYPE_GLOBAL,
    )
  };
  if f.tag == JS_TAG_EXCEPTION {
    unsafe { JS_FreeValue(ctx, JS_GetException(ctx)) };
    return unsafe { JS_NewArray(ctx) };
  }
  let mut arg = [attributes];
  let r = unsafe { JS_Call(ctx, f, jsv_undefined(), 1, arg.as_mut_ptr()) };
  unsafe { JS_FreeValue(ctx, f) };
  if r.tag == JS_TAG_EXCEPTION {
    unsafe { JS_FreeValue(ctx, JS_GetException(ctx)) };
    return unsafe { JS_NewArray(ctx) };
  }
  r
}

unsafe fn build_static_import_attrs(
  ctx: *mut JSContext,
  pairs: &[ImportAttribute],
) -> JSValue {
  let arr = unsafe { JS_NewArray(ctx) };
  if arr.tag == JS_TAG_EXCEPTION {
    return arr;
  }

  let mut out = 0u32;
  for (key, value, offset) in pairs {
    let (Ok(ck), Ok(cv)) =
      (CString::new(key.as_str()), CString::new(value.as_str()))
    else {
      continue;
    };
    unsafe {
      JS_SetPropertyUint32(ctx, arr, out, JS_NewString(ctx, ck.as_ptr()));
      JS_SetPropertyUint32(ctx, arr, out + 1, JS_NewString(ctx, cv.as_ptr()));
      JS_SetPropertyUint32(ctx, arr, out + 2, JS_NewInt32(ctx, *offset));
    }
    out += 3;
  }

  arr
}

fn import_attribute_key(
  mut pairs: Vec<(std::string::String, std::string::String)>,
) -> std::string::String {
  pairs.sort_unstable();
  let mut key = std::string::String::new();
  for (name, value) in pairs {
    key.push_str(&format!("{}:{name}{}:{value}", name.len(), value.len()));
  }
  key
}

fn static_import_attribute_key(
  pairs: &[ImportAttribute],
) -> std::string::String {
  import_attribute_key(
    pairs
      .iter()
      .map(|(name, value, _)| (name.clone(), value.clone()))
      .collect(),
  )
}

unsafe fn module_import_attribute_key(
  ctx: *mut JSContext,
  attributes: JSValue,
) -> std::string::String {
  if !jsv_is_object(&attributes) {
    return std::string::String::new();
  }
  let mut properties: *mut ModulePropertyEnum = ptr::null_mut();
  let mut len = 0u32;
  if unsafe {
    JS_GetOwnPropertyNames(
      ctx,
      &mut properties,
      &mut len,
      attributes,
      JS_GPN_STRING_MASK | JS_GPN_ENUM_ONLY,
    )
  } < 0
  {
    let exception = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exception) };
    return std::string::String::new();
  }

  let mut pairs = Vec::with_capacity(len as usize);
  for index in 0..len as usize {
    let atom = unsafe { (*properties.add(index)).atom };
    let name = unsafe { JS_AtomToString(ctx, atom) };
    let value = unsafe { JS_GetProperty(ctx, attributes, atom) };
    if name.tag != JS_TAG_EXCEPTION && value.tag != JS_TAG_EXCEPTION {
      pairs.push((unsafe { jsval_to_rust(ctx, name) }, unsafe {
        jsval_to_rust(ctx, value)
      }));
    }
    if name.tag == JS_TAG_EXCEPTION || value.tag == JS_TAG_EXCEPTION {
      let exception = unsafe { JS_GetException(ctx) };
      unsafe { JS_FreeValue(ctx, exception) };
    }
    unsafe {
      JS_FreeValue(ctx, name);
      JS_FreeValue(ctx, value);
      JS_FreeAtom(ctx, atom);
    }
  }
  if !properties.is_null() {
    unsafe { js_free(ctx, properties.cast::<std::os::raw::c_void>()) };
  }
  import_attribute_key(pairs)
}

unsafe extern "C" fn dynamic_import_hook(
  ctx: *mut JSContext,
  function: JSValue,
  basename: JSValue,
  specifier: JSValue,
  attributes: JSValue,
  resolving_funcs: *const JSValue,
) {
  let resolve = unsafe { *resolving_funcs };
  let reject = unsafe { *resolving_funcs.add(1) };

  if std::env::var_os("QJS_DBG_DYNIMPORT").is_some() {
    let specifier = unsafe { jsval_to_rust(ctx, specifier) };
    eprintln!("[dynimport] specifier={specifier:?}");
  }

  let reject_with = |msg: &str| unsafe {
    if let Ok(cm) = CString::new(msg) {
      let mut a = [JS_NewString(ctx, cm.as_ptr())];
      let r = JS_Call(ctx, reject, jsv_undefined(), 1, a.as_mut_ptr());
      JS_FreeValue(ctx, r);
      JS_FreeValue(ctx, a[0]);
    }
  };

  let Some(cb) = DYN_IMPORT_CB.with(|c| c.get()) else {
    reject_with("dynamic import: no host callback");
    return;
  };

  let context = intern_ctx(ctx);
  let base_str = unsafe { jsval_to_rust(ctx, basename) };
  let mut host_opts = current_host_defined_options();
  if host_opts.is_null() {
    host_opts = host_defined_options_for_script_value(function);
  }
  if host_opts.is_null() {
    host_opts = host_defined_options_for_name(&base_str);
  }
  let host_opts = if host_opts.is_null() {
    empty_host_defined_options(ctx)
  } else {
    host_opts
  };
  // Scripts compiled without a resource name run under our synthetic
  // "<eval>"/"<anonymous>" filenames; V8 reports an EMPTY referrer for
  // those (deno prints "(no referrer)").
  let referrer = if base_str == "<eval>" || base_str == "<anonymous>" {
    let empty = unsafe { JS_NewString(ctx, c"".as_ptr()) };
    intern::<Value>(empty)
  } else {
    intern_dup::<Value>(ctx, basename)
  };
  let spec_handle = intern_dup::<V8String>(ctx, specifier);
  let attrs_handle =
    intern::<FixedArray>(unsafe { build_dyn_attrs(ctx, attributes) });
  let (Some(ctx_l), Some(ho_l), Some(ref_l), Some(spec_l), Some(attr_l)) = (
    unsafe { crate::Local::from_raw(context) },
    unsafe { crate::Local::from_raw(host_opts) },
    unsafe { crate::Local::from_raw(referrer) },
    unsafe { crate::Local::from_raw(spec_handle) },
    unsafe { crate::Local::from_raw(attrs_handle) },
  ) else {
    reject_with("dynamic import: handle alloc failed");
    return;
  };

  // Windows ABI: leading return-slot out-param (see run_synthetic_eval_steps).
  #[cfg(not(target_os = "windows"))]
  let promise_ptr = unsafe { cb(ctx_l, ho_l, ref_l, spec_l, attr_l) };
  #[cfg(target_os = "windows")]
  let promise_ptr = unsafe {
    let mut ret = ptr::null_mut();
    cb(&mut ret, ctx_l, ho_l, ref_l, spec_l, attr_l);
    ret
  };
  if promise_ptr.is_null() {
    if unsafe { JS_HasException(ctx) } {
      let mut a = [unsafe { JS_GetException(ctx) }];
      unsafe { v82jsc_mark_skip_next_async_frame(ctx, a[0], basename) };
      let r =
        unsafe { JS_Call(ctx, reject, jsv_undefined(), 1, a.as_mut_ptr()) };
      unsafe { JS_FreeValue(ctx, r) };
      unsafe { JS_FreeValue(ctx, a[0]) };
    } else {
      reject_with("dynamic import: host callback returned null");
    }
    return;
  }
  let d = jsval_of(promise_ptr as *const Value);

  let chain = DYN_IMPORT_CHAIN.with(|c| {
    if let Some(cur) = c.get() {
      return cur;
    }
    let src = c"(d,res,rej,mark,base)=>{Promise.resolve(d).then(res,e=>mark(e,base,rej));}";
    let f = unsafe {
      JS_Eval(
        ctx,
        src.as_ptr(),
        src.to_bytes().len(),
        c"<dynimport-chain>".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      )
    };
    if f.tag != JS_TAG_EXCEPTION {
      c.set(Some(f));
    }
    f
  });
  if chain.tag == JS_TAG_EXCEPTION {
    reject_with("dynamic import: chain helper failed");
    return;
  }
  let mark = unsafe {
    JS_NewCFunction(
      ctx,
      dynamic_import_reject,
      c"markDynamicImportReject".as_ptr(),
      3,
    )
  };
  if mark.tag == JS_TAG_EXCEPTION {
    reject_with("dynamic import: reject helper failed");
    return;
  }
  let mut args = [d, resolve, reject, mark, basename];
  let r = unsafe { JS_Call(ctx, chain, jsv_undefined(), 5, args.as_mut_ptr()) };
  unsafe {
    JS_FreeValue(ctx, mark);
    JS_FreeValue(ctx, r);
  }
}

unsafe extern "C" fn dynamic_import_reject(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: int,
  argv: *mut JSValue,
) -> JSValue {
  if argc < 3 || argv.is_null() {
    return jsv_undefined();
  }
  let reason = unsafe { *argv };
  let basename = unsafe { *argv.add(1) };
  let reject = unsafe { *argv.add(2) };
  unsafe {
    v82jsc_mark_skip_next_async_frame(ctx, reason, basename);
    JS_Call(ctx, reject, jsv_undefined(), 1, argv)
  }
}

/// Tape replay: bytecode-born module defs bypass the loader, so their
/// `import.meta` must be populated explicitly before evaluation.
pub(crate) fn populate_import_meta_for_replay(
  ctx: *mut JSContext,
  def: usize,
  name: &str,
) {
  unsafe { populate_import_meta(ctx, def as *mut JSModuleDef, name) }
}

unsafe fn populate_import_meta(
  ctx: *mut JSContext,
  def: *mut JSModuleDef,
  name: &str,
) {
  if def.is_null() || !IMPORT_META_ENABLED.with(|c| c.get()) {
    return;
  }
  let source_name = source_name_for_module_name(name);
  let source_name = source_name.as_str();

  // Preferred path: hand the meta object to deno's real
  // HostInitializeImportMetaObjectCallback. It populates url/main/filename/
  // dirname and installs `import.meta.resolve` backed by the embedder's loader —
  // none of which the native fallback below can do faithfully. Requires the
  // exact Module wrapper deno registered (matched by Global identity).
  if let Some(cb) = IMPORT_META_CB.with(|c| c.get()) {
    if let Some(wrapper_val) = lookup_module_wrapper(name) {
      // Re-intern a fresh scope-bound handle naming deno's registered wrapper
      // object (the stored handle's box may already have been freed).
      let wrapper = intern_dup::<Module>(ctx, wrapper_val);
      if wrapper.is_null() {
        return;
      }
      let meta = unsafe { JS_GetImportMeta(ctx, def) };
      if meta.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return;
      }
      unsafe { v82jsc_mark_import_meta_initialized(def) };
      let context = intern_ctx(ctx);
      let meta_handle = intern::<Object>(meta);
      let (Some(ctx_l), Some(mod_l), Some(meta_l)) = (
        unsafe { crate::Local::from_raw(context) },
        unsafe { crate::Local::from_raw(wrapper) },
        unsafe { crate::Local::from_raw(meta_handle) },
      ) else {
        return;
      };
      unsafe { cb(ctx_l, mod_l, meta_l) };
      // deno's callback may throw (e.g. WebAssembly-unavailable); clear so a
      // spurious pending exception doesn't poison the subsequent eval.
      if unsafe { JS_HasException(ctx) } {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
      }
      return;
    }
  }

  if !(source_name.starts_with("file://")
    || source_name.starts_with("http://")
    || source_name.starts_with("https://"))
  {
    return;
  }
  let meta = unsafe { JS_GetImportMeta(ctx, def) };
  if meta.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return;
  }
  unsafe { v82jsc_mark_import_meta_initialized(def) };
  if let Ok(curl) = CString::new(source_name) {
    let url_val = unsafe { JS_NewString(ctx, curl.as_ptr()) };
    unsafe { JS_SetPropertyStr(ctx, meta, c"url".as_ptr(), url_val) };

    // import.meta.resolve(spec): deno's host callback installs this; we bypass
    // the callback and populate import.meta natively, so add it here. Resolves
    // relative to this module's URL — matches deno for relative/absolute
    // specifiers (import-map / npm: resolution would need deno's resolver).
    let factory_src = c"(u)=>(s)=>new URL(s,u).href";
    let factory = unsafe {
      JS_Eval(
        ctx,
        factory_src.as_ptr(),
        factory_src.to_bytes().len(),
        c"<import-meta-resolve>".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      )
    };
    if factory.tag != JS_TAG_EXCEPTION {
      let mut arg = [unsafe { JS_NewString(ctx, curl.as_ptr()) }];
      let resolve_fn =
        unsafe { JS_Call(ctx, factory, jsv_undefined(), 1, arg.as_mut_ptr()) };
      unsafe { JS_FreeValue(ctx, arg[0]) };
      if resolve_fn.tag != JS_TAG_EXCEPTION {
        unsafe {
          JS_SetPropertyStr(ctx, meta, c"resolve".as_ptr(), resolve_fn)
        };
      } else {
        unsafe { JS_FreeValue(ctx, JS_GetException(ctx)) };
      }
    } else {
      unsafe { JS_FreeValue(ctx, JS_GetException(ctx)) };
    }
    unsafe { JS_FreeValue(ctx, factory) };
  }
  let is_main = if is_main_module(source_name) { 1 } else { 0 };
  unsafe {
    JS_SetPropertyStr(ctx, meta, c"main".as_ptr(), JS_NewBool(ctx, is_main))
  };

  if source_name.ends_with(".wasm") {
    let global = unsafe { JS_GetGlobalObject(ctx) };
    let wasm =
      unsafe { JS_GetPropertyStr(ctx, global, c"WebAssembly".as_ptr()) };
    if jsv_is_object(&wasm) {
      let inst = unsafe { JS_GetPropertyStr(ctx, wasm, c"Instance".as_ptr()) };
      unsafe { JS_SetPropertyStr(ctx, meta, c"WasmInstance".as_ptr(), inst) };
    }
    unsafe { JS_FreeValue(ctx, wasm) };
    let src = c"new Map()";
    let map = unsafe {
      JS_Eval(
        ctx,
        src.as_ptr(),
        src.to_bytes().len(),
        c"<wasmmap>".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      )
    };
    if map.tag == JS_TAG_EXCEPTION {
      unsafe {
        let e = JS_GetException(ctx);
        JS_FreeValue(ctx, e);
      }
    } else {
      unsafe { JS_SetPropertyStr(ctx, meta, c"wasmInstances".as_ptr(), map) };
    }
    unsafe { JS_FreeValue(ctx, global) };
  }
  unsafe { JS_FreeValue(ctx, meta) };
}

fn lookup_resolved_specifier(
  base: &str,
  spec: &str,
) -> Option<std::string::String> {
  RESOLVED_SPECIFIERS.with(|t| {
    t.borrow()
      .get(&(base.to_string(), spec.to_string()))
      .cloned()
  })
}

fn record_resolved_specifier(base: &str, spec: &str, resolved: &str) {
  RESOLVED_SPECIFIERS.with(|t| {
    t.borrow_mut()
      .insert((base.to_string(), spec.to_string()), resolved.to_string());
  });
}

fn record_resolved_module_target(
  resolved: &str,
  attributes: &[ImportAttribute],
  module: *const Module,
) {
  if resolved.is_empty() || module.is_null() {
    return;
  }
  let key = static_import_attribute_key(attributes);
  if !key.is_empty() {
    if let Some(module_def) =
      with_module_state(module, |state| state.module_def)
    {
      if !module_def.is_null() {
        ATTRIBUTED_MODULE_DEFS.with(|defs| {
          defs.borrow_mut().insert(module_def as usize);
        });
      }
    }
  }
  RESOLVED_MODULE_TARGETS.with(|targets| {
    targets
      .borrow_mut()
      .insert((resolved.to_string(), key), handle_key(module));
  });
}

fn lookup_resolved_module_target(
  resolved: &str,
  attributes: &str,
) -> Option<usize> {
  RESOLVED_MODULE_TARGETS.with(|targets| {
    targets
      .borrow()
      .get(&(resolved.to_string(), attributes.to_string()))
      .copied()
  })
}

fn lookup_resolved_specifier_any(spec: &str) -> Option<std::string::String> {
  RESOLVED_SPECIFIERS.with(|t| {
    t.borrow()
      .iter()
      .find(|((_, s), _)| s == spec)
      .map(|(_, resolved)| resolved.clone())
  })
}

fn mark_all_modules_evaluated() {
  // Evaluating a root pulls its whole dependency graph, so its deps flip to
  // Evaluated together — but ONLY those. Query quickjs per-def instead of
  // sweeping every registered module: a compiled-but-never-evaluated module
  // (e.g. an extension entry point awaiting its own mod_evaluate) must stay
  // Instantiated, or deno_core's "already evaluated" early-return skips it.
  let evaluated_names: std::collections::HashSet<std::string::String> =
    MODULE_DEF_CACHE.with(|c| {
      c.borrow()
        .iter()
        .filter(|(_, d)| unsafe {
          v82jsc_module_is_evaluated(**d as *mut JSModuleDef) != 0
        })
        .map(|(n, _)| n.clone())
        .collect()
    });
  MODULE_STATE.with(|t| {
    for m in t.borrow_mut().values_mut() {
      if m.status == ModuleStatus::Errored {
        continue;
      }
      let def_evaluated = !m.module_def.is_null()
        && unsafe { v82jsc_module_is_evaluated(m.module_def) != 0 };
      if def_evaluated || evaluated_names.contains(&m.module_name) {
        m.status = ModuleStatus::Evaluated;
      }
    }
  });
  AFTER_FIRST_EVAL.with(|f| f.set(true));
}

struct ModuleEvalGuard {
  nested: bool,
}

impl ModuleEvalGuard {
  fn should_drain_jobs(&self) -> bool {
    !self.nested
  }
}

impl Drop for ModuleEvalGuard {
  fn drop(&mut self) {
    MODULE_EVAL_DEPTH.with(|depth| {
      depth.set(depth.get().saturating_sub(1));
    });
  }
}

fn enter_module_eval() -> ModuleEvalGuard {
  MODULE_EVAL_DEPTH.with(|depth| {
    let current = depth.get();
    depth.set(current.saturating_add(1));
    ModuleEvalGuard {
      nested: current != 0,
    }
  })
}

#[allow(dead_code)]
fn after_first_eval() -> bool {
  AFTER_FIRST_EVAL.with(|f| f.get())
}

/// Public key for a module wrapper: the underlying JS object pointer.
pub(crate) fn module_obj_key(this: *const Module) -> usize {
  handle_key(this)
}

pub(crate) fn module_name_for_value(v: JSValue) -> Option<std::string::String> {
  if v.tag >= 0 {
    return None;
  }
  let key = unsafe { v.u.ptr as usize };
  MODULE_STATE.with(|t| t.borrow().get(&key).map(|m| m.module_name.clone()))
}

pub(crate) struct ModuleSnapshotInfo {
  pub name: std::string::String,
  pub source: Option<std::string::String>,
  pub evaluated: bool,
  pub synthetic: bool,
  pub synthetic_exports: Vec<(std::string::String, JSValue)>,
}

pub(crate) fn module_snapshot_info_for_value(
  v: JSValue,
) -> Option<ModuleSnapshotInfo> {
  if v.tag >= 0 {
    return None;
  }
  let key = unsafe { v.u.ptr as usize };
  MODULE_STATE.with(|t| {
    t.borrow().get(&key).map(|m| {
      let source = if m.source_text.is_empty() {
        lookup_module_source_by_name(&m.module_name)
      } else {
        Some(m.source_text.clone())
      };
      let evaluated = matches!(m.status, ModuleStatus::Evaluated)
        || (!m.module_def.is_null()
          && unsafe { v82jsc_module_is_evaluated(m.module_def) != 0 });
      let synthetic_exports = if m.synthetic {
        let def = SYNTHETIC_DEFS
          .with(|defs| defs.borrow().get(&key).copied())
          .unwrap_or(m.module_def as usize);
        let values = SYNTHETIC_NS_EXPORTS
          .with(|exports| exports.borrow().get(&def).cloned())
          .or_else(|| {
            SYNTHETIC_EXPORTS
              .with(|exports| exports.borrow().get(&def).cloned())
          })
          .unwrap_or_default();
        let mut names = SYNTHETIC_EXPORT_NAMES
          .with(|names| {
            names
              .borrow()
              .get(&def)
              .map(|names| names.iter().cloned().collect::<Vec<_>>())
          })
          .unwrap_or_else(|| {
            values.iter().map(|(name, _)| name.clone()).collect()
          });
        names.sort_unstable();
        names.dedup();
        names
          .into_iter()
          .map(|name| {
            let value = values
              .iter()
              .rev()
              .find(|(candidate, _)| candidate == &name)
              .map(|(_, value)| *value)
              .unwrap_or_else(jsv_undefined);
            (name, value)
          })
          .collect()
      } else {
        Vec::new()
      };
      ModuleSnapshotInfo {
        name: m.module_name.clone(),
        source,
        evaluated,
        synthetic: m.synthetic,
        synthetic_exports,
      }
    })
  })
}

pub(crate) fn snapshot_module_values(ctx: *mut JSContext) -> Vec<JSValue> {
  let mut modules = MODULE_WRAPPER_BY_NAME.with(|modules| {
    modules
      .borrow()
      .iter()
      .filter(|(_, value)| {
        let key = if value.tag < 0 {
          unsafe { value.u.ptr as usize }
        } else {
          return false;
        };
        MODULE_STATE.with(|states| {
          states
            .borrow()
            .get(&key)
            .is_some_and(|module| module.context == ctx)
        })
      })
      .map(|(name, value)| (name.clone(), *value))
      .collect::<Vec<_>>()
  });
  modules.sort_unstable_by(|a, b| a.0.cmp(&b.0));
  modules.into_iter().map(|(_, value)| value).collect()
}

pub(crate) fn snapshot_module_namespace(
  ctx: *mut JSContext,
  value: JSValue,
) -> Option<JSValue> {
  let key = if value.tag < 0 {
    unsafe { value.u.ptr as usize }
  } else {
    return None;
  };
  let (status, engine_synthetic, mut def, name) =
    MODULE_STATE.with(|modules| {
      modules.borrow().get(&key).map(|module| {
        (
          clone_status(&module.status),
          module.engine_synthetic,
          module.module_def,
          module.module_name.clone(),
        )
      })
    })?;
  if def.is_null()
    && let Ok(name) = CString::new(name.as_str())
  {
    def = unsafe { v82jsc_get_loaded_module(ctx, name.as_ptr()) };
  }
  if def.is_null() {
    def = MODULE_DEF_CACHE
      .with(|cache| cache.borrow().get(&name).copied())
      .unwrap_or(0) as *mut JSModuleDef;
  }
  let evaluated = matches!(status, ModuleStatus::Evaluated)
    || (!def.is_null() && unsafe { v82jsc_module_is_evaluated(def) != 0 });
  if !evaluated || def.is_null() {
    return None;
  }
  if !engine_synthetic {
    let namespace = unsafe { JS_GetModuleNamespace(ctx, def) };
    if namespace.tag == JS_TAG_EXCEPTION {
      let exc = unsafe { JS_GetException(ctx) };
      unsafe { JS_FreeValue(ctx, exc) };
      return None;
    }
    return Some(namespace);
  }

  let info = module_snapshot_info_for_value(value)?;
  let namespace = unsafe { new_synthetic_namespace(ctx) };
  if namespace.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return None;
  }
  for (name, value) in info.synthetic_exports {
    let Ok(name) = CString::new(name) else {
      unsafe { JS_FreeValue(ctx, namespace) };
      return None;
    };
    if !unsafe {
      add_synthetic_namespace_export(
        ctx,
        namespace,
        &name,
        JS_DupValue(ctx, value),
      )
    } {
      let exc = unsafe { JS_GetException(ctx) };
      unsafe {
        JS_FreeValue(ctx, exc);
        JS_FreeValue(ctx, namespace);
      }
      return None;
    }
  }
  unsafe { finish_synthetic_namespace(ctx, namespace) }
}

#[inline]
fn handle_key(this: *const Module) -> usize {
  let v = jsval_of(this);
  unsafe { v.u.ptr as usize }
}

fn with_module_state<R>(
  this: *const Module,
  f: impl FnOnce(&mut ModuleState) -> R,
) -> Option<R> {
  let key = handle_key(this);
  MODULE_STATE.with(|t| t.borrow_mut().get_mut(&key).map(f))
}

fn record_module_state(this: *const Module, st: ModuleState) {
  let key = handle_key(this);
  MODULE_STATE.with(|t| {
    t.borrow_mut().insert(key, st);
  });
}

unsafe fn jsval_to_rust(
  ctx: *mut JSContext,
  v: JSValue,
) -> std::string::String {
  if ctx.is_null() {
    return std::string::String::new();
  }
  let mut len: usize = 0;
  let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, v) };
  if cstr.is_null() {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return std::string::String::new();
  }
  let bytes = unsafe { std::slice::from_raw_parts(cstr as *const u8, len) };
  let s = std::string::String::from_utf8_lossy(bytes).into_owned();
  unsafe { JS_FreeCString(ctx, cstr) };
  s
}

fn parse_import_specifiers(
  src: &str,
) -> Vec<(std::string::String, Option<std::string::String>)> {
  let mut out = Vec::new();
  let bytes = src.as_bytes();
  let mut i = 0usize;
  let n = bytes.len();
  while i < n {
    while i < n && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    if i >= n {
      break;
    }

    if i + 1 < n && bytes[i] == b'/' && bytes[i + 1] == b'/' {
      while i < n && bytes[i] != b'\n' {
        i += 1;
      }
      continue;
    }

    if i + 1 < n && bytes[i] == b'/' && bytes[i + 1] == b'*' {
      i += 2;
      while i + 1 < n && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
        i += 1;
      }
      i += 2;
      continue;
    }

    if bytes[i] == b'"' || bytes[i] == b'\'' {
      i = skip_string(bytes, i, bytes[i]);
      continue;
    }
    if bytes[i] == b'`' {
      i = skip_template(bytes, i);
      continue;
    }

    let at_boundary = i == 0
      || matches!(
        bytes[i - 1],
        b' ' | b'\t' | b'\r' | b'\n' | b';' | b'{' | b'}'
      );
    if !at_boundary {
      i += 1;
      continue;
    }

    let rest = &src[i..];
    let is_import = rest.starts_with("import")
      && rest[6..]
        .chars()
        .next()
        .map(|c| {
          c == ' ' || c == '{' || c == '*' || c == '"' || c == '\'' || c == '('
        })
        .unwrap_or(false);

    let after_export = rest.get(6..).map(|s| s.trim_start()).unwrap_or("");
    let is_export = rest.starts_with("export")
      && (after_export.starts_with('*') || after_export.starts_with('{'));
    if is_import || is_export {
      let dynamic = rest.starts_with("import(") || rest.starts_with("import (");

      let mut j = i;
      let mut depth = 0i32;
      let mut end = n;
      while j < n {
        match bytes[j] {
          b'"' | b'\'' => {
            j = skip_string(bytes, j, bytes[j]);
            continue;
          }
          b'`' => {
            j = skip_template(bytes, j);
            continue;
          }
          b'{' => depth += 1,
          b'}' => depth -= 1,
          b';' if depth <= 0 => {
            end = j;
            break;
          }
          b'\n' if depth <= 0 && j > i => {
            let seg = &src[i..j];
            if seg.contains(" from ") || seg.contains("\"") || seg.contains("'")
            {
              if has_balanced_quotes(seg) {
                end = j;
                break;
              }
            }
          }
          _ => {}
        }
        j += 1;
      }
      let stmt = &src[i..end.min(n)];
      if !dynamic {
        let has_from = find_from_specifier_start(stmt).is_some();

        let bare_immediate = stmt
          .strip_prefix("import")
          .map(|r| {
            let t = r.trim_start();
            t.starts_with('"') || t.starts_with('\'')
          })
          .unwrap_or(false);
        let bare = is_import && !has_from && bare_immediate;

        let bare_ok = bare && bare_import_well_formed(stmt);
        if has_from || bare_ok {
          if let Some(spec) = extract_specifier(stmt) {
            if !spec.is_empty() {
              let ty = extract_attr_type(stmt);
              if std::env::var_os("QJS_DEBUG_PARSE").is_some() {
                let snip: std::string::String = stmt.chars().take(80).collect();
                eprintln!(
                  "[QJS parse] spec={spec:?} type={ty:?} stmt={snip:?}"
                );
              }
              out.push((spec, ty));
            }
          }
        }
      }
      i = end.min(n) + 1;
      continue;
    }

    i += 1;
  }
  out
}

/// Byte offset of each specifier literal's opening quote in `text`, parallel to
/// `specifiers`. This is V8's `ModuleRequest::source_offset` /
/// deno's `referrer_source_offset`. A cursor advances past each match so a later
/// identical specifier resolves to its own occurrence.
/// Extract the last `//# sourceMappingURL=<url>` (or legacy `//@`) magic comment
/// from module source, mirroring V8's UnboundModuleScript::GetSourceMappingURL.
/// V8 honours the LAST occurrence; the URL is the trimmed remainder of the line.
/// Trailing content/blank lines after the directive are ignored (deno#21988).
fn extract_source_mapping_url(text: &str) -> Option<std::string::String> {
  let mut found: Option<std::string::String> = None;
  for line in text.lines() {
    let t = line.trim();
    for prefix in ["//# sourceMappingURL=", "//@ sourceMappingURL="] {
      if let Some(rest) = t.strip_prefix(prefix) {
        let url = rest.trim();
        if !url.is_empty() {
          found = Some(url.to_string());
        }
      }
    }
  }
  found
}

fn compute_import_offsets(
  text: &str,
  specifiers: &[std::string::String],
) -> Vec<i32> {
  let mut cursor = 0usize;
  let mut out = Vec::with_capacity(specifiers.len());
  for spec in specifiers {
    let mut found = -1i32;
    for quote in ['"', '\''] {
      let needle = format!("{quote}{spec}{quote}");
      if let Some(rel) = text[cursor..].find(&needle) {
        let pos = cursor + rel;
        if found < 0 || (pos as i32) < found {
          found = pos as i32;
        }
      }
    }
    if found >= 0 {
      cursor = (found as usize + 1).min(text.len());
      out.push(found);
    } else {
      out.push(0);
    }
  }
  out
}

/// Full per-import `with { ... }` / `assert { ... }` attribute key/value pairs,
/// parallel to `specifiers`. QuickJS's loader doesn't expose attributes through
/// its C-ABI, so scan the clause that immediately follows each specifier literal
/// in the source. Best-effort, sufficient for deno's flat `key: "value"` form.
fn compute_import_attributes(
  text: &str,
  specifiers: &[std::string::String],
) -> Vec<Vec<ImportAttribute>> {
  let offsets = compute_import_offsets(text, specifiers);
  let mut out = Vec::with_capacity(specifiers.len());
  for (i, spec) in specifiers.iter().enumerate() {
    let mut attrs = Vec::new();
    let off = offsets[i];
    if off >= 0 {
      // Position just past the specifier's closing quote.
      let after = (off as usize + spec.len() + 2).min(text.len());
      let tail_full = &text[after..];
      let leading_ws = tail_full.len() - tail_full.trim_start().len();
      let tail_start = after + leading_ws;
      let tail = &tail_full[leading_ws..];
      let kw = ["with", "assert"].iter().find_map(|k| {
        tail
          .strip_prefix(*k)
          .filter(|rest| {
            rest.starts_with(|c: char| c.is_whitespace() || c == '{')
          })
          .map(|rest| (k.len(), rest))
      });
      if let Some((kw_len, rest)) = kw {
        if let Some(open) = rest.find('{') {
          if let Some(close_rel) = rest[open + 1..].find('}') {
            let body = &rest[open + 1..open + 1 + close_rel];
            let body_start = tail_start + kw_len + open + 1;
            let mut body_cursor = 0usize;
            for pair in body.split(',') {
              if let Some((k, v)) = pair.split_once(':') {
                let key =
                  k.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
                let val =
                  v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
                if !key.is_empty() {
                  let key_leading = k.len() - k.trim_start().len();
                  let quote_offset =
                    usize::from(k.trim_start().starts_with(['"', '\'']));
                  let key_offset =
                    body_start + body_cursor + key_leading + quote_offset;
                  attrs.push((key, val, key_offset as i32));
                }
              }
              body_cursor += pair.len() + 1;
            }
          }
        }
      }
    }
    out.push(attrs);
  }
  out
}

thread_local! {
    static SRC_PHASE_COUNTER: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}
fn next_src_phase_id() -> u64 {
  SRC_PHASE_COUNTER.with(|c| {
    let v = c.get();
    c.set(v + 1);
    v
  })
}

fn rewrite_source_phase(
  src: &str,
) -> (std::string::String, Vec<(u64, std::string::String)>) {
  if !src.contains("import source") {
    return (src.to_string(), Vec::new());
  }
  let bytes = src.as_bytes();
  let n = bytes.len();
  let mut out = std::string::String::with_capacity(src.len());
  let mut records = Vec::new();
  let mut i = 0usize;
  let mut last = 0usize;
  while i < n {
    let boundary = i == 0
      || matches!(
        bytes[i - 1],
        b'\n' | b'\r' | b';' | b'{' | b'}' | b' ' | b'\t'
      );
    if boundary && src[i..].starts_with("import") {
      if let Some((consumed, binding, spec)) = parse_source_phase_at(&src[i..])
      {
        out.push_str(&src[last..i]);
        let id = next_src_phase_id();
        out.push_str(&format!(
          "const {binding}=globalThis.__v82jsc_wasm_src.get({id});"
        ));
        records.push((id, spec));
        i += consumed;
        last = i;
        continue;
      }
    }
    i += 1;
  }
  out.push_str(&src[last..]);
  (out, records)
}

fn rewrite_deferred_imports(
  src: &str,
) -> (std::string::String, Vec<DeferredImport>) {
  if !src.contains("import defer") {
    return (src.to_string(), Vec::new());
  }
  let bytes = src.as_bytes();
  let mut out = std::string::String::with_capacity(src.len());
  let mut records = Vec::new();
  let mut i = 0usize;
  let mut last = 0usize;
  while i < bytes.len() {
    let boundary = i == 0
      || matches!(
        bytes[i - 1],
        b'\n' | b'\r' | b';' | b'{' | b'}' | b' ' | b'\t'
      );
    if boundary
      && src[i..].starts_with("import")
      && let Some((consumed, binding, specifier, specifier_offset)) =
        parse_deferred_import_at(&src[i..])
    {
      out.push_str(&src[last..i]);
      let id = next_src_phase_id();
      out.push_str(&format!(
        "const {binding}=globalThis.__v82jsc_defer_ns.get({id});"
      ));
      let statement = &src[i..i + consumed];
      let mut attributes =
        compute_import_attributes(statement, std::slice::from_ref(&specifier))
          .pop()
          .unwrap_or_default();
      for (_, _, offset) in &mut attributes {
        *offset += i as i32;
      }
      records.push(DeferredImport {
        id,
        specifier,
        source_offset: (i + specifier_offset) as i32,
        attributes,
      });
      i += consumed;
      last = i;
      continue;
    }
    i += 1;
  }
  out.push_str(&src[last..]);
  (out, records)
}

fn parse_deferred_import_at(
  s: &str,
) -> Option<(usize, std::string::String, std::string::String, usize)> {
  let bytes = s.as_bytes();
  let len = bytes.len();
  let skip_ws = |mut p: usize| {
    while p < len && bytes[p].is_ascii_whitespace() {
      p += 1;
    }
    p
  };
  let consume_keyword = |p: usize, keyword: &str| {
    s.get(p..)?.strip_prefix(keyword).and_then(|rest| {
      let end = p + keyword.len();
      let boundary = rest
        .as_bytes()
        .first()
        .is_none_or(|c| !c.is_ascii_alphanumeric() && *c != b'_' && *c != b'$');
      boundary.then_some(end)
    })
  };

  let mut p = consume_keyword(0, "import")?;
  let next = skip_ws(p);
  if next == p {
    return None;
  }
  p = consume_keyword(next, "defer")?;
  let next = skip_ws(p);
  if next == p || bytes.get(next) != Some(&b'*') {
    return None;
  }
  p = skip_ws(next + 1);
  p = consume_keyword(p, "as")?;
  let next = skip_ws(p);
  if next == p {
    return None;
  }
  p = next;
  let binding_start = p;
  while p < len
    && (bytes[p].is_ascii_alphanumeric()
      || bytes[p] == b'_'
      || bytes[p] == b'$')
  {
    p += 1;
  }
  if p == binding_start {
    return None;
  }
  let binding = s[binding_start..p].to_string();
  p = skip_ws(p);
  p = consume_keyword(p, "from")?;
  let next = skip_ws(p);
  if next == p {
    return None;
  }
  p = next;
  let quote = *bytes.get(p)?;
  if quote != b'"' && quote != b'\'' {
    return None;
  }
  let specifier_offset = p;
  p += 1;
  let specifier_start = p;
  while p < len && bytes[p] != quote {
    if bytes[p] == b'\\' {
      p += 1;
    }
    p += 1;
  }
  if p >= len {
    return None;
  }
  let specifier = s[specifier_start..p].to_string();
  p += 1;

  let after_specifier = p;
  let maybe_attributes = skip_ws(p);
  if let Some(after_keyword) = consume_keyword(maybe_attributes, "with") {
    p = skip_ws(after_keyword);
    if bytes.get(p) != Some(&b'{') {
      return None;
    }
    let mut depth = 1usize;
    p += 1;
    while p < len && depth != 0 {
      match bytes[p] {
        b'"' | b'\'' => p = skip_string(bytes, p, bytes[p]),
        b'{' => {
          depth += 1;
          p += 1;
        }
        b'}' => {
          depth -= 1;
          p += 1;
        }
        _ => p += 1,
      }
    }
    if depth != 0 {
      return None;
    }
  } else {
    p = after_specifier;
  }
  while p < len && matches!(bytes[p], b' ' | b'\t' | b'\r') {
    p += 1;
  }
  if bytes.get(p) == Some(&b';') {
    p += 1;
  }
  Some((p, binding, specifier, specifier_offset))
}

fn parse_source_phase_at(
  s: &str,
) -> Option<(usize, std::string::String, std::string::String)> {
  let b = s.as_bytes();
  let n = b.len();
  let skip_ws = |p: usize| {
    let mut p = p;
    while p < n && b[p].is_ascii_whitespace() {
      p += 1;
    }
    p
  };
  if !s.starts_with("import") {
    return None;
  }
  let mut p = 6;
  let p1 = skip_ws(p);
  if p1 == p {
    return None;
  }
  p = p1;
  if !s[p..].starts_with("source") {
    return None;
  }
  p += 6;
  let p2 = skip_ws(p);
  if p2 == p {
    return None;
  }
  p = p2;
  let bstart = p;
  while p < n && (b[p].is_ascii_alphanumeric() || b[p] == b'_' || b[p] == b'$')
  {
    p += 1;
  }
  if p == bstart {
    return None;
  }
  let binding = s[bstart..p].to_string();
  p = skip_ws(p);
  if !s[p..].starts_with("from") {
    return None;
  }
  p += 4;
  p = skip_ws(p);
  if p >= n {
    return None;
  }
  let q = b[p];
  if q != b'"' && q != b'\'' {
    return None;
  }
  p += 1;
  let sstart = p;
  while p < n && b[p] != q {
    p += 1;
  }
  if p >= n {
    return None;
  }
  let spec = s[sstart..p].to_string();
  p += 1;
  if p < n && b[p] == b';' {
    p += 1;
  }
  Some((p, binding, spec))
}

fn skip_string(bytes: &[u8], i: usize, quote: u8) -> usize {
  let n = bytes.len();
  let mut j = i + 1;
  while j < n {
    match bytes[j] {
      b'\\' => j += 2,
      c if c == quote => return j + 1,
      b'\n' => return j,
      _ => j += 1,
    }
  }
  n
}

fn skip_template(bytes: &[u8], i: usize) -> usize {
  let n = bytes.len();
  let mut j = i + 1;
  while j < n {
    match bytes[j] {
      b'\\' => j += 2,
      b'`' => return j + 1,
      b'$' if j + 1 < n && bytes[j + 1] == b'{' => {
        let mut depth = 1i32;
        j += 2;
        while j < n && depth > 0 {
          match bytes[j] {
            b'{' => {
              depth += 1;
              j += 1;
            }
            b'}' => {
              depth -= 1;
              j += 1;
            }
            b'"' | b'\'' => j = skip_string(bytes, j, bytes[j]),
            b'`' => j = skip_template(bytes, j),
            b'\\' => j += 2,
            _ => j += 1,
          }
        }
      }
      _ => j += 1,
    }
  }
  n
}

fn bare_import_well_formed(stmt: &str) -> bool {
  let b = stmt.as_bytes();

  let after_kw = match stmt.strip_prefix("import") {
    Some(r) => r,
    None => return false,
  };
  let lead_ws =
    r#"import"#.len() + (after_kw.len() - after_kw.trim_start().len());
  let q = match b.get(lead_ws) {
    Some(&c) if c == b'"' || c == b'\'' => c,
    _ => return false,
  };

  let mut j = lead_ws + 1;
  while j < b.len() {
    match b[j] {
      b'\\' => j += 2,
      c if c == q => break,
      b'\n' => return false,
      _ => j += 1,
    }
  }
  if j >= b.len() {
    return false;
  }

  let mut k = j + 1;
  while k < b.len() && (b[k] == b' ' || b[k] == b'\t' || b[k] == b'\r') {
    k += 1;
  }
  match b.get(k) {
    None => true,
    Some(&c) if c == b';' || c == b'\n' => true,
    _ => stmt[k..].starts_with("with") || stmt[k..].starts_with("assert"),
  }
}

fn has_balanced_quotes(s: &str) -> bool {
  let dq = s.matches('"').count();
  let sq = s.matches('\'').count();
  dq % 2 == 0 && sq % 2 == 0 && (dq + sq) >= 2
}

fn extract_specifier(line: &str) -> Option<std::string::String> {
  let scan = match find_from_specifier_start(line) {
    Some(p) => &line[p..],
    None => line,
  };
  let bytes = scan.as_bytes();
  let mut open = None;
  let mut q = b'"';
  for (i, &b) in bytes.iter().enumerate() {
    if b == b'"' || b == b'\'' {
      open = Some(i);
      q = b;
      break;
    }
  }
  let open = open?;
  for i in (open + 1)..bytes.len() {
    if bytes[i] == q {
      return Some(scan[open + 1..i].to_string());
    }
  }
  None
}

fn find_from_specifier_start(stmt: &str) -> Option<usize> {
  let b = stmt.as_bytes();
  let mut result = None;
  let mut idx = 0;
  while let Some(p) = stmt[idx..].find("from") {
    let at = idx + p;
    idx = at + 4;
    let prev_ok = at == 0 || {
      let c = b[at - 1];
      !(c.is_ascii_alphanumeric() || c == b'_' || c == b'$')
    };
    let post_ident = b
      .get(at + 4)
      .map(|&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$')
      .unwrap_or(false);
    let after = stmt[at + 4..].trim_start();
    let next_quote = after.starts_with('"') || after.starts_with('\'');
    if prev_ok && !post_ident && next_quote {
      result = Some(at + 4);
    }
  }
  result
}

fn extract_attr_type(stmt: &str) -> Option<std::string::String> {
  let kw = stmt
    .find(" with ")
    .or_else(|| stmt.find(" with{"))
    .or_else(|| stmt.find(" assert "))
    .or_else(|| stmt.find(" assert{"))?;
  let clause = &stmt[kw..];

  let tpos = clause.find("type")?;
  let after = &clause[tpos + 4..];
  let bytes = after.as_bytes();
  let mut open = None;
  let mut q = b'"';
  for (i, &b) in bytes.iter().enumerate() {
    if b == b'"' || b == b'\'' {
      open = Some(i);
      q = b;
      break;
    }
  }
  let open = open?;
  for i in (open + 1)..bytes.len() {
    if bytes[i] == q {
      return Some(after[open + 1..i].to_string());
    }
  }
  None
}

fn has_top_level_await(src: &str) -> bool {
  let mut depth = 0usize;
  for mut line in src.lines() {
    if let Some((before, _)) = line.split_once("//") {
      line = before;
    }
    let t = line.trim_start();
    if depth == 0 && (t.starts_with("await ") || t.starts_with("for await")) {
      return true;
    }
    for ch in line.chars() {
      match ch {
        '{' | '(' | '[' => depth = depth.saturating_add(1),
        '}' | ')' | ']' => depth = depth.saturating_sub(1),
        _ => {}
      }
    }
  }
  false
}

thread_local! {

    static JOBS_DRAINING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

unsafe fn drain_jobs(rt: *mut JSRuntime) {
  if rt.is_null() {
    return;
  }

  if JOBS_DRAINING.with(|d| d.replace(true)) {
    if std::env::var("V82_WASM_TRACE").is_ok() {
      eprintln!("[jobs] re-entrant drain_jobs BLOCKED");
    }
    return;
  }
  loop {
    let mut pctx: *mut JSContext = ptr::null_mut();
    let r = unsafe { JS_ExecutePendingJob(rt, &mut pctx) };
    if r <= 0 {
      break;
    }
  }
  JOBS_DRAINING.with(|d| d.set(false));
}

pub(crate) fn register_module_source(name: &str, source: &str) {
  if name.is_empty() {
    return;
  }
  super::core::register_script_source(name, source);
  MODULE_SOURCES_BY_NAME.with(|t| {
    t.borrow_mut().insert(name.to_string(), source.to_string());
  });
}

fn lookup_module_source_by_name(name: &str) -> Option<std::string::String> {
  MODULE_SOURCES_BY_NAME.with(|t| t.borrow().get(name).cloned())
}

fn module_name_for_source(
  source_name: &str,
  source: &str,
  script_id: int,
) -> std::string::String {
  if source_name.is_empty() {
    return "<module>".to_string();
  }
  match lookup_module_source_by_name(source_name) {
    Some(existing) if existing != source => {
      format!("{source_name}#v8x:{script_id}")
    }
    _ => source_name.to_string(),
  }
}

fn source_name_for_module_name(name: &str) -> std::string::String {
  MODULE_STATE
    .with(|t| {
      t.borrow()
        .values()
        .find(|m| m.module_name == name)
        .map(|m| m.source_name.clone())
    })
    .unwrap_or_else(|| name.to_string())
}

/// Snapshot replay: fetch a registered module source by exact name.
pub(crate) fn lookup_module_source(name: &str) -> Option<std::string::String> {
  lookup_module_source_by_name(name)
}

/// Intern a module WRAPPER handle into an immortal slot. Wrapper pointers ARE
/// module identity (every side table + the embedder's Globals key on them),
/// so they must never be recycled by handle-scope pops the way arena slots
/// are. Owns the +1 JSValue; reclaimed only at registry teardown.
fn intern_module(v: JSValue) -> *const Module {
  Box::into_raw(Box::new(v)) as *const Module
}

/// Tape replay: after the deferred ModuleEval entries ran, resolve each
/// tape wrapper's def by name and lift its status to match the engine.
/// Register a module def under a name in the def cache (tape replay: defs
/// born from bytecode reads are invisible to the engine's loaded-module
/// registry).
pub(crate) fn cache_module_def(name: &str, def: usize) {
  MODULE_DEF_CACHE.with(|c| {
    c.borrow_mut().insert(name.to_string(), def);
  });
}

pub(crate) fn refresh_tape_module_state(
  ctx: *mut JSContext,
  wrapper: *const Module,
) {
  let Some(name) =
    with_module_state(wrapper as *const Module, |m| m.module_name.clone())
  else {
    return;
  };
  let mut def = MODULE_DEF_CACHE
    .with(|c| c.borrow().get(&name).copied())
    .unwrap_or(0) as *mut JSModuleDef;
  if def.is_null() && !ctx.is_null() {
    // ModuleEval replays through raw JS_Eval, which registers the def in the
    // ENGINE registry, not our cache — look it up there.
    if let Ok(cn) = CString::new(name.clone()) {
      def = unsafe { v82jsc_get_loaded_module(ctx, cn.as_ptr()) }
        as *mut JSModuleDef;
    }
  }
  if std::env::var_os("QJS_DEBUG_TAPE").is_some() {
    eprintln!(
      "[qjs tape] refresh module {name}: def={def:?} src_known={}",
      lookup_module_source_by_name(&name).is_some()
    );
  }
  if !def.is_null() {
    let evaluated = unsafe { v82jsc_module_is_evaluated(def) != 0 }
      || unsafe { v82jsc_module_eval_started(def) != 0 };
    with_module_state(wrapper as *const Module, |m| {
      m.module_def = def;
      if evaluated {
        m.status = ModuleStatus::Evaluated;
      }
    });
    return;
  }
  // No def and no source: a synthetic module (e.g. deno's virtual ops
  // module). It WAS evaluated on the creator — reflect that; the restoring
  // embedder rebuilt its exports natively (ops re-binding), and nothing
  // imports the def through the engine after restore.
  if lookup_module_source_by_name(&name).is_none() {
    with_module_state(wrapper as *const Module, |m| {
      m.status = ModuleStatus::Evaluated;
    });
  }
}

pub(crate) fn mark_tape_module_evaluated(wrapper: *const Module) {
  with_module_state(wrapper, |m| {
    m.status = ModuleStatus::Evaluated;
  });
}

pub(crate) fn mark_tape_module_synthetic(
  wrapper: *const Module,
  synthetic: bool,
) {
  with_module_state(wrapper, |m| {
    m.synthetic = synthetic;
  });
}

/// True when `p` is a registered module WRAPPER handle. Module identity in
/// every side table is the wrapper pointer itself, so handle copies
/// (Global/Local) must preserve it — see `core::is_non_value_handle`.
pub(crate) fn is_module_wrapper(p: *const std::os::raw::c_void) -> bool {
  // Module identity keys on the wrapped JS OBJECT pointer (handle_key), so
  // any handle copy of a wrapper matches.
  MODULE_STATE
    .with(|t| t.borrow().contains_key(&handle_key(p as *const Module)))
}

/// Tape replay: recreate a module WRAPPER handle for `name`. The wrapper is
/// deno_core's module identity (Global<Module> in its map); its side-table
/// state carries just enough for post-restore queries — status comes from
/// the def cache once the ModuleEval tape entry has run, the namespace path
/// falls back to the def cache by name.
pub(crate) fn tape_make_module_handle(
  ctx: *mut JSContext,
  name: &str,
) -> *const Module {
  let handle_val = unsafe { JS_NewObject(ctx) };
  if handle_val.tag == JS_TAG_EXCEPTION {
    let e = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, e) };
    return std::ptr::null();
  }
  let this = intern_module(handle_val);
  if this.is_null() {
    return this;
  }
  let source = lookup_module_source_by_name(name).unwrap_or_default();
  let script_id = module_script_id_for_name(name);
  let def = MODULE_DEF_CACHE
    .with(|c| c.borrow().get(name).copied())
    .unwrap_or(0) as *mut JSModuleDef;
  let evaluated =
    !def.is_null() && unsafe { v82jsc_module_is_evaluated(def) != 0 };
  record_module_state(
    this,
    ModuleState {
      context: ctx,
      status: if evaluated {
        ModuleStatus::Evaluated
      } else {
        ModuleStatus::Instantiated
      },
      module_def: def,
      bytecode: None,
      import_specifiers: parse_import_specifiers(&source),
      import_offsets: Vec::new(),
      import_attributes: Vec::new(),
      source_imports: Vec::new(),
      deferred_imports: Vec::new(),
      synthetic: false,
      engine_synthetic: false,
      is_async: has_top_level_await(&source),
      source_text: source.clone(),
      source_name: name.to_string(),
      module_name: name.to_string(),
      script_id,
      source_map_url: None,
    },
  );
  record_module_wrapper(name, this);
  this
}

pub(crate) fn restore_synthetic_module(
  ctx: *mut JSContext,
  name: &str,
  exports: Vec<(std::string::String, JSValue)>,
  evaluated: bool,
) -> *const Module {
  restore_module_from_snapshot_exports(ctx, name, exports, evaluated, true)
}

pub(crate) fn restore_module_from_snapshot_exports(
  ctx: *mut JSContext,
  name: &str,
  exports: Vec<(std::string::String, JSValue)>,
  evaluated: bool,
  synthetic: bool,
) -> *const Module {
  let handle_val = unsafe { JS_NewObject(ctx) };
  if handle_val.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    for (_, value) in exports {
      unsafe { JS_FreeValue(ctx, value) };
    }
    return ptr::null();
  }
  let this = intern_module(handle_val);
  if !restore_module_from_snapshot_exports_for_wrapper(
    ctx, this, name, exports, evaluated, synthetic,
  ) {
    return ptr::null();
  }
  this
}

pub(crate) fn restore_module_from_snapshot_exports_in_place(
  ctx: *mut JSContext,
  wrapper: JSValue,
  name: &str,
  exports: Vec<(std::string::String, JSValue)>,
  evaluated: bool,
  synthetic: bool,
) -> bool {
  let this = &wrapper as *const JSValue as *const Module;
  restore_module_from_snapshot_exports_for_wrapper(
    ctx, this, name, exports, evaluated, synthetic,
  )
}

fn restore_module_from_snapshot_exports_for_wrapper(
  ctx: *mut JSContext,
  this: *const Module,
  name: &str,
  exports: Vec<(std::string::String, JSValue)>,
  evaluated: bool,
  synthetic: bool,
) -> bool {
  let free_exports = |exports: Vec<(std::string::String, JSValue)>| {
    for (_, value) in exports {
      unsafe { JS_FreeValue(ctx, value) };
    }
  };
  let Ok(cname) = CString::new(name) else {
    free_exports(exports);
    return false;
  };
  let def = unsafe {
    JS_NewCModule(ctx, cname.as_ptr(), Some(synthetic_module_init_callback))
  };
  if def.is_null() {
    free_exports(exports);
    return false;
  }

  let mut export_names = std::collections::HashSet::new();
  for (export_name, _) in &exports {
    let Ok(export_name) = CString::new(export_name.as_str()) else {
      free_exports(exports);
      return false;
    };
    if unsafe { JS_AddModuleExport(ctx, def, export_name.as_ptr()) } < 0 {
      let exc = unsafe { JS_GetException(ctx) };
      unsafe { JS_FreeValue(ctx, exc) };
      free_exports(exports);
      return false;
    }
    export_names.insert(export_name.to_string_lossy().into_owned());
  }

  let key = handle_key(this);
  SYNTHETIC_DEFS.with(|defs| {
    defs.borrow_mut().insert(key, def as usize);
  });
  SYNTHETIC_EXPORT_NAMES.with(|names| {
    names.borrow_mut().insert(def as usize, export_names);
  });
  SYNTHETIC_EXPORTS.with(|stored| {
    stored.borrow_mut().insert(def as usize, exports);
  });
  MODULE_DEF_CACHE.with(|cache| {
    cache.borrow_mut().insert(name.to_string(), def as usize);
  });
  record_module_state(
    this,
    ModuleState {
      context: ctx,
      status: ModuleStatus::Uninstantiated,
      module_def: def,
      bytecode: None,
      import_specifiers: Vec::new(),
      import_offsets: Vec::new(),
      import_attributes: Vec::new(),
      source_imports: Vec::new(),
      deferred_imports: Vec::new(),
      synthetic,
      engine_synthetic: true,
      is_async: false,
      source_text: std::string::String::new(),
      source_name: name.to_string(),
      module_name: name.to_string(),
      script_id: assign_module_script_id(name),
      source_map_url: None,
    },
  );
  record_module_wrapper(name, this);

  if evaluated {
    let module = make_value(
      JS_TAG_MODULE,
      JSValueUnion {
        ptr: def as *mut std::os::raw::c_void,
      },
    );
    let module = unsafe { JS_DupValue(ctx, module) };
    let result = unsafe { JS_EvalFunction(ctx, module) };
    let isolate = current_iso();
    if !isolate.is_null() {
      unsafe { drain_jobs(iso_state(isolate).rt) };
    }
    if result.tag == JS_TAG_EXCEPTION {
      let exc = unsafe { JS_GetException(ctx) };
      unsafe { JS_FreeValue(ctx, exc) };
      with_module_state(this, |module| module.status = ModuleStatus::Errored);
      return false;
    }
    unsafe { JS_FreeValue(ctx, result) };
    with_module_state(this, |module| module.status = ModuleStatus::Evaluated);
  }

  true
}

/// Reset every module registry on this thread. Called from
/// `v8__Isolate__Dispose`: these thread-locals are keyed by module NAME or by
/// pointers into the disposed runtime, so a later isolate on the same thread
/// (e.g. a runtime restored from a snapshot the disposed isolate created)
/// would otherwise resolve its modules to dangling defs from the old runtime.
/// Values are dropped WITHOUT JS_FreeValue — their runtime is already gone.
pub(crate) fn clear_thread_module_caches() {
  super::snapshot::clear_thread_snapshot_caches();
  MODULE_STATE.with(|c| c.borrow_mut().clear());
  MODULE_SOURCES_BY_NAME.with(|c| c.borrow_mut().clear());
  MODULE_DEF_CACHE.with(|c| c.borrow_mut().clear());
  RESOLVED_MODULE_TARGETS.with(|c| c.borrow_mut().clear());
  ATTRIBUTED_MODULE_DEFS.with(|c| c.borrow_mut().clear());
  SYNTHETIC_EXPORTS.with(|c| c.borrow_mut().clear());
  SYNTHETIC_NS_EXPORTS.with(|c| c.borrow_mut().clear());
  SYNTHETIC_DEFS.with(|c| c.borrow_mut().clear());
  SYNTHETIC_EXPORT_NAMES.with(|c| c.borrow_mut().clear());
  SYNTHETIC_EVAL_STEPS.with(|c| c.borrow_mut().clear());
  AFTER_FIRST_EVAL.with(|c| c.set(false));
  RESOLVED_SPECIFIERS.with(|c| c.borrow_mut().clear());
  SCRIPT_SOURCE_MAP_URLS.with(|c| c.borrow_mut().clear());
  MODULE_WRAPPER_BY_NAME.with(|c| c.borrow_mut().clear());
  MAIN_MODULE_URL.with(|c| c.borrow_mut().take());
  MODULE_SCRIPT_IDS_BY_NAME.with(|c| c.borrow_mut().clear());
  NEXT_MODULE_SCRIPT_ID.with(|c| c.set(1));
}

pub(crate) unsafe extern "C" fn module_normalize_callback(
  ctx: *mut JSContext,
  module_base_name: *const std::os::raw::c_char,
  module_name: *const std::os::raw::c_char,
  _opaque: *mut std::os::raw::c_void,
) -> *mut std::os::raw::c_char {
  let base = unsafe { std::ffi::CStr::from_ptr(module_base_name) }
    .to_str()
    .unwrap_or("");
  let name = unsafe { std::ffi::CStr::from_ptr(module_name) }
    .to_str()
    .unwrap_or("");

  let mut canonical = lookup_resolved_specifier(base, name);

  if canonical.is_none() && (name.starts_with("./") || name.starts_with("../"))
  {
    canonical = Some(resolve_relative_specifier(base, name));
  }

  if canonical.is_none()
    && !name.contains(':')
    && !name.starts_with('.')
    && !name.starts_with('/')
  {
    let node_name = format!("node:{name}");

    let cname = CString::new(node_name.as_str()).ok();
    let loaded = cname
      .as_ref()
      .map(|c| unsafe { v82jsc_has_loaded_module(ctx, c.as_ptr()) != 0 })
      .unwrap_or(false);
    if loaded || lookup_module_source_by_name(&node_name).is_some() {
      canonical = Some(node_name);
    }
  }

  if canonical.is_none() && !name.starts_with('.') && !name.starts_with('/') {
    canonical = lookup_resolved_specifier_any(name);
  }
  if std::env::var_os("QJS_DEBUG_MOD").is_some() {
    eprintln!(
      "[QJS normalize] base={base} name={name} -> {}",
      canonical.as_deref().unwrap_or("(identity)")
    );
  }
  match canonical {
    Some(resolved) => match CString::new(resolved) {
      Ok(c) => unsafe { js_strdup(ctx, c.as_ptr()) },
      Err(_) => unsafe { js_strdup(ctx, module_name) },
    },
    None => unsafe { js_strdup(ctx, module_name) },
  }
}

fn resolve_relative_specifier(base: &str, name: &str) -> std::string::String {
  let (scheme, base_path) = match base.find("://") {
    Some(i) => base.split_at(i + 3),
    None => ("", base),
  };

  let dir = match base_path.rfind('/') {
    Some(i) => &base_path[..i],
    None => "",
  };
  let mut segments: Vec<&str> =
    dir.split('/').filter(|s| !s.is_empty()).collect();
  for seg in name.split('/') {
    match seg {
      "" | "." => {}
      ".." => {
        segments.pop();
      }
      s => segments.push(s),
    }
  }

  let joined = segments.join("/");
  if base_path.starts_with('/') {
    format!("{scheme}/{joined}")
  } else {
    format!("{scheme}{joined}")
  }
}

unsafe fn build_resolution_map(
  ctx: *mut JSContext,
  context: *const Context,
  cb: ResolveModuleCallback,
  source_cb: Option<ResolveSourceCallback>,
  root: *const Module,
) -> bool {
  use std::collections::HashSet;
  let mut visited: HashSet<usize> = HashSet::new();
  let mut stack: Vec<*const Module> = vec![root];
  let mut deferred_targets = Vec::new();
  while let Some(m) = stack.pop() {
    if !visited.insert(handle_key(m)) {
      continue;
    }
    let Some((base, specs, attrs, src_imports, deferred_imports)) =
      with_module_state(m, |st| {
        (
          st.module_name.clone(),
          st.import_specifiers.clone(),
          st.import_attributes.clone(),
          st.source_imports.clone(),
          st.deferred_imports.clone(),
        )
      })
    else {
      continue;
    };

    if let Some(scb) = source_cb {
      for (id, spec) in &src_imports {
        unsafe { resolve_source_import(ctx, context, scb, m, *id, spec) };
      }
    }
    for deferred in deferred_imports {
      let Some(resolved) = (unsafe {
        resolve_module_import(
          ctx,
          context,
          cb,
          m,
          &deferred.specifier,
          &deferred.attributes,
        )
      }) else {
        return false;
      };
      if let Some(rname) =
        with_module_state(resolved, |st| st.module_name.clone())
      {
        record_resolved_module_target(&rname, &deferred.attributes, resolved);
        if !rname.is_empty() && rname != deferred.specifier {
          record_resolved_specifier(&base, &deferred.specifier, &rname);
        }
        stack.push(resolved);
      }
      deferred_targets.push((deferred.id, resolved));
    }
    let mut resolved_requests = HashSet::new();
    for (si, (spec, _ty)) in specs.into_iter().enumerate() {
      let attr_pairs = attrs.get(si).map(Vec::as_slice).unwrap_or(&[]);
      if !resolved_requests
        .insert((spec.clone(), static_import_attribute_key(attr_pairs)))
      {
        continue;
      }
      let Some(resolved) = (unsafe {
        resolve_module_import(ctx, context, cb, m, &spec, attr_pairs)
      }) else {
        return false;
      };
      if let Some(rname) =
        with_module_state(resolved, |st| st.module_name.clone())
      {
        record_resolved_module_target(&rname, attr_pairs, resolved);
        if !rname.is_empty() && rname != spec {
          record_resolved_specifier(&base, &spec, &rname);
        }
        stack.push(resolved);
      }
    }
  }
  let mut linked = HashSet::new();
  for (id, module) in deferred_targets {
    if linked.insert(handle_key(module)) {
      let module_def = unsafe { compile_module_for_instantiation(ctx, module) };
      if module_def.is_null()
        || unsafe { v82jsc_link_module(ctx, module_def) } < 0
      {
        unsafe { annotate_module_link_exception(ctx, module) };
        with_module_state(module, |state| state.status = ModuleStatus::Errored);
        return false;
      }
      with_module_state(module, |state| {
        if state.status == ModuleStatus::Uninstantiated {
          state.status = ModuleStatus::Instantiated;
        }
      });
    }
    if !unsafe { store_deferred_namespace(ctx, id, module) } {
      return false;
    }
  }
  true
}

unsafe fn resolve_module_import(
  ctx: *mut JSContext,
  context: *const Context,
  cb: ResolveModuleCallback,
  referrer: *const Module,
  specifier: &str,
  attributes: &[ImportAttribute],
) -> Option<*const Module> {
  let specifier = CString::new(specifier).ok()?;
  let value = unsafe { JS_NewString(ctx, specifier.as_ptr()) };
  if value.tag == JS_TAG_EXCEPTION {
    return None;
  }
  let specifier = intern::<V8String>(value);
  let attributes =
    intern::<FixedArray>(unsafe { build_static_import_attrs(ctx, attributes) });
  let (Some(context), Some(specifier), Some(attributes), Some(referrer)) = (
    unsafe { crate::Local::from_raw(context) },
    unsafe { crate::Local::from_raw(specifier) },
    unsafe { crate::Local::from_raw(attributes) },
    unsafe { crate::Local::from_raw(referrer) },
  ) else {
    return None;
  };
  // Windows ABI: leading return-slot out-param (see run_synthetic_eval_steps).
  #[cfg(not(target_os = "windows"))]
  let resolved = unsafe { cb(context, specifier, attributes, referrer) };
  #[cfg(target_os = "windows")]
  let resolved = unsafe {
    let mut ret = ptr::null();
    cb(&mut ret, context, specifier, attributes, referrer);
    ret
  };
  if unsafe { JS_HasException(ctx) } {
    return None;
  }
  let resolved: *const Module = unsafe { std::mem::transmute(resolved) };
  (!resolved.is_null()).then_some(resolved)
}

unsafe fn store_deferred_namespace(
  ctx: *mut JSContext,
  id: u64,
  module: *const Module,
) -> bool {
  let namespace =
    v8__Module__GetModuleNamespace2(module, ModuleImportPhase::kDefer);
  if namespace.is_null() {
    return false;
  }
  let global = unsafe { JS_GetGlobalObject(ctx) };
  let mut map =
    unsafe { JS_GetPropertyStr(ctx, global, c"__v82jsc_defer_ns".as_ptr()) };
  if !jsv_is_object(&map) {
    unsafe { JS_FreeValue(ctx, map) };
    let source = c"(globalThis.__v82jsc_defer_ns=new Map())";
    map = unsafe {
      JS_Eval(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"<deferns>".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      )
    };
  }
  if map.tag == JS_TAG_EXCEPTION {
    unsafe { JS_FreeValue(ctx, global) };
    return false;
  }
  let set = unsafe { JS_GetPropertyStr(ctx, map, c"set".as_ptr()) };
  let mut args = [unsafe { JS_NewInt64(ctx, id as i64) }, unsafe {
    JS_DupValue(ctx, jsval_of(namespace))
  }];
  let result = unsafe { JS_Call(ctx, set, map, 2, args.as_mut_ptr()) };
  let ok = result.tag != JS_TAG_EXCEPTION;
  unsafe {
    JS_FreeValue(ctx, result);
    JS_FreeValue(ctx, args[0]);
    JS_FreeValue(ctx, args[1]);
    JS_FreeValue(ctx, set);
    JS_FreeValue(ctx, map);
    JS_FreeValue(ctx, global);
  }
  ok
}

unsafe fn resolve_source_import(
  ctx: *mut JSContext,
  context: *const Context,
  scb: ResolveSourceCallback,
  m: *const Module,
  id: u64,
  spec: &str,
) {
  let Ok(cspec) = CString::new(spec) else {
    return;
  };
  let sval = unsafe { JS_NewString(ctx, cspec.as_ptr()) };
  if sval.tag == JS_TAG_EXCEPTION {
    unsafe {
      let e = JS_GetException(ctx);
      JS_FreeValue(ctx, e);
    }
    return;
  }
  let spec_handle = intern::<V8String>(sval);
  let attrs_handle = intern::<FixedArray>(unsafe { JS_NewArray(ctx) });
  let (Some(ctx_local), Some(spec_local), Some(attrs_local), Some(ref_local)) = (
    unsafe { crate::Local::from_raw(context) },
    unsafe { crate::Local::from_raw(spec_handle) },
    unsafe { crate::Local::from_raw(attrs_handle) },
    unsafe { crate::Local::from_raw(m) },
  ) else {
    return;
  };
  // Windows ABI: leading return-slot out-param (see run_synthetic_eval_steps).
  #[cfg(not(target_os = "windows"))]
  let ret = unsafe { scb(ctx_local, spec_local, attrs_local, ref_local) };
  #[cfg(target_os = "windows")]
  let ret = unsafe {
    let mut ret = ptr::null();
    scb(&mut ret, ctx_local, spec_local, attrs_local, ref_local);
    ret
  };

  let obj: *const Object = unsafe { std::mem::transmute(ret) };
  if obj.is_null() {
    if std::env::var_os("QJS_TRACE_MOD").is_some() {
      eprintln!("[src-phase] resolve({spec}) -> null");
    }
    return;
  }
  let obj_val = jsval_of(obj);

  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let mut map = JS_GetPropertyStr(ctx, global, c"__v82jsc_wasm_src".as_ptr());
    if !jsv_is_object(&map) {
      JS_FreeValue(ctx, map);
      let src = c"(globalThis.__v82jsc_wasm_src=new Map())";
      map = JS_Eval(
        ctx,
        src.as_ptr(),
        src.to_bytes().len(),
        c"<wasmsrc>".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
    }
    let set_fn = JS_GetPropertyStr(ctx, map, c"set".as_ptr());
    let mut args = [JS_NewInt64(ctx, id as i64), JS_DupValue(ctx, obj_val)];
    let r = JS_Call(ctx, set_fn, map, 2, args.as_mut_ptr());
    if r.tag == JS_TAG_EXCEPTION {
      let e = JS_GetException(ctx);
      JS_FreeValue(ctx, e);
    } else {
      JS_FreeValue(ctx, r);
    }
    JS_FreeValue(ctx, args[0]);
    JS_FreeValue(ctx, args[1]);
    JS_FreeValue(ctx, set_fn);
    JS_FreeValue(ctx, map);
    JS_FreeValue(ctx, global);
    if std::env::var_os("QJS_TRACE_MOD").is_some() {
      eprintln!("[src-phase] resolve({spec}) -> stored id={id}");
    }
  }
}

unsafe fn load_module_by_name(
  ctx: *mut JSContext,
  name: &str,
  allow_existing: bool,
) -> *mut JSModuleDef {
  let Some(source) = lookup_module_source_by_name(name) else {
    if std::env::var_os("QJS_DEBUG_EXC").is_some() {
      eprintln!("[QJS module loader] no source for {name}");
    }
    return ptr::null_mut();
  };
  let source_buffer = eval_source_buffer(&source);
  let Ok(name_c) = CString::new(name) else {
    return ptr::null_mut();
  };

  let existing = if allow_existing {
    let existing = unsafe { v82jsc_get_loaded_module(ctx, name_c.as_ptr()) };
    let is_attributed = !existing.is_null()
      && ATTRIBUTED_MODULE_DEFS
        .with(|defs| defs.borrow().contains(&(existing as usize)));
    if is_attributed {
      ptr::null_mut()
    } else {
      existing
    }
  } else {
    ptr::null_mut()
  };
  if std::env::var_os("QJS_DEBUG_MOD").is_some() && name.contains("stream") {
    eprintln!("[loader] {name} existing_loaded={}", !existing.is_null());
  }
  if !existing.is_null() {
    MODULE_DEF_CACHE.with(|c| {
      c.borrow_mut().insert(name.to_string(), existing as usize);
    });
    return existing;
  }

  let key = bc_key(&source, name);
  if let Some(bytes) = bc_load(key) {
    if std::env::var_os("QJS_DEBUG_MOD").is_some() && name.contains("stream") {
      eprintln!("[loader] {name} -> BYTECODE path");
    }
    let m = read_cached_bytecode(ctx, &bytes);
    if m.tag == JS_TAG_MODULE {
      let def = unsafe { m.u.ptr } as *mut JSModuleDef;
      MODULE_DEF_CACHE.with(|c| {
        c.borrow_mut().insert(name.to_string(), def as usize);
      });
      unsafe { populate_import_meta(ctx, def, name) };
      return def;
    }

    if m.tag == JS_TAG_EXCEPTION {
      let exc = unsafe { JS_GetException(ctx) };
      unsafe { JS_FreeValue(ctx, exc) };
    } else {
      unsafe { JS_FreeValue(ctx, m) };
    }
  }

  if std::env::var_os("QJS_DEBUG_MOD").is_some() {
    let already = unsafe { v82jsc_has_loaded_module(ctx, name_c.as_ptr()) };
    eprintln!("[loader compile fresh] {name} (already_loaded={already})");
  }
  let result = unsafe {
    JS_Eval(
      ctx,
      source_buffer.as_ptr() as *const std::os::raw::c_char,
      source.len(),
      name_c.as_ptr(),
      JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY,
    )
  };
  if result.tag == JS_TAG_EXCEPTION {
    if std::env::var_os("QJS_DEBUG_EXC").is_some() {
      unsafe {
        let exc = JS_GetException(ctx);
        let mut l = 0usize;
        let s = JS_ToCStringLen(ctx, &mut l, exc);
        if !s.is_null() {
          let b = std::slice::from_raw_parts(s as *const u8, l);
          eprintln!(
            "[QJS module loader] parse failed for {name}: {}",
            std::string::String::from_utf8_lossy(b)
          );
          JS_FreeCString(ctx, s);
        }
        JS_FreeValue(ctx, exc);
      }
    }
    return ptr::null_mut();
  }

  unsafe { bc_write(ctx, key, result) };
  let m = unsafe { result.u.ptr } as *mut JSModuleDef;
  MODULE_DEF_CACHE.with(|c| {
    c.borrow_mut().insert(name.to_string(), m as usize);
  });
  unsafe { populate_import_meta(ctx, m, name) };

  m
}

pub(crate) unsafe extern "C" fn module_loader_callback(
  ctx: *mut JSContext,
  module_name: *const std::os::raw::c_char,
  _opaque: *mut std::os::raw::c_void,
  attributes: JSValue,
) -> *mut JSModuleDef {
  let name = match unsafe { std::ffi::CStr::from_ptr(module_name) }.to_str() {
    Ok(s) => s,
    Err(_) => return ptr::null_mut(),
  };
  let attribute_key = unsafe { module_import_attribute_key(ctx, attributes) };

  if let Some(module_key) = lookup_resolved_module_target(name, &attribute_key)
  {
    let target = MODULE_STATE.with(|states| {
      states
        .borrow()
        .get(&module_key)
        .map(|state| (state.module_def, state.module_name.clone()))
    });
    if let Some((module_def, target_name)) = target {
      if !module_def.is_null() {
        if !attribute_key.is_empty() {
          ATTRIBUTED_MODULE_DEFS.with(|defs| {
            defs.borrow_mut().insert(module_def as usize);
          });
        }
        return module_def;
      }

      let target_name = if target_name.is_empty() {
        name
      } else {
        target_name.as_str()
      };
      // Plain modules retain QuickJS's name-based identity, including reuse of
      // an in-flight definition while linking cycles. Attributed requests must
      // bypass that registry because the same name can select another module.
      let module_def = unsafe {
        load_module_by_name(ctx, target_name, attribute_key.is_empty())
      };
      if !module_def.is_null() {
        if !attribute_key.is_empty() {
          ATTRIBUTED_MODULE_DEFS.with(|defs| {
            defs.borrow_mut().insert(module_def as usize);
          });
        }
        MODULE_STATE.with(|states| {
          if let Some(state) = states.borrow_mut().get_mut(&module_key) {
            state.module_def = module_def;
          }
        });
      }
      return module_def;
    }
  }

  unsafe { load_module_by_name(ctx, name, true) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FixedArray__Length(this: *const FixedArray) -> int {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  let v = jsval_of(this);
  let len_val = unsafe { JS_GetPropertyStr(ctx, v, c"length".as_ptr()) };
  if len_val.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return 0;
  }
  let n = match len_val.tag {
    JS_TAG_INT => unsafe { len_val.u.int32 },
    JS_TAG_FLOAT64 => (unsafe { len_val.u.float64 }) as i32,
    _ => 0,
  };
  unsafe { JS_FreeValue(ctx, len_val) };
  n as int
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FixedArray__Get(
  this: *const FixedArray,
  index: int,
) -> *const Data {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() || index < 0 {
    return ptr::null();
  }
  let v = jsval_of(this);

  let elem = unsafe { JS_GetPropertyUint32(ctx, v, index as u32) };
  if elem.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  intern::<Data>(elem)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__GetUnboundScript(
  script: *const Script,
) -> *const UnboundScript {
  let ctx = current_ctx();
  intern_dup::<UnboundScript>(ctx, jsval_of(script))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrigin__CONSTRUCT(
  buf: *mut MaybeUninit<ScriptOrigin>,
  resource_name: *const Value,
  resource_line_offset: i32,
  resource_column_offset: i32,
  _resource_is_shared_cross_origin: bool,
  script_id: i32,
  source_map_url: *const Value,
  _resource_is_opaque: bool,
  _is_wasm: bool,
  _is_module: bool,
  host_defined_options: *const Data,
) {
  if !buf.is_null() {
    unsafe {
      ptr::write_bytes(buf as *mut u8, 0u8, size_of::<ScriptOrigin>());
      let raw = buf as *mut RawScriptOrigin;
      (*raw).resource_name = resource_name as usize;
      (*raw).source_map_url = source_map_url as usize;
      (*raw).script_id = script_id;
      (*raw).resource_line_offset = resource_line_offset;
      (*raw).resource_column_offset = resource_column_offset;
      (*raw).host_defined_options = host_defined_options as usize;
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__Source__CONSTRUCT(
  buf: *mut MaybeUninit<Source>,
  source_string: *const V8String,
  origin: *const ScriptOrigin,
  cached_data: *mut CachedData,
) {
  if buf.is_null() {
    return;
  }
  unsafe {
    ptr::write_bytes(buf as *mut u8, 0u8, size_of::<Source>());
    let raw = buf as *mut RawSource;
    (*raw).source_string = source_string as usize;
    (*raw).cached_data = cached_data as usize;
    if !origin.is_null() {
      let origin = origin as *const RawScriptOrigin;
      (*raw).resource_name = (*origin).resource_name;
      (*raw).resource_line_offset = (*origin).resource_line_offset;
      (*raw).resource_column_offset = (*origin).resource_column_offset;
      (*raw).source_map_url = (*origin).source_map_url;
      (*raw).host_defined_options = (*origin).host_defined_options;
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__Source__DESTRUCT(this: *mut Source) {
  if this.is_null() {
    return;
  }
  let cached_data = unsafe { (*(this as *const RawSource)).cached_data };
  if cached_data != 0 {
    v8__ScriptCompiler__CachedData__DELETE(cached_data as *mut CachedData);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__Source__GetCachedData<'a>(
  this: *const Source,
) -> *const CachedData<'a> {
  if this.is_null() {
    return ptr::null();
  }
  unsafe { (*(this as *const RawSource)).cached_data as *const CachedData<'a> }
}

#[inline]
unsafe fn source_string_of(source: *mut Source) -> *const V8String {
  if source.is_null() {
    return ptr::null();
  }
  unsafe { (*(source as *const RawSource)).source_string as *const V8String }
}

#[inline]
unsafe fn host_defined_options_of(source: *mut Source) -> *const Data {
  if source.is_null() {
    return ptr::null();
  }
  unsafe { (*(source as *const RawSource)).host_defined_options as *const Data }
}

#[inline]
unsafe fn resource_name_of(
  ctx: *mut JSContext,
  source: *mut Source,
) -> std::string::String {
  if source.is_null() || ctx.is_null() {
    return std::string::String::new();
  }
  let name_ptr =
    unsafe { (*(source as *const RawSource)).resource_name } as *const Value;
  if name_ptr.is_null() {
    return std::string::String::new();
  }
  let v = jsval_of(name_ptr);
  if jsv_is_undefined(&v) || jsv_is_null(&v) {
    return std::string::String::new();
  }
  unsafe { jsval_to_rust(ctx, v) }
}

#[inline]
unsafe fn source_map_url_of(
  ctx: *mut JSContext,
  source: *mut Source,
) -> Option<std::string::String> {
  if source.is_null() || ctx.is_null() {
    return None;
  }
  let url_ptr =
    unsafe { (*(source as *const RawSource)).source_map_url } as *const Value;
  if url_ptr.is_null() {
    return None;
  }
  let v = jsval_of(url_ptr);
  if jsv_is_undefined(&v) || jsv_is_null(&v) {
    return None;
  }
  let url = unsafe { jsval_to_rust(ctx, v) };
  if url.is_empty() { None } else { Some(url) }
}

#[inline]
fn script_value_key(v: JSValue) -> Option<usize> {
  match v.tag {
    JS_TAG_STRING | JS_TAG_STRING_ROPE | JS_TAG_OBJECT => {
      let ptr = jsv_get_ptr(&v) as usize;
      (ptr != 0).then_some(ptr)
    }
    _ => None,
  }
}

fn record_script_source_map_url(
  source_value: JSValue,
  url: Option<std::string::String>,
) {
  let Some(key) = script_value_key(source_value) else {
    return;
  };
  SCRIPT_SOURCE_MAP_URLS.with(|m| {
    let mut map = m.borrow_mut();
    if map.len() > 256 && !map.contains_key(&key) {
      map.clear();
    }
    match url {
      Some(url) => {
        map.insert(key, url);
      }
      None => {
        map.remove(&key);
      }
    }
  });
}

fn script_source_map_url(source_value: JSValue) -> Option<std::string::String> {
  let key = script_value_key(source_value)?;
  SCRIPT_SOURCE_MAP_URLS.with(|m| m.borrow().get(&key).cloned())
}

unsafe fn source_map_url_for_source(
  ctx: *mut JSContext,
  source: *mut Source,
  text: &str,
) -> Option<std::string::String> {
  unsafe { source_map_url_of(ctx, source) }
    .or_else(|| extract_source_mapping_url(text))
}

fn new_string_value(ctx: *mut JSContext, text: &str) -> *const Value {
  if ctx.is_null() {
    return intern::<Value>(jsv_undefined());
  }
  let val = unsafe {
    JS_NewStringLen(
      ctx,
      text.as_ptr() as *const std::os::raw::c_char,
      text.len(),
    )
  };
  intern::<Value>(val)
}

#[repr(C)]
struct RawCachedData {
  data: *const u8,
  length: i32,
  rejected: bool,
  buffer_policy: i32,
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CachedData__NEW<'a>(
  data: *const u8,
  length: i32,
) -> *mut CachedData<'a> {
  let boxed = Box::new(RawCachedData {
    data,
    length,
    rejected: false,
    buffer_policy: 0,
  });
  Box::into_raw(boxed) as *mut CachedData<'a>
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CachedData__DELETE<'a>(
  this: *mut CachedData<'a>,
) {
  if this.is_null() {
    return;
  }
  unsafe {
    let raw = Box::from_raw(this as *mut RawCachedData);
    if raw.buffer_policy == 1 && !raw.data.is_null() && raw.length > 0 {
      let slice = std::slice::from_raw_parts_mut(
        raw.data as *mut u8,
        raw.length as usize,
      );
      drop(Box::from_raw(slice as *mut [u8]));
    }
    drop(raw);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__Compile(
  context: *const Context,
  source: *mut Source,
  _options: CompileOptions,
  _no_cache_reason: NoCacheReason,
) -> *const Script {
  let ctx = ctx_of(context);
  let src = unsafe { source_string_of(source) };
  if ctx.is_null() || src.is_null() {
    return ptr::null();
  }
  let src_val = jsval_of(src);
  let text = unsafe { jsval_to_rust(ctx, src_val) };
  let source_map_url = unsafe { source_map_url_for_source(ctx, source, &text) };
  let source_buffer = eval_source_buffer(&text);
  let len = text.len();
  let compiled = unsafe {
    JS_Eval(
      ctx,
      source_buffer.as_ptr() as *const std::os::raw::c_char,
      len,
      c"<compile>".as_ptr(),
      JS_EVAL_TYPE_GLOBAL | JS_EVAL_FLAG_COMPILE_ONLY,
    )
  };
  if compiled.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  note_compiled_bytecode(current_iso(), len);
  note_compilation_cache_miss();
  unsafe { JS_FreeValue(ctx, compiled) };

  let script_value = new_script_value(ctx, text.as_bytes());
  if script_value.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  record_script_source_map_url(script_value, source_map_url);
  let script = intern::<Script>(script_value);
  unsafe {
    record_script_host_defined_options(script, host_defined_options_of(source));
  }
  let name = unsafe { resource_name_of(ctx, source) };
  record_script_resource_name(script, &name);
  script
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CompileModule(
  isolate: *mut RealIsolate,
  source: *mut Source,
  _options: CompileOptions,
  _no_cache_reason: NoCacheReason,
) -> *const Module {
  if isolate.is_null() {
    return ptr::null();
  }
  let st = iso_state(isolate);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  let src = unsafe { source_string_of(source) };
  if ctx.is_null() || src.is_null() {
    return ptr::null();
  }
  let src_val = jsval_of(src);
  let raw_text = unsafe { jsval_to_rust(ctx, src_val) };

  let intrinsic_text = super::core::rewrite_v8_native_intrinsics(&raw_text);
  let input = intrinsic_text.as_deref().unwrap_or(&raw_text);
  let dynamic_text = rewrite_dynamic_phase_imports(input, true);
  if dynamic_text.is_some() && input.contains("import.defer") {
    unsafe { ensure_dynamic_defer_import_global(ctx) };
  }
  let input = dynamic_text.as_deref().unwrap_or(input);
  let (deferred_text, deferred_imports) = rewrite_deferred_imports(input);
  let (text, source_imports) = rewrite_source_phase(&deferred_text);
  let specifier = unsafe { resource_name_of(ctx, source) };
  let fname = if specifier.is_empty() {
    "<module>".to_string()
  } else {
    specifier.clone()
  };
  let source_map_url = unsafe { source_map_url_for_source(ctx, source, &text) };
  let script_id = assign_module_script_id(&fname);
  let module_name = module_name_for_source(&fname, &text, script_id);

  let import_specifiers = parse_import_specifiers(&text);
  let spec_strs: Vec<std::string::String> =
    import_specifiers.iter().map(|(s, _)| s.clone()).collect();
  let import_offsets = compute_import_offsets(&text, &spec_strs);
  let import_attributes = compute_import_attributes(&text, &spec_strs);
  let is_async = has_top_level_await(&text);

  register_module_source(&module_name, &text);
  note_main_module(&fname);
  if std::env::var_os("QJS_DEBUG_MOD").is_some() {
    eprintln!(
      "[QJS CompileModule] {fname} as {module_name} imports={import_specifiers:?}"
    );
  }

  let handle_val = unsafe { JS_NewObject(ctx) };
  if handle_val.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  let this = intern_module(handle_val);
  if this.is_null() {
    return ptr::null();
  }
  record_module_state(
    this,
    ModuleState {
      context: ctx,
      status: ModuleStatus::Uninstantiated,
      module_def: ptr::null_mut(),
      bytecode: None,
      import_specifiers,
      import_offsets,
      import_attributes,
      source_imports,
      deferred_imports,
      synthetic: false,
      engine_synthetic: false,
      is_async,
      source_text: text,
      source_name: fname.clone(),
      module_name: module_name.clone(),
      script_id,
      source_map_url,
    },
  );
  // Map name -> this wrapper so deno's import.meta callback can be handed the
  // exact handle it registered (it looks modules up by Global identity).
  record_module_wrapper(&module_name, this);
  unsafe {
    record_host_defined_options_for_name(
      &module_name,
      host_defined_options_of(source),
    );
  }
  this
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CompileFunction(
  context: *const Context,
  source: *mut Source,
  arguments_count: usize,
  arguments: *const *const V8String,
  context_extensions_count: usize,
  context_extensions: *const *const Object,
  _options: CompileOptions,
  _no_cache_reason: NoCacheReason,
) -> *const Function {
  let ctx = ctx_of(context);
  let src = unsafe { source_string_of(source) };
  if ctx.is_null() || src.is_null() {
    if !ctx.is_null() {
      unsafe {
        JS_ThrowTypeError(ctx, c"compile_function: invalid source".as_ptr())
      };
    }
    return ptr::null();
  }
  let mut body = unsafe { jsval_to_rust(ctx, jsval_of(src)) };
  if let Some(rewritten) = rewrite_dynamic_phase_imports(&body, false) {
    if body.contains("import.defer") {
      unsafe { ensure_dynamic_defer_import_global(ctx) };
    }
    body = rewritten;
  }

  if body.starts_with("#!") {
    body.replace_range(0..2, "//");
  }

  let mut arg_names: Vec<std::string::String> = Vec::new();
  if !arguments.is_null() {
    for i in 0..arguments_count {
      let a = unsafe { *arguments.add(i) };
      if a.is_null() {
        continue;
      }
      arg_names.push(unsafe { jsval_to_rust(ctx, jsval_of(a)) });
    }
  }

  let mut extension_names: Vec<std::string::String> = Vec::new();
  let has_extensions =
    context_extensions_count > 0 && !context_extensions.is_null();
  if has_extensions {
    extension_names.reserve(context_extensions_count);
    for i in 0..context_extensions_count {
      extension_names.push(format!("__v8_ext{i}"));
    }
  }

  let wrapped = if has_extensions {
    let mut wrapped = format!("(function({}) {{\n", extension_names.join(","));
    for name in &extension_names {
      wrapped.push_str("with (");
      wrapped.push_str(name);
      wrapped.push_str(") {\n");
    }
    wrapped.push_str("return (function(");
    wrapped.push_str(&arg_names.join(","));
    wrapped.push_str(") {\n");
    wrapped.push_str(&body);
    wrapped.push_str("\n});\n");
    for _ in &extension_names {
      wrapped.push_str("}\n");
    }
    wrapped.push_str("})");
    wrapped
  } else {
    format!("(function({}) {{\n{}\n}})", arg_names.join(","), body)
  };
  let wrapper_line_count = if has_extensions {
    extension_names.len() as i32 + 2
  } else {
    1
  };
  let source_buffer = eval_source_buffer(&wrapped);
  let len = wrapped.len();

  let name = unsafe { resource_name_of(ctx, source) };
  if !name.is_empty() {
    super::core::register_script_source(&name, &body);
  }
  let name_c = CString::new(if name.is_empty() {
    "<function>".to_string()
  } else {
    name.clone()
  })
  .unwrap_or_else(|_| CString::new("<function>").unwrap());
  let compiled = unsafe {
    JS_Eval(
      ctx,
      source_buffer.as_ptr() as *const std::os::raw::c_char,
      len,
      name_c.as_ptr(),
      JS_EVAL_TYPE_GLOBAL,
    )
  };
  if compiled.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  let result = if has_extensions {
    if unsafe { !JS_IsFunction(ctx, compiled) } {
      unsafe { JS_FreeValue(ctx, compiled) };
      return ptr::null();
    }
    let mut args: Vec<JSValue> = Vec::with_capacity(context_extensions_count);
    for i in 0..context_extensions_count {
      let extension = unsafe { *context_extensions.add(i) };
      if extension.is_null() {
        args.push(jsv_undefined());
      } else {
        args.push(jsval_of(extension));
      }
    }
    let result = unsafe {
      JS_Call(
        ctx,
        compiled,
        jsv_undefined(),
        args.len() as _,
        args.as_mut_ptr(),
      )
    };
    unsafe { JS_FreeValue(ctx, compiled) };
    if result.tag == JS_TAG_EXCEPTION {
      return ptr::null();
    }
    result
  } else {
    compiled
  };
  if unsafe { !JS_IsFunction(ctx, result) } {
    unsafe { JS_FreeValue(ctx, result) };
    return ptr::null();
  }
  let resource_line_offset =
    unsafe { (*(source as *const RawSource)).resource_line_offset };
  unsafe {
    v82jsc_adjust_function_line_number(
      result,
      resource_line_offset.saturating_sub(wrapper_line_count),
    );
  }
  let function = intern::<Function>(result);
  unsafe {
    record_script_host_defined_options(
      function,
      host_defined_options_of(source),
    );
  }
  function
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__CreateCodeCache(
  _script: *const UnboundScript,
) -> *mut CachedData<'static> {
  make_placeholder_code_cache()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundModuleScript__CreateCodeCache(
  _script: *const UnboundModuleScript,
) -> *mut CachedData<'static> {
  make_placeholder_code_cache()
}

/// Header of the code caches this backend produces. QuickJS recompiles from
/// source anyway, so the cache carries no code, only enough for
/// `CompatibilityCheck` to tell its own caches from foreign bytes: a magic
/// number and `CachedDataVersionTag`, both little-endian `u32`s, the way V8
/// starts its own header.
const CODE_CACHE_MAGIC: u32 = u32::from_le_bytes(*b"QJSc");
const CODE_CACHE_HEADER_LEN: usize = 8;

fn code_cache_header() -> [u8; CODE_CACHE_HEADER_LEN] {
  let mut header = [0u8; CODE_CACHE_HEADER_LEN];
  header[..4].copy_from_slice(&CODE_CACHE_MAGIC.to_le_bytes());
  header[4..].copy_from_slice(
    &super::misc::v8__ScriptCompiler__CachedDataVersionTag().to_le_bytes(),
  );
  header
}

/// Values of `v8::SerializedCodeSanityCheckResult`.
const SANITY_CHECK_SUCCESS: int = 0;
const SANITY_CHECK_MAGIC_NUMBER_MISMATCH: int = 1;
const SANITY_CHECK_VERSION_MISMATCH: int = 2;
const SANITY_CHECK_INVALID_HEADER: int = 7;

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CachedData__CompatibilityCheck(
  this: *const CachedData,
  _isolate: *mut RealIsolate,
) -> int {
  if this.is_null() {
    return SANITY_CHECK_INVALID_HEADER;
  }
  let raw = unsafe { &*(this as *const RawCachedData) };
  if raw.data.is_null() || (raw.length as usize) < CODE_CACHE_HEADER_LEN {
    return SANITY_CHECK_INVALID_HEADER;
  }
  let bytes =
    unsafe { std::slice::from_raw_parts(raw.data, CODE_CACHE_HEADER_LEN) };
  let expected = code_cache_header();
  if bytes[..4] != expected[..4] {
    return SANITY_CHECK_MAGIC_NUMBER_MISMATCH;
  }
  if bytes[4..] != expected[4..] {
    return SANITY_CHECK_VERSION_MISMATCH;
  }
  SANITY_CHECK_SUCCESS
}

/// Mirrors `v8::ScriptCompiler::CompileOptionsIsValid`.
#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CompileOptionsIsValid(
  compile_options: int,
) -> bool {
  const CONSUME_CODE_CACHE: int = 1 << 0;
  const EAGER_COMPILE: int = 1 << 1;
  const PRODUCE_COMPILE_HINTS: int = 1 << 2;
  const CONSUME_COMPILE_HINTS: int = 1 << 3;
  // Consuming a code cache and eager compilation each exclude every other
  // flag, and compile hints cannot be produced and consumed at once.
  if compile_options & CONSUME_CODE_CACHE != 0
    && compile_options != CONSUME_CODE_CACHE
  {
    return false;
  }
  if compile_options & EAGER_COMPILE != 0 && compile_options != EAGER_COMPILE {
    return false;
  }
  let hints = PRODUCE_COMPILE_HINTS | CONSUME_COMPILE_HINTS;
  compile_options & hints != hints
}

/// A stable id per script, assigned on first request from the same sequence
/// as module script ids, so scripts and modules never share one.
fn script_id_of(script: *const std::ffi::c_void) -> int {
  let Some(key) = script_value_key(jsval_of(script as *const Value)) else {
    return 0;
  };
  SCRIPT_IDS.with(|m| {
    *m.borrow_mut()
      .entry(key)
      .or_insert_with(next_module_script_id)
  })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__ScriptId(script: *const Script) -> int {
  script_id_of(script as *const std::ffi::c_void)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__ScriptId(
  script: *const UnboundScript,
) -> int {
  script_id_of(script as *const std::ffi::c_void)
}

pub(crate) fn make_placeholder_code_cache() -> *mut CachedData<'static> {
  #[repr(C)]
  struct RawCachedData {
    data: *const u8,
    length: i32,
    rejected: bool,
    buffer_policy: i32,
  }
  let v = code_cache_header().to_vec().into_boxed_slice();
  let len = v.len() as i32;
  let data = Box::into_raw(v) as *const u8;
  let boxed = Box::new(RawCachedData {
    data,
    length: len,
    rejected: false,
    buffer_policy: 1,
  });
  Box::into_raw(boxed) as *mut CachedData<'static>
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CompileUnboundScript(
  isolate: *mut RealIsolate,
  source: *mut Source,
  _options: CompileOptions,
  _no_cache_reason: NoCacheReason,
) -> *const UnboundScript {
  if isolate.is_null() {
    return ptr::null();
  }
  let st = crate::quickjs::core::iso_state(isolate);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  let src = unsafe { source_string_of(source) };
  if ctx.is_null() || src.is_null() {
    return ptr::null();
  }
  let src_val = jsval_of(src);
  let text = unsafe { jsval_to_rust(ctx, src_val) };
  let source_map_url = unsafe { source_map_url_for_source(ctx, source, &text) };
  let source_buffer = eval_source_buffer(&text);
  let len = text.len();
  let compiled = unsafe {
    JS_Eval(
      ctx,
      source_buffer.as_ptr() as *const std::os::raw::c_char,
      len,
      c"<compile>".as_ptr(),
      JS_EVAL_TYPE_GLOBAL | JS_EVAL_FLAG_COMPILE_ONLY,
    )
  };
  if compiled.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  note_compiled_bytecode(isolate, len);
  unsafe { JS_FreeValue(ctx, compiled) };
  let script_value = new_script_value(ctx, text.as_bytes());
  if script_value.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  record_script_source_map_url(script_value, source_map_url);
  let script = intern::<UnboundScript>(script_value);
  unsafe {
    record_script_host_defined_options(script, host_defined_options_of(source));
  }
  let name = unsafe { resource_name_of(ctx, source) };
  record_script_resource_name(script, &name);
  script
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__BindToCurrentContext(
  script: *const UnboundScript,
) -> *const Script {
  if script.is_null() {
    return ptr::null();
  }
  let ctx = current_ctx();
  intern_dup::<Script>(ctx, jsval_of(script))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__GetSourceMappingURL(
  script: *const UnboundScript,
) -> *const Value {
  if script.is_null() {
    return intern::<Value>(jsv_undefined());
  }
  let ctx = current_ctx();
  let value = jsval_of(script);
  let url = script_source_map_url(value).or_else(|| {
    if ctx.is_null() {
      return None;
    }
    let source = script_source_value(ctx, script);
    if source.tag == JS_TAG_EXCEPTION {
      return None;
    }
    let text = unsafe { jsval_to_rust(ctx, source) };
    unsafe { JS_FreeValue(ctx, source) };
    extract_source_mapping_url(&text)
  });
  if let Some(url) = url {
    return new_string_value(ctx, &url);
  }
  intern::<Value>(jsv_undefined())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundModuleScript__GetSourceMappingURL(
  script: *const UnboundModuleScript,
) -> *const Value {
  let ctx = current_ctx();
  // GetUnboundModuleScript returns the Module value unchanged (dup'd), so the
  // `script` handle shares the Module's underlying JSObject pointer — recover the
  // `//# sourceMappingURL=` extracted at compile time. deno reads this to
  // register native source maps (inline `data:` or external `.map`).
  let url =
    with_module_state(script as *const Module, |m| m.source_map_url.clone())
      .flatten();
  if !ctx.is_null() {
    if let Some(url) = url {
      let val = unsafe {
        JS_NewStringLen(
          ctx,
          url.as_ptr() as *const std::os::raw::c_char,
          url.len(),
        )
      };
      return intern::<Value>(val);
    }
  }
  intern::<Value>(jsv_undefined())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundModuleScript__GetSourceURL(
  _script: *const UnboundModuleScript,
) -> *const Value {
  intern::<Value>(jsv_undefined())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetStatus(this: *const Module) -> ModuleStatus {
  with_module_state(this, |m| clone_status(&m.status))
    .unwrap_or(ModuleStatus::Errored)
}

fn clone_status(s: &ModuleStatus) -> ModuleStatus {
  match s {
    ModuleStatus::Uninstantiated => ModuleStatus::Uninstantiated,
    ModuleStatus::Instantiating => ModuleStatus::Instantiating,
    ModuleStatus::Instantiated => ModuleStatus::Instantiated,
    ModuleStatus::Evaluating => ModuleStatus::Evaluating,
    ModuleStatus::Evaluated => ModuleStatus::Evaluated,
    ModuleStatus::Errored => ModuleStatus::Errored,
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetException(
  this: *const Module,
) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  let module_def = with_module_state(this, |module| module.module_def)
    .unwrap_or(ptr::null_mut());
  if module_def.is_null() {
    return intern::<Value>(jsv_undefined());
  }
  intern::<Value>(unsafe { v82jsc_module_get_exception(ctx, module_def) })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetModuleRequests(
  this: *const Module,
) -> *const FixedArray {
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  let (specs, offsets, attrs, src_imports, deferred_imports) =
    with_module_state(this, |m| {
      (
        m.import_specifiers.clone(),
        m.import_offsets.clone(),
        m.import_attributes.clone(),
        m.source_imports.clone(),
        m.deferred_imports.clone(),
      )
    })
    .unwrap_or_default();

  let arr = unsafe { JS_NewArray(ctx) };
  if arr.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  let mut idx = 0u32;

  for (_id, spec) in src_imports.iter() {
    let req = unsafe { JS_NewObject(ctx) };
    if req.tag == JS_TAG_EXCEPTION {
      unsafe { JS_FreeValue(ctx, req) };
      continue;
    }
    if let Ok(cspec) = CString::new(spec.as_str()) {
      let sval = unsafe { JS_NewString(ctx, cspec.as_ptr()) };
      unsafe { JS_SetPropertyStr(ctx, req, c"specifier".as_ptr(), sval) };
    }
    unsafe {
      JS_SetPropertyStr(ctx, req, c"__src_phase".as_ptr(), JS_NewBool(ctx, 1));
      JS_SetPropertyStr(
        ctx,
        req,
        c"__v8jsc_module_request".as_ptr(),
        JS_NewBool(ctx, 1),
      );
      JS_SetPropertyUint32(ctx, arr, idx, req);
    }
    idx += 1;
  }
  for deferred in deferred_imports {
    let req = unsafe { JS_NewObject(ctx) };
    if req.tag == JS_TAG_EXCEPTION {
      unsafe { JS_FreeValue(ctx, req) };
      continue;
    }
    if let Ok(specifier) = CString::new(deferred.specifier) {
      unsafe {
        JS_SetPropertyStr(
          ctx,
          req,
          c"specifier".as_ptr(),
          JS_NewString(ctx, specifier.as_ptr()),
        );
      }
    }
    unsafe {
      JS_SetPropertyStr(
        ctx,
        req,
        c"__defer_phase".as_ptr(),
        JS_NewBool(ctx, 1),
      );
      JS_SetPropertyStr(
        ctx,
        req,
        c"__source_offset".as_ptr(),
        JS_NewInt32(ctx, deferred.source_offset),
      );
    }
    if !deferred.attributes.is_empty() {
      let attributes = unsafe { JS_NewArray(ctx) };
      if attributes.tag != JS_TAG_EXCEPTION {
        for (index, (key, value, _)) in deferred.attributes.iter().enumerate() {
          let (Ok(key), Ok(value)) =
            (CString::new(key.as_str()), CString::new(value.as_str()))
          else {
            continue;
          };
          unsafe {
            JS_SetPropertyUint32(
              ctx,
              attributes,
              (index * 2) as u32,
              JS_NewString(ctx, key.as_ptr()),
            );
            JS_SetPropertyUint32(
              ctx,
              attributes,
              (index * 2 + 1) as u32,
              JS_NewString(ctx, value.as_ptr()),
            );
          }
        }
        unsafe {
          JS_SetPropertyStr(ctx, req, c"__attr_kv".as_ptr(), attributes)
        };
      }
    }
    unsafe {
      JS_SetPropertyStr(
        ctx,
        req,
        c"__v8jsc_module_request".as_ptr(),
        JS_NewBool(ctx, 1),
      );
      JS_SetPropertyUint32(ctx, arr, idx, req);
    }
    idx += 1;
  }
  for (si, (spec, ty)) in specs.iter().enumerate() {
    let req = unsafe { JS_NewObject(ctx) };
    if req.tag == JS_TAG_EXCEPTION {
      unsafe { JS_FreeValue(ctx, req) };
      continue;
    }
    if let Ok(cspec) = CString::new(spec.as_str()) {
      let sval = unsafe { JS_NewString(ctx, cspec.as_ptr()) };

      unsafe { JS_SetPropertyStr(ctx, req, c"specifier".as_ptr(), sval) };
    }

    if let Some(t) = ty {
      if let Ok(ct) = CString::new(t.as_str()) {
        let tval = unsafe { JS_NewString(ctx, ct.as_ptr()) };
        unsafe { JS_SetPropertyStr(ctx, req, c"__attr_type".as_ptr(), tval) };
      }
    }

    // Byte offset of the specifier literal's opening quote (deno's
    // `referrer_source_offset`), surfaced via `GetSourceOffset`.
    let off = offsets.get(si).copied().unwrap_or(0);
    unsafe {
      JS_SetPropertyStr(
        ctx,
        req,
        c"__source_offset".as_ptr(),
        JS_NewInt32(ctx, off),
      );
    }

    // Full `with { ... }` attributes as a flat [k0, v0, k1, v1, ...] string
    // array, surfaced as `[k, v, source_offset]` triples by GetImportAttributes.
    if let Some(pairs) = attrs.get(si) {
      if !pairs.is_empty() {
        let kv = unsafe { JS_NewArray(ctx) };
        if kv.tag != JS_TAG_EXCEPTION {
          let mut ki = 0u32;
          for (k, v, _offset) in pairs.iter() {
            if let (Ok(ck), Ok(cv)) =
              (CString::new(k.as_str()), CString::new(v.as_str()))
            {
              unsafe {
                JS_SetPropertyUint32(
                  ctx,
                  kv,
                  ki,
                  JS_NewString(ctx, ck.as_ptr()),
                );
                JS_SetPropertyUint32(
                  ctx,
                  kv,
                  ki + 1,
                  JS_NewString(ctx, cv.as_ptr()),
                );
              }
              ki += 2;
            }
          }
          unsafe { JS_SetPropertyStr(ctx, req, c"__attr_kv".as_ptr(), kv) };
        } else {
          unsafe { JS_FreeValue(ctx, kv) };
        }
      }
    }

    unsafe {
      JS_SetPropertyStr(
        ctx,
        req,
        c"__v8jsc_module_request".as_ptr(),
        JS_NewBool(ctx, 1),
      );

      JS_SetPropertyUint32(ctx, arr, idx, req);
    }
    idx += 1;
  }
  intern::<FixedArray>(arr)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__SourceOffsetToLocation(
  this: *const Module,
  offset: int,
  out: *mut Location,
) {
  if out.is_null() {
    return;
  }

  let source =
    with_module_state(this, |m| m.source_text.clone()).unwrap_or_default();
  let target = if offset <= 0 {
    0
  } else {
    (offset as usize).min(source.len())
  };
  let bytes = source.as_bytes();
  let mut line = 0i32;
  let mut column = 0i32;
  let mut i = 0usize;
  while i < target {
    match bytes[i] {
      b'\n' => {
        line += 1;
        column = 0;
      }
      b'\r' => {
        line += 1;
        column = 0;
        if i + 1 < target && bytes[i + 1] == b'\n' {
          i += 1;
        }
      }
      _ => column += 1,
    }
    i += 1;
  }

  unsafe {
    let p = out as *mut i32;
    *p = line;
    *p.add(1) = column;
  }
}

fn is_ascii_ident_start(b: u8) -> bool {
  b == b'_' || b == b'$' || b.is_ascii_alphabetic()
}

fn is_ascii_ident_continue(b: u8) -> bool {
  is_ascii_ident_start(b) || b.is_ascii_digit()
}

fn skip_ascii_ws<'a>(rest: &'a str, column: &mut usize) -> &'a str {
  let skipped = rest.len() - rest.trim_start_matches([' ', '\t']).len();
  *column += skipped;
  &rest[skipped..]
}

fn exported_function_positions(
  source: &str,
) -> Vec<(std::string::String, int, int)> {
  let mut out = Vec::new();
  for (line, original) in source.lines().enumerate() {
    let mut column =
      original.len() - original.trim_start_matches([' ', '\t']).len();
    let mut rest = &original[column..];

    let Some(after_export) = rest.strip_prefix("export") else {
      continue;
    };
    rest = after_export;
    column += "export".len();
    rest = skip_ascii_ws(rest, &mut column);

    if let Some(after_default) = rest.strip_prefix("default") {
      rest = after_default;
      column += "default".len();
      rest = skip_ascii_ws(rest, &mut column);
    }

    if let Some(after_async) = rest.strip_prefix("async") {
      rest = after_async;
      column += "async".len();
      rest = skip_ascii_ws(rest, &mut column);
    }

    let Some(after_function) = rest.strip_prefix("function") else {
      continue;
    };
    rest = after_function;
    column += "function".len();
    rest = skip_ascii_ws(rest, &mut column);

    let Some(first) = rest.as_bytes().first().copied() else {
      continue;
    };
    if !is_ascii_ident_start(first) {
      continue;
    }

    let name_len = rest
      .as_bytes()
      .iter()
      .copied()
      .take_while(|b| is_ascii_ident_continue(*b))
      .count();
    if name_len == 0 {
      continue;
    }

    out.push((
      rest[..name_len].to_string(),
      line as int,
      (column + name_len) as int,
    ));
  }
  out
}

unsafe fn annotate_namespace_function_positions(
  ctx: *mut JSContext,
  namespace: JSValue,
  module: *const Module,
) {
  let Some((source_text, source_name, script_id, source_map_url)) =
    with_module_state(module, |m| {
      (
        m.source_text.clone(),
        m.source_name.clone(),
        m.script_id,
        m.source_map_url.clone(),
      )
    })
  else {
    return;
  };
  if source_text.is_empty() {
    return;
  }

  for (name, line, column) in exported_function_positions(&source_text) {
    let Ok(cname) = CString::new(name) else {
      continue;
    };
    let value = unsafe { JS_GetPropertyStr(ctx, namespace, cname.as_ptr()) };
    if value.tag == JS_TAG_EXCEPTION {
      let exc = unsafe { JS_GetException(ctx) };
      unsafe { JS_FreeValue(ctx, exc) };
      continue;
    }
    if unsafe { JS_IsFunction(ctx, value) } {
      super::function::record_function_script_position(
        value,
        line,
        column,
        script_id,
        (!source_name.is_empty()).then_some(source_name.clone()),
        source_map_url.clone(),
      );
    }
    unsafe { JS_FreeValue(ctx, value) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetModuleNamespace(
  this: *const Module,
) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  super::snapshot::restore_snapshot_module_exports(ctx);
  // Synthetic modules (e.g. dynamically-imported JSON) are `JS_NewCModule`s with
  // no `func_obj`; QuickJS's `JS_GetModuleNamespace` -> `js_build_module_ns`
  // dereferences `func_obj` and segfaults. Build the namespace object directly
  // from the recorded synthetic exports instead.
  if with_module_state(this, |m| m.engine_synthetic).unwrap_or(false) {
    let ns = unsafe { new_synthetic_namespace(ctx) };
    if let Some(def) =
      SYNTHETIC_DEFS.with(|t| t.borrow().get(&handle_key(this)).copied())
    {
      // Prefer the post-init copy (static path). For a dynamically-imported
      // synthetic module (e.g. JSON) the CModule init never ran, so run deno's
      // evaluation_steps to populate SYNTHETIC_EXPORTS and read that. We must NOT
      // call JS_SetModuleExport on the dynamic module — it was never
      // QuickJS-instantiated, so its export slots don't exist (segfault).
      let mut exports =
        SYNTHETIC_NS_EXPORTS.with(|t| t.borrow().get(&def).cloned());
      if exports.is_none() {
        exports = SYNTHETIC_EXPORTS.with(|t| t.borrow().get(&def).cloned());
      }
      if exports.is_none() {
        if !unsafe { run_synthetic_eval_steps(ctx, def as *mut JSModuleDef) } {
          unsafe { JS_FreeValue(ctx, ns) };
          return ptr::null();
        }
        exports = SYNTHETIC_EXPORTS.with(|t| t.borrow().get(&def).cloned());
      }
      if let Some(exports) = exports {
        for (name, val) in exports {
          if let Ok(c) = CString::new(name) {
            let dup = unsafe { JS_DupValue(ctx, val) };
            if !unsafe { add_synthetic_namespace_export(ctx, ns, &c, dup) } {
              let exception = unsafe { JS_GetException(ctx) };
              unsafe {
                JS_FreeValue(ctx, exception);
                JS_FreeValue(ctx, ns);
              }
              return ptr::null();
            }
          }
        }
      }
    }
    let Some(ns) = (unsafe { finish_synthetic_namespace(ctx, ns) }) else {
      return ptr::null();
    };
    return intern::<Value>(ns);
  }
  let mut def =
    with_module_state(this, |m| m.module_def).unwrap_or(ptr::null_mut());
  if def.is_null() {
    def = unsafe { materialize_module_def(ctx, this) };
  }
  if !def.is_null() {
    let ns = unsafe { JS_GetModuleNamespace(ctx, def) };
    if ns.tag != JS_TAG_EXCEPTION {
      unsafe { annotate_namespace_function_positions(ctx, ns, this) };
      return intern::<Value>(ns);
    }
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
  }

  let obj = unsafe { JS_NewObject(ctx) };
  intern::<Value>(obj)
}

unsafe fn materialize_module_def(
  ctx: *mut JSContext,
  this: *const Module,
) -> *mut JSModuleDef {
  let existing =
    with_module_state(this, |m| m.module_def).unwrap_or(ptr::null_mut());
  if !existing.is_null() {
    return existing;
  }
  let Some((source_text, module_name)) =
    with_module_state(this, |m| (m.source_text.clone(), m.module_name.clone()))
  else {
    return ptr::null_mut();
  };
  if source_text.is_empty() {
    return ptr::null_mut();
  }

  if !module_name.is_empty() {
    if let Ok(cn) = CString::new(module_name.clone()) {
      let loaded = unsafe { v82jsc_get_loaded_module(ctx, cn.as_ptr()) };
      if !loaded.is_null() {
        with_module_state(this, |m| m.module_def = loaded);
        MODULE_DEF_CACHE.with(|c| {
          c.borrow_mut().insert(module_name.clone(), loaded as usize);
        });

        let is_ev = unsafe { v82jsc_module_is_evaluated(loaded) };
        let ev_started = unsafe { v82jsc_module_eval_started(loaded) };
        if is_ev == 0 && ev_started == 0 {
          let mv = make_value(
            JS_TAG_MODULE,
            JSValueUnion {
              ptr: loaded as *mut std::os::raw::c_void,
            },
          );
          let mv = unsafe { JS_DupValue(ctx, mv) };
          let result = unsafe { JS_EvalFunction(ctx, mv) };
          let iso2 = current_iso();
          let rt2 = if iso2.is_null() {
            ptr::null_mut()
          } else {
            iso_state(iso2).rt
          };
          unsafe { drain_jobs(rt2) };
          if result.tag == JS_TAG_EXCEPTION {
            let exc = unsafe { JS_GetException(ctx) };
            unsafe { JS_FreeValue(ctx, exc) };
          } else {
            unsafe { JS_FreeValue(ctx, result) };
          }
        }
        return loaded;
      }
    }
  }
  let iso = current_iso();
  let rt = if iso.is_null() {
    ptr::null_mut()
  } else {
    iso_state(iso).rt
  };
  let cache_name = if module_name.is_empty() {
    "<module>"
  } else {
    module_name.as_str()
  };
  let key = bc_key(&source_text, cache_name);
  let Ok(cname) = CString::new(if module_name.is_empty() {
    "<module>".to_string()
  } else {
    module_name.clone()
  }) else {
    return ptr::null_mut();
  };

  let mut module_val: Option<JSValue> = None;
  if let Some(bytes) = bc_load(key) {
    let m = read_cached_bytecode(ctx, &bytes);
    if m.tag == JS_TAG_MODULE {
      module_val = Some(m);
    } else if m.tag == JS_TAG_EXCEPTION {
      let exc = unsafe { JS_GetException(ctx) };
      unsafe { JS_FreeValue(ctx, exc) };
    } else {
      unsafe { JS_FreeValue(ctx, m) };
    }
  }
  if module_val.is_none() {
    let source_buffer = eval_source_buffer(&source_text);
    let c = unsafe {
      JS_Eval(
        ctx,
        source_buffer.as_ptr() as *const std::os::raw::c_char,
        source_text.len(),
        cname.as_ptr(),
        JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY,
      )
    };
    if c.tag == JS_TAG_MODULE {
      unsafe { bc_write(ctx, key, c) };
      module_val = Some(c);
    } else if c.tag == JS_TAG_EXCEPTION {
      let exc = unsafe { JS_GetException(ctx) };
      unsafe { JS_FreeValue(ctx, exc) };
    } else {
      unsafe { JS_FreeValue(ctx, c) };
    }
  }
  let Some(mv) = module_val else {
    return ptr::null_mut();
  };

  let def = unsafe { mv.u.ptr } as *mut JSModuleDef;
  let meta_name = with_module_state(this, |m| {
    m.module_def = def;
    m.status = ModuleStatus::Evaluated;
    m.module_name.clone()
  })
  .unwrap_or_default();
  unsafe { populate_import_meta(ctx, def, &meta_name) };
  let result = unsafe { JS_EvalFunction(ctx, mv) };
  unsafe { drain_jobs(rt) };
  if result.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    if std::env::var_os("QJS_DEBUG_MOD").is_some() {
      let s = unsafe { jsval_to_rust(ctx, exc) };
      eprintln!(
        "[qjs materialize_module {}] exception: {s}",
        cname.to_string_lossy()
      );
    }
    unsafe { JS_FreeValue(ctx, exc) };
  } else {
    unsafe { JS_FreeValue(ctx, result) };
  }
  def
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetModuleNamespace2(
  this: *const Module,
  phase: ModuleImportPhase,
) -> *const Value {
  if phase != ModuleImportPhase::kDefer {
    return v8__Module__GetModuleNamespace(this);
  }
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  let target = unsafe { JS_NewObject(ctx) };
  let handler = unsafe { JS_NewObject(ctx) };
  if target.tag == JS_TAG_EXCEPTION || handler.tag == JS_TAG_EXCEPTION {
    unsafe {
      JS_FreeValue(ctx, target);
      JS_FreeValue(ctx, handler);
    }
    return ptr::null();
  }
  let data = unsafe { JS_DupValue(ctx, jsval_of(this)) };
  let mut func_data = [data];
  let get = unsafe {
    JS_NewCFunctionData(
      ctx,
      deferred_namespace_get,
      3,
      0,
      1,
      func_data.as_mut_ptr(),
    )
  };
  unsafe {
    JS_FreeValue(ctx, data);
    JS_SetPropertyStr(ctx, handler, c"get".as_ptr(), get);
  }
  let proxy = unsafe { JS_NewProxy(ctx, target, handler) };
  unsafe {
    JS_FreeValue(ctx, target);
    JS_FreeValue(ctx, handler);
  }
  if proxy.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  intern::<Value>(proxy)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__EvaluateForImportDefer(
  _this: *const Module,
  context: *const Context,
) -> *const Value {
  let ctx = ctx_of(context);
  if ctx.is_null() {
    return ptr::null();
  }
  make_resolved_promise(ctx)
}

unsafe extern "C" fn deferred_namespace_get(
  ctx: *mut JSContext,
  _this_val: JSValue,
  argc: int,
  argv: *mut JSValue,
  _magic: int,
  func_data: *mut JSValue,
) -> JSValue {
  if ctx.is_null() || argc < 2 || argv.is_null() || func_data.is_null() {
    return jsv_undefined();
  }
  let module = func_data as *const Module;
  let property = unsafe { *argv.add(1) };
  if jsv_is_string(&property)
    && unsafe { jsval_to_rust(ctx, property) } == "then"
  {
    return jsv_undefined();
  }
  let evaluated = with_module_state(module, |state| {
    matches!(state.status, ModuleStatus::Evaluated)
  })
  .unwrap_or(false);
  if !evaluated {
    let context = intern_ctx(ctx);
    let promise = v8__Module__Evaluate(module, context);
    if promise.is_null() {
      return if unsafe { JS_HasException(ctx) } {
        jsv_exception()
      } else {
        jsv_undefined()
      };
    }
    let promise = jsval_of(promise);
    if unsafe { JS_IsPromise(promise) } {
      match unsafe { JS_PromiseState(ctx, promise) } {
        2 => {
          let reason = unsafe { JS_PromiseResult(ctx, promise) };
          return unsafe { JS_Throw(ctx, reason) };
        }
        0 => {
          return unsafe {
            JS_ThrowTypeError(
              ctx,
              c"deferred module evaluation is still pending".as_ptr(),
            )
          };
        }
        _ => {}
      }
    }
  }
  let namespace = v8__Module__GetModuleNamespace(module);
  if namespace.is_null() {
    return jsv_undefined();
  }
  let global = unsafe { JS_GetGlobalObject(ctx) };
  let reflect = unsafe { JS_GetPropertyStr(ctx, global, c"Reflect".as_ptr()) };
  let get = unsafe { JS_GetPropertyStr(ctx, reflect, c"get".as_ptr()) };
  let mut args = [jsval_of(namespace), property];
  let value = unsafe { JS_Call(ctx, get, reflect, 2, args.as_mut_ptr()) };
  unsafe {
    JS_FreeValue(ctx, get);
    JS_FreeValue(ctx, reflect);
    JS_FreeValue(ctx, global);
  }
  value
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetIdentityHash(this: *const Module) -> int {
  (handle_key(this) as int) ^ 0x4d4f_44
}

unsafe fn compile_module_for_instantiation(
  ctx: *mut JSContext,
  this: *const Module,
) -> *mut JSModuleDef {
  let Some((existing, bytecode_def, source_text, module_name)) =
    with_module_state(this, |m| {
      (
        m.module_def,
        m.bytecode
          .map(|value| unsafe { value.u.ptr } as *mut JSModuleDef)
          .unwrap_or(ptr::null_mut()),
        m.source_text.clone(),
        m.module_name.clone(),
      )
    })
  else {
    return ptr::null_mut();
  };
  if !existing.is_null() {
    return existing;
  }
  if !bytecode_def.is_null() {
    with_module_state(this, |m| m.module_def = bytecode_def);
    return bytecode_def;
  }
  let cache_name = if module_name.is_empty() {
    "<module>"
  } else {
    module_name.as_str()
  };
  let key = bc_key(&source_text, cache_name);
  let Ok(cname) = CString::new(cache_name) else {
    return ptr::null_mut();
  };

  let mut module_value = None;
  if let Some(bytes) = bc_load(key) {
    let cached = unsafe { read_cached_bytecode(ctx, &bytes) };
    if cached.tag == JS_TAG_MODULE {
      module_value = Some(cached);
    } else if cached.tag == JS_TAG_EXCEPTION {
      let exception = unsafe { JS_GetException(ctx) };
      unsafe { JS_FreeValue(ctx, exception) };
    } else {
      unsafe { JS_FreeValue(ctx, cached) };
    }
  }
  if module_value.is_none() {
    let source_buffer = eval_source_buffer(&source_text);
    let compiled = unsafe {
      JS_Eval(
        ctx,
        source_buffer.as_ptr() as *const std::os::raw::c_char,
        source_text.len(),
        cname.as_ptr(),
        JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY,
      )
    };
    if compiled.tag == JS_TAG_EXCEPTION {
      unsafe { super::exception::mark_pending_parse_error(ctx) };
      return ptr::null_mut();
    }
    if compiled.tag != JS_TAG_MODULE {
      unsafe { JS_FreeValue(ctx, compiled) };
      return ptr::null_mut();
    }
    unsafe { bc_write(ctx, key, compiled) };
    module_value = Some(compiled);
  }

  let module_value = module_value.unwrap();
  let module_def = unsafe { module_value.u.ptr } as *mut JSModuleDef;
  with_module_state(this, |m| {
    m.module_def = module_def;
    m.bytecode = Some(module_value);
  });
  if !module_name.is_empty() {
    MODULE_DEF_CACHE.with(|cache| {
      cache
        .borrow_mut()
        .insert(module_name.clone(), module_def as usize);
    });
  }
  unsafe { populate_import_meta(ctx, module_def, &module_name) };
  module_def
}

fn missing_export_location(
  source: &str,
  message: &str,
) -> Option<(i32, i32, Option<std::string::String>)> {
  const REQUEST_START: &str = "The requested module '";
  const EXPORT_START: &str = "does not provide an export named '";

  let request_start = message.find(REQUEST_START)? + REQUEST_START.len();
  let specifier_end = message[request_start..].find('\'')? + request_start;
  let specifier = &message[request_start..specifier_end];
  let export_start = message.find(EXPORT_START)? + EXPORT_START.len();
  let export_end = message[export_start..].find('\'')? + export_start;
  let export_name = &message[export_start..export_end];

  let exact = (|| {
    let single_quoted = format!("'{specifier}'");
    let double_quoted = format!("\"{specifier}\"");
    let specifier_offset = source
      .find(&single_quoted)
      .or_else(|| source.find(&double_quoted))?;
    let name_offset = source[..specifier_offset]
      .match_indices(export_name)
      .filter(|(offset, _)| {
        let before = source[..*offset].chars().next_back();
        let after = source[offset + export_name.len()..].chars().next();
        !before.is_some_and(js_identifier_continue)
          && !after.is_some_and(js_identifier_continue)
      })
      .map(|(offset, _)| offset)
      .last()?;
    let line_start = source[..name_offset].rfind('\n').map_or(0, |i| i + 1);
    let line_end = source[name_offset..]
      .find(['\n', '\r'])
      .map_or(source.len(), |i| name_offset + i);
    let line = source[..line_start].bytes().filter(|b| *b == b'\n').count() + 1;
    let column = source[line_start..name_offset].chars().count();
    Some((
      line as i32,
      column as i32,
      Some(source[line_start..line_end].to_string()),
    ))
  })();
  Some(exact.unwrap_or((1, 0, None)))
}

fn js_identifier_continue(ch: char) -> bool {
  ch == '$' || ch == '_' || ch.is_alphanumeric()
}

unsafe fn annotate_module_link_exception(
  ctx: *mut JSContext,
  this: *const Module,
) {
  if !unsafe { JS_HasException(ctx) } {
    return;
  }
  let exception = unsafe { JS_GetException(ctx) };
  let message = unsafe { jsval_to_rust(ctx, exception) };
  let importer_name =
    unsafe { super::exception::read_str_prop(ctx, exception, c"fileName") };
  let location = if let Some(importer_name) = importer_name {
    let source_name = source_name_for_module_name(&importer_name);
    let source_text = lookup_module_source_by_name(&importer_name)
      .or_else(|| lookup_module_source_by_name(&source_name))
      .unwrap_or_default();
    missing_export_location(&source_text, &message).map(
      |(line, column, source_line)| (source_name, line, column, source_line),
    )
  } else {
    with_module_state(this, |module| {
      missing_export_location(&module.source_text, &message).map(
        |(line, column, source_line)| {
          (module.source_name.clone(), line, column, source_line)
        },
      )
    })
    .flatten()
  };
  if let Some((file_name, line, column, source_line)) = location {
    let stack_file_name = std::ffi::CString::new(file_name.as_bytes()).ok();
    let file_name_value = unsafe {
      JS_NewStringLen(
        ctx,
        file_name.as_ptr() as *const std::os::raw::c_char,
        file_name.len(),
      )
    };
    unsafe {
      JS_SetPropertyStr(ctx, exception, c"fileName".as_ptr(), file_name_value);
      JS_SetPropertyStr(
        ctx,
        exception,
        c"lineNumber".as_ptr(),
        JS_NewInt32(ctx, line),
      );
      JS_SetPropertyStr(
        ctx,
        exception,
        c"columnNumber".as_ptr(),
        JS_NewInt32(ctx, column),
      );
      if let Some(source_line) = source_line {
        super::exception::set_message_source_line(ctx, exception, &source_line);
      }
      super::exception::set_module_link_frame(ctx, exception);
      if let Some(file_name) = stack_file_name {
        v82jsc_set_module_error_backtrace(
          ctx,
          exception,
          file_name.as_ptr(),
          line,
          column + 1,
        );
      }
    }
  }
  unsafe { JS_Throw(ctx, exception) };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__InstantiateModule(
  this: *const Module,
  context: *const Context,
  cb: ResolveModuleCallback,
  source_callback: Option<ResolveSourceCallback>,
) -> MaybeBool {
  if let Some(scb) = source_callback {
    SOURCE_CB.with(|c| c.set(Some(unsafe { std::mem::transmute(scb) })));
  }
  let ctx = ctx_of(context);
  if !ctx.is_null() {
    super::snapshot::restore_snapshot_module_exports(ctx);
    if !unsafe { build_resolution_map(ctx, context, cb, source_callback, this) }
    {
      return MaybeBool::Nothing;
    }
    let module_def = unsafe { compile_module_for_instantiation(ctx, this) };
    if module_def.is_null()
      || unsafe { v82jsc_link_module(ctx, module_def) } < 0
    {
      unsafe { annotate_module_link_exception(ctx, this) };
      with_module_state(this, |m| m.status = ModuleStatus::Errored);
      return MaybeBool::Nothing;
    }
  }

  match with_module_state(this, |m| {
    if matches!(m.status, ModuleStatus::Uninstantiated) {
      m.status = ModuleStatus::Instantiated;
    }
    true
  }) {
    Some(true) => MaybeBool::JustTrue,
    _ => MaybeBool::JustFalse,
  }
}

fn make_resolved_promise(ctx: *mut JSContext) -> *const Value {
  let global = unsafe { JS_GetGlobalObject(ctx) };
  let promise_ctor =
    unsafe { JS_GetPropertyStr(ctx, global, c"Promise".as_ptr()) };
  let resolve_fn =
    unsafe { JS_GetPropertyStr(ctx, promise_ctor, c"resolve".as_ptr()) };
  let r = unsafe { JS_Call(ctx, resolve_fn, promise_ctor, 0, ptr::null_mut()) };
  unsafe {
    JS_FreeValue(ctx, resolve_fn);
    JS_FreeValue(ctx, promise_ctor);
    JS_FreeValue(ctx, global);
  }
  if r.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return intern::<Value>(jsv_undefined());
  }
  intern::<Value>(r)
}

fn make_rejected_promise(ctx: *mut JSContext, reason: JSValue) -> *const Value {
  let mut funcs: [JSValue; 2] = [jsv_undefined(), jsv_undefined()];
  let promise = unsafe { JS_NewPromiseCapability(ctx, funcs.as_mut_ptr()) };
  if promise.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe {
      JS_FreeValue(ctx, exc);
      JS_FreeValue(ctx, reason);
    }
    return intern::<Value>(jsv_undefined());
  }
  let resolve = funcs[0];
  let reject = funcs[1];
  let mut args = [reason];
  let r =
    unsafe { JS_Call(ctx, reject, jsv_undefined(), 1, args.as_mut_ptr()) };
  if r.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
  } else {
    unsafe { JS_FreeValue(ctx, r) };
  }
  unsafe {
    JS_FreeValue(ctx, reason);
    JS_FreeValue(ctx, resolve);
    JS_FreeValue(ctx, reject);
  }
  intern::<Value>(promise)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__Evaluate(
  this: *const Module,
  context: *const Context,
) -> *const Value {
  let ctx = ctx_of(context);
  if ctx.is_null() {
    return ptr::null();
  }
  super::snapshot::restore_snapshot_module_exports(ctx);
  if std::env::var_os("QJS_DEBUG_EVAL").is_some() {
    let nm =
      with_module_state(this, |m| m.source_name.clone()).unwrap_or_default();
    let md =
      with_module_state(this, |m| !m.module_def.is_null()).unwrap_or(false);
    eprintln!(
      "[EVAL-ENTRY] this={:?} name={nm:?} has_module_def={md}",
      this
    );
  }
  let iso = current_iso();
  let rt = if iso.is_null() {
    ptr::null_mut()
  } else {
    iso_state(iso).rt
  };
  let evaluate_called_reentrantly =
    MODULE_EVAL_DEPTH.with(|depth| depth.get() != 0);
  // If `Isolate::TerminateExecution` is pending, V8's `Module::Evaluate` returns
  // an empty handle so the embedder surfaces the termination (deno's mod_evaluate
  // then sends `ExecutionTerminated`). Mirror that here: otherwise the per-op
  // interrupt thrown during evaluation is swallowed below and a *resolved*
  // promise is returned, whose deno completion callback is itself blocked by the
  // still-pending termination → the evaluation future never settles (hang). This
  // is the terminate feature's concern, not module loading.
  if !iso.is_null() && iso_state(iso).is_terminating() {
    return ptr::null();
  }
  let _t0 = if std::env::var_os("V82JSC_TIMING").is_some() {
    Some(std::time::Instant::now())
  } else {
    None
  };

  if with_module_state(this, |m| m.engine_synthetic).unwrap_or(false) {
    let already_evaluated =
      with_module_state(this, |m| matches!(m.status, ModuleStatus::Evaluated))
        .unwrap_or(false);
    if !already_evaluated {
      let def =
        with_module_state(this, |m| m.module_def).unwrap_or(ptr::null_mut());
      if !def.is_null() {
        let module = make_value(
          JS_TAG_MODULE,
          JSValueUnion {
            ptr: def as *mut std::os::raw::c_void,
          },
        );
        let module = unsafe { JS_DupValue(ctx, module) };
        let eval_guard = enter_module_eval();
        let result = unsafe { JS_EvalFunction(ctx, module) };
        if eval_guard.should_drain_jobs() {
          unsafe { drain_jobs(rt) };
        }
        if result.tag == JS_TAG_EXCEPTION {
          let exc = unsafe { JS_GetException(ctx) };
          with_module_state(this, |m| m.status = ModuleStatus::Errored);
          return make_rejected_promise(ctx, exc);
        }
        if result.tag == JS_TAG_OBJECT && unsafe { JS_IsPromise(result) } {
          let status = if unsafe { JS_PromiseState(ctx, result) } == 2 {
            ModuleStatus::Errored
          } else {
            ModuleStatus::Evaluated
          };
          with_module_state(this, |m| m.status = status);
          return intern::<Value>(result);
        }
        unsafe { JS_FreeValue(ctx, result) };
      }
      with_module_state(this, |m| m.status = ModuleStatus::Evaluated);
    }
    return make_resolved_promise(ctx);
  }

  super::inspector::maybe_pause_on_next_statement();

  let (bytecode, source_text, source_name, module_name) =
    with_module_state(this, |m| {
      m.status = ModuleStatus::Evaluated;
      (
        m.bytecode.take(),
        m.source_text.clone(),
        m.source_name.clone(),
        m.module_name.clone(),
      )
    })
    .unwrap_or((
      None,
      std::string::String::new(),
      std::string::String::new(),
      std::string::String::new(),
    ));
  let source_name_dbg = source_name.clone();
  let module_name_dbg = module_name.clone();
  if !module_name.is_empty() {
    let cached =
      MODULE_DEF_CACHE.with(|c| c.borrow().get(&module_name).copied());
    if std::env::var_os("QJS_DEBUG_MOD").is_some()
      && source_name.contains("stream")
    {
      let (isev, evst) = match cached {
        Some(d) => unsafe {
          (
            v82jsc_module_is_evaluated(d as *mut JSModuleDef),
            v82jsc_module_eval_started(d as *mut JSModuleDef),
          )
        },
        None => (-1, -1),
      };
      eprintln!(
        "[STREAM-EVAL] {module_name} cached={} is_evaluated={isev} eval_started={evst} bytecode={}",
        cached.is_some(),
        bytecode.is_some()
      );
    }

    if let Some(d) = cached {
      let def = d as *mut JSModuleDef;

      if unsafe { v82jsc_module_is_evaluated(def) } == 0
        && unsafe { v82jsc_module_eval_started(def) } != 0
      {
        with_module_state(this, |m| m.module_def = def);
        return make_resolved_promise(ctx);
      }
      with_module_state(this, |m| m.module_def = def);
      let mv = make_value(
        JS_TAG_MODULE,
        JSValueUnion {
          ptr: def as *mut std::os::raw::c_void,
        },
      );
      let mv = unsafe { JS_DupValue(ctx, mv) };
      let eval_guard = enter_module_eval();
      let result = unsafe { JS_EvalFunction(ctx, mv) };
      if eval_guard.should_drain_jobs() {
        unsafe { drain_jobs(rt) };
      }
      if result.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        if std::env::var_os("QJS_DEBUG_MOD").is_some() {
          let s = unsafe { jsval_to_rust(ctx, exc) };
          eprintln!("[qjs Evaluate reuse {module_name}] exception: {s}");
        }
        with_module_state(this, |m| m.status = ModuleStatus::Errored);
        return make_rejected_promise(ctx, exc);
      }
      if result.tag == JS_TAG_OBJECT
        && unsafe { JS_IsPromise(result) }
        && unsafe { JS_PromiseState(ctx, result) } == 2
      {
        with_module_state(this, |m| m.status = ModuleStatus::Errored);
        return intern::<Value>(result);
      }
      mark_all_modules_evaluated();
      return intern::<Value>(result);
    }
  }

  if std::env::var_os("QJS_DEBUG_MOD").is_some() {
    eprintln!(
      "[QJS Evaluate] {} as {} (precompiled={})",
      source_name,
      module_name,
      bytecode.is_some()
    );
  }

  let mut async_promise: Option<JSValue> = None;
  if let Some(bc) = bytecode {
    let eval_guard = enter_module_eval();
    let result = unsafe { JS_EvalFunction(ctx, bc) };
    if eval_guard.should_drain_jobs() {
      unsafe { drain_jobs(rt) };
    }
    if result.tag == JS_TAG_EXCEPTION {
      let exc = unsafe { JS_GetException(ctx) };
      if !iso.is_null() {
        let s = unsafe { jsval_to_rust(ctx, exc) };
        if !s.is_empty() {
          eprintln!("[qjs] Module::evaluate exception: {s}");
        }
      }
      with_module_state(this, |m| m.status = ModuleStatus::Errored);
      return make_rejected_promise(ctx, exc);
    } else if result.tag == JS_TAG_OBJECT
      && unsafe { JS_IsPromise(result) }
      && unsafe { JS_PromiseState(ctx, result) } == 0
    {
      async_promise = Some(result);
    } else if result.tag == JS_TAG_OBJECT
      && unsafe { JS_IsPromise(result) }
      && unsafe { JS_PromiseState(ctx, result) } == 2
    {
      with_module_state(this, |m| m.status = ModuleStatus::Errored);
      return intern::<Value>(result);
    } else {
      unsafe { JS_FreeValue(ctx, result) };
    }
  } else if !source_text.is_empty() {
    let cache_name = if module_name.is_empty() {
      "<module>"
    } else {
      module_name.as_str()
    };
    let key = bc_key(&source_text, cache_name);
    let cname = CString::new(if module_name.is_empty() {
      "<module>".to_string()
    } else {
      module_name
    })
    .ok();
    if let Some(cname) = cname {
      let mut module_val: Option<JSValue> = None;
      if let Some(bytes) = bc_load(key) {
        let m = read_cached_bytecode(ctx, &bytes);
        if m.tag == JS_TAG_MODULE {
          module_val = Some(m);
        } else if m.tag == JS_TAG_EXCEPTION {
          let exc = unsafe { JS_GetException(ctx) };
          unsafe { JS_FreeValue(ctx, exc) };
        } else {
          unsafe { JS_FreeValue(ctx, m) };
        }
      }
      if module_val.is_none() {
        let source_buffer = eval_source_buffer(&source_text);
        let c = unsafe {
          JS_Eval(
            ctx,
            source_buffer.as_ptr() as *const std::os::raw::c_char,
            source_text.len(),
            cname.as_ptr(),
            JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY,
          )
        };
        if c.tag == JS_TAG_MODULE {
          unsafe { bc_write(ctx, key, c) };
          module_val = Some(c);
        } else if c.tag == JS_TAG_EXCEPTION {
          let exc = unsafe { JS_GetException(ctx) };
          unsafe { JS_FreeValue(ctx, exc) };
        } else {
          unsafe { JS_FreeValue(ctx, c) };
        }
      }

      let mut should_drain_jobs = true;
      let result = if let Some(mv) = module_val {
        let def = unsafe { mv.u.ptr } as *mut JSModuleDef;
        with_module_state(this, |m| m.module_def = def);
        unsafe { populate_import_meta(ctx, def, &module_name_dbg) };
        let eval_guard = enter_module_eval();
        let result = unsafe { JS_EvalFunction(ctx, mv) };
        should_drain_jobs = eval_guard.should_drain_jobs();
        result
      } else {
        let source_buffer = eval_source_buffer(&source_text);
        unsafe {
          JS_Eval(
            ctx,
            source_buffer.as_ptr() as *const std::os::raw::c_char,
            source_text.len(),
            cname.as_ptr(),
            JS_EVAL_TYPE_MODULE,
          )
        }
      };

      if std::env::var_os("QJS_DBG_UNINIT").is_some() {
        let he = unsafe { JS_HasException(ctx) };
        eprintln!("[eval-result] tag={} has_exception={he}", result.tag);
      }
      let sync_exc = if result.tag == JS_TAG_EXCEPTION {
        Some(unsafe { JS_GetException(ctx) })
      } else {
        None
      };
      if should_drain_jobs {
        unsafe { drain_jobs(rt) };
      }
      if let Some(exc) = sync_exc {
        let s = unsafe { jsval_to_rust(ctx, exc) };
        if !s.is_empty() {
          eprintln!(
            "[qjs] Module::evaluate (deferred) exception: {s} (module={source_name_dbg})"
          );
          if std::env::var_os("QJS_DEBUG_MOD").is_some() {
            let stk = unsafe { JS_GetPropertyStr(ctx, exc, c"stack".as_ptr()) };
            if !jsv_is_undefined(&stk) {
              let ss = unsafe { jsval_to_rust(ctx, stk) };
              eprintln!("[qjs] stack:\n{ss}");
            }
            unsafe { JS_FreeValue(ctx, stk) };
          }
        }
        with_module_state(this, |m| m.status = ModuleStatus::Errored);
        return make_rejected_promise(ctx, exc);
      } else {
        if std::env::var_os("QJS_DEBUG_MOD").is_some() {
          if result.tag == JS_TAG_OBJECT && unsafe { JS_IsPromise(result) } {
            let state = unsafe { JS_PromiseState(ctx, result) };
            eprintln!("[QJS Evaluate-result] promise state={state}");
            if state == 2 {
              let pr = unsafe { JS_PromiseResult(ctx, result) };
              let s = unsafe { jsval_to_rust(ctx, pr) };
              eprintln!("[QJS Evaluate-result] rejection: {s}");
              let stk =
                unsafe { JS_GetPropertyStr(ctx, pr, c"stack".as_ptr()) };
              if !jsv_is_undefined(&stk) {
                let ss = unsafe { jsval_to_rust(ctx, stk) };
                eprintln!("[QJS Evaluate-result] stack:\n{ss}");
              }
              unsafe { JS_FreeValue(ctx, stk) };
              unsafe { JS_FreeValue(ctx, pr) };
            }
          } else {
            eprintln!("[QJS Evaluate-result] tag={}", result.tag);
          }
        }
        if result.tag == JS_TAG_OBJECT && unsafe { JS_IsPromise(result) } {
          match unsafe { JS_PromiseState(ctx, result) } {
            0 => async_promise = Some(result),
            2 => {
              with_module_state(this, |m| m.status = ModuleStatus::Errored);
              return intern::<Value>(result);
            }
            _ => unsafe { JS_FreeValue(ctx, result) },
          }
        } else if result.tag != JS_TAG_UNDEFINED {
          unsafe { JS_FreeValue(ctx, result) };
        }
      }
    }
  }

  if let Some(t0) = _t0 {
    eprintln!(
      "[V82JSC_TIMING] Module::Evaluate {} took {:.2} ms",
      source_name_dbg,
      t0.elapsed().as_secs_f64() * 1000.0
    );
    crate::quickjs::function::timing::dump();
  }

  if let Some(p) = async_promise {
    return intern::<Value>(p);
  }

  mark_all_modules_evaluated();

  let mut funcs: [JSValue; 2] = [jsv_undefined(), jsv_undefined()];
  let promise = unsafe { JS_NewPromiseCapability(ctx, funcs.as_mut_ptr()) };
  if promise.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  let resolve = funcs[0];
  let reject = funcs[1];
  let mut args = [jsv_undefined()];
  let r =
    unsafe { JS_Call(ctx, resolve, jsv_undefined(), 1, args.as_mut_ptr()) };
  if r.tag != JS_TAG_EXCEPTION {
    unsafe { JS_FreeValue(ctx, r) };
  } else {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
  }
  unsafe {
    JS_FreeValue(ctx, resolve);
    JS_FreeValue(ctx, reject);
    if !evaluate_called_reentrantly {
      drain_jobs(rt);
    }
  }
  intern::<Value>(promise)
}

fn module_key_for_name(name: &str) -> Option<usize> {
  MODULE_WRAPPER_BY_NAME.with(|t| {
    t.borrow().get(name).and_then(|v| {
      if v.tag < 0 {
        Some(unsafe { v.u.ptr as usize })
      } else {
        None
      }
    })
  })
}

fn module_graph_is_async_key(
  key: usize,
  visited: &mut std::collections::HashSet<usize>,
) -> bool {
  if !visited.insert(key) {
    return false;
  }
  let Some((is_async, base, specs)) = MODULE_STATE.with(|t| {
    t.borrow().get(&key).map(|m| {
      (
        m.is_async,
        m.module_name.clone(),
        m.import_specifiers
          .iter()
          .map(|(spec, _)| spec.clone())
          .collect::<Vec<_>>(),
      )
    })
  }) else {
    return false;
  };
  if is_async {
    return true;
  }
  for spec in specs {
    let mut candidates = Vec::new();
    if let Some(resolved) = lookup_resolved_specifier(&base, &spec) {
      candidates.push(resolved);
    }
    candidates.push(spec.clone());
    candidates.push(resolve_relative_specifier(&base, &spec));
    if let Some(resolved) = lookup_resolved_specifier_any(&spec) {
      candidates.push(resolved);
    }
    for name in candidates {
      let Some(child_key) = module_key_for_name(&name) else {
        continue;
      };
      if module_graph_is_async_key(child_key, visited) {
        return true;
      }
    }
  }
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsGraphAsync(this: *const Module) -> bool {
  module_graph_is_async_key(
    handle_key(this),
    &mut std::collections::HashSet::new(),
  )
}

/// Whether the module's own source uses top-level await. Unlike
/// `IsGraphAsync` this ignores dependencies, and answers before instantiation.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__HasTopLevelAwait(this: *const Module) -> bool {
  with_module_state(this, |m| !m.synthetic && m.is_async).unwrap_or(false)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsSyntheticModule(this: *const Module) -> bool {
  with_module_state(this, |m| m.synthetic).unwrap_or(false)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__CreateSyntheticModule(
  isolate: *const RealIsolate,
  module_name: *const V8String,
  export_names_len: usize,
  export_names_raw: *const *const V8String,
  evaluation_steps: SyntheticModuleEvaluationSteps,
) -> *const Module {
  let iso = isolate as *mut RealIsolate;
  if iso.is_null() {
    return ptr::null();
  }
  let st = iso_state(iso);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  if ctx.is_null() {
    return ptr::null();
  }

  let name = if module_name.is_null() {
    "<synthetic>".to_string()
  } else {
    unsafe { jsval_to_rust(ctx, jsval_of(module_name)) }
  };
  let Ok(cname) = CString::new(name.clone()) else {
    return ptr::null();
  };

  let def = unsafe {
    JS_NewCModule(ctx, cname.as_ptr(), Some(synthetic_module_init_callback))
  };
  if def.is_null() {
    return ptr::null();
  }
  let mut export_names = std::collections::HashSet::new();
  if !export_names_raw.is_null() {
    for i in 0..export_names_len {
      let n = unsafe { *export_names_raw.add(i) };
      if n.is_null() {
        continue;
      }
      let s = unsafe { jsval_to_rust(ctx, jsval_of(n)) };
      if let Ok(c) = CString::new(s.as_str()) {
        unsafe { JS_AddModuleExport(ctx, def, c.as_ptr()) };
        export_names.insert(s);
      }
    }
  }

  let handle_val = unsafe { JS_NewObject(ctx) };
  if handle_val.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  let this = intern_module(handle_val);
  if this.is_null() {
    return ptr::null();
  }
  if std::env::var_os("QJS_DEBUG_MOD").is_some() {
    let nm = unsafe { jsval_to_rust(ctx, jsval_of(module_name)) };
    eprintln!(
      "[QJS CreateSyntheticModule] {nm} def={def:?} n_export_names={export_names_len}"
    );
  }
  SYNTHETIC_DEFS.with(|t| {
    t.borrow_mut().insert(handle_key(this), def as usize);
  });
  SYNTHETIC_EXPORT_NAMES.with(|t| {
    t.borrow_mut().insert(def as usize, export_names);
  });
  {
    let nm = unsafe { jsval_to_rust(ctx, jsval_of(module_name)) };
  }

  let steps: SyntheticModuleEvaluationSteps<'static> =
    unsafe { std::mem::transmute(evaluation_steps) };

  let handle_dup = unsafe { JS_DupValue(ctx, handle_val) };
  SYNTHETIC_EVAL_STEPS.with(|t| {
    t.borrow_mut().insert(def as usize, (steps, handle_dup));
  });
  record_module_state(
    this,
    ModuleState {
      context: ctx,
      status: ModuleStatus::Uninstantiated,
      module_def: def,
      bytecode: None,
      import_specifiers: Vec::new(),
      import_offsets: Vec::new(),
      import_attributes: Vec::new(),
      source_imports: Vec::new(),
      deferred_imports: Vec::new(),
      synthetic: true,
      engine_synthetic: true,
      is_async: false,
      source_map_url: None,
      source_text: std::string::String::new(),
      source_name: std::string::String::new(),
      module_name: name.clone(),
      script_id: assign_module_script_id(""),
    },
  );
  record_module_wrapper(&name, this);
  this
}

// Run deno's `SyntheticModuleEvaluationSteps` for module `m`, which call
// `SetSyntheticModuleExport` to populate SYNTHETIC_EXPORTS. Does NOT touch the
// QuickJS module export slots (caller decides).
unsafe fn run_synthetic_eval_steps(
  ctx: *mut JSContext,
  m: *mut JSModuleDef,
) -> bool {
  let steps =
    SYNTHETIC_EVAL_STEPS.with(|t| t.borrow().get(&(m as usize)).copied());
  if let Some((eval_steps, handle_jsval)) = steps {
    let ctx_handle = super::core::intern_ctx(ctx);
    let mod_handle = super::core::intern_dup::<Module>(ctx, handle_jsval);
    unsafe {
      if let (Some(ctx_l), Some(mod_l)) = (
        crate::Local::from_raw(ctx_handle),
        crate::Local::from_raw(mod_handle),
      ) {
        #[cfg(not(target_os = "windows"))]
        let result =
          std::mem::transmute::<_, *const Value>(eval_steps(ctx_l, mod_l));
        #[cfg(target_os = "windows")]
        let result = {
          let mut result = ptr::null();
          eval_steps(&mut result, ctx_l, mod_l);
          result
        };
        if result.is_null() {
          if !JS_HasException(ctx) {
            JS_ThrowInternalError(
              ctx,
              c"synthetic module evaluation failed".as_ptr(),
            );
          }
          return false;
        }
      }
    }
  }
  true
}

unsafe extern "C" fn synthetic_module_init_callback(
  ctx: *mut JSContext,
  m: *mut JSModuleDef,
) -> std::os::raw::c_int {
  if !unsafe { run_synthetic_eval_steps(ctx, m) } {
    return -1;
  }

  let exports = SYNTHETIC_EXPORTS
    .with(|t| t.borrow_mut().remove(&(m as usize)).unwrap_or_default());
  if std::env::var_os("QJS_DEBUG_MOD").is_some() {
    eprintln!(
      "[QJS synthetic init] def={:?} n_exports={}",
      m,
      exports.len()
    );
  }
  let mut ns_copy: Vec<(std::string::String, JSValue)> = Vec::new();
  for (name, value) in exports {
    let Ok(name_c) = CString::new(name.clone()) else {
      unsafe { JS_FreeValue(ctx, value) };
      continue;
    };
    // Keep a dup for the namespace (JS_SetModuleExport consumes `value`).
    ns_copy.push((name, unsafe { JS_DupValue(ctx, value) }));
    unsafe { JS_SetModuleExport(ctx, m, name_c.as_ptr(), value) };
  }
  SYNTHETIC_NS_EXPORTS.with(|t| {
    t.borrow_mut().insert(m as usize, ns_copy);
  });
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__SetSyntheticModuleExport(
  this: *const Module,
  isolate: *const RealIsolate,
  export_name: *const V8String,
  export_value: *const Value,
) -> MaybeBool {
  let iso = isolate as *mut RealIsolate;
  let ctx = if iso.is_null() {
    current_ctx()
  } else {
    let st = iso_state(iso);
    st.contexts.last().copied().unwrap_or(st.ctx)
  };
  if ctx.is_null() {
    return MaybeBool::JustFalse;
  }
  let def = SYNTHETIC_DEFS
    .with(|t| t.borrow().get(&handle_key(this)).copied())
    .map(|p| p as *mut JSModuleDef);
  let Some(def) = def else {
    return MaybeBool::JustFalse;
  };
  let name = unsafe { jsval_to_rust(ctx, jsval_of(export_name)) };
  let declared = SYNTHETIC_EXPORT_NAMES.with(|t| {
    t.borrow()
      .get(&(def as usize))
      .map(|names| names.contains(&name))
      .unwrap_or(false)
  });
  if !declared {
    if let Ok(msg) =
      CString::new(format!("synthetic module has no export named '{name}'"))
    {
      unsafe { JS_ThrowReferenceError(ctx, msg.as_ptr()) };
    } else {
      unsafe {
        JS_ThrowReferenceError(
          ctx,
          c"synthetic module has no such export".as_ptr(),
        )
      };
    }
    return MaybeBool::Nothing;
  }

  let val = unsafe { JS_DupValue(ctx, jsval_of(export_value)) };
  if std::env::var_os("QJS_DEBUG_MOD").is_some() {
    eprintln!("[QJS SetSyntheticExport] def={def:?} name={name}");
  }
  SYNTHETIC_EXPORTS.with(|t| {
    t.borrow_mut()
      .entry(def as usize)
      .or_default()
      .push((name, val));
  });
  MaybeBool::JustTrue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetUnboundModuleScript(
  this: *const Module,
) -> *const UnboundModuleScript {
  let ctx = current_ctx();
  intern_dup::<UnboundModuleScript>(ctx, jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetStalledTopLevelAwaitMessage(
  this: *const Module,
  _isolate: *const RealIsolate,
  out_vec: *mut StalledTopLevelAwaitMessage,
  vec_len: usize,
) -> usize {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() || out_vec.is_null() || vec_len == 0 {
    return 0;
  }

  // Resolve `this` module's underlying QuickJS def. After evaluation the def is
  // recorded in ModuleState; fall back to the by-name cache for paths that only
  // populate it there.
  let (def, source_name, source_text) = with_module_state(this, |m| {
    let def = if !m.module_def.is_null() {
      m.module_def
    } else {
      MODULE_DEF_CACHE
        .with(|c| c.borrow().get(&m.module_name).copied())
        .map(|d| d as *mut JSModuleDef)
        .unwrap_or(ptr::null_mut())
    };
    (def, m.source_name.clone(), m.source_text.clone())
  })
  .unwrap_or((
    ptr::null_mut(),
    std::string::String::new(),
    std::string::String::new(),
  ));

  // Only a module still parked in EVALUATING_ASYNC has an unresolved top-level
  // await. A resolved TLA advances the module to EVALUATED; deno only reaches
  // here once the event loop is otherwise idle, so this state means "stalled".
  // deno iterates the whole module graph itself (root first, then every module),
  // so checking `this` alone covers every realm without walking dependencies.
  if def.is_null() || unsafe { v82jsc_module_is_evaluating_async(def) } == 0 {
    return 0;
  }

  let mut line = 1;
  let mut column = 1;
  unsafe {
    v82jsc_module_stalled_location(def, &mut line, &mut column);
  }
  let message = build_stalled_tla_message(
    ctx,
    &source_name,
    &source_text,
    line.max(1),
    column.max(1),
  );
  if message.is_null() {
    return 0;
  }

  let module = intern_dup::<Module>(ctx, jsval_of(this));
  unsafe {
    (*out_vec) = StalledTopLevelAwaitMessage { module, message };
  }
  1
}

/// Build the `v8::Message` that deno surfaces for a stalled top-level await.
/// `v8__Message__*` accessors read the value's string form (`.get()`) plus the
/// `fileName`/`lineNumber`/`columnNumber` properties, so we hand back a plain
/// object whose `toString` is V8's fixed text and whose location points at the
/// suspended top-level await.
fn build_stalled_tla_message(
  ctx: *mut JSContext,
  source_name: &str,
  source_text: &str,
  line: i32,
  column: i32,
) -> *const Message {
  let factory_src = c"(file,line,column)=>{const o={fileName:file,lineNumber:line,columnNumber:column};o.toString=()=>\"Top-level await promise never resolved\";return o;}";
  let factory = unsafe {
    JS_Eval(
      ctx,
      factory_src.as_ptr(),
      factory_src.to_bytes().len(),
      c"<stalled-tla>".as_ptr(),
      JS_EVAL_TYPE_GLOBAL,
    )
  };
  if factory.tag == JS_TAG_EXCEPTION {
    unsafe { JS_FreeValue(ctx, JS_GetException(ctx)) };
    return ptr::null();
  }
  let file = CString::new(source_name).unwrap_or_default();
  let mut args = [
    unsafe { JS_NewString(ctx, file.as_ptr()) },
    unsafe { JS_NewInt32(ctx, line) },
    unsafe { JS_NewInt32(ctx, column - 1) },
  ];
  let obj =
    unsafe { JS_Call(ctx, factory, jsv_undefined(), 3, args.as_mut_ptr()) };
  unsafe {
    for arg in args {
      JS_FreeValue(ctx, arg);
    }
    JS_FreeValue(ctx, factory);
  }
  if obj.tag == JS_TAG_EXCEPTION {
    unsafe { JS_FreeValue(ctx, JS_GetException(ctx)) };
    return ptr::null();
  }
  unsafe {
    super::exception::set_message_text_verbatim(
      ctx,
      obj,
      "Top-level await promise never resolved",
    );
    super::exception::set_message_source_line(
      ctx,
      obj,
      source_text
        .lines()
        .nth((line - 1) as usize)
        .unwrap_or(source_text),
    );
  };
  intern::<Message>(obj)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetSpecifier(
  this: *const ModuleRequest,
) -> *const V8String {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  let v = jsval_of(this);

  let spec = unsafe { JS_GetPropertyStr(ctx, v, c"specifier".as_ptr()) };
  if spec.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  intern::<V8String>(spec)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetPhase(
  this: *const ModuleRequest,
) -> ModuleImportPhase {
  let ctx = current_ctx();
  if !ctx.is_null() && !this.is_null() {
    let source = unsafe {
      JS_GetPropertyStr(ctx, jsval_of(this), c"__src_phase".as_ptr())
    };
    let is_source = unsafe { JS_ToBool(ctx, source) } != 0;
    unsafe { JS_FreeValue(ctx, source) };
    if is_source {
      return ModuleImportPhase::kSource;
    }
    let deferred = unsafe {
      JS_GetPropertyStr(ctx, jsval_of(this), c"__defer_phase".as_ptr())
    };
    let is_deferred = unsafe { JS_ToBool(ctx, deferred) } != 0;
    unsafe { JS_FreeValue(ctx, deferred) };
    if is_deferred {
      return ModuleImportPhase::kDefer;
    }
  }
  ModuleImportPhase::kEvaluation
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetSourceOffset(
  this: *const ModuleRequest,
) -> int {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  let v = jsval_of(this);
  let off = unsafe { JS_GetPropertyStr(ctx, v, c"__source_offset".as_ptr()) };
  let n = match off.tag {
    JS_TAG_INT => unsafe { off.u.int32 },
    JS_TAG_FLOAT64 => (unsafe { off.u.float64 }) as i32,
    _ => 0,
  };
  unsafe { JS_FreeValue(ctx, off) };
  n as int
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetImportAttributes(
  this: *const ModuleRequest,
) -> *const FixedArray {
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  let arr = unsafe { JS_NewArray(ctx) };
  if arr.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }

  if !this.is_null() {
    let req = jsval_of(this);
    // Prefer the full `with { ... }` attribute set (flat [k0,v0,k1,v1,...]
    // string array) so deno's validate-import-attributes callback and custom
    // module-type machinery see every key, not just `type`.
    let kv = unsafe { JS_GetPropertyStr(ctx, req, c"__attr_kv".as_ptr()) };
    let mut emitted = false;
    if jsv_is_object(&kv) {
      let len_v = unsafe { JS_GetPropertyStr(ctx, kv, c"length".as_ptr()) };
      let kv_len = match len_v.tag {
        JS_TAG_INT => unsafe { len_v.u.int32 },
        JS_TAG_FLOAT64 => (unsafe { len_v.u.float64 }) as i32,
        _ => 0,
      };
      unsafe { JS_FreeValue(ctx, len_v) };
      let mut out = 0u32;
      let mut i = 0i32;
      while i + 1 < kv_len {
        let k = unsafe { JS_GetPropertyUint32(ctx, kv, i as u32) };
        let v = unsafe { JS_GetPropertyUint32(ctx, kv, (i + 1) as u32) };
        unsafe {
          // Static-import attributes are triples (key, value, source_offset).
          JS_SetPropertyUint32(ctx, arr, out, k);
          JS_SetPropertyUint32(ctx, arr, out + 1, v);
          JS_SetPropertyUint32(ctx, arr, out + 2, JS_NewInt32(ctx, 0));
        }
        out += 3;
        emitted = true;
        i += 2;
      }
    }
    unsafe { JS_FreeValue(ctx, kv) };

    if !emitted {
      // Fallback: legacy `type`-only path (synthetic/source-phase requests that
      // never recorded a full `with {}` clause).
      let ty = unsafe { JS_GetPropertyStr(ctx, req, c"__attr_type".as_ptr()) };
      if jsv_is_string(&ty) {
        unsafe {
          let key = JS_NewString(ctx, c"type".as_ptr());
          JS_SetPropertyUint32(ctx, arr, 0, key);

          JS_SetPropertyUint32(ctx, arr, 1, ty);
          JS_SetPropertyUint32(ctx, arr, 2, JS_NewInt32(ctx, 0));
        }
      } else {
        unsafe { JS_FreeValue(ctx, ty) };
      }
    }
  }
  intern::<FixedArray>(arr)
}

// ---------------------------------------------------------------------------
// Link-stubs for v8 C-ABI symbols that `test_api.rs` references but this
// backend doesn't implement yet. Each returns a benign default
// (null / 0 / false / `Nothing`) so the target LINKS and the many tests that
// don't touch these paths run; tests that do exercise them fail gracefully
// without crashing. Promote individual stubs to real implementations over time.
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsSourceTextModule(
  this: *const std::os::raw::c_void,
) -> bool {
  let this = this as *const Module;
  with_module_state(this, |m| !m.synthetic).unwrap_or(false)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__ScriptId(
  this: *const std::os::raw::c_void,
) -> crate::support::int {
  with_module_state(this as *const Module, |m| m.script_id).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrigin__ResourceName(
  origin: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  if origin.is_null() {
    return std::ptr::null();
  }
  unsafe {
    (*(origin as *const RawScriptOrigin)).resource_name
      as *const std::os::raw::c_void
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrigin__ScriptId(
  origin: *const std::os::raw::c_void,
) -> i32 {
  if origin.is_null() {
    return 0;
  }
  unsafe { (*(origin as *const RawScriptOrigin)).script_id }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrigin__SourceMapUrl(
  origin: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  if origin.is_null() {
    return std::ptr::null();
  }
  unsafe {
    (*(origin as *const RawScriptOrigin)).source_map_url
      as *const std::os::raw::c_void
  }
}
