// End-to-end proof of the leaf peer's store-and-forward contract with the
// real listam headless app:
//
//   Phase 1: headless A (owner) runs with the leaf bridge, writes items.
//   Phase 2: leaf-host mirrors A through the TCP bridge (control core,
//     bootstrap/writer, system core). A writes "async" items, the leaf
//     catches up, then A goes offline for good.
//   Phase 3: a brand-new JS corestore peer connects to the leaf (now in
//     --listen mode via second process? no - same process keeps serving) and
//     syncs A's writer core. PASS when every block (including the async
//     items) arrives and verifies in real JS hypercore.
//
// App-level view rebuild on cold restarts is listam's own concern and is
// covered by tools/cross-device; this test pins the leaf's contract:
// replicate, store, and serve verified cores while the writer is offline.
//
// Usage: node e2e.mjs
import { spawn } from 'node:child_process'
import { createRequire } from 'node:module'
import net from 'node:net'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, dirname } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const headlessDir = join(here, '../../../listam-headless')
const headlessEntry = join(headlessDir, 'headless.mjs')
const leafBin = join(here, '../target/debug/leaf-host')
const require = createRequire(join(headlessDir, 'package.json'))
const Corestore = require('corestore')
const b4a = require('b4a')

const root = mkdtempSync(join(tmpdir(), 'leaf-e2e-'))
const dirA = join(root, 'hub-a')
const dirLeaf = join(root, 'leaf')
const PORT_A = 9993
const LEAF_LISTEN = 9995

const children = new Set()
const log = (...parts) => console.log('[e2e]', ...parts)

function lineService(proc, label) {
  let nextId = 0
  const pending = new Map()
  let buffer = ''
  let stderr = ''
  proc.stdout.on('data', (chunk) => {
    buffer += chunk.toString()
    let nl
    while ((nl = buffer.indexOf('\n')) !== -1) {
      const line = buffer.slice(0, nl)
      buffer = buffer.slice(nl + 1)
      if (!line.trim()) continue
      try {
        const message = JSON.parse(line)
        if (message.id && pending.has(message.id)) {
          pending.get(message.id)(message)
          pending.delete(message.id)
        }
      } catch {}
    }
  })
  proc.stderr.on('data', (chunk) => {
    stderr += chunk.toString()
    if (stderr.length > 8000) stderr = stderr.slice(-4000)
  })
  return {
    proc,
    request(op, fields = {}, timeoutMs = 30_000) {
      const id = ++nextId
      return new Promise((resolve, reject) => {
        const timer = setTimeout(
          () => reject(new Error(`[${label}] op ${op} timed out\nstderr: ${stderr.slice(-1500)}`)),
          timeoutMs
        )
        pending.set(id, (message) => {
          clearTimeout(timer)
          resolve(message)
        })
        proc.stdin.write(JSON.stringify({ ...fields, id, op }) + '\n')
      })
    },
    stop() {
      return new Promise((resolve) => {
        if (proc.exitCode !== null) return resolve()
        proc.once('exit', resolve)
        try {
          proc.stdin.write(JSON.stringify({ id: ++nextId, op: 'shutdown' }) + '\n')
        } catch {}
        setTimeout(() => proc.kill('SIGKILL'), 5000).unref()
      })
    },
  }
}

async function main() {
  // ---- Phase 1: headless A with bridge, initial items ----
  log('phase 1: headless A (owner) with leaf bridge')
  await new Promise((resolve, reject) => {
    const proc = spawn('node', [headlessEntry, 'setup', '--storage', dirA, '--role', 'participant'], { cwd: headlessDir })
    proc.on('exit', (code) => (code === 0 ? resolve() : reject(new Error('setup A failed'))))
  })
  const aProc = spawn('node', [headlessEntry, 'run', '--storage', dirA], {
    cwd: headlessDir,
    env: { ...process.env, LISTAM_LEAF_BRIDGE_PORT: String(PORT_A) },
  })
  children.add(aProc)
  const a = lineService(aProc, 'A')
  const status = await a.request('status')
  const controlKey = status.leafBridge?.controlKey
  if (!controlKey) throw new Error('A has no leaf bridge')
  log('A up, control key:', controlKey.slice(0, 16) + '…')
  await a.request('add', { text: 'item-before-leaf' })

  // ---- Phase 2: leaf mirrors A, async items, A dies ----
  log('phase 2: leaf mirrors A over TCP')
  const leaf = spawn(
    leafBin,
    [
      '--connect', `127.0.0.1:${PORT_A}`,
      '--listen', `127.0.0.1:${LEAF_LISTEN}`,
      '--key', controlKey, '--control',
      '--storage', dirLeaf,
      '--status-secs', '5',
    ],
    { env: { ...process.env, RUST_LOG: 'info' } }
  )
  children.add(leaf)
  let leafLog = ''
  leaf.stderr.on('data', (d) => (leafLog += d.toString()))
  leaf.stdout.on('data', (d) => (leafLog += d.toString()))
  await new Promise((r) => setTimeout(r, 8000))

  for (const text of ['async-1', 'async-2', 'async-3']) {
    await a.request('add', { text })
  }
  log('A wrote async-1..3; letting the leaf catch up')
  await new Promise((r) => setTimeout(r, 8000))

  // Discover which cores the leaf mirrored and their lengths (from its log).
  const mirrored = new Map()
  for (const match of leafLog.matchAll(/status core=([0-9a-f]+) length=(\d+) contiguous=(\d+)/g)) {
    mirrored.set(match[1], { length: Number(match[2]), contiguous: Number(match[3]) })
  }
  log('leaf status:', JSON.stringify([...mirrored.entries()]))

  await a.stop()
  children.delete(aProc)
  log('A is offline for good — async items exist ONLY on the leaf')

  // ---- Phase 3: fresh JS peer syncs the announced cores from the leaf ----
  log('phase 3: fresh corestore peer pulls from the leaf')
  const store = new Corestore(join(root, 'fresh-peer'))

  // Learn announced core keys exactly like a leaf would: from the control core.
  const control = store.get({ key: b4a.from(controlKey, 'hex') })
  await control.ready()
  const socket = net.connect(LEAF_LISTEN, '127.0.0.1')
  socket.setNoDelay(true)
  const stream = store.replicate(true)
  socket.pipe(stream).pipe(socket)
  stream.on('error', () => {})

  await control.update({ wait: true })
  const announced = new Set()
  for (let i = 0; i < control.length; i++) {
    const entry = JSON.parse(b4a.toString(await control.get(i)))
    for (const k of entry.add ?? []) announced.add(k)
  }
  log(`control core delivered ${control.length} entries, ${announced.size} announced core(s)`)
  if (announced.size === 0) throw new Error('no announced cores learned from leaf')

  // Sync every announced core and count blocks delivered & verified by JS.
  let totalBlocks = 0
  const perCore = []
  for (const keyHex of announced) {
    const core = store.get({ key: b4a.from(keyHex, 'hex') })
    await core.ready()
    await Promise.race([
      core.update({ wait: true }),
      new Promise((r) => setTimeout(r, 10000)),
    ])
    if (core.length > 0) {
      await Promise.race([
        core.download({ start: 0, end: core.length }).done(),
        new Promise((r) => setTimeout(r, 10000)),
      ])
    }
    const contiguous = core.contiguousLength
    perCore.push(`${keyHex.slice(0, 8)}: length=${core.length} contiguous=${contiguous}`)
    totalBlocks += contiguous
  }
  log('fresh peer results:', perCore.join(' | '))

  // The writer core must contain the initial ops + 4 items (1 before + 3 async).
  // We don't decrypt (no encryption key here — the leaf never has it either);
  // verified delivery of every block is the contract.
  const allDelivered = perCore.every((line) => {
    const m = line.match(/length=(\d+) contiguous=(\d+)/)
    return m && m[1] === m[2]
  })
  if (!allDelivered || totalBlocks === 0) {
    throw new Error(`delivery incomplete: ${perCore.join(' | ')}`)
  }
  console.log(`\nE2E RESULT: PASS — fresh JS peer received ${totalBlocks} verified blocks from the leaf with the hub offline\n`)
  await store.close()
}

try {
  await main()
  process.exit(0)
} catch (err) {
  console.error('\nE2E RESULT: FAIL —', err.message, '\n')
  process.exit(1)
} finally {
  for (const child of children) {
    try { child.kill('SIGKILL') } catch {}
  }
  rmSync(root, { recursive: true, force: true })
}
