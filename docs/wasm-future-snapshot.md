# Feature Request: Expose `FutureSnapshot` in NAPI-RS Bindings

## Problem

The `monty-js` NAPI-RS bindings do not expose the `FutureSnapshot` API or
`ExternalResult::Future` variant to JavaScript consumers. Python code using
`asyncio.gather()` with external functions executes **sequentially** in
JS/WASM — each external call blocks the entire VM until resolved. True
concurrent `Promise.all()` resolution is impossible.

```python
async def main():
    a, b = await asyncio.gather(fetch("url1"), fetch("url2"))
```

Both fetches execute serially. The second cannot start until the first
completes, even though `asyncio.gather` explicitly requests concurrency.

## Why This Matters

- **LLM tool-use agents** — concurrent tool calls from Python (primary use
  case for downstream consumers like `dart_monty`)
- **Network-heavy scripts** — parallel HTTP requests, API fanouts
- **Downstream language bindings** (Dart, etc.) that wrap the WASM package
  cannot implement async/futures parity with the native Rust API

## Root Cause

**This is an API gap, not a WASM or architectural limitation.** The Rust
core already has the full snapshot/yield state machine for concurrent async.
NAPI-RS can expose complex Rust types across the WASM boundary. The bindings
simply haven't wired `FutureSnapshot<T>`, `ExternalResult::Future`, or
`RunProgress::ResolveFutures` through to JavaScript yet.

### Where the Gap Is

#### 1. `progress_to_result()` panics on `ResolveFutures`

`crates/monty-js/src/monty_cls.rs` (~line 867):

```rust
RunProgress::ResolveFutures(_) => {
    panic!("Async futures (ResolveFutures) are not yet supported in the JS bindings")
}
```

This function converts `RunProgress<T>` into JS-visible types. It handles
`Complete` and `FunctionCall` but panics on `ResolveFutures` — meaning if
Python code ever reaches a futures-resolve point, the process crashes.

#### 2. `MontySnapshot::resume()` only accepts sync results

`crates/monty-js/src/monty_cls.rs` (~line 611):

```rust
let external_result = match (options.return_value, options.exception) {
    (Some(value), None) => ExternalResult::Return(monty_value),
    (None, Some(exc)) => ExternalResult::Error(monty_exc),
    // ...no ExternalResult::Future branch
};
```

`ResumeOptions` only offers `returnValue` or `exception`. There is no way
for a JS consumer to say "this call returns a future — keep executing other
coroutines."

#### 3. No `FutureSnapshot` JS class exported

`crates/monty-js/src/lib.rs:36-41` — the NAPI-RS exports are:

```
Monty, MontySnapshot, MontyComplete, MontyOptions, ResumeOptions,
RunOptions, StartOptions, SnapshotLoadOptions, JsMontyException,
MontyTypingError, JsResourceLimits, ExceptionInput
```

No `FutureSnapshot`. No `MontyFutureSnapshot`. The type exists only in
Rust (`crates/monty/src/run.rs` ~line 462).

#### 4. `runMontyAsync` wrapper is a serial loop

`crates/monty-js/wrapper.js` — the convenience wrapper awaits each
external function call individually:

```javascript
if (result && typeof result.then === 'function') {
    result = await result;  // serial — blocks entire VM
}
progress = progress.resume({ returnValue: result });
```

## What Already Exists in the Rust Core

The Rust API (`crates/monty/src/run.rs`) has everything needed:

| Type / Method | Location | Purpose |
|---|---|---|
| `ExternalResult::Future` | `run.rs` ~line 363 | Marker telling VM "this is a future" |
| `MontyFuture` | `run.rs` ~line 353 | Marker struct for async coroutines |
| `RunProgress::ResolveFutures(FutureSnapshot<T>)` | `run.rs` ~line 234 | VM state when all coroutines are blocked on `await` |
| `FutureSnapshot<T>` | `run.rs` ~line 462 | Holds pending call IDs + resume capability |
| `FutureSnapshot::pending_call_ids()` | `run.rs` ~line 484 | Returns `&[u32]` of pending external call IDs |
| `FutureSnapshot::resume()` | `run.rs` ~line 509 | Accepts `Vec<(u32, ExternalResult)>`, resolves futures incrementally |
| `Snapshot::run(ExternalResult::Future)` | `run.rs` ~line 408 | Pushes `Value::ExternalFuture`, continues VM |
| `Snapshot::run_pending()` | `run.rs` ~line 448 | Convenience: resume without immediate result (async pattern) |

### How the Protocol Works

1. `Monty::start()` returns `RunProgress::FunctionCall { state: Snapshot<T> }`
   — VM paused at an external call.
2. Host calls `snapshot.run(ExternalResult::Future)` — tells the VM "this
   returns a future, keep executing other coroutines."
3. VM continues until all coroutines are blocked on `await`, returns
   `RunProgress::ResolveFutures(FutureSnapshot<T>)`.
4. `FutureSnapshot<T>` exposes `pending_call_ids() -> &[u32]` and
   `resume(Vec<(u32, ExternalResult)>)`.
5. Host resolves all futures concurrently (e.g., `Promise.all()`), then
   passes all results back at once.

This is a natural fit for the JS event loop — the VM yields, JS dispatches
concurrent Promises, and feeds results back when all resolve.

Note: `FutureSnapshot::resume()` also supports **incremental resolution** —
the host can pass a subset of results and get back another
`FutureSnapshot` with the remaining pending calls. The JS wrapper resolves
all at once via `Promise.all()`, but incremental resolution is available
for hosts that need it.

## Proposed Solution

### 1. New NAPI-RS class: `MontyFutureSnapshot`

Expose `FutureSnapshot<T>` as a JS-accessible class in `monty_cls.rs`,
mirroring the `EitherSnapshot` pattern:

```rust
enum EitherFutureSnapshot {
    NoLimit(FutureSnapshot<NoLimitTracker>),
    Limited(FutureSnapshot<LimitedTracker>),
    Done,
}

#[napi]
pub struct MontyFutureSnapshot {
    snapshot: EitherFutureSnapshot,
    script_name: String,
    print_callback: Option<JsPrintCallbackRef>,
}

#[napi]
impl MontyFutureSnapshot {
    #[napi(getter)]
    pub fn pending_call_ids(&self) -> Vec<u32> { /* delegate */ }

    #[napi]
    pub fn resume(
        &mut self,
        env: &Env,
        results: Vec<FutureResumeItem>,
    ) -> Result<Either4<MontySnapshot, MontyComplete, JsMontyException, MontyFutureSnapshot>> {
        // Must consume snapshot via std::mem::replace (same pattern as MontySnapshot::resume)
        // Must take print_callback via std::mem::take for borrow checker
        // Build PrintWriter from callback, then delegate to FutureSnapshot::resume()
    }

    #[napi]
    pub fn dump(&mut self) -> Result<Buffer> { /* serialize for suspend/resume */ }

    #[napi]
    pub fn load(data: Buffer, options: Option<SnapshotLoadOptions>) -> Result<Self> { /* deserialize */ }
}

// Note: uses Unknown<'env> (not JsUnknown) to comply with NAPI-RS object lifetimes
#[napi(object)]
pub struct FutureResumeItem<'env> {
    pub call_id: u32,
    pub return_value: Option<Unknown<'env>>,
    pub exception: Option<ExceptionInput>,
}
```

**Serialization:** `MontyFutureSnapshot` must support `dump()`/`load()`,
requiring `SerializedFutureSnapshot` and `SerializedFutureSnapshotOwned`
structs mirroring the existing `SerializedSnapshot` pattern.

### 2. Add `call_id` and `resume_as_future()` to `MontySnapshot`

The `MontySnapshot` struct must be extended to store `call_id: u32`.
Currently `progress_to_result()` discards it via `..` in the
`FunctionCall` match arm — this must be captured and stored.
`SerializedSnapshot` and `SerializedSnapshotOwned` must also be updated
to include `call_id` so it survives `dump()`/`load()`.

```rust
#[napi]
pub struct MontySnapshot {
    snapshot: EitherSnapshot,
    script_name: String,
    function_name: String,
    args: Vec<MontyObject>,
    kwargs: Vec<(MontyObject, MontyObject)>,
    call_id: u32,                          // NEW — currently discarded
    print_callback: Option<JsPrintCallbackRef>,
}

#[napi]
impl MontySnapshot {
    #[napi(getter)]
    pub fn call_id(&self) -> u32 { self.call_id }

    #[napi]
    pub fn resume_as_future(
        &mut self,
        env: &Env,
    ) -> Result<Either4<MontySnapshot, MontyComplete, JsMontyException, MontyFutureSnapshot>> {
        // Consume snapshot via std::mem::replace(&mut self.snapshot, EitherSnapshot::Done)
        // Take print_callback via std::mem::take(&mut self.print_callback)
        // Call snapshot.run_pending(&mut print_writer)
        // Pass result through updated progress_to_result()
    }
}
```

### 3. Update `progress_to_result()`

Replace the panic with proper conversion and capture `call_id`:

```rust
// Return type changes from Either3 to Either4
fn progress_to_result<T>(
    progress: RunProgress<T>,
    print_callback: Option<JsPrintCallbackRef>,
    script_name: String,
) -> Either4<MontySnapshot, MontyComplete, JsMontyException, MontyFutureSnapshot>

// FunctionCall arm — stop discarding call_id:
RunProgress::FunctionCall {
    function_name, args, kwargs, call_id, state, ..
} => {
    Either4::A(MontySnapshot {
        snapshot: EitherSnapshot::from_snapshot(state),
        script_name,
        function_name,
        args,
        kwargs,
        call_id,          // NEW — was previously discarded via ..
        print_callback,
    })
}

// ResolveFutures arm — replace panic:
RunProgress::ResolveFutures(future_snapshot) => {
    Either4::D(MontyFutureSnapshot {
        snapshot: EitherFutureSnapshot::from_snapshot(future_snapshot),
        script_name,
        print_callback,
    })
}
```

**Important:** Because `MontySnapshot::resume()` can yield
`RunProgress::ResolveFutures` (the VM may hit an `await` on pending
futures after a sync resume), its return type must also change from
`Either3` to `Either4`.

### 4. Update `runMontyAsync` wrapper

```javascript
export async function runMontyAsync(montyRunner, options = {}) {
    const { externalFunctions = {}, inputs, limits } = options;
    let progress = montyRunner.start({ inputs, limits });
    const pendingPromises = new Map();

    while (!(progress instanceof MontyComplete)) {
        // Must check for exceptions — resume() and resumeAsFuture()
        // can return JsMontyException via Either4
        if (progress instanceof JsMontyException) {
            throw progress;
        }

        if (progress instanceof MontySnapshot) {
            const fn = externalFunctions[progress.functionName];
            let result = fn(...progress.args, progress.kwargs);

            if (result && typeof result.then === 'function') {
                const callId = progress.callId;
                pendingPromises.set(callId, result);
                progress = progress.resumeAsFuture();
            } else {
                progress = progress.resume({ returnValue: result });
            }
        } else if (progress instanceof MontyFutureSnapshot) {
            const ids = progress.pendingCallIds;
            const promises = ids.map(id => pendingPromises.get(id));
            const resolved = await Promise.all(
                promises.map(p => p.then(
                    v => ({ ok: true, value: v }),
                    e => ({ ok: false, error: e })
                ))
            );
            const results = ids.map((id, i) => {
                pendingPromises.delete(id);
                return resolved[i].ok
                    ? { callId: id, returnValue: resolved[i].value }
                    : { callId: id, exception: {
                        type: 'Error',
                        message: String(resolved[i].error)
                    }};
            });
            progress = progress.resume(results);
        }
    }
    return progress.output;
}
```

Fully backwards compatible — scripts without async external functions never
hit the `MontyFutureSnapshot` branch.

## Risk Assessment

| Dimension | Level | Rationale |
|---|---|---|
| Breaking existing API | **Medium** | `MontySnapshot.resume()` return type changes from `Either3` to `Either4` — it can now yield `MontyFutureSnapshot` when the VM hits an `await` after a sync resume. `Monty.start()` also widens. |
| TypeScript union type change | **Medium** | TS consumers with exhaustive `switch`/`if-else` on `start()` or `resume()` return types will get compilation errors until they add a `MontyFutureSnapshot` branch. Runtime-compatible but compile-time breaking. |
| Backwards compatibility (runtime) | **Low** | Scripts without async external functions never produce `MontyFutureSnapshot` at runtime, but the union type is still wider |
| Implementation complexity | **Medium** | Requires: `EitherFutureSnapshot` enum, `SerializedFutureSnapshot`/`SerializedFutureSnapshotOwned` for dump/load, `call_id` added to `MontySnapshot` + `SerializedSnapshot`, `progress_to_result()` return type from `Either3` to `Either4` |
| Testing | **Medium** | Needs async Python test fixtures with `asyncio.gather` + external functions; concurrent `Promise.all()` resolution in ava tests; dump/load round-trip for `MontyFutureSnapshot` |
| Security | **Low** | No new sandbox escape vectors — `FutureSnapshot` uses the same `Snapshot` internals; external function results still go through `ExternalResult` validation |

## Files That Need Changes

| File | Change |
|------|--------|
| `crates/monty-js/src/monty_cls.rs` | Add `call_id: u32` to `MontySnapshot` struct; add `EitherFutureSnapshot`, `MontyFutureSnapshot`, `FutureResumeItem<'env>`, `SerializedFutureSnapshot`, `SerializedFutureSnapshotOwned`; add `call_id` getter + `resume_as_future()` + `dump()`/`load()` for futures; update `progress_to_result()` to capture `call_id` and handle `ResolveFutures`; widen `MontySnapshot::resume()` return type from `Either3` to `Either4` |
| `crates/monty-js/src/lib.rs` | Export `MontyFutureSnapshot`, `FutureResumeItem` |
| `crates/monty-js/wrapper.js` | Update `runMontyAsync` loop: add `JsMontyException` check, `MontyFutureSnapshot` branch with `Promise.all()` |
| `crates/monty-js/index.d.ts` | TypeScript declarations for new types (auto-generated by napi-rs) |
| `crates/monty-js/__test__/` | Add ava tests: async futures with `asyncio.gather`, concurrent resolution, dump/load round-trip for `MontyFutureSnapshot` |
| `crates/monty-js/README.md` | Document new API surface |

### Suggested PR Split

1. **PR 1 — Structural prep:** Add `call_id` to `MontySnapshot` +
   `SerializedSnapshot`, capture it in `progress_to_result()`, add
   `resume_as_future()` method.
2. **PR 2 — Futures support:** Introduce `Either4`,
   `MontyFutureSnapshot`, `FutureResumeItem`, serialization structs,
   update `progress_to_result()` to handle `ResolveFutures`, update
   `runMontyAsync` wrapper.

## Current API Surface Comparison

| Capability | Rust Core | JS/WASM (NAPI-RS) |
|---|---|---|
| `start()` — begin iterative execution | Yes | Yes |
| `resume(value)` — resume with sync return | Yes | Yes |
| `resume(error)` — resume with error | Yes | Yes |
| `Snapshot.run(ExternalResult::Future)` — tell VM "this is a future" | **Yes** | **Not exposed** |
| `FutureSnapshot.pending_call_ids()` — get pending call IDs | **Yes** | **Not exposed** |
| `FutureSnapshot.resume(results)` — resolve pending futures | **Yes** | **Not exposed** |
| `RunProgress::ResolveFutures` state | **Yes** | **Panics** |
| Concurrent external calls via `asyncio.gather` | **Yes** | **No** |
