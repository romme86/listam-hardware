# Plan: wiring the leaf bridge into the mobile app

Status: **plan only** (not implemented). Grounded in the current mobile app
(Expo RN + BareKit worklet) as of 2026-06.

## TL;DR recommendation

**Don't make the phone a hub for normal use.** The bridge turns an app into a
TCP *server* that the board dials. A phone is the worst host for that: it's
NAT'd, its IP changes, the OS kills background work, and it's battery- and
data-constrained. For mobile to benefit from the board, run an **always-on
headless hub** (Pi / VM / Mac) — the board mirrors through it, and the phone
syncs the project over Hyperswarm as it already does. Mobile needs *no* bridge
for this; it already works.

Wire mobile-as-bridge **only** for one real niche: **direct phone ↔ board sync
on a local network with no hub and no internet** (e.g. two devices in the
field). Everything below is for that case, and it's mostly about the OS
background-execution problem, not the listam code.

## What's easy: the wiring (≈ the desktop change)

The bridge is already transport-pluggable (`lib/leaf-bridge.mjs` takes an
injected `tcp` module). The mobile worklet is Bare, so it injects `bare-tcp`
exactly like the desktop worker does.

1. **Dependency** — add `"bare-tcp": "^2.5.1"` to `listam-mobile/package.json`.
   `bare-pack --linked` (the existing `bundle:backend:ios/android` scripts)
   traces and bundles it automatically; prebuilds ship per-arch like the other
   `bare-*` addons. `npm install` + rebundle.

2. **Worklet entry** — `listam-mobile/backend/backend.mjs` (today 4 lines):
   ```js
   import { startBackend } from '@listam/backend/backend'
   import { createBareKitPlatform } from '@listam/backend/platform/bare-kit'
   const platform = createBareKitPlatform({ Bare, BareKit })
   await startBackend(platform)

   // new: opt-in leaf bridge
   const port = Number(platform.argv?.[4] ?? Bare?.env?.LISTAM_LEAF_BRIDGE_PORT ?? 0)
   if (Number.isInteger(port) && port > 0 && port < 65536) {
     try {
       const tcp = (await import('bare-tcp')).default ?? (await import('bare-tcp'))
       const { startLeafBridge } = await import('@listam/backend/lib/leaf-bridge.mjs')
       await startLeafBridge({ port, logger: console, tcp })
     } catch (err) {
       console.error('[leaf-bridge] failed to start:', err?.message ?? err)
     }
   }
   ```
   This is the whole code change. The bridge reads `store`/`autobase` from
   `state.mjs` after `startBackend`, same as headless/desktop.

3. **Config surface** — the worklet is booted once with an `argv` array in
   `app/hooks/_useWorklet.ts` (slots 0–3 used; **slot 4 is free**). Two paths:
   - **Dev / quick test:** hardcode or read an env value, append as `argv[4]`.
   - **Product:** a Settings toggle "Act as a local leaf hub (advanced)" →
     store in `expo-secure-store` → pass as `argv[4]` at boot. Since the
     worklet only boots once, enabling/disabling means a worklet restart; or
     add an `RPC_SET_LEAF_BRIDGE { port }` command so it can start/stop live
     (mirrors the existing RN↔worklet RPC channel).

That's it for listam code. None of it is hard.

## What's hard: the OS, not the code

### Background execution (the actual blocker)

The BareKit worklet runs inside the app process. **When the app backgrounds,
the OS suspends/kills it — the TCP server dies with it.** So out of the box the
mobile bridge only works *while the app is open and foregrounded*. For a "leaf"
(whose entire value is being *always available*) that's a contradiction.

To keep a server alive in the background:

- **Android** — a **foreground service** with a persistent notification
  (`FOREGROUND_SERVICE` + `FOREGROUND_SERVICE_DATA_SYNC` on Android 14+,
  `WAKE_LOCK`). Doable but: a permanent notification, real battery drain, and
  Expo-managed needs a config plugin or a dev-client/bare-workflow to add the
  native service. Android 15 tightens data-sync service runtime caps further.
- **iOS** — there is **no supported way** to run a long-lived TCP listener in
  the background. `UIBackgroundModes` are task-specific (audio/voip/location/
  processing); abusing `voip` for a socket server is the classic hack and a
  near-certain App Store rejection. Realistically iOS mobile-as-hub is
  **foreground-only**.

**Conclusion:** persistent mobile-as-hub is an Android-only, foreground-service
project with meaningful battery/UX cost; on iOS it's foreground-only. This is
why the always-on headless hub is the recommended topology.

### NAT / changing IP / discovery

Even foregrounded, the board has to *find* the phone. The board dials a
`host:port`; a phone's LAN IP changes (DHCP, network switches) and it's often
behind carrier NAT off-LAN. Static `hub_addr` won't hold. Options if we pursue
this: mDNS/`_listam-leaf._tcp` discovery on the LAN (board scans instead of
using a fixed IP), or the inverse direction below.

### Better direction for the offline-LAN niche: board listens, phone dials

Instead of phone-as-server, flip it: the **board listens** and the **phone
dials the board**. This sidesteps "phones can't run background servers" — the
phone only needs an *outbound* connection while foregrounded, which any app can
do.

- `leaf-host` already has `--listen`; the **ESP firmware would need a listen
  mode added** (it's dial-only today — a known follow-up).
- The phone side then needs a small *client* of the leaf protocol (dial the
  board, `store.replicate(socket)` over a `bare-tcp` *connection*, not a
  server). Same `bare-tcp` dep, `tcp.createConnection` instead of
  `createServer`.
- Board is reachable at its own LAN IP (it prints it); phone discovers via mDNS
  or a typed-in address.

This is the more natural shape for field sync and avoids the entire background
-server problem. It does require (a) ESP listen mode and (b) a phone-side leaf
client, so it's a larger build than the foreground server.

## Recommended phasing

- **Phase 0 — decide it's needed.** For almost everything, the headless hub
  covers mobile. Only build mobile bridging for genuine no-hub/offline LAN
  sync. If that's not a real requirement, **stop here**.
- **Phase 1 — foreground-only dev wiring** (small): the 3 steps above, env/
  argv-gated, off by default. Verify on Android + iOS while foregrounded with
  `leaf-host` dialing the phone's LAN IP. Good enough to demo LAN sync.
- **Phase 2 — Android foreground service** (heavy, Android-only): config
  plugin / dev-client, persistent notification, battery review. iOS stays
  foreground-only.
- **Phase 3 — discovery + UX**: mDNS so the board finds the phone without a
  typed IP; a Settings toggle + provisioning (show/scan the control key);
  clear "this runs while the app is open" messaging.
- **Alt track (recommended if the niche matters)**: instead of Phases 2–3, add
  **ESP listen mode + a phone-side leaf client** (board listens, phone dials).
  Cleaner lifecycle, no background server.

## Effort estimate

- Phase 1 (foreground dev wiring): ~half a day, mirrors the desktop change.
- Phase 2 (Android FG service): 2–4 days incl. native config + battery testing;
  iOS not viable.
- Alt track (ESP listen + phone client + mDNS): ~1 week, but the *right* shape.

## What's already reusable

- `lib/leaf-bridge.mjs` — transport-agnostic, no change needed.
- `bare-tcp` injection pattern — proven under Bare (desktop bare-runtime test).
- The control-core provisioning model — identical on mobile.
