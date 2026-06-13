# ESP32-S3 Listam Hardware Bridge

This directory contains a starter kit for flashing an ESP32-S3 board (such as the N16R8) and bridging it to your Listam desktop database using a USB Serial connection.

## Architecture

```text
+-----------------------+                +-------------------------+                +-----------------------+
|   ESP32-S3 Firmware   |  USB Serial    |     Mac Node Bridge     |  P2P Sync /    |  Listam Desktop App   |
| (C++ / Arduino framework) ------------> | (Headless Listam Client)| <------------> |  (Pear Desktop App)   |
|  - Boot status & stats|  "ADD:apples"  | - Reads serial messages |  Hyperdht DHT  |  - Real-time UI lists |
|  - Periodic mock adds |                | - Sends RPC_ADD to list |  Autobase DB   |                       |
+-----------------------+                +-------------------------+                +-----------------------+
```

---

## 1. Firmware Setup (ESP32-S3)

The firmware is located in `firmware/` and is structured as a standard **PlatformIO** project.

### Flashing via VS Code & PlatformIO
1. **Open in VS Code:**
   Open the `hardware/esp32-s3-listam-bridge/firmware/` directory in VS Code (or open the root `listam` workspace).
2. **PlatformIO Setup:**
   Ensure the PlatformIO extension is installed. PlatformIO will automatically read `platformio.ini` and set up the compiler toolchains for Espressif32.
3. **Flash the Board:**
   Connect the ESP32-S3's native USB port to your Mac (it shows up as `/dev/cu.usbmodem1234561`).
4. **Build and Upload:**
   Click the **PlatformIO: Upload** arrow icon in the VS Code status bar (or press `Cmd+Option+U`).
5. **Verify Serial Console:**
   You can verify it works by opening the PlatformIO Serial Monitor (plug icon in VS Code) or running:
   ```sh
   screen /dev/cu.usbmodem1234561 115200
   ```
   *(Exit screen by typing `Ctrl+A` then `Ctrl+\` and confirming).*
   
   On boot, the board will print:
   ```text
   READY:S3
   --- SYSTEM STATS ---
   Total Heap: X bytes
   PSRAM detected: YES
   Total PSRAM: 8388608 bytes
   Flash size: 16777216 bytes
   --------------------
   ```

---

## 2. Mac Node Bridge Setup

The bridge is located in `bridge/` and boots a headless Listam database node using the local `@listam/*` core packages.

### Installing Dependencies
Run the package setup from the `bridge/` directory. This will link to the local packages in the monorepo and compile the native macOS serialport bindings:

```sh
cd bridge
npm install
```

### Joining Your Listam Desktop Database
Listam is a local-first, peer-to-peer app. To have the ESP32-S3 bridge push items to your actual Listam list, the bridge needs to join your list using an invite code.

1. Open your **Listam Desktop App** (running via Pear).
2. Create or copy an **Invite Code / Key** from the UI.
3. Start the bridge with the `--invite` argument to join the database:
   ```sh
   node index.js --invite "YOUR_COPIED_INVITE_CODE_HERE"
   ```
4. The bridge will initialize, connect to the public Hyperdht swarm, join the list, and open the serial port `/dev/cu.usbmodem1234561`.
5. Once joined successfully, you can stop the bridge (`Ctrl+C`). It saves the DB state in `./storage/`, so you can restart it anytime without passing the `--invite` flag:
   ```sh
   node index.js
   ```

### Command Options
*   `--port <path>` : Port name. Defaults to `/dev/cu.usbmodem1234561`.
*   `--baud <rate>` : Baud rate. Defaults to `115200`.
*   `--invite <key>`: Initial invite code to join a list.
*   `--storage <path>`: Directory to persist the Listam database. Defaults to `./storage`.
