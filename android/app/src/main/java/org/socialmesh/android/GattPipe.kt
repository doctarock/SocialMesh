package org.socialmesh.android

import android.annotation.SuppressLint
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothGattServer
import android.bluetooth.BluetoothGattServerCallback
import android.bluetooth.BluetoothGattService
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothProfile
import android.bluetooth.BluetoothStatusCodes
import android.content.Context
import android.os.Build
import android.util.Log
import java.io.IOException
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Semaphore
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

/**
 * The GATT pipe (SPEC.md §9.2): links for peers without L2CAP channels
 * (Windows, and anything else that advertises PSM 0).
 *
 * The connecting end (central) enables notifications on TX, which opens the
 * link, then writes without response to RX; the other end (peripheral)
 * notifies on TX. Each write or notification is one chunk of the stream, at
 * most ATT_MTU - 3 bytes. A disconnect, or turning notifications off, ends it.
 */
internal object Pipe {
    val SERVICE: UUID = BleCodec.SERVICE_UUID
    val RX: UUID = UUID.fromString("191e3b37-5476-45bf-82eb-a2c4070948e7")
    val TX: UUID = UUID.fromString("191e3b38-5476-45bf-82eb-a2c4070948e7")
    val INFO: UUID = UUID.fromString("191e3b39-5476-45bf-82eb-a2c4070948e7")
    val CCCD: UUID = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")

    /** The largest ATT MTU; we ask for it and take what we get. */
    const val MAX_MTU = 517
    const val DEFAULT_MTU = 23
    /** Give up on a chunk the stack hasn't taken after this long. */
    const val STALL_SECONDS = 10L
    const val BUSY_RETRIES = 50
    const val BUSY_SLEEP_MS = 10L
}

/** The peripheral end: the SocialMesh service, accepting pipe links from centrals. */
@SuppressLint("MissingPermission")
internal class GattPipeServer(private val driver: BleDriver, context: Context, manager: BluetoothManager) {
    private val tx = BluetoothGattCharacteristic(Pipe.TX, BluetoothGattCharacteristic.PROPERTY_NOTIFY, 0).apply {
        addDescriptor(
            BluetoothGattDescriptor(
                Pipe.CCCD,
                BluetoothGattDescriptor.PERMISSION_READ or BluetoothGattDescriptor.PERMISSION_WRITE,
            ),
        )
    }
    private val byDevice = ConcurrentHashMap<String, ServerLink>()
    private val mtus = ConcurrentHashMap<String, Int>()
    /** Android takes one notification at a time; the next waits for onNotificationSent. */
    private val notifySlot = Semaphore(1)
    private val server: BluetoothGattServer =
        manager.openGattServer(context, Callback()) ?: throw IOException("no GATT server on this device")

    init {
        val service = BluetoothGattService(Pipe.SERVICE, BluetoothGattService.SERVICE_TYPE_PRIMARY)
        service.addCharacteristic(
            BluetoothGattCharacteristic(
                Pipe.RX,
                BluetoothGattCharacteristic.PROPERTY_WRITE_NO_RESPONSE or BluetoothGattCharacteristic.PROPERTY_WRITE,
                BluetoothGattCharacteristic.PERMISSION_WRITE,
            ),
        )
        service.addCharacteristic(tx)
        service.addCharacteristic(
            BluetoothGattCharacteristic(
                Pipe.INFO,
                BluetoothGattCharacteristic.PROPERTY_READ,
                BluetoothGattCharacteristic.PERMISSION_READ,
            ),
        )
        if (!server.addService(service)) {
            server.close()
            throw IOException("could not add the SocialMesh service")
        }
    }

    fun close() {
        for (l in byDevice.values) l.close("stopped")
        server.close()
    }

    private fun open(device: BluetoothDevice) {
        val addr = device.address
        if (byDevice.containsKey(addr)) return
        val link = ServerLink(driver.newHostLink(), device, (mtus[addr] ?: Pipe.DEFAULT_MTU) - 3)
        if (byDevice.putIfAbsent(addr, link) != null) return
        driver.adopt(link)
        link.open = true
        link.tell(BleCodec.incoming(link.id, link.payload, addr))
        link.startWriter()
    }

    private fun respond(device: BluetoothDevice, requestId: Int, status: Int, offset: Int, value: ByteArray?) {
        try {
            server.sendResponse(device, requestId, status, offset, value)
        } catch (e: RuntimeException) {
            Log.w(TAG, "GATT response failed", e)
        }
    }

    private inner class Callback : BluetoothGattServerCallback() {
        override fun onConnectionStateChange(device: BluetoothDevice, status: Int, newState: Int) {
            if (newState == BluetoothProfile.STATE_DISCONNECTED) {
                mtus.remove(device.address)
                byDevice[device.address]?.close("disconnected")
            }
        }

        override fun onMtuChanged(device: BluetoothDevice, mtu: Int) {
            mtus[device.address] = mtu
            byDevice[device.address]?.payload = mtu - 3
        }

        override fun onDescriptorWriteRequest(
            device: BluetoothDevice,
            requestId: Int,
            descriptor: BluetoothGattDescriptor,
            preparedWrite: Boolean,
            responseNeeded: Boolean,
            offset: Int,
            value: ByteArray?,
        ) {
            val ok = descriptor.uuid == Pipe.CCCD && descriptor.characteristic?.uuid == Pipe.TX && !preparedWrite
            // Answer first, so the central is ready by the time the node writes.
            if (responseNeeded) {
                respond(device, requestId, if (ok) BluetoothGatt.GATT_SUCCESS else BluetoothGatt.GATT_WRITE_NOT_PERMITTED, offset, value)
            }
            if (!ok) return
            val on = value != null && value.isNotEmpty() && (value[0].toInt() and 1) != 0
            if (on) open(device) else byDevice[device.address]?.close("unsubscribed")
        }

        override fun onDescriptorReadRequest(
            device: BluetoothDevice,
            requestId: Int,
            offset: Int,
            descriptor: BluetoothGattDescriptor,
        ) {
            if (descriptor.uuid != Pipe.CCCD) {
                respond(device, requestId, BluetoothGatt.GATT_READ_NOT_PERMITTED, offset, null)
                return
            }
            val v = if (byDevice.containsKey(device.address)) {
                BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE
            } else {
                BluetoothGattDescriptor.DISABLE_NOTIFICATION_VALUE
            }
            respond(device, requestId, BluetoothGatt.GATT_SUCCESS, 0, v)
        }

        override fun onCharacteristicWriteRequest(
            device: BluetoothDevice,
            requestId: Int,
            characteristic: BluetoothGattCharacteristic,
            preparedWrite: Boolean,
            responseNeeded: Boolean,
            offset: Int,
            value: ByteArray?,
        ) {
            val ok = characteristic.uuid == Pipe.RX && !preparedWrite && offset == 0
            if (ok && value != null && value.isNotEmpty()) {
                // Writes before the central subscribed have no link to go to.
                byDevice[device.address]?.let { it.tell(BleCodec.data(it.id, value, value.size)) }
            }
            if (responseNeeded) {
                respond(device, requestId, if (ok) BluetoothGatt.GATT_SUCCESS else BluetoothGatt.GATT_WRITE_NOT_PERMITTED, 0, null)
            }
        }

        override fun onCharacteristicReadRequest(
            device: BluetoothDevice,
            requestId: Int,
            offset: Int,
            characteristic: BluetoothGattCharacteristic,
        ) {
            if (characteristic.uuid != Pipe.INFO) {
                respond(device, requestId, BluetoothGatt.GATT_READ_NOT_PERMITTED, offset, null)
                return
            }
            // The advert's service data, for centrals whose scans don't show it.
            val v = driver.advertData ?: ByteArray(0)
            if (offset > v.size) {
                respond(device, requestId, BluetoothGatt.GATT_INVALID_OFFSET, offset, null)
            } else {
                respond(device, requestId, BluetoothGatt.GATT_SUCCESS, offset, v.copyOfRange(offset, v.size))
            }
        }

        override fun onNotificationSent(device: BluetoothDevice, status: Int) {
            if (notifySlot.availablePermits() == 0) notifySlot.release()
        }
    }

    /** A central that subscribed to TX. */
    private inner class ServerLink(id: Long, private val device: BluetoothDevice, @Volatile var payload: Int) :
        Link(driver, id) {
        // Binder callbacks must not block: events go through the driver's ordered queue.
        override fun tell(event: ByteArray) = driver.post(event)

        override fun write(b: ByteArray) {
            var off = 0
            while (off < b.size) {
                val n = minOf(payload.coerceAtLeast(20), b.size - off)
                notify(b.copyOfRange(off, off + n))
                off += n
            }
        }

        private fun notify(chunk: ByteArray) {
            var tries = 0
            while (true) {
                if (isClosed) throw IOException("closed")
                if (!notifySlot.tryAcquire(Pipe.STALL_SECONDS, TimeUnit.SECONDS)) {
                    // A confirmation went missing (a central vanished mid-send). Carry on.
                    Log.w(TAG, "notification slot timed out")
                }
                if (send(chunk)) return
                if (notifySlot.availablePermits() == 0) notifySlot.release()
                if (++tries > Pipe.BUSY_RETRIES) throw IOException("notification refused")
                Thread.sleep(Pipe.BUSY_SLEEP_MS)
            }
        }

        @Suppress("DEPRECATION")
        private fun send(chunk: ByteArray): Boolean = try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                server.notifyCharacteristicChanged(device, tx, false, chunk) == BluetoothStatusCodes.SUCCESS
            } else {
                // One shared characteristic: safe because notifySlot lets one send through at a time.
                tx.value = chunk
                server.notifyCharacteristicChanged(device, tx, false)
            }
        } catch (e: RuntimeException) {
            false
        }

        override fun release() {
            byDevice.remove(device.address, this)
            try {
                server.cancelConnection(device)
            } catch (e: RuntimeException) {
            }
        }
    }

    companion object {
        private const val TAG = "SocialMeshGatt"
    }
}

/** The central end: a pipe link to a peer that advertised PSM 0. */
@SuppressLint("MissingPermission")
internal class GattClientLink(driver: BleDriver, id: Long, private val device: BluetoothDevice) : Link(driver, id) {
    @Volatile private var gatt: BluetoothGatt? = null
    @Volatile private var rx: BluetoothGattCharacteristic? = null
    @Volatile private var payload = Pipe.DEFAULT_MTU - 3
    private val discovering = AtomicBoolean(false)
    /** One write in flight: the next waits for onCharacteristicWrite. */
    private val canWrite = Semaphore(0)

    override fun tell(event: ByteArray) = driver.post(event)

    fun connect(context: Context) {
        val g = device.connectGatt(context, false, callback, BluetoothDevice.TRANSPORT_LE)
        if (g == null) {
            close("could not start a GATT connection")
            return
        }
        gatt = g
        if (isClosed) g.close() // closed while connectGatt was returning
    }

    private fun discover(g: BluetoothGatt) {
        if (discovering.compareAndSet(false, true) && !g.discoverServices()) close("service discovery failed")
    }

    private val callback = object : BluetoothGattCallback() {
        override fun onConnectionStateChange(g: BluetoothGatt, status: Int, newState: Int) {
            if (newState == BluetoothProfile.STATE_CONNECTED && status == BluetoothGatt.GATT_SUCCESS) {
                gatt = g
                // Bigger chunks if the peer agrees; discover services once that's settled.
                if (!g.requestMtu(Pipe.MAX_MTU)) discover(g)
            } else {
                close(if (open) "disconnected" else "GATT connect failed ($status)")
            }
        }

        override fun onMtuChanged(g: BluetoothGatt, mtu: Int, status: Int) {
            if (status == BluetoothGatt.GATT_SUCCESS) payload = mtu - 3
            discover(g)
        }

        override fun onServicesDiscovered(g: BluetoothGatt, status: Int) {
            val service = g.getService(Pipe.SERVICE)
            val r = service?.getCharacteristic(Pipe.RX)
            val t = service?.getCharacteristic(Pipe.TX)
            val cccd = t?.getDescriptor(Pipe.CCCD)
            if (status != BluetoothGatt.GATT_SUCCESS || r == null || t == null || cccd == null) {
                close("peer has no SocialMesh pipe")
                return
            }
            rx = r
            if (!g.setCharacteristicNotification(t, true) || !subscribe(g, cccd)) close("could not subscribe")
        }

        override fun onDescriptorWrite(g: BluetoothGatt, descriptor: BluetoothGattDescriptor, status: Int) {
            if (descriptor.uuid != Pipe.CCCD || isClosed) return
            if (status != BluetoothGatt.GATT_SUCCESS) {
                close("subscribe refused ($status)")
                return
            }
            open = true
            tell(BleCodec.connected(id, payload))
            canWrite.release()
            startWriter()
        }

        override fun onCharacteristicWrite(g: BluetoothGatt, characteristic: BluetoothGattCharacteristic, status: Int) {
            if (canWrite.availablePermits() == 0) canWrite.release()
        }

        override fun onCharacteristicChanged(g: BluetoothGatt, characteristic: BluetoothGattCharacteristic, value: ByteArray) {
            if (characteristic.uuid == Pipe.TX) received(value)
        }

        @Deprecated("Deprecated in Java")
        @Suppress("DEPRECATION")
        override fun onCharacteristicChanged(g: BluetoothGatt, characteristic: BluetoothGattCharacteristic) {
            // Android 13+ calls the overload above instead.
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU && characteristic.uuid == Pipe.TX) {
                received(characteristic.value ?: return)
            }
        }
    }

    private fun received(value: ByteArray) {
        if (open && !isClosed && value.isNotEmpty()) tell(BleCodec.data(id, value, value.size))
    }

    @Suppress("DEPRECATION")
    private fun subscribe(g: BluetoothGatt, cccd: BluetoothGattDescriptor): Boolean =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            g.writeDescriptor(cccd, BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE) == BluetoothStatusCodes.SUCCESS
        } else {
            cccd.value = BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE
            g.writeDescriptor(cccd)
        }

    override fun write(b: ByteArray) {
        var off = 0
        while (off < b.size) {
            val n = minOf(payload.coerceAtLeast(20), b.size - off)
            val chunk = b.copyOfRange(off, off + n)
            if (!canWrite.tryAcquire(Pipe.STALL_SECONDS, TimeUnit.SECONDS)) throw IOException("write stalled")
            var tries = 0
            while (!send(chunk)) {
                if (isClosed || ++tries > Pipe.BUSY_RETRIES) throw IOException("write refused")
                Thread.sleep(Pipe.BUSY_SLEEP_MS)
            }
            off += n
        }
    }

    @Suppress("DEPRECATION")
    private fun send(chunk: ByteArray): Boolean {
        val g = gatt ?: return false
        val c = rx ?: return false
        return try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                g.writeCharacteristic(c, chunk, BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE) ==
                    BluetoothStatusCodes.SUCCESS
            } else {
                c.writeType = BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE
                c.value = chunk
                g.writeCharacteristic(c)
            }
        } catch (e: RuntimeException) {
            false
        }
    }

    override fun release() {
        canWrite.release() // wake the writer so it sees the link is closed
        gatt?.let {
            it.disconnect()
            it.close()
        }
    }
}
