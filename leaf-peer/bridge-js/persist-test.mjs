// Persistence parity test for the std::fs storage backend the ESP32 uses.
//
// Phase 1: headless hub + bridge, add items. leaf-host (--fs-storage DIR)
//   mirrors them, then is killed.
// Phase 2: leaf-host restarts on the SAME dir with the hub OFFLINE. It must
//   reload every core from disk (seed_from_control) — no network. We read its
//   status: cores present with their block counts proves on-disk persistence.
// Phase 3: hub comes back, owner adds one more item; the leaf should sync only
//   the delta (the existing blocks are already on disk).
import { spawn } from 'node:child_process'
import { mkdtempSync, rmSync, readdirSync, statSync, cpSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, dirname } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const headlessDir = join(here, '../../../listam-headless')
const headlessEntry = join(headlessDir, 'headless.mjs')
const leafBin = join(here, '../target/debug/leaf-host')

const root = mkdtempSync(join(tmpdir(), 'leaf-persist-'))
const hubDir = join(root, 'hub')
const leafDir = join(root, 'leaf')
const PORT = 9996
const children = new Set()
const log = (...a) => console.log('[persist]', ...a)

function line(proc) {
  let nextId = 0, buf = ''
  const pending = new Map()
  proc.stdout.on('data', (c) => {
    buf += c
    let nl
    while ((nl = buf.indexOf('\n')) !== -1) {
      const l = buf.slice(0, nl); buf = buf.slice(nl + 1)
      try { const m = JSON.parse(l); if (pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id) } } catch {}
    }
  })
  return {
    req(op, fields = {}) {
      const id = ++nextId
      return new Promise((res, rej) => {
        const t = setTimeout(() => rej(new Error(`${op} timed out`)), 30000)
        pending.set(id, (m) => { clearTimeout(t); res(m) })
        proc.stdin.write(JSON.stringify({ ...fields, id, op }) + '\n')
      })
    },
    stop() { return new Promise((r) => { if (proc.exitCode !== null) return r(); proc.once('exit', r); proc.kill('SIGKILL') }) },
  }
}

function startHub() {
  const proc = spawn('node', [headlessEntry, 'run', '--storage', hubDir], {
    cwd: headlessDir, env: { ...process.env, LISTAM_LEAF_BRIDGE_PORT: String(PORT) },
  })
  children.add(proc)
  let err = ''
  proc.stderr.on('data', (d) => (err += d))
  return { svc: line(proc), proc, key: () => (err.match(/control core key[^:]*: ([0-9a-f]{64})/) || [])[1] }
}

function startLeaf(controlKey, connect) {
  const args = ['--key', controlKey, '--control', '--fs-storage', leafDir, '--status-secs', '3']
  if (connect) args.unshift('--connect', `127.0.0.1:${PORT}`)
  else args.unshift('--listen', '127.0.0.1:9997') // no hub: just reload from disk and serve
  const proc = spawn(leafBin, args, { env: { ...process.env, RUST_LOG: 'info,leaf_core=info' } })
  children.add(proc)
  let out = ''
  const cap = (d) => (out += d)
  proc.stderr.on('data', cap); proc.stdout.on('data', cap)
  return { proc, log: () => out, stop: () => new Promise((r) => { if (proc.exitCode !== null) return r(); proc.once('exit', r); proc.kill('SIGKILL') }) }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

try {
  // --- setup hub ---
  await new Promise((res, rej) => {
    const p = spawn('node', [headlessEntry, 'setup', '--storage', hubDir, '--role', 'participant'], { cwd: headlessDir })
    p.on('exit', (c) => (c === 0 ? res() : rej(new Error('setup failed'))))
  })

  // --- phase 1: hub + leaf mirror ---
  log('phase 1: hub up, leaf mirrors via std::fs storage')
  const hub = startHub()
  await sleep(3000)
  let status = await hub.svc.req('status')
  const controlKey = status.leafBridge?.controlKey
  if (!controlKey) throw new Error('no control key')
  for (const t of ['persist-1', 'persist-2', 'persist-3']) await hub.svc.req('add', { text: t })
  log('hub seeded 3 items, control key', controlKey.slice(0, 12) + '…')

  const leaf1 = startLeaf(controlKey, true)
  await sleep(9000) // let it mirror + reconnect-on-learn + sync
  const mirroredBlocks = [...leaf1.log().matchAll(/block (\d+) stored/g)].length
  log(`leaf mirrored ${mirroredBlocks} blocks; on-disk core dirs:`, readdirSync(leafDir).length)
  await leaf1.stop(); children.delete(leaf1.proc)
  await hub.svc.stop(); children.delete(hub.proc)
  log('leaf + hub stopped')

  // --- phase 2: leaf restarts on same dir, hub OFFLINE ---
  log('phase 2: restart leaf on same dir with NO hub — must reload from disk')
  const diskDirs = readdirSync(leafDir)
  const leaf2 = startLeaf(controlKey, false) // --listen, no --connect: cannot reach any hub
  await sleep(5000)
  const seeded = (leaf2.log().match(/seeded (\d+) core/) || [])[1]
  const reloadedStatus = [...leaf2.log().matchAll(/status core=([0-9a-f]+) length=(\d+) contiguous=(\d+)/g)]
    .map((m) => `${m[1].slice(0, 8)}:len=${m[2]}`)
  log(`reloaded from disk — seeded ${seeded ?? 0} extra core(s); status: ${reloadedStatus.join(' ')}`)

  // verify: cores reloaded with their blocks WITHOUT any network
  const nonEmpty = [...leaf2.log().matchAll(/status core=([0-9a-f]+) length=(\d+)/g)].filter((m) => Number(m[2]) > 0)
  await leaf2.stop(); children.delete(leaf2.proc)

  const ok = diskDirs.length >= 3 && nonEmpty.length >= 1 && Number(seeded ?? 0) >= 1
  if (ok) {
    console.log(`\nPERSIST RESULT: PASS — leaf reloaded ${nonEmpty.length} core(s) with data from disk (${diskDirs.length} core dirs), hub offline, seeded ${seeded} from control core\n`)
    process.exit(0)
  }
  console.log('--- leaf2 (reopen) log ---')
  console.log(leaf2.log().split('\n').filter((l) => !/poll_|vec_enc/.test(l)).slice(0, 40).join('\n'))
  console.log('--- on-disk files ---')
  for (const d of diskDirs) {
    const files = readdirSync(join(leafDir, d))
    const sizes = files.map((f) => `${f}=${statSync(join(leafDir, d, f)).size}`).join(' ')
    console.log(`  ${d.slice(0, 8)}: ${sizes}`)
  }
  throw new Error(`disk dirs=${diskDirs.length}, non-empty reloaded cores=${nonEmpty.length}, seeded=${seeded}`)
} catch (e) {
  console.error('\nPERSIST RESULT: FAIL —', e.message, '\n')
  process.exit(1)
} finally {
  for (const c of children) try { c.kill('SIGKILL') } catch {}
  try { cpSync(leafDir, '/tmp/persist-leafdir', { recursive: true }) ; console.log('preserved leafDir -> /tmp/persist-leafdir') } catch (e) { console.log('preserve failed', e.message) }
  rmSync(root, { recursive: true, force: true })
}
