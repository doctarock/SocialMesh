package org.socialmesh.android

/**
 * The SocialMesh node, in Rust (apps/mesh-mobile, built into libsocialmesh.so).
 * A node is a `Long` handle. Every call blocks and is safe from any thread.
 */
object Native {
    init {
        System.loadLibrary("socialmesh")
    }

    /** Open or create the node in [home] and start it. Throws on failure. */
    @JvmStatic external fun start(home: String, beacon: Boolean, lan: Boolean): Long

    @JvmStatic external fun stop(handle: Long)

    /** Release the node: once, after [stop] and after the Bluetooth driver has stopped. */
    @JvmStatic external fun free(handle: Long)

    /** Next encoded command for the Bluetooth driver, or null after [timeoutMs]. */
    @JvmStatic external fun nextRadioCommand(handle: Long, timeoutMs: Int): ByteArray?

    /** Hand the node an encoded Bluetooth event. Blocks while the node catches up. */
    @JvmStatic external fun radioEvent(handle: Long, event: ByteArray): Boolean

    /** Publish a signed text post. Returns its id (hex). */
    @JvmStatic external fun publish(handle: Long, text: String): String?

    /** Newest posts as a JSON array of {id, author, mine, created_ms, body}. */
    @JvmStatic external fun feedJson(handle: Long, limit: Int): String?

    /** Next line worth showing the user, or null after [timeoutMs]. */
    @JvmStatic external fun nextEvent(handle: Long, timeoutMs: Int): String?

    /** Live links, "peer medium" per line. */
    @JvmStatic external fun links(handle: Long): String?

    /** "posting-id link-key" (short forms). */
    @JvmStatic external fun ids(handle: Long): String?

    /** The newest public post as sound: 16-bit mono PCM, 8 kHz. Throws if nothing is public. */
    @JvmStatic external fun soundOfNewest(handle: Long, fast: Boolean): ByteArray?

    /** Microphone audio (16-bit mono PCM at [rate]). Returns lines to show, newline-separated, or null. */
    @JvmStatic external fun hear(handle: Long, pcm: ByteArray, rate: Int): String?

    /** Forget partly heard sound. */
    @JvmStatic external fun stopHearing(handle: Long)
}
