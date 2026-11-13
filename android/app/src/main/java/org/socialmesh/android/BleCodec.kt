package org.socialmesh.android

import java.nio.ByteBuffer
import java.nio.charset.StandardCharsets
import java.util.UUID

/**
 * The byte encoding between the Bluetooth driver and the node
 * (mirror of transports/mesh-transport-ble/src/codec.rs). Big-endian;
 * a string is a u16 length then UTF-8; a final byte field runs to the end.
 */
object BleCodec {
    val SERVICE_UUID: UUID = UUID.fromString("191e3b36-5476-45bf-82eb-a2c4070948e7")

    /** Link ids chosen by the driver (incoming links) have the top bit set. */
    const val HOST_LINK: Long = Long.MIN_VALUE

    sealed class Command {
        object Listen : Command()
        class Advertise(val serviceData: ByteArray) : Command()
        object StopAdvertising : Command()
        object Scan : Command()
        object StopScan : Command()
        class Connect(val link: Long, val psm: Int, val device: String) : Command()
        class Send(val link: Long, val data: ByteArray) : Command()
        class Close(val link: Long) : Command()
        object RotateAddress : Command()
    }

    /** Null for anything malformed or unknown (a newer node); the driver skips it. */
    fun decodeCommand(b: ByteArray): Command? {
        if (b.isEmpty()) return null
        val r = ByteBuffer.wrap(b, 1, b.size - 1)
        return try {
            when (b[0].toInt()) {
                1 -> Command.Listen
                2 -> Command.Advertise(rest(r))
                3 -> Command.StopAdvertising
                4 -> Command.Scan
                5 -> Command.StopScan
                6 -> {
                    val link = r.long
                    val psm = r.short.toInt() and 0xffff
                    Command.Connect(link, psm, str(r))
                }
                7 -> {
                    val link = r.long
                    Command.Send(link, rest(r))
                }
                8 -> Command.Close(r.long)
                9 -> Command.RotateAddress
                else -> null
            }
        } catch (e: java.nio.BufferUnderflowException) {
            null
        }
    }

    fun listening(psm: Int): ByteArray =
        ByteBuffer.allocate(3).put(1).putShort(psm.toShort()).array()

    fun advert(device: String, serviceData: ByteArray, rssi: Int): ByteArray {
        val d = device.toByteArray(StandardCharsets.UTF_8)
        return ByteBuffer.allocate(1 + 2 + 2 + d.size + serviceData.size)
            .put(2).putShort(rssi.toShort()).putShort(d.size.toShort()).put(d).put(serviceData).array()
    }

    fun incoming(link: Long, mtu: Int, device: String): ByteArray {
        val d = device.toByteArray(StandardCharsets.UTF_8)
        return ByteBuffer.allocate(1 + 8 + 4 + 2 + d.size)
            .put(3).putLong(link).putInt(mtu).putShort(d.size.toShort()).put(d).array()
    }

    fun connected(link: Long, mtu: Int): ByteArray =
        ByteBuffer.allocate(13).put(4).putLong(link).putInt(mtu).array()

    fun connectFailed(link: Long, reason: String): ByteArray {
        val d = reason.take(500).toByteArray(StandardCharsets.UTF_8)
        return ByteBuffer.allocate(1 + 8 + 2 + d.size).put(5).putLong(link).putShort(d.size.toShort()).put(d).array()
    }

    fun data(link: Long, bytes: ByteArray, len: Int): ByteArray =
        ByteBuffer.allocate(9 + len).put(6).putLong(link).put(bytes, 0, len).array()

    fun closed(link: Long): ByteArray = ByteBuffer.allocate(9).put(7).putLong(link).array()

    private fun rest(r: ByteBuffer): ByteArray {
        val out = ByteArray(r.remaining())
        r.get(out)
        return out
    }

    private fun str(r: ByteBuffer): String {
        val n = r.short.toInt() and 0xffff
        val out = ByteArray(n)
        r.get(out)
        return String(out, StandardCharsets.UTF_8)
    }
}
