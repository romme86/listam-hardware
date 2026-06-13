// Interop spike server: serves one corestore-7 named core (v11 manifest)
// over raw TCP, exactly the way the listam bridge will.
//
// Usage: node spike-server.mjs [--port 9991] [--storage /tmp/leaf-spike-store]
//        [--append-every 5000] [--count 4]
import { createRequire } from 'node:module'
import net from 'node:net'

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
const port = parseInt(arg('--port', '9991'))
const storage = arg('--storage', '/tmp/leaf-spike-store')
const appendEvery = parseInt(arg('--append-every', '5000'))
const count = parseInt(arg('--count', '4'))

const store = new Corestore(storage)
const core = store.get({ name: 'leaf-spike' })
await core.ready()

if (core.length === 0) {
  const initial = []
  for (let i = 0; i < count; i++) initial.push(b4a.from(`item-${i}`))
  await core.append(initial)
}

console.log(`KEY=${b4a.toString(core.key, 'hex')}`)
console.log(`DKEY=${b4a.toString(core.discoveryKey, 'hex')}`)
console.log(`LENGTH=${core.length}`)

let appended = core.length
if (appendEvery > 0) {
  setInterval(async () => {
    await core.append(b4a.from(`live-item-${appended++}`))
    console.log(`APPENDED length=${core.length}`)
  }, appendEvery)
}

const server = net.createServer((socket) => {
  console.log(`CONNECTION from ${socket.remoteAddress}:${socket.remotePort}`)
  socket.setNoDelay(true)
  const stream = store.replicate(false)
  socket.pipe(stream).pipe(socket)
  stream.on('error', (err) => console.log(`REPL_ERROR ${err.message}`))
  socket.on('error', (err) => console.log(`SOCK_ERROR ${err.message}`))
  socket.on('close', () => console.log('CONNECTION closed'))
})
server.listen(port, () => console.log(`LISTENING on ${port}`))
