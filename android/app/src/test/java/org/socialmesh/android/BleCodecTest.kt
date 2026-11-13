package org.socialmesh.android

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** Same vectors as `golden_vectors` in transports/mesh-transport-ble/src/codec.rs. */
class BleCodecTest {
    private fun h(s: String): ByteArray =
        s.replace(" ", "").chunked(2).map { it.toInt(16).toByte() }.toByteArray()

    @Test
    fun eventsMatchTheNode() {
        assertArrayEquals(h("01 0081"), BleCodec.listening(0x81))
        assertArrayEquals(h("02 ffc4 0002 4142 0102"), BleCodec.advert("AB", byteArrayOf(1, 2), -60))
        assertArrayEquals(
            h("03 8000000000000001 00000800 0002 4142"),
            BleCodec.incoming(BleCodec.HOST_LINK or 1L, 2048, "AB"),
        )
        assertArrayEquals(h("04 0000000000000005 000000f4"), BleCodec.connected(5, 244))
        assertArrayEquals(h("05 0000000000000005 0002 6e6f"), BleCodec.connectFailed(5, "no"))
        assertArrayEquals(h("06 0000000000000005 0908"), BleCodec.data(5, byteArrayOf(9, 8, 7), 2))
        assertArrayEquals(h("07 0000000000000005"), BleCodec.closed(5))
    }

    @Test
    fun commandsFromTheNodeDecode() {
        assertTrue(BleCodec.decodeCommand(h("01")) is BleCodec.Command.Listen)
        val adv = BleCodec.decodeCommand(h("02 0102")) as BleCodec.Command.Advertise
        assertArrayEquals(byteArrayOf(1, 2), adv.serviceData)
        assertTrue(BleCodec.decodeCommand(h("04")) is BleCodec.Command.Scan)
        val c = BleCodec.decodeCommand(h("06 0000000000000005 0081 0002 4142")) as BleCodec.Command.Connect
        assertEquals(5L, c.link)
        assertEquals(0x81, c.psm)
        assertEquals("AB", c.device)
        val s = BleCodec.decodeCommand(h("07 0000000000000005 010203")) as BleCodec.Command.Send
        assertEquals(5L, s.link)
        assertArrayEquals(byteArrayOf(1, 2, 3), s.data)
        assertEquals(5L, (BleCodec.decodeCommand(h("08 0000000000000005")) as BleCodec.Command.Close).link)
        assertTrue(BleCodec.decodeCommand(h("09")) is BleCodec.Command.RotateAddress)
    }

    @Test
    fun junkIsSkipped() {
        assertNull(BleCodec.decodeCommand(ByteArray(0)))
        assertNull(BleCodec.decodeCommand(h("2a")))
        assertNull(BleCodec.decodeCommand(h("06 0000")))
    }
}
