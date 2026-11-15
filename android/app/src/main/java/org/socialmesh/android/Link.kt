package org.socialmesh.android

import android.bluetooth.BluetoothSocket
import java.io.IOException
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.atomic.AtomicBoolean

/**
 * One link the node opened or accepted. The node's bytes wait in [queue]
 * (bounded: a full queue blocks the driver's command loop, which is the flow
 * control) and a writer thread sends them with [write].
 */
internal abstract class Link(protected val driver: BleDriver, val id: Long) {
    val queue = ArrayBlockingQueue<ByteArray>(64)
    private val done = AtomicBoolean(false)

    /** True once the node has been told the link exists (Connected or Incoming). */
    @Volatile var open = false
    val isClosed: Boolean get() = done.get()

    /** Send [b] in full, or throw. Only the writer thread calls this. */
    protected abstract fun write(b: ByteArray)

    /** Let go of the radio side. Called once, from [close]. */
    protected abstract fun release()

    /** Hand an event about this link to the node, in order. */
    open fun tell(event: ByteArray) = driver.event(event)

    fun startWriter() {
        Thread({
            try {
                while (true) {
                    val b = queue.take()
                    if (b === EOF) break
                    write(b)
                }
            } catch (e: IOException) {
            } catch (e: InterruptedException) {
            }
            close("write failed")
        }, "mesh-ble-write").start()
    }

    /**
     * Idempotent. Tells the node once: Closed if the link was open, otherwise
     * ConnectFailed with [reason].
     */
    fun close(reason: String = "closed") {
        if (!done.compareAndSet(false, true)) return
        driver.forget(this)
        release()
        queue.clear()
        queue.offer(EOF)
        tell(if (open) BleCodec.closed(id) else BleCodec.connectFailed(id, reason))
    }

    companion object {
        private val EOF = ByteArray(0)
    }
}

/** An L2CAP connection-oriented channel: a stream socket, so a reader thread and a writer thread. */
internal class L2capLink(driver: BleDriver, id: Long, private val socket: BluetoothSocket) : Link(driver, id) {
    val mtu: Int get() = socket.maxTransmitPacketSize

    override fun write(b: ByteArray) {
        val out = socket.outputStream
        out.write(b)
        out.flush()
    }

    override fun release() {
        try {
            socket.close()
        } catch (e: IOException) {
        }
    }

    /** Blocks until connected; throws if it can't be. */
    fun connect() = socket.connect()

    fun startReader() {
        Thread({
            val buf = ByteArray(maxOf(socket.maxReceivePacketSize, 512))
            try {
                val input = socket.inputStream
                while (true) {
                    val n = input.read(buf)
                    if (n < 0) break
                    // Blocks while the node catches up: backpressure onto the channel.
                    if (n > 0) tell(BleCodec.data(id, buf, n))
                }
            } catch (e: IOException) {
            }
            close("channel ended")
        }, "mesh-ble-read").start()
    }
}
