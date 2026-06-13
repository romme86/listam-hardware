// Minimal repro: headless A + leaf, check the leaf actually mirrors the
// announced autobase cores (not just the control core).
import { spawn } from 'node:child_process'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, dirname } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const headlessDir = join(here, '../../../listam-headless')
const headlessEntry = join(headlessDir, 'headless.mjs')
const leafBin = join(here, '../target/debug/leaf-host')

const root = mkdtempSync(join(tmpdir(), 'leaf-p2-'))
const dirA = join(root, 'hub-a')

await new Promise((resolve, reject) => {
  const proc = spawn('node', [headlessEntry, 'setup', '--storage', dirA, '--role', 'participant'], { cwd: headlessDir })
  proc.on('exit', (code) => (code === 0 ? resolve() : reject(new Error('setup failed'))))
})

const a = spawn('node', [headlessEntry, 'run', '--storage', dirA], {
  cwd: headlessDir,
  env: { ...process.env, LISTAM_LEAF_BRIDGE_PORT: '9993' },
})
let nextId = 0
const pending = new Map()
let buf = ''
a.stdout.on('data', (chunk) => {
  buf += chunk
  let nl
  while ((nl = buf.indexOf('\n')) !== -1) {
    const line = buf.slice(0, nl); buf = buf.slice(nl + 1)
    try { const m = JSON.parse(line); if (pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id) } } catch {}
  }
})
a.stderr.on('data', (d) => process.stdout.write(`[A] ${d}`))
function req(op, fields = {}) {
  const id = ++nextId
  return new Promise((res) => { pending.set(id, res); a.stdin.write(JSON.stringify({ ...fields, id, op }) + '\n') })
}

const status = await req('status')
console.log('[p2] controlKey:', status.leafBridge?.controlKey)
await req('add', { text: 'hello-leaf-1' })
await req('add', { text: 'hello-leaf-2' })

const leaf = spawn(leafBin, ['--connect', '127.0.0.1:9993', '--key', status.leafBridge.controlKey, '--control', '--status-secs', '4'],
  { env: { ...process.env, RUST_LOG: 'leaf_core=debug,hypercore_protocol=trace' } })
leaf.stderr.on('data', (d) => process.stdout.write(`[leaf] ${d}`))
leaf.stdout.on('data', (d) => process.stdout.write(`[leaf] ${d}`))

await new Promise((r) => setTimeout(r, 25000))
leaf.kill(); a.kill()
rmSync(root, { recursive: true, force: true })
process.exit(0)
