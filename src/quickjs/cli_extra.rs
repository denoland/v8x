//! Extra symbols the full deno cli references beyond deno_core (QuickJS backend).
#![allow(non_snake_case)]
use std::os::raw::c_char;
use std::sync::{Mutex, OnceLock};

// Process-wide default locale (BCP47 language tag). QuickJS has no ICU default
// locale, so initialize it from the standard POSIX locale environment and
// remember any later `icu_set_default_locale` override.
fn locale_store() -> &'static Mutex<String> {
  static LOCALE: OnceLock<Mutex<String>> = OnceLock::new();
  LOCALE.get_or_init(|| Mutex::new(default_locale_from_environment()))
}

fn canonicalize_locale_id(locale: &str) -> Option<String> {
  let locale = locale.split(['.', '@']).next().unwrap_or(locale);
  if locale.eq_ignore_ascii_case("C") || locale.eq_ignore_ascii_case("POSIX") {
    return Some("en-US".to_string());
  }
  let locale = locale.replace('_', "-");
  icu_locale::Locale::try_from_str(&locale)
    .ok()
    .map(|locale| locale.to_string())
}

fn default_locale_from_environment() -> String {
  ["LC_ALL", "LC_MESSAGES", "LANG"]
    .into_iter()
    .filter_map(|name| std::env::var(name).ok())
    .find_map(|locale| {
      (!locale.is_empty())
        .then(|| canonicalize_locale_id(&locale))
        .flatten()
    })
    .unwrap_or_else(|| "en-US".to_string())
}

// Canonicalize a locale id to a BCP47 language tag the way ICU's
// `uloc_toLanguageTag` does, e.g. "nb_NO" -> "nb-NO".
pub(crate) fn set_default_locale_str(s: &str) {
  let Some(tag) = canonicalize_locale_id(s) else {
    return;
  };
  if let Ok(mut g) = locale_store().lock() {
    *g = tag;
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn icu_get_default_locale(
  output: *mut c_char,
  output_len: usize,
) -> usize {
  let guard = locale_store().lock();
  let loc: Vec<u8> = match guard {
    Ok(g) => g.as_bytes().to_vec(),
    Err(_) => b"en-US".to_vec(),
  };
  if output.is_null() || output_len == 0 {
    return loc.len();
  }
  let n = loc.len().min(output_len.saturating_sub(1));
  unsafe {
    std::ptr::copy_nonoverlapping(loc.as_ptr() as *const c_char, output, n);
    *output.add(n) = 0;
  }
  n
}

// Process-wide default time zone, the counterpart of ICU's default zone.
// QuickJS computes local time with the C library, so installing a zone also
// points `TZ` at it; like ICU's default, that is process-wide state that the
// embedder must not change while other threads evaluate code.
fn time_zone_store() -> &'static Mutex<String> {
  static TIME_ZONE: OnceLock<Mutex<String>> = OnceLock::new();
  TIME_ZONE.get_or_init(|| Mutex::new(host_time_zone()))
}

fn host_time_zone() -> String {
  if let Ok(tz) = std::env::var("TZ") {
    let tz = tz.trim_start_matches(':');
    if is_known_time_zone(tz) {
      return tz.to_string();
    }
  }
  #[cfg(unix)]
  if let Ok(target) = std::fs::read_link("/etc/localtime") {
    let target = target.to_string_lossy();
    if let Some((_, id)) = target.split_once("zoneinfo/")
      && is_known_time_zone(id)
    {
      return id.to_string();
    }
  }
  // ICU's id for a host zone it could not determine.
  "Etc/Unknown".to_string()
}

/// Parses ICU's custom offset ids, "GMT+5", "GMT+05:00", "GMT-0530", into
/// signed minutes east of UTC.
fn custom_offset_minutes(id: &str) -> Option<i32> {
  let rest = id.strip_prefix("GMT")?;
  let (sign, rest) = match rest.as_bytes().first()? {
    b'+' => (1, &rest[1..]),
    b'-' => (-1, &rest[1..]),
    _ => return None,
  };
  let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
  let (hours, minutes) = match rest.split_once(':') {
    Some((h, m)) if digits(h) && h.len() <= 2 && digits(m) && m.len() == 2 => {
      (h.parse::<i32>().ok()?, m.parse::<i32>().ok()?)
    }
    None if digits(rest) && rest.len() <= 2 => (rest.parse::<i32>().ok()?, 0),
    None if digits(rest) && rest.len() == 4 => (
      rest[..2].parse::<i32>().ok()?,
      rest[2..].parse::<i32>().ok()?,
    ),
    _ => return None,
  };
  (hours <= 23 && minutes <= 59).then_some(sign * (hours * 60 + minutes))
}

fn is_known_time_zone(id: &str) -> bool {
  if id.is_empty() || id == "Etc/Unknown" {
    return false;
  }
  custom_offset_minutes(id).is_some()
    || temporal_rs::TimeZone::try_from_identifier_str_with_provider(
      id,
      super::temporal::provider(),
    )
    .is_ok()
}

#[cfg(unix)]
unsafe extern "C" {
  fn tzset();
}

#[cfg(windows)]
unsafe extern "C" {
  #[link_name = "_tzset"]
  fn tzset();
}

fn apply_time_zone_to_libc(id: &str) {
  let tz = match custom_offset_minutes(id) {
    // POSIX TZ offsets count west of UTC, the opposite of ICU's.
    Some(minutes) => {
      let west = -minutes;
      let sign = if west < 0 { '-' } else { '+' };
      format!("<{id}>{sign}{:02}:{:02}", west.abs() / 60, west.abs() % 60)
    }
    None => id.to_string(),
  };
  // SAFETY: see `time_zone_store`; the embedder owns synchronizing this.
  unsafe {
    std::env::set_var("TZ", tz);
    tzset();
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn icu_get_default_time_zone(
  output: *mut c_char,
  output_len: usize,
) -> usize {
  let zone = time_zone_store()
    .lock()
    .map(|g| g.clone())
    .unwrap_or_else(|_| "Etc/Unknown".to_string());
  if output.is_null() || output_len == 0 {
    return zone.len();
  }
  let n = zone.len().min(output_len.saturating_sub(1));
  unsafe {
    std::ptr::copy_nonoverlapping(zone.as_ptr() as *const c_char, output, n);
    *output.add(n) = 0;
  }
  n
}

#[unsafe(no_mangle)]
pub extern "C" fn icu_set_default_time_zone(
  time_zone_id: *const c_char,
) -> bool {
  if time_zone_id.is_null() {
    return false;
  }
  let Ok(id) = unsafe { std::ffi::CStr::from_ptr(time_zone_id) }.to_str()
  else {
    return false;
  };
  if !is_known_time_zone(id) {
    return false;
  }
  let Ok(mut zone) = time_zone_store().lock() else {
    return false;
  };
  apply_time_zone_to_libc(id);
  *zone = id.to_string();
  true
}

#[cfg(test)]
mod tests {
  use super::canonicalize_locale_id;

  #[test]
  fn canonicalizes_icu_and_posix_locale_ids() {
    assert_eq!(canonicalize_locale_id("pl_PL"), Some("pl-PL".into()));
    assert_eq!(
      canonicalize_locale_id("sr_Latn_RS.UTF-8@calendar=gregorian"),
      Some("sr-Latn-RS".into())
    );
    assert_eq!(canonicalize_locale_id("C"), Some("en-US".into()));
    assert_eq!(canonicalize_locale_id("not a locale"), None);
  }
}
