// Interop spike client: a fresh JS corestore peer that syncs a core FROM the
// Rust leaf (which acts as server). Proves the leaf can serve data after the
// original writer is gone (store-and-forward).
//
// Usage: node spike-client.mjs --connect 127.0.0.1:9992 --key <hex>
import { createRequire } from 'node:module'
import net from 'node:net'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const require = createRequire(
  new URL('../../../listam-headless/package.json', import.meta.url)
)
const Corestore = require('corestore')
const b4a = require('b4a')

const args = process.argv.slice(2)
function arg(name, fallback) {
  const i = args.indexOf(name)
  return i === -1 ? fallback : args[i + 1]
}
const [host, port] = arg('--connect', '127.0.0.1:9992').split(':')
const key = b4a.from(arg('--key', ''), 'hex')
if (key.byteLength !== 32) {
  console.error('need --key <64 hex chars>')
  process.exit(1)
}

const store = new Corestore(mkdtempSync(join(tmpdir(), 'leaf-client-')))
const core = store.get({ key })
await core.ready()

const socket = net.connect(parseInt(port), host)
socket.setNoDelay(true)
const stream = store.replicate(true)
socket.pipe(stream).pipe(socket)
stream.on('error', (err) => console.log(`REPL_ERROR ${err.message}`))

await core.update({ wait: true })
console.log(`UPDATED length=${core.length}`)
const timeout = setTimeout(() => {
  console.log('TIMEOUT waiting for blocks')
  process.exit(2)
}, 15000)

for (let i = 0; i < core.length; i++) {
  const value = await core.get(i)
  console.log(`BLOCK ${i}: ${b4a.toString(value)}`)
}
clearTimeout(timeout)
console.log(`DONE blocks=${core.length}`)
process.exit(0)
