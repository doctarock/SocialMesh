package org.socialmesh.android

import android.annotation.SuppressLint
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothServerSocket
import android.bluetooth.le.AdvertiseCallback
import android.bluetooth.le.AdvertiseData
import android.bluetooth.le.AdvertiseSettings
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanFilter
import android.bluetooth.le.ScanResult
import android.bluetooth.le.ScanSettings
import android.content.Context
import android.os.ParcelUuid
import android.util.Log
import java.io.IOException
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executors
import java.util.concurrent.RejectedExecutionException
import java.util.concurrent.atomic.AtomicLong

/**
 * The Android Bluetooth driver for the node (SPEC.md §9).
 *
 * It does only radio work: it takes commands from the node (advertise,
 * scan, listen, connect, send, close) and reports what happens. Adverts,
 * when to connect, encryption and address rotation are decided in Rust.
 *
 * Links are insecure L2CAP connection-oriented channels when the peer
 * advertises a PSM, and the GATT pipe ([GattPipeServer], [GattClientLink])
 * when it doesn't. There is no pairing, because the node's own encrypted
 * channel runs over every link. Accepting offers both.
 *
 * Needs BLUETOOTH_SCAN, BLUETOOTH_ADVERTISE and BLUETOOTH_CONNECT (Android 12+)
 * or location permission (Android 10-11), granted before [start].
 */
@SuppressLint("MissingPermission")
class BleDriver(context: Context, private val node: Node) {
    /** The node end of the driver ([Mesh] in the app). */
    interface Node {
        /** Next encoded command, or null after [timeoutMs]. */
        fun nextRadioCommand(timeoutMs: Int): ByteArray?

        /** Hand over an encoded event; blocks while the node catches up. */
        fun radioEvent(event: ByteArray): Boolean
    }

    private val context: Context = context.applicationContext
    private val manager: BluetoothManager = this.context.getSystemService(BluetoothManager::class.java)
    private val adapter: BluetoothAdapter = manager.adapter
    private val serviceUuid = ParcelUuid(BleCodec.SERVICE_UUID)

    private val links = ConcurrentHashMap<Long, Link>()
    private val nextHostLink = AtomicLong(1)
    /** Devices from scan results: they carry the address type a bare address string lacks. */
    private val seen = ConcurrentHashMap<String, BluetoothDevice>()
    /** Scan callbacks arrive on the main thread; reporting can block, so it happens here. */
    private val reporter = Executors.newSingleThreadExecutor()
    /** GATT callbacks arrive on binder threads that mustn't block: their events queue here, in order. */
    private val gattEvents = Executors.newSingleThreadExecutor()

    @Volatile private var running = false
    @Volatile private var l2capServer: BluetoothServerSocket? = null
    @Volatile private var gattServer: GattPipeServer? = null
    private var listening = false
    private var advertising: AdvertiseCallback? = null
    /** The current advert's service data; the pipe also serves it for reading. */
    @Volatile internal var advertData: ByteArray? = null
    private var scanning: ScanCallback? = null
    private var commandThread: Thread? = null
    /** What the node asked for, so it can be picked up again when Bluetooth comes back on. */
    private var wantListen = false
    private var wantScan = false
    /** Bluetooth is on (as far as we know): radio work is attempted. */
    @Volatile private var radioOn = true

    /** Start taking commands from the node. [on]: Bluetooth is on now. */
    fun start(on: Boolean) {
        radioOn = on
        running = true
        commandThread = Thread(::commandLoop, "mesh-ble-commands").also { it.start() }
    }

    /** Stop everything. Call before stopping the node. */
    fun stop() {
        running = false
        commandThread?.join(2000)
        stopAdvertising()
        stopScan()
        try {
            l2capServer?.close()
        } catch (e: IOException) {
        }
        gattServer?.close()
        for (l in links.values) l.close("stopped")
        reporter.shutdownNow()
        gattEvents.shutdownNow()
    }

    /**
     * Bluetooth was turned off: drop links and servers, but remember what the
     * node asked for. Commands keep arriving and are remembered too.
     */
    @Synchronized
    fun radioOff() {
        if (!radioOn) return
        radioOn = false
        advertising = null
        scanning = null // the stack forgets both when the radio goes off
        try {
            l2capServer?.close()
        } catch (e: IOException) {
        }
        l2capServer = null
        gattServer?.close()
        gattServer = null
        listening = false
        for (l in links.values) l.close("Bluetooth off")
    }

    /** Bluetooth is back on: listen, scan and advertise as the node last asked. */
    @Synchronized
    fun radioOn() {
        if (radioOn || !running) return
        radioOn = true
        try {
            if (wantListen) listen()
            if (wantScan) startScan()
            advertData?.let { startAdvertising(it) }
        } catch (e: SecurityException) {
            Log.e(TAG, "Bluetooth permission missing", e)
        } catch (e: Exception) {
            Log.w(TAG, "resuming failed", e)
        }
    }

    // --- used by links ---

    /** Hand an event to the node now (blocking). */
    internal fun event(bytes: ByteArray) {
        if (running && !node.radioEvent(bytes)) Log.w(TAG, "node refused an event")
    }

    /** Hand an event to the node from a thread that mustn't block, keeping order. */
    internal fun post(bytes: ByteArray) {
        try {
            gattEvents.execute { event(bytes) }
        } catch (e: RejectedExecutionException) {
            // stopping
        }
    }

    internal fun newHostLink(): Long = BleCodec.HOST_LINK or nextHostLink.getAndIncrement()

    internal fun adopt(link: Link) {
        links[link.id] = link
    }

    internal fun forget(link: Link) {
        links.remove(link.id, link)
    }

    // --- commands ---

    private fun commandLoop() {
        while (running) {
            val bytes = node.nextRadioCommand(250) ?: continue
            val cmd = BleCodec.decodeCommand(bytes) ?: continue
            try {
                // Sending can block (flow control), so it stays outside the lock.
                if (cmd is BleCodec.Command.Send) execute(cmd) else synchronized(this) { execute(cmd) }
            } catch (e: SecurityException) {
                Log.e(TAG, "Bluetooth permission missing", e)
            } catch (e: Exception) {
                Log.w(TAG, "command failed: $cmd", e)
            }
        }
    }

    private fun execute(cmd: BleCodec.Command) {
        when (cmd) {
            is BleCodec.Command.Listen -> {
                wantListen = true
                if (radioOn) listen()
            }
            is BleCodec.Command.Advertise -> {
                advertData = cmd.serviceData
                if (radioOn) startAdvertising(cmd.serviceData)
            }
            is BleCodec.Command.StopAdvertising -> {
                advertData = null
                stopAdvertising()
            }
            is BleCodec.Command.Scan -> {
                wantScan = true
                if (radioOn) startScan()
            }
            is BleCodec.Command.StopScan -> {
                wantScan = false
                stopScan()
            }
            is BleCodec.Command.Connect ->
                if (radioOn) connect(cmd) else event(BleCodec.connectFailed(cmd.link, "Bluetooth is off"))
            // Blocks only if this link's queue is full: flow control.
            is BleCodec.Command.Send -> links[cmd.link]?.queue?.put(cmd.data)
            is BleCodec.Command.Close -> links[cmd.link]?.close("closed by node")
            is BleCodec.Command.RotateAddress -> {
                // Android picks the advertising address itself (a resolvable
                // private address that it rotates on its own timer). Starting a
                // fresh advertisement is the most an app can do to move to a new one.
                stopAdvertising()
                if (radioOn) advertData?.let { startAdvertising(it) }
            }
        }
    }

    /** Accept links on both an L2CAP channel and the GATT pipe, so every kind of peer can reach us. */
    private fun listen() {
        if (listening) return
        if (gattServer == null) {
            gattServer = try {
                GattPipeServer(this, context, manager)
            } catch (e: Exception) {
                Log.w(TAG, "GATT pipe unavailable", e)
                null
            }
        }
        val psm = try {
            val s = adapter.listenUsingInsecureL2capChannel()
            l2capServer = s
            acceptLoop(s)
            s.psm
        } catch (e: IOException) {
            Log.w(TAG, "L2CAP channel unavailable", e)
            0
        }
        if (psm == 0 && gattServer == null) {
            Log.e(TAG, "can't accept links on Bluetooth")
            return
        }
        listening = true
        event(BleCodec.listening(psm))
    }

    private fun acceptLoop(s: BluetoothServerSocket) {
        Thread({
            while (running) {
                val socket = try {
                    s.accept()
                } catch (e: IOException) {
                    break // server closed
                }
                val link = L2capLink(this, newHostLink(), socket)
                adopt(link)
                link.open = true
                link.tell(BleCodec.incoming(link.id, link.mtu, socket.remoteDevice.address))
                link.startWriter()
                link.startReader()
            }
        }, "mesh-ble-accept").start()
    }

    private fun connect(cmd: BleCodec.Command.Connect) {
        val device = seen[cmd.device] ?: try {
            adapter.getRemoteDevice(cmd.device)
        } catch (e: IllegalArgumentException) {
            event(BleCodec.connectFailed(cmd.link, "bad device address"))
            return
        }
        if (cmd.psm == 0) {
            // Until connected, a Close from the node (it gave up) cancels the attempt.
            val link = GattClientLink(this, cmd.link, device)
            adopt(link)
            link.connect(context)
            return
        }
        val link = try {
            L2capLink(this, cmd.link, device.createInsecureL2capChannel(cmd.psm))
        } catch (e: IOException) {
            event(BleCodec.connectFailed(cmd.link, e.message ?: "connect failed"))
            return
        }
        adopt(link)
        Thread({
            try {
                link.connect()
            } catch (e: IOException) {
                link.close(e.message ?: "connect failed")
                return@Thread
            }
            if (link.isClosed) return@Thread
            link.open = true
            link.tell(BleCodec.connected(link.id, link.mtu))
            link.startWriter()
            link.startReader()
        }, "mesh-ble-connect").start()
    }

    private fun startAdvertising(serviceData: ByteArray) {
        val advertiser = adapter.bluetoothLeAdvertiser ?: return
        stopAdvertising()
        val settings = AdvertiseSettings.Builder()
            .setAdvertiseMode(AdvertiseSettings.ADVERTISE_MODE_BALANCED)
            .setTxPowerLevel(AdvertiseSettings.ADVERTISE_TX_POWER_MEDIUM)
            .setConnectable(true)
            .setTimeout(0)
            .build()
        // Service data only: no device name, no TX power, nothing else to fingerprint.
        val data = AdvertiseData.Builder()
            .setIncludeDeviceName(false)
            .setIncludeTxPowerLevel(false)
            .addServiceData(serviceUuid, serviceData)
            .build()
        // The bare service UUID, for platforms that filter scans by UUID list (iOS, Windows).
        val scanResponse = AdvertiseData.Builder()
            .setIncludeDeviceName(false)
            .setIncludeTxPowerLevel(false)
            .addServiceUuid(serviceUuid)
            .build()
        val cb = object : AdvertiseCallback() {
            override fun onStartFailure(errorCode: Int) {
                Log.w(TAG, "advertising failed: $errorCode")
            }
        }
        advertising = cb
        advertiser.startAdvertising(settings, data, scanResponse, cb)
    }

    private fun stopAdvertising() {
        val cb = advertising ?: return
        advertising = null
        try {
            adapter.bluetoothLeAdvertiser?.stopAdvertising(cb)
        } catch (e: IllegalStateException) {
            // Bluetooth turned off.
        }
    }

    private fun startScan() {
        if (scanning != null) return
        val scanner = adapter.bluetoothLeScanner ?: return
        // Any advert with SocialMesh service data (an empty pattern matches
        // every value), or with just the service UUID (iOS can't send service
        // data). Filters also keep scanning alive with the screen off.
        val filters = listOf(
            ScanFilter.Builder().setServiceData(serviceUuid, ByteArray(0)).build(),
            ScanFilter.Builder().setServiceUuid(serviceUuid).build(),
        )
        val settings = ScanSettings.Builder().setScanMode(ScanSettings.SCAN_MODE_BALANCED).build()
        val cb = object : ScanCallback() {
            override fun onScanResult(callbackType: Int, result: ScanResult) = report(result)
            override fun onBatchScanResults(results: MutableList<ScanResult>) = results.forEach(::report)
            override fun onScanFailed(errorCode: Int) {
                Log.w(TAG, "scan failed: $errorCode")
            }
        }
        scanning = cb
        scanner.startScan(filters, settings, cb)
    }

    private fun report(result: ScanResult) {
        val record = result.scanRecord ?: return
        // No service data: a peer heard without a token, reached over the GATT pipe (§9.3).
        val data = record.getServiceData(serviceUuid)
            ?: if (record.serviceUuids?.contains(serviceUuid) == true) ByteArray(0) else return
        val device = result.device
        if (seen.size > 1024) seen.clear()
        seen[device.address] = device
        try {
            reporter.execute { event(BleCodec.advert(device.address, data, result.rssi)) }
        } catch (e: RejectedExecutionException) {
            // stopping
        }
    }

    private fun stopScan() {
        val cb = scanning ?: return
        scanning = null
        try {
            adapter.bluetoothLeScanner?.stopScan(cb)
        } catch (e: IllegalStateException) {
            // Bluetooth turned off.
        }
    }

    companion object {
        private const val TAG = "SocialMeshBle"
    }
}
