import type { ExecutionContext } from 'ava'
import test from 'ava'

import {
  Monty,
  MontySnapshot,
  MontyComplete,
  MontyFutureSnapshot,
  MontyRuntimeError,
  runMontyAsync,
  type FutureResumeItem,
  type ResourceLimits,
} from '../wrapper'

function makePrintCollector(t: ExecutionContext) {
  const output: string[] = []
  const callback = (stream: string, text: string) => {
    t.is(stream, 'stdout')
    output.push(text)
  }
  return { callback, output }
}

// =============================================================================
// resumeAsFuture() basic tests
// =============================================================================

test('resumeAsFuture returns MontyFutureSnapshot with correct pendingCallIds', (t) => {
  const code = `
import asyncio
results = await asyncio.gather(async_call(1), async_call(2))
results
`
  const m = new Monty(code, { externalFunctions: ['async_call'] })

  // Start - first external call
  let progress = m.start()
  t.true(progress instanceof MontySnapshot)
  const snap1 = progress as MontySnapshot
  t.is(snap1.functionName, 'async_call')
  t.deepEqual(snap1.args, [1])
  const callId1 = snap1.callId

  // Resume as future
  progress = snap1.resumeAsFuture()
  t.true(progress instanceof MontySnapshot)
  const snap2 = progress as MontySnapshot
  t.is(snap2.functionName, 'async_call')
  t.deepEqual(snap2.args, [2])
  const callId2 = snap2.callId

  // Resume second as future too
  progress = snap2.resumeAsFuture()
  t.true(progress instanceof MontyFutureSnapshot)
  const futureSnap = progress as MontyFutureSnapshot
  const pendingIds = futureSnap.pendingCallIds
  t.is(pendingIds.length, 2)
  t.true(pendingIds.includes(callId1))
  t.true(pendingIds.includes(callId2))
})

test('callId getter on MontySnapshot', (t) => {
  const m = new Monty('func()', { externalFunctions: ['func'] })
  const progress = m.start()
  t.true(progress instanceof MontySnapshot)
  const snapshot = progress as MontySnapshot
  t.is(typeof snapshot.callId, 'number')
})

// =============================================================================
// FutureSnapshot resume() → MontyComplete
// =============================================================================

test('resume FutureSnapshot with all results returns MontyComplete', (t) => {
  const code = `
import asyncio
results = await asyncio.gather(async_call(10), async_call(20))
results
`
  const m = new Monty(code, { externalFunctions: ['async_call'] })

  // Dispatch both calls as futures
  let progress = m.start()
  const snap1 = progress as MontySnapshot
  const callId1 = snap1.callId
  progress = snap1.resumeAsFuture()

  const snap2 = progress as MontySnapshot
  const callId2 = snap2.callId
  progress = snap2.resumeAsFuture()

  t.true(progress instanceof MontyFutureSnapshot)
  const futureSnap = progress as MontyFutureSnapshot

  // Resolve all futures
  const result = futureSnap.resume([
    { callId: callId1, returnValue: 10 },
    { callId: callId2, returnValue: 20 },
  ])

  t.true(result instanceof MontyComplete)
  t.deepEqual((result as MontyComplete).output, [10, 20])
})

// =============================================================================
// Incremental resolution
// =============================================================================

test('incremental resolution: partial results yield another MontyFutureSnapshot', (t) => {
  const code = `
import asyncio

async def double(x):
    val = await async_call(x)
    return val * 2

results = await asyncio.gather(double(5), async_call(100))
results
`
  const m = new Monty(code, { externalFunctions: ['async_call'] })

  // Dispatch all external calls as futures
  const callIds: number[] = []
  let progress: MontySnapshot | MontyComplete | MontyFutureSnapshot = m.start()

  while (progress instanceof MontySnapshot) {
    callIds.push(progress.callId)
    progress = progress.resumeAsFuture()
  }

  t.true(progress instanceof MontyFutureSnapshot)
  let futureSnap = progress as MontyFutureSnapshot

  // callIds[0] = async_call(100) (gather's direct external call)
  // callIds[1] = async_call(5) (double's inner call)
  // Resolve the gather's direct call first with its correct return value.
  progress = futureSnap.resume([{ callId: callIds[0], returnValue: 100 }])

  // After resolving the gather's direct call, double(5) is still blocked on
  // its own async_call(5). Keep dispatching any new snapshots as futures.
  while (progress instanceof MontySnapshot) {
    callIds.push(progress.callId)
    progress = progress.resumeAsFuture()
  }

  if (progress instanceof MontyFutureSnapshot) {
    futureSnap = progress as MontyFutureSnapshot
    // Resolve all remaining — double's async_call(5) returns 5
    const remaining = futureSnap.pendingCallIds.map((callId) => ({
      callId,
      returnValue: 5,
    }))
    progress = futureSnap.resume(remaining)
  }

  // May need more iterations for nested coroutine
  while (progress instanceof MontySnapshot) {
    progress = progress.resume({ returnValue: progress.args[0] })
  }
  while (progress instanceof MontyFutureSnapshot) {
    const ids = (progress as MontyFutureSnapshot).pendingCallIds
    progress = (progress as MontyFutureSnapshot).resume(ids.map((callId) => ({ callId, returnValue: 5 })))
  }

  t.true(progress instanceof MontyComplete)
  const output = (progress as MontyComplete).output
  t.true(Array.isArray(output))
  t.is(output[0], 10) // double(5) = 5 * 2
  t.is(output[1], 100) // async_call(100) = 100
})

// =============================================================================
// Error handling
// =============================================================================

test('FutureResumeItem with both returnValue and exception errors', (t) => {
  const code = `
import asyncio
results = await asyncio.gather(async_call(1))
results
`
  const m = new Monty(code, { externalFunctions: ['async_call'] })

  let progress = m.start()
  const snap = progress as MontySnapshot
  const callId = snap.callId
  progress = snap.resumeAsFuture()

  t.true(progress instanceof MontyFutureSnapshot)
  const futureSnap = progress as MontyFutureSnapshot

  const error = t.throws(() =>
    futureSnap.resume([
      {
        callId,
        returnValue: 42,
        exception: { type: 'ValueError', message: 'oops' },
      } as unknown as FutureResumeItem,
    ]),
  )
  t.true(error?.message.includes('both'))
})

test('FutureResumeItem with neither returnValue nor exception errors', (t) => {
  const code = `
import asyncio
results = await asyncio.gather(async_call(1))
results
`
  const m = new Monty(code, { externalFunctions: ['async_call'] })

  let progress = m.start()
  const snap = progress as MontySnapshot
  const callId = snap.callId
  progress = snap.resumeAsFuture()

  t.true(progress instanceof MontyFutureSnapshot)
  const futureSnap = progress as MontyFutureSnapshot

  const error = t.throws(() => futureSnap.resume([{ callId } as unknown as FutureResumeItem]))
  t.true(error?.message.includes('neither'))
})

test('exception in one future propagates as MontyRuntimeError', (t) => {
  const code = `
import asyncio
results = await asyncio.gather(async_call(1))
results
`
  const m = new Monty(code, { externalFunctions: ['async_call'] })

  let progress = m.start()
  const snap = progress as MontySnapshot
  const callId = snap.callId
  progress = snap.resumeAsFuture()

  t.true(progress instanceof MontyFutureSnapshot)
  const futureSnap = progress as MontyFutureSnapshot

  const error = t.throws(
    () => futureSnap.resume([{ callId, exception: { type: 'ValueError', message: 'async failure' } }]),
    { instanceOf: MontyRuntimeError },
  )
  t.true(error.message.includes('ValueError'))
  t.true(error.message.includes('async failure'))
})

// =============================================================================
// Serialization
// =============================================================================

test('MontyFutureSnapshot dump/load roundtrip', (t) => {
  const code = `
import asyncio
results = await asyncio.gather(async_call(1), async_call(2))
results
`
  const m = new Monty(code, { externalFunctions: ['async_call'] })

  // Build up the future snapshot
  let progress = m.start()
  const snap1 = progress as MontySnapshot
  const callId1 = snap1.callId
  progress = snap1.resumeAsFuture()

  const snap2 = progress as MontySnapshot
  const callId2 = snap2.callId
  progress = snap2.resumeAsFuture()

  t.true(progress instanceof MontyFutureSnapshot)
  const futureSnap = progress as MontyFutureSnapshot

  // Serialize and deserialize
  const data = futureSnap.dump()
  const restored = MontyFutureSnapshot.load(data)

  t.deepEqual(restored.pendingCallIds.sort(), [callId1, callId2].sort())

  // Resume the restored snapshot
  const result = restored.resume([
    { callId: callId1, returnValue: 'a' },
    { callId: callId2, returnValue: 'b' },
  ])

  t.true(result instanceof MontyComplete)
  t.deepEqual((result as MontyComplete).output, ['a', 'b'])
})

test('dump after resume errors', (t) => {
  const code = `
import asyncio
results = await asyncio.gather(async_call(1))
results
`
  const m = new Monty(code, { externalFunctions: ['async_call'] })

  let progress = m.start()
  const snap = progress as MontySnapshot
  const callId = snap.callId
  progress = snap.resumeAsFuture()

  t.true(progress instanceof MontyFutureSnapshot)
  const futureSnap = progress as MontyFutureSnapshot

  // Resume it
  futureSnap.resume([{ callId, returnValue: 42 }])

  // Dump after resume should fail
  const error = t.throws(() => futureSnap.dump())
  t.true(error?.message.includes('already'))
})

test('MontySnapshot dump/load preserves callId', (t) => {
  const m = new Monty('func(1, 2)', { externalFunctions: ['func'] })
  const progress = m.start()
  t.true(progress instanceof MontySnapshot)
  const snapshot = progress as MontySnapshot
  const originalCallId = snapshot.callId

  // Serialize and deserialize
  const data = snapshot.dump()
  const restored = MontySnapshot.load(data)

  t.is(restored.callId, originalCallId)
  t.is(restored.functionName, 'func')
  t.deepEqual(restored.args, [1, 2])

  // Resume the restored snapshot
  const result = restored.resume({ returnValue: 100 })
  t.true(result instanceof MontyComplete)
  t.is((result as MontyComplete).output, 100)
})

// =============================================================================
// Concurrent resolution via low-level API
// =============================================================================

test('concurrent: two async externals via asyncio.gather', async (t) => {
  const code = `
import asyncio
results = await asyncio.gather(async_call(10), async_call(20))
results
`
  const m = new Monty(code, { externalFunctions: ['async_call'] })

  // Dispatch both calls as futures, tracking promises
  const pendingCalls = new Map<number, Promise<unknown>>()
  let progress: MontySnapshot | MontyComplete | MontyFutureSnapshot = m.start()

  while (progress instanceof MontySnapshot) {
    const snapshot = progress
    const arg = snapshot.args[0] as number
    const callId = snapshot.callId

    // Start the async work
    pendingCalls.set(callId, new Promise((resolve) => setTimeout(() => resolve(arg), 5)))
    // Resume as a pending future
    progress = snapshot.resumeAsFuture()
  }

  t.true(progress instanceof MontyFutureSnapshot)
  const futureSnap = progress as MontyFutureSnapshot

  // Resolve all concurrently
  const results = await Promise.all(
    futureSnap.pendingCallIds.map(async (callId) => ({
      callId,
      returnValue: await pendingCalls.get(callId)!,
    })),
  )

  const final = futureSnap.resume(results)
  t.true(final instanceof MontyComplete)
  t.deepEqual((final as MontyComplete).output, [10, 20])
})

test('concurrent: three async externals', async (t) => {
  const code = `
import asyncio
results = await asyncio.gather(async_call('a'), async_call('b'), async_call('c'))
results
`
  const m = new Monty(code, { externalFunctions: ['async_call'] })

  const pendingCalls = new Map<number, Promise<unknown>>()
  let progress: MontySnapshot | MontyComplete | MontyFutureSnapshot = m.start()

  while (progress instanceof MontySnapshot) {
    const snapshot = progress
    const arg = snapshot.args[0] as string
    const callId = snapshot.callId

    pendingCalls.set(callId, new Promise((resolve) => setTimeout(() => resolve(arg.toUpperCase()), 5)))
    progress = snapshot.resumeAsFuture()
  }

  t.true(progress instanceof MontyFutureSnapshot)
  const futureSnap = progress as MontyFutureSnapshot

  const results = await Promise.all(
    futureSnap.pendingCallIds.map(async (callId) => ({
      callId,
      returnValue: await pendingCalls.get(callId)!,
    })),
  )

  const final = futureSnap.resume(results)
  t.true(final instanceof MontyComplete)
  t.deepEqual((final as MontyComplete).output, ['A', 'B', 'C'])
})

test('concurrent: exception in one future', async (t) => {
  const code = `
import asyncio
results = await asyncio.gather(async_call(1))
results
`
  const m = new Monty(code, { externalFunctions: ['async_call'] })

  let progress: MontySnapshot | MontyComplete | MontyFutureSnapshot = m.start()
  const snap = progress as MontySnapshot
  const callId = snap.callId
  progress = snap.resumeAsFuture()

  t.true(progress instanceof MontyFutureSnapshot)
  const futureSnap = progress as MontyFutureSnapshot

  const error = t.throws(
    () => futureSnap.resume([{ callId, exception: { type: 'ValueError', message: 'async boom' } }]),
    { instanceOf: MontyRuntimeError },
  )
  t.true(error.message.includes('ValueError'))
})

// =============================================================================
// runMontyAsync backwards compatibility
// =============================================================================

test('runMontyAsync: pure sync external backwards compatible', async (t) => {
  const m = new Monty('get_value()', { externalFunctions: ['get_value'] })

  const result = await runMontyAsync(m, {
    externalFunctions: {
      get_value: () => 42,
    },
  })

  t.is(result, 42)
})

test('runMontyAsync: no external functions backwards compatible', async (t) => {
  const m = new Monty('1 + 2 + 3')
  const result = await runMontyAsync(m)
  t.is(result, 6)
})

// =============================================================================
// Getter tests
// =============================================================================

test('MontyFutureSnapshot scriptName getter', (t) => {
  const code = `
import asyncio
results = await asyncio.gather(async_call(1))
results
`
  const m = new Monty(code, { scriptName: 'test_futures.py', externalFunctions: ['async_call'] })

  let progress = m.start()
  progress = (progress as MontySnapshot).resumeAsFuture()

  t.true(progress instanceof MontyFutureSnapshot)
  t.is((progress as MontyFutureSnapshot).scriptName, 'test_futures.py')
})

test('MontyFutureSnapshot pendingCallIds getter', (t) => {
  const code = `
import asyncio
results = await asyncio.gather(async_call(1), async_call(2))
results
`
  const m = new Monty(code, { externalFunctions: ['async_call'] })

  let progress = m.start()
  progress = (progress as MontySnapshot).resumeAsFuture()
  progress = (progress as MontySnapshot).resumeAsFuture()

  t.true(progress instanceof MontyFutureSnapshot)
  const ids = (progress as MontyFutureSnapshot).pendingCallIds
  t.is(ids.length, 2)
  t.true(ids.every((id) => typeof id === 'number'))
})

test('MontyFutureSnapshot repr()', (t) => {
  const code = `
import asyncio
results = await asyncio.gather(async_call(1))
results
`
  const m = new Monty(code, { externalFunctions: ['async_call'] })

  let progress = m.start()
  progress = (progress as MontySnapshot).resumeAsFuture()

  t.true(progress instanceof MontyFutureSnapshot)
  const repr = (progress as MontyFutureSnapshot).repr()
  t.true(repr.includes('MontyFutureSnapshot'))
  t.true(repr.includes('pendingCallIds'))
})

// =============================================================================
// FutureSnapshot resume cannot be called twice
// =============================================================================

test('FutureSnapshot resume cannot be called twice', (t) => {
  const code = `
import asyncio
results = await asyncio.gather(async_call(1))
results
`
  const m = new Monty(code, { externalFunctions: ['async_call'] })

  let progress = m.start()
  const snap = progress as MontySnapshot
  const callId = snap.callId
  progress = snap.resumeAsFuture()

  t.true(progress instanceof MontyFutureSnapshot)
  const futureSnap = progress as MontyFutureSnapshot

  // First resume succeeds
  futureSnap.resume([{ callId, returnValue: 42 }])

  // Second resume should fail
  const error = t.throws(() => futureSnap.resume([{ callId, returnValue: 99 }]))
  t.true(error?.message.includes('already'))
})

// =============================================================================
// EitherFutureSnapshot::Limited branch (resource limits during futures flow)
// =============================================================================

test('futures flow with resource limits exercises Limited branch', (t) => {
  const code = `
import asyncio
results = await asyncio.gather(async_call(1), async_call(2))
results
`
  const m = new Monty(code, { externalFunctions: ['async_call'] })
  const limits: ResourceLimits = { maxInstructions: 100000 }

  // Start with limits — triggers LimitedTracker path
  let progress = m.start({ limits })
  t.true(progress instanceof MontySnapshot)
  const snap1 = progress as MontySnapshot
  const callId1 = snap1.callId

  progress = snap1.resumeAsFuture()
  t.true(progress instanceof MontySnapshot)
  const snap2 = progress as MontySnapshot
  const callId2 = snap2.callId

  progress = snap2.resumeAsFuture()
  t.true(progress instanceof MontyFutureSnapshot)
  const futureSnap = progress as MontyFutureSnapshot
  t.is(futureSnap.pendingCallIds.length, 2)

  // Resolve all futures
  const result = futureSnap.resume([
    { callId: callId1, returnValue: 100 },
    { callId: callId2, returnValue: 200 },
  ])

  t.true(result instanceof MontyComplete)
  t.deepEqual((result as MontyComplete).output, [100, 200])
})

// =============================================================================
// Print callback during futures flow
// =============================================================================

test('print callback captures output during futures flow', (t) => {
  const code = `
import asyncio

async def work(x):
    val = await async_call(x)
    print(f'got {val}')
    return val

results = await asyncio.gather(work(1), work(2))
print(f'done: {results}')
results
`
  const m = new Monty(code, { externalFunctions: ['async_call'] })
  const { output, callback } = makePrintCollector(t)

  // Start with printCallback
  let progress = m.start({ printCallback: callback })
  t.true(progress instanceof MontySnapshot)
  const snap1 = progress as MontySnapshot
  const callId1 = snap1.callId

  progress = snap1.resumeAsFuture()
  t.true(progress instanceof MontySnapshot)
  const snap2 = progress as MontySnapshot
  const callId2 = snap2.callId

  progress = snap2.resumeAsFuture()
  t.true(progress instanceof MontyFutureSnapshot)
  const futureSnap = progress as MontyFutureSnapshot

  // Resolve all futures
  const result = futureSnap.resume([
    { callId: callId1, returnValue: 10 },
    { callId: callId2, returnValue: 20 },
  ])

  t.true(result instanceof MontyComplete)
  t.deepEqual((result as MontyComplete).output, [10, 20])

  // Verify print output was captured
  const joined = output.join('')
  t.true(joined.includes('got 10'))
  t.true(joined.includes('got 20'))
  t.true(joined.includes('done: [10, 20]'))
})
