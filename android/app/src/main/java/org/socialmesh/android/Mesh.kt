package org.socialmesh.android

import android.content.Context
import android.net.wifi.WifiManager
import android.util.Log
import java.io.File
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.TimeUnit
import java.util.concurrent.locks.ReentrantReadWriteLock

/**
 * The one node in this app. [MeshService] starts and stops it; the screen
 * reads from it while it's open.
 *
 * Every call into the node goes through here under a read lock, and the node
 * is freed under the write lock, so a call in flight can never touch a freed
 * node.
 */
object Mesh {
    fun interface Listener {
        /** A line worth showing (see `Mobile::next_event`), plus "Started" and "Stopped". Any thread. */
        fun onLine(line: String)
    }

    private const val TAG = "SocialMesh"
    private const val KEEP_LINES = 100

    private val lock = ReentrantReadWriteLock()
    @Volatile private var handle = 0L
    private var driver: BleDriver? = null
    private var multicast: WifiManager.MulticastLock? = null
    private var watcher: Thread? = null
    @Volatile private var watching = false
    private val listeners = CopyOnWriteArrayList<Listener>()
    private val recent = ArrayDeque<String>()

    val running: Boolean get() = handle != 0L

    /** Start the node if it isn't running. Blocks: call it off the main thread. Throws if the node can't start. */
    @Synchronized
    fun start(context: Context) {
        if (handle != 0L) return
        val app = context.applicationContext
        handle = Native.start(File(app.filesDir, "mesh").absolutePath, true, true)
        // Android drops LAN broadcasts (LAN discovery) unless an app holds this.
        multicast = try {
            app.getSystemService(WifiManager::class.java)?.createMulticastLock("socialmesh")?.apply {
                setReferenceCounted(false)
                acquire()
            }
        } catch (e: RuntimeException) {
            Log.w(TAG, "no multicast lock; LAN discovery may not hear others", e)
            null
        }
        driver = BleDriver(app, radio).also { it.start() }
        watching = true
        watcher = Thread(::watch, "mesh-events").also { it.start() }
        say("Started")
    }

    /** Stop and free the node. Blocks for up to a few seconds: call it off the main thread. */
    @Synchronized
    fun stop() {
        val h = handle
        if (h == 0L) return
        watching = false
        driver?.stop()
        driver = null
        call { Native.stop(it) }
        watcher?.join(2000)
        watcher = null
        multicast?.let { if (it.isHeld) it.release() }
        multicast = null
        val w = lock.writeLock()
        if (w.tryLock(5, TimeUnit.SECONDS)) {
            try {
                handle = 0
                Native.free(h)
            } finally {
                w.unlock()
            }
        } else {
            // Something is stuck inside the node. Leaking it beats freeing it under a caller.
            handle = 0
            Log.w(TAG, "node still busy at stop; not freed")
        }
        say("Stopped")
    }

    fun publish(text: String): String? = call { Native.publish(it, text) }

    fun feedJson(limit: Int): String? = call { Native.feedJson(it, limit) }

    /** Live links, "peer medium" per line. */
    fun links(): String? = call { Native.links(it) }

    /** "posting-id link-key" (short forms). */
    fun ids(): String? = call { Native.ids(it) }

    /** The newest public post as 8 kHz PCM, or null if the node isn't running. Throws if nothing is public. */
    fun soundOfNewest(fast: Boolean): ByteArray? = call { Native.soundOfNewest(it, fast) }

    /** Microphone audio; anything worth saying about it is said to the listeners. */
    fun hear(pcm: ByteArray, rate: Int) {
        val lines = call { Native.hear(it, pcm, rate) } ?: return
        for (l in lines.split('\n')) say(l)
    }

    fun stopHearing() {
        call { Native.stopHearing(it) }
    }

    /** The latest lines, oldest first. */
    fun recentLines(): List<String> = synchronized(recent) { recent.toList() }

    fun addListener(l: Listener) {
        listeners.add(l)
    }

    fun removeListener(l: Listener) {
        listeners.remove(l)
    }

    /** Show a line as if the node had said it. */
    fun report(line: String) = say(line)

    private val radio = object : BleDriver.Node {
        override fun nextRadioCommand(timeoutMs: Int): ByteArray? = call { Native.nextRadioCommand(it, timeoutMs) }

        override fun radioEvent(event: ByteArray): Boolean = call { Native.radioEvent(it, event) } ?: false
    }

    /** [f] with the live node, or null if there isn't one. */
    private inline fun <T> call(f: (Long) -> T): T? {
        val r = lock.readLock()
        r.lock()
        try {
            val h = handle
            return if (h == 0L) null else f(h)
        } finally {
            r.unlock()
        }
    }

    private fun watch() {
        while (watching) {
            val line = call { Native.nextEvent(it, 500) } ?: continue
            say(line)
        }
    }

    private fun say(line: String) {
        synchronized(recent) {
            recent.addLast(line)
            while (recent.size > KEEP_LINES) recent.removeFirst()
        }
        for (l in listeners) l.onLine(line)
    }
}
