package org.socialmesh.android

import android.annotation.SuppressLint
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothServerSocket
import android.bluetooth.BluetoothSocket
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
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicLong

/**
 * The Android Bluetooth driver for the node (SPEC.md §9).
 *
 * It does only radio work: it takes commands from the node (advertise,
 * scan, listen, connect, send, close) and reports what happens. Adverts,
 * when to connect, encryption and address rotation are decided in Rust.
 *
 * Links are insecure L2CAP connection-oriented channels: no pairing, because
 * the node's own encrypted channel runs over every link. The GATT pipe
 * (for Windows peers) is not implemented on Android yet; adverts with
 * PSM 0 can't be connected to from here.
 *
 * Needs BLUETOOTH_SCAN, BLUETOOTH_ADVERTISE and BLUETOOTH_CONNECT (Android 12+)
 * or location permission (Android 10-11), granted before [start].
 */
@SuppressLint("MissingPermission")
class BleDriver(context: Context, private val handle: Long) {
    private val adapter: BluetoothAdapter =
        context.getSystemService(BluetoothManager::class.java).adapter
    private val serviceUuid = ParcelUuid(BleCodec.SERVICE_UUID)

    private val links = ConcurrentHashMap<Long, Link>()
    private val nextHostLink = AtomicLong(1)
    /** Devices from scan results: they carry the address type a bare address string lacks. */
    private val seen = ConcurrentHashMap<String, BluetoothDevice>()
    /** Scan callbacks arrive on the main thread; reporting can block, so it happens here. */
    private val reporter = Executors.newSingleThreadExecutor()

    @Volatile private var running = false
    @Volatile private var server: BluetoothServerSocket? = null
    private var advertising: AdvertiseCallback? = null
    private var advertData: ByteArray? = null
    private var scanning: ScanCallback? = null
    private var commandThread: Thread? = null

    fun start() {
        running = true
        commandThread = Thread(::commandLoop, "mesh-ble-commands").also { it.start() }
    }

    /** Stop everything. Call before [Native.stop]. */
    fun stop() {
        running = false
        commandThread?.join(2000)
        stopAdvertising()
        stopScan()
        try {
            server?.close()
        } catch (e: IOException) {
        }
        for (l in links.values) l.close()
        reporter.shutdownNow()
    }

    private fun event(bytes: ByteArray) {
        if (running && !Native.radioEvent(handle, bytes)) Log.w(TAG, "node refused an event")
    }

    private fun commandLoop() {
        while (running) {
            val bytes = Native.nextRadioCommand(handle, 250) ?: continue
            val cmd = BleCodec.decodeCommand(bytes) ?: continue
            try {
                execute(cmd)
            } catch (e: SecurityException) {
                Log.e(TAG, "Bluetooth permission missing", e)
            } catch (e: Exception) {
                Log.w(TAG, "command failed: $cmd", e)
            }
        }
    }

    private fun execute(cmd: BleCodec.Command) {
        when (cmd) {
            is BleCodec.Command.Listen -> listen()
            is BleCodec.Command.Advertise -> {
                advertData = cmd.serviceData
                startAdvertising(cmd.serviceData)
            }
            is BleCodec.Command.StopAdvertising -> {
                advertData = null
                stopAdvertising()
            }
            is BleCodec.Command.Scan -> startScan()
            is BleCodec.Command.StopScan -> stopScan()
            is BleCodec.Command.Connect -> connect(cmd)
            // Blocks only if this link's queue is full: flow control.
            is BleCodec.Command.Send -> links[cmd.link]?.queue?.put(cmd.data)
            is BleCodec.Command.Close -> links[cmd.link]?.close()
            is BleCodec.Command.RotateAddress -> {
                // Android picks the advertising address itself (a resolvable
                // private address that it rotates on its own timer). Starting a
                // fresh advertisement is the most an app can do to move to a new one.
                stopAdvertising()
                advertData?.let { startAdvertising(it) }
            }
        }
    }

    private fun listen() {
        if (server != null) return
        val s = adapter.listenUsingInsecureL2capChannel()
        server = s
        event(BleCodec.listening(s.psm))
        Thread({
            while (running) {
                val socket = try {
                    s.accept()
                } catch (e: IOException) {
                    break // server closed
                }
                val id = BleCodec.HOST_LINK or nextHostLink.getAndIncrement()
                val link = Link(id, socket)
                links[id] = link
                event(BleCodec.incoming(id, socket.maxTransmitPacketSize, socket.remoteDevice.address))
                link.start()
            }
        }, "mesh-ble-accept").start()
    }

    private fun connect(cmd: BleCodec.Command.Connect) {
        if (cmd.psm == 0) {
            event(BleCodec.connectFailed(cmd.link, "GATT pipe not supported on Android yet"))
            return
        }
        val device = seen[cmd.device] ?: adapter.getRemoteDevice(cmd.device)
        Thread({
            try {
                val socket = device.createInsecureL2capChannel(cmd.psm)
                socket.connect()
                val link = Link(cmd.link, socket)
                links[cmd.link] = link
                event(BleCodec.connected(cmd.link, socket.maxTransmitPacketSize))
                link.start()
            } catch (e: IOException) {
                event(BleCodec.connectFailed(cmd.link, e.message ?: "connect failed"))
            }
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
        val cb = object : AdvertiseCallback() {
            override fun onStartFailure(errorCode: Int) {
                Log.w(TAG, "advertising failed: $errorCode")
            }
        }
        advertising = cb
        advertiser.startAdvertising(settings, data, cb)
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
        // Matches any advert with SocialMesh service data (an empty pattern
        // matches every value). A filter also keeps scanning alive with the
        // screen off.
        val filter = ScanFilter.Builder().setServiceData(serviceUuid, ByteArray(0)).build()
        val settings = ScanSettings.Builder().setScanMode(ScanSettings.SCAN_MODE_BALANCED).build()
        val cb = object : ScanCallback() {
            override fun onScanResult(callbackType: Int, result: ScanResult) = report(result)
            override fun onBatchScanResults(results: MutableList<ScanResult>) = results.forEach(::report)
            override fun onScanFailed(errorCode: Int) {
                Log.w(TAG, "scan failed: $errorCode")
            }
        }
        scanning = cb
        scanner.startScan(listOf(filter), settings, cb)
    }

    private fun report(result: ScanResult) {
        val data = result.scanRecord?.getServiceData(serviceUuid) ?: return
        val device = result.device
        if (seen.size > 1024) seen.clear()
        seen[device.address] = device
        try {
            reporter.execute { event(BleCodec.advert(device.address, data, result.rssi)) }
        } catch (e: java.util.concurrent.RejectedExecutionException) {
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

    /** One L2CAP channel: a writer thread fed by a bounded queue, and a reader thread. */
    private inner class Link(val id: Long, private val socket: BluetoothSocket) {
        val queue = ArrayBlockingQueue<ByteArray>(64)

        fun start() {
            Thread({
                try {
                    val out = socket.outputStream
                    while (true) {
                        val b = queue.take()
                        if (b === EOF) break
                        out.write(b)
                        out.flush()
                    }
                } catch (e: IOException) {
                } catch (e: InterruptedException) {
                }
                close()
            }, "mesh-ble-write").start()
            Thread({
                val buf = ByteArray(maxOf(socket.maxReceivePacketSize, 512))
                try {
                    val input = socket.inputStream
                    while (true) {
                        val n = input.read(buf)
                        if (n < 0) break
                        if (n > 0) event(BleCodec.data(id, buf, n))
                    }
                } catch (e: IOException) {
                }
                close()
            }, "mesh-ble-read").start()
        }

        /** Idempotent; tells the node once. */
        fun close() {
            if (links.remove(id) == null) return
            try {
                socket.close()
            } catch (e: IOException) {
            }
            queue.clear()
            queue.offer(EOF)
            event(BleCodec.closed(id))
        }
    }

    companion object {
        private const val TAG = "SocialMeshBle"
        private val EOF = ByteArray(0)
    }
}
