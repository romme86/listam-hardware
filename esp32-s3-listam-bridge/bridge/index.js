import process from 'node:process'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { startBackend, createNodePlatform } from '@listam/backend'
import { createBackendChannel } from '@listam/client'
import { RPC_ADD, RPC_JOIN_KEY } from '@listam/protocol'
import { SerialPort } from 'serialport'
import { ReadlineParser } from '@serialport/parser-readline'

// Setup path helpers
const __dirname = path.dirname(fileURLToPath(import.meta.url))

// Parse command line arguments
const args = {}
for (let i = 2; i < process.argv.length; i++) {
    const arg = process.argv[i]
    if (arg.startsWith('--')) {
        const key = arg.slice(2)
        const val = process.argv[i + 1]
        if (val && !val.startsWith('--')) {
            args[key] = val
            i++
        } else {
            args[key] = true
        }
    }
}

const PORT = args.port || '/dev/cu.usbmodem1234561'
const BAUD = parseInt(args.baud || '115200', 10)
const INVITE = args.invite || null
const STORAGE = path.resolve(__dirname, args.storage || './storage')

console.log('--- STARTING LISTAM HARDWARE BRIDGE ---')
console.log(`Serial Port:        ${PORT}`)
console.log(`Baud Rate:          ${BAUD}`)
console.log(`Storage Directory:  ${STORAGE}`)
if (INVITE) {
    console.log(`Invite Code:        [Provided]`)
}
console.log('---------------------------------------')

// 1. Initialize Listam client/backend channel
const channel = createBackendChannel()
const state = {
    items: [],
    joined: false,
    peerCount: 0
}

channel.client.onEvent((event) => {
    if (event.type === 'persist-secret') {
        // Ack that secret is handled in-memory for this bridge session
        event.reply(JSON.stringify({ stored: false, mode: 'bridge-memory' }))
        return
    }

    if (event.type === 'sync-list') {
        state.items = Array.isArray(event.items) ? event.items : []
        console.log(`[Listam] Synced! List has ${state.items.length} items.`)
    }

    if (event.type === 'add-from-backend') {
        state.items = [event.item, ...state.items.filter((i) => i.id !== event.item.id)]
        console.log(`[Listam] Item added: "${event.item.text}" (ID: ${event.item.id})`)
    }

    if (event.type === 'update-from-backend') {
        state.items = state.items.map((i) => (i.id === event.item.id ? event.item : i))
        console.log(`[Listam] Item updated: "${event.item.text}" (Done: ${!!event.item.isDone})`)
    }

    if (event.type === 'delete-from-backend') {
        state.items = state.items.filter((i) => i.id !== event.item.id)
        console.log(`[Listam] Item deleted: ID ${event.item.id}`)
    }

    if (event.type === 'message') {
        const payload = event.payload
        if (payload?.type === 'peer-count') {
            const count = payload.count ?? 0
            if (count !== state.peerCount) {
                state.peerCount = count
                console.log(`[Listam] Connected peers: ${state.peerCount}`)
            }
        }
        if (payload?.type === 'join-success') {
            state.joined = true
            console.log('[Listam] Successfully joined list!')
        }
        if (payload?.type === 'join-error') {
            console.error('[Listam] Join error:', payload.message)
        }
    }
})

// 2. Boot the Listam Backend
console.log('[Listam] Booting backend...')
const platform = createNodePlatform({
    argv: [STORAGE, '', '', ''],
    storageNamespace: 'hardware-bridge'
})
platform.createRpc = channel.platform.createRpc

const backend = await startBackend(platform)
console.log('[Listam] Backend online.')

// 3. Handle Invite if passed
if (INVITE) {
    console.log('[Listam] Joining list with invite key...')
    try {
        await channel.client.send(RPC_JOIN_KEY, { key: INVITE })
    } catch (err) {
        console.error('[Listam] Failed to send join RPC:', err.message)
    }
}

// 4. Open Serial Port to ESP32-S3
console.log(`[Serial] Connecting to ${PORT}...`)
const port = new SerialPort({
    path: PORT,
    baudRate: BAUD,
    autoOpen: false
})

const parser = port.pipe(new ReadlineParser({ delimiter: '\r\n' }))

parser.on('data', async (line) => {
    line = line.trim()
    if (!line) return

    console.log(`[Serial RX] ${line}`)

    // Check for ADD command
    if (line.startsWith('ADD:')) {
        const itemText = line.substring(4).trim()
        if (itemText) {
            console.log(`[Bridge] Dispatched add request for: "${itemText}"`)
            try {
                await channel.client.send(RPC_ADD, { text: itemText })
                console.log(`[Bridge] RPC_ADD succeeded for: "${itemText}"`)
            } catch (err) {
                console.error(`[Bridge] RPC_ADD failed:`, err.message)
            }
        }
    }
})

port.open((err) => {
    if (err) {
        console.error(`[Serial] Failed to open port: ${err.message}`)
        console.error('Please make sure the ESP32-S3 is connected and port path is correct.')
    } else {
        console.log(`[Serial] Connected! Reading data...`)
    }
})

port.on('close', () => {
    console.log('[Serial] Connection closed.')
})

port.on('error', (err) => {
    console.error('[Serial] Error:', err.message)
})

// 5. Clean Shutdown
async function shutdown() {
    console.log('\n[Bridge] Shutting down...')
    
    if (port.isOpen) {
        port.close()
    }
    
    try {
        await backend.shutdown()
        console.log('[Listam] Backend shut down.')
    } catch (err) {
        console.error('[Listam] Error during backend shutdown:', err.message)
    }
    
    process.exit(0)
}

process.on('SIGINT', shutdown)
process.on('SIGTERM', shutdown)
