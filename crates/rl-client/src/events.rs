//! Machine-readable events on stdout (`--json`), for the UI.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;

static JSON: AtomicBool = AtomicBool::new(false);

pub fn set_json(on: bool) {
    JSON.store(on, Ordering::Relaxed);
}

pub fn json() -> bool {
    JSON.load(Ordering::Relaxed)
}

/// Emit `{"event": name, ...fields}` as one line when in JSON mode.
pub fn emit(name: &str, fields: Value) {
    if !json() {
        return;
    }
    let mut obj = match fields {
        Value::Object(m) => m,
        _ => Default::default(),
    };
    obj.insert("event".into(), Value::String(name.into()));
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", Value::Object(obj));
    let _ = out.flush();
}
