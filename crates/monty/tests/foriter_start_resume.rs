//! Reproducer for dart_monty#225: ForIter stack corruption in start/resume path.
//!
//! These tests verify that the `start()` → `NameLookup::resume()` →
//! `FunctionCall::resume()` path correctly preserves iterator stack state
//! across nested for-loops with try/except blocks.
//!
//! The bug: 13 of 32 variants crash with
//! "ForIter: expected iterator ref on stack" when executed via start/resume,
//! but pass when executed via run() (which handles everything internally).
//!
//! Trigger conditions (ALL three required):
//! - 3+ for-loops in the same scope
//! - try/except in 2+ of those loops
//! - 3rd loop iterates 4+ items

use std::sync::{Arc, atomic::AtomicBool};

use monty::{CancellableTracker, MontyObject, MontyRun, NameLookupResult, NoLimitTracker, PrintWriter, RunProgress};

/// Drive execution to completion, resolving NameLookups as Undefined
/// and FunctionCalls with simple returns.
///
/// This mimics what dart_monty's `process_progress` does: NameLookups
/// for unknown names get `Undefined` (VM uses built-in lookup), and
/// any external function call gets a simple return value.
fn drive_to_completion(
    mut progress: RunProgress<NoLimitTracker>,
    ext_fn_names: &[&str],
) -> Result<MontyObject, monty::MontyException> {
    let mut print_output = Vec::new();

    loop {
        match progress {
            RunProgress::Complete(obj) => return Ok(obj),
            RunProgress::NameLookup(lookup) => {
                let result = if ext_fn_names.contains(&lookup.name.as_str()) {
                    NameLookupResult::Value(MontyObject::Function {
                        name: lookup.name.clone(),
                        docstring: None,
                    })
                } else {
                    NameLookupResult::Undefined
                };
                progress = lookup.resume(result, PrintWriter::Stdout)?;
            }
            RunProgress::FunctionCall(call) => {
                // For print-like functions, return None
                if call.function_name == "capture" {
                    // Capture the args for verification
                    for arg in &call.args {
                        print_output.push(format!("{:?}", arg));
                    }
                    progress = call.resume(MontyObject::None, PrintWriter::Stdout)?;
                } else {
                    progress = call.resume(MontyObject::None, PrintWriter::Stdout)?;
                }
            }
            RunProgress::ResolveFutures(_) => {
                panic!("unexpected ResolveFutures in synchronous test");
            }
            RunProgress::OsCall(_) => {
                panic!("unexpected OsCall in synchronous test");
            }
        }
    }
}

/// Helper: run code via start/resume path with no external functions.
/// All NameLookups resolved as Undefined (VM handles builtins internally).
fn run_via_start_resume(code: &str) -> Result<MontyObject, monty::MontyException> {
    let runner = MontyRun::new(code.to_owned(), "test.py", vec![]).unwrap();
    let progress = runner.start(vec![], NoLimitTracker, PrintWriter::Stdout)?;
    drive_to_completion(progress, &[])
}

/// Helper: run code via start/resume path with named external functions.
fn run_via_start_resume_with_ext(code: &str, ext_fns: &[&str]) -> Result<MontyObject, monty::MontyException> {
    let runner = MontyRun::new(code.to_owned(), "test.py", vec![]).unwrap();
    let progress = runner.start(vec![], NoLimitTracker, PrintWriter::Stdout)?;
    drive_to_completion(progress, ext_fns)
}

/// Drive execution matching dart_monty's handle.rs pattern EXACTLY:
/// - CancellableTracker wrapping NoLimitTracker
/// - PrintWriter::Collect with short-lived buffer per NameLookup
/// - Buffer created and destroyed on each NameLookup iteration
fn drive_to_completion_dartmonty_style(
    mut progress: RunProgress<CancellableTracker<NoLimitTracker>>,
    ext_fn_names: &[&str],
) -> Result<MontyObject, monty::MontyException> {
    // Initial start() already consumed one PrintWriter::Collect.
    // Now handle NameLookups with NEW short-lived buffers each time
    // (matching handle.rs process_progress).
    loop {
        match progress {
            RunProgress::Complete(obj) => return Ok(obj),
            RunProgress::NameLookup(lookup) => {
                // dart_monty creates a NEW buffer for EVERY NameLookup
                let mut buf = String::new();
                let result = if ext_fn_names.contains(&lookup.name.as_str()) {
                    NameLookupResult::Value(MontyObject::Function {
                        name: lookup.name.clone(),
                        docstring: None,
                    })
                } else {
                    NameLookupResult::Undefined
                };
                let next = lookup.resume(result, PrintWriter::Collect(&mut buf))?;
                // buf dropped here (matching handle.rs)
                progress = next;
            }
            RunProgress::FunctionCall(call) => {
                let mut buf = String::new();
                progress = call.resume(MontyObject::None, PrintWriter::Collect(&mut buf))?;
            }
            RunProgress::ResolveFutures(_) => {
                panic!("unexpected ResolveFutures");
            }
            RunProgress::OsCall(_) => {
                panic!("unexpected OsCall");
            }
        }
    }
}

/// Helper: run via start/resume matching dart_monty's EXACT pattern.
/// Uses CancellableTracker + PrintWriter::Collect with short-lived buffers.
fn run_via_start_resume_dartmonty_style(code: &str) -> Result<MontyObject, monty::MontyException> {
    let runner = MontyRun::new(code.to_owned(), "test.py", vec![]).unwrap();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let tracker = CancellableTracker::with_flag(NoLimitTracker, cancel_flag);

    // dart_monty's run_snapshot_op creates a short-lived buffer for start() too
    let mut start_buf = String::new();
    let progress = runner.start(vec![], tracker, PrintWriter::Collect(&mut start_buf))?;
    // start_buf contents would be drained here in dart_monty
    drive_to_completion_dartmonty_style(progress, &[])
}

/// Helper: run code via run() path (single internal loop, no start/resume).
fn run_via_run(code: &str) -> Result<MontyObject, monty::MontyException> {
    let runner = MontyRun::new(code.to_owned(), "test.py", vec![]).unwrap();
    runner.run(vec![], NoLimitTracker, PrintWriter::Stdout)
}

// ---------------------------------------------------------------------------
// Minimal crash code from dart_monty#225 debug plan
// ---------------------------------------------------------------------------

const MINIMAL_CRASH: &str = r#"
data = [{"a": 1, "b": 2}, {"a": 3, "b": 4}, {"a": 5, "b": 6}, {"a": 7, "b": 8}]
first = data[0]

found = None
for k in first:
    if k == "a":
        found = k
        break

if found is None:
    for k in first:
        try:
            _ = float(first.get(k, ""))
            found = k
            break
        except Exception:
            continue

total = 0
for row in data:
    try:
        total = total + row.get(found, 0)
    except Exception:
        continue

total
"#;

/// The exact minimal crash code from the debug plan.
/// This MUST pass via run() (baseline).
#[test]
fn minimal_crash_via_run() {
    let result = run_via_run(MINIMAL_CRASH).unwrap();
    assert_eq!(result, MontyObject::Int(16));
}

/// The exact minimal crash code via start/resume.
/// This is the bug: if it crashes with "ForIter: expected iterator ref on stack",
/// the bug is in monty's start/resume snapshot/restore path.
#[test]
fn minimal_crash_via_start_resume() {
    let result = run_via_start_resume(MINIMAL_CRASH).unwrap();
    assert_eq!(result, MontyObject::Int(16));
}

// ---------------------------------------------------------------------------
// Variant K: 3 loops, try in 2nd and 3rd (simplest failing variant)
// ---------------------------------------------------------------------------

const VARIANT_K: &str = r#"
rows = [
    {"id": 1, "name": "Alice", "value": 250.0},
    {"id": 2, "name": "Bob", "value": 175.5},
    {"id": 3, "name": "Charlie", "value": ""},
    {"id": 4, "name": "Dana", "value": 324.5},
]
first_row = rows[0]
candidate = None
for key in first_row:
    if key == "value":
        candidate = key
        break
if candidate is None:
    for key in first_row:
        try:
            _ = float(first_row.get(key, ""))
            candidate = key
            break
        except Exception:
            continue
total = 0.0
if candidate:
    for r in rows:
        try:
            total += float(r.get(candidate, ""))
        except Exception:
            continue
total
"#;

#[test]
fn variant_k_via_run() {
    let result = run_via_run(VARIANT_K).unwrap();
    assert_eq!(result, MontyObject::Float(750.0));
}

#[test]
fn variant_k_via_start_resume() {
    let result = run_via_start_resume(VARIANT_K).unwrap();
    assert_eq!(result, MontyObject::Float(750.0));
}

// ---------------------------------------------------------------------------
// Variant D: 3 loops, NO try/except (should pass in both paths)
// ---------------------------------------------------------------------------

const VARIANT_D: &str = r#"
rows = [
    {"id": 1, "name": "Alice", "value": 250.0},
    {"id": 2, "name": "Bob", "value": 175.5},
    {"id": 3, "name": "Charlie", "value": ""},
    {"id": 4, "name": "Dana", "value": 324.5},
]
first_row = rows[0]
candidate = None
for key in first_row:
    if key == "value":
        candidate = key
        break
if candidate is None:
    for key in first_row:
        candidate = key
        break
total = 0.0
if candidate:
    for r in rows:
        total += 1
total
"#;

#[test]
fn variant_d_no_try_via_run() {
    let result = run_via_run(VARIANT_D).unwrap();
    assert_eq!(result, MontyObject::Float(4.0));
}

#[test]
fn variant_d_no_try_via_start_resume() {
    let result = run_via_start_resume(VARIANT_D).unwrap();
    assert_eq!(result, MontyObject::Float(4.0));
}

// ---------------------------------------------------------------------------
// Variant AC: 3 loops, try in 2nd+3rd, but only 3 rows (should pass)
// ---------------------------------------------------------------------------

const VARIANT_AC: &str = r#"
rows = [
    {"id": 1, "name": "Alice", "value": 250.0},
    {"id": 2, "name": "Bob", "value": 175.5},
    {"id": 3, "name": "Charlie", "value": 324.5},
]
first_row = rows[0]
candidate = None
for key in first_row:
    if key == "value":
        candidate = key
        break
if candidate is None:
    for key in first_row:
        try:
            _ = float(first_row.get(key, ""))
            candidate = key
            break
        except Exception:
            continue
total = 0.0
if candidate:
    for r in rows:
        try:
            total += float(r.get(candidate, ""))
        except Exception:
            continue
total
"#;

#[test]
fn variant_ac_3rows_via_run() {
    let result = run_via_run(VARIANT_AC).unwrap();
    assert_eq!(result, MontyObject::Float(750.0));
}

#[test]
fn variant_ac_3rows_via_start_resume() {
    let result = run_via_start_resume(VARIANT_AC).unwrap();
    assert_eq!(result, MontyObject::Float(750.0));
}

// ---------------------------------------------------------------------------
// Variant W: bare 3 loops with dict, try in 2nd+3rd (passes in dart_monty)
// ---------------------------------------------------------------------------

const VARIANT_W: &str = r#"
d = {"a": 1, "b": 2, "c": 3}
x = None
for k in d:
    x = k
    break
if x is None:
    for k in d:
        try:
            x = k
            break
        except Exception:
            continue
total = 0
for k in d:
    try:
        total += d[k]
    except Exception:
        continue
total
"#;

#[test]
fn variant_w_bare_dict_via_run() {
    let result = run_via_run(VARIANT_W).unwrap();
    assert_eq!(result, MontyObject::Int(6));
}

#[test]
fn variant_w_bare_dict_via_start_resume() {
    let result = run_via_start_resume(VARIANT_W).unwrap();
    assert_eq!(result, MontyObject::Int(6));
}

// ---------------------------------------------------------------------------
// Variant O: full step6 pattern, pure python data, 4 rows (FAILS in dart)
// ---------------------------------------------------------------------------

const VARIANT_O: &str = r#"
rows = [
    {"id": 1, "name": "Alice", "value": 250.0},
    {"id": 2, "name": "Bob", "value": 175.5},
    {"id": 3, "name": "Charlie", "value": ""},
    {"id": 4, "name": "Dana", "value": 324.5},
]
first_row = rows[0]
candidate = None
for key in first_row:
    low = key.lower()
    if "value" in low:
        candidate = key
        break
if candidate is None:
    for key in first_row:
        try:
            _ = float(first_row.get(key, ""))
            candidate = key
            break
        except Exception:
            continue
total = 0.0
if candidate:
    for r in rows:
        try:
            val = r.get(candidate, "")
            total += float(val)
        except Exception:
            continue
total
"#;

#[test]
fn variant_o_full_pattern_via_run() {
    let result = run_via_run(VARIANT_O).unwrap();
    assert_eq!(result, MontyObject::Float(750.0));
}

#[test]
fn variant_o_full_pattern_via_start_resume() {
    let result = run_via_start_resume(VARIANT_O).unwrap();
    assert_eq!(result, MontyObject::Float(750.0));
}

// ---------------------------------------------------------------------------
// Variant AB: K-style, pure data, 4 rows (FAILS in dart)
// ---------------------------------------------------------------------------

const VARIANT_AB: &str = r#"
rows = [
    {"id": 1, "name": "Alice", "value": 250.0},
    {"id": 2, "name": "Bob", "value": 175.5},
    {"id": 3, "name": "Charlie", "value": ""},
    {"id": 4, "name": "Dana", "value": 324.5},
]
first_row = rows[0]
candidate = None
for key in first_row:
    if key == "value":
        candidate = key
        break
if candidate is None:
    for key in first_row:
        try:
            _ = float(first_row.get(key, ""))
            candidate = key
            break
        except Exception:
            continue
total = 0.0
if candidate:
    for r in rows:
        try:
            total += float(r.get(candidate, ""))
        except Exception:
            continue
total
"#;

#[test]
fn variant_ab_4rows_via_run() {
    let result = run_via_run(VARIANT_AB).unwrap();
    assert_eq!(result, MontyObject::Float(750.0));
}

#[test]
fn variant_ab_4rows_via_start_resume() {
    let result = run_via_start_resume(VARIANT_AB).unwrap();
    assert_eq!(result, MontyObject::Float(750.0));
}

// ---------------------------------------------------------------------------
// With external functions: mimics dart_monty's print preamble pattern
// ---------------------------------------------------------------------------

const WITH_EXT_FN: &str = r#"
rows = [
    {"id": 1, "name": "Alice", "value": 250.0},
    {"id": 2, "name": "Bob", "value": 175.5},
    {"id": 3, "name": "Charlie", "value": ""},
    {"id": 4, "name": "Dana", "value": 324.5},
]
first_row = rows[0]
candidate = None
for key in first_row:
    if key == "value":
        candidate = key
        break
if candidate is None:
    for key in first_row:
        try:
            _ = float(first_row.get(key, ""))
            candidate = key
            break
        except Exception:
            continue
total = 0.0
if candidate:
    for r in rows:
        try:
            total += float(r.get(candidate, ""))
        except Exception:
            continue
capture(total)
total
"#;

/// Same variant K but with an external function call at the end.
/// This adds a FunctionCall yield/resume cycle after the for-loops.
#[test]
fn variant_k_with_ext_fn_via_start_resume() {
    let result = run_via_start_resume_with_ext(WITH_EXT_FN, &["capture"]).unwrap();
    assert_eq!(result, MontyObject::Float(750.0));
}

// ===========================================================================
// dart_monty-style tests: CancellableTracker + PrintWriter::Collect
// with short-lived buffers per NameLookup (matching handle.rs exactly)
// ===========================================================================

// ---------------------------------------------------------------------------
// WITH PRINT PREAMBLE: the exact code dart_monty prepends before user code
// ---------------------------------------------------------------------------

/// The exact print preamble dart_monty injects (from default_monty_bridge.dart).
const PREAMBLE: &str = r#"
def _cw(*a, sep=' ', end='\n', **k):
    __console_write__(sep.join(str(x) for x in a) + end)
print = _cw
"#;

/// Run with preamble + CancellableTracker + Collect buffers + __console_write__ as ext fn.
/// This matches what dart_monty's DefaultMontyBridge actually does.
fn run_with_preamble(code: &str) -> Result<MontyObject, monty::MontyException> {
    let full_code = format!("{}\n{}", PREAMBLE, code);
    let runner = MontyRun::new(full_code, "test.py", vec![]).unwrap();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let tracker = CancellableTracker::with_flag(NoLimitTracker, cancel_flag);
    let mut start_buf = String::new();
    let progress = runner.start(vec![], tracker, PrintWriter::Collect(&mut start_buf))?;
    drive_to_completion_dartmonty_style(progress, &["__console_write__"])
}

#[test]
fn minimal_crash_with_preamble() {
    let result = run_with_preamble(MINIMAL_CRASH).unwrap();
    assert_eq!(result, MontyObject::Int(16));
}

#[test]
fn variant_k_with_preamble() {
    let result = run_with_preamble(VARIANT_K).unwrap();
    assert_eq!(result, MontyObject::Float(750.0));
}

#[test]
fn variant_o_with_preamble() {
    let result = run_with_preamble(VARIANT_O).unwrap();
    assert_eq!(result, MontyObject::Float(750.0));
}

#[test]
fn variant_ab_with_preamble() {
    let result = run_with_preamble(VARIANT_AB).unwrap();
    assert_eq!(result, MontyObject::Float(750.0));
}

#[test]
fn minimal_crash_dartmonty_style() {
    let result = run_via_start_resume_dartmonty_style(MINIMAL_CRASH).unwrap();
    assert_eq!(result, MontyObject::Int(16));
}

#[test]
fn variant_k_dartmonty_style() {
    let result = run_via_start_resume_dartmonty_style(VARIANT_K).unwrap();
    assert_eq!(result, MontyObject::Float(750.0));
}

#[test]
fn variant_o_dartmonty_style() {
    let result = run_via_start_resume_dartmonty_style(VARIANT_O).unwrap();
    assert_eq!(result, MontyObject::Float(750.0));
}

#[test]
fn variant_ab_dartmonty_style() {
    let result = run_via_start_resume_dartmonty_style(VARIANT_AB).unwrap();
    assert_eq!(result, MontyObject::Float(750.0));
}
