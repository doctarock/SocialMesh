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

    /** Sites held here: JSON array of {address, short, title, version, files, have, size, ownerless}. */
    @JvmStatic external fun sitesJson(handle: Long): String?

    /** Queue a site's files for download. Returns its first page's path. Throws if the site isn't held. */
    @JvmStatic external fun siteWant(handle: Long, address: String): String?

    /** Files present and in total, as `have shl 32 or all`, or -1. */
    @JvmStatic external fun siteProgress(handle: Long, address: String): Long

    /** One file for the viewer, packed (see SiteActivity.unpack), or null. */
    @JvmStatic external fun siteFile(handle: Long, address: String, path: String): ByteArray?

    /** `{title, address, version}` for an app's code. */
    @JvmStatic external fun appInfo(handle: Long, address: String): String?

    /** Entries sent to an app, as its code sees them (JSON array). */
    @JvmStatic external fun appEntries(handle: Long, address: String): String?

    /** The short ID this person's anonymous entries to an app are signed with. */
    @JvmStatic external fun appAnonymousId(handle: Long, address: String): String?

    /** Publish an entry the person agreed to (anonymously for this app, or signed as them). Returns its ID. */
    @JvmStatic external fun appSubmit(handle: Long, address: String, data: String, anonymous: Boolean): String?

    /** This device's site keys: JSON array of {key, address, short, title, version}. */
    @JvmStatic external fun mySitesJson(handle: Long): String?

    /**
     * Publish a site: a new version of the site under [key], a new site if [key] is null, or an
     * anonymous one if [ownerless]. [files] are packed (see PublishSiteActivity.pack). Throws with
     * a message starting "METADATA " for a file whose hidden details can't be removed.
     */
    @JvmStatic external fun publishSite(
        handle: Long,
        key: String?,
        ownerless: Boolean,
        title: String,
        index: String,
        files: ByteArray,
        keepMetadata: Boolean,
    ): String?
}
