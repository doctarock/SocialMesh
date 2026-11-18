package org.socialmesh.android

import android.Manifest
import android.content.Context
import android.content.pm.ApplicationInfo
import android.content.pm.PackageManager
import android.media.AudioManager
import android.os.Handler
import android.os.Looper
import java.io.File
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import kotlin.random.Random

/**
 * Sound: the microphone, and the sound modem.
 *
 * The **sound modem** is for when there's no Bluetooth and no Wi-Fi: the
 * phone listens all the time and, every minute or so, plays one of its
 * newest public posts, taking turns through them. It holds back while it
 * hears someone else playing, and doesn't hear itself. It switches on by
 * itself when both radios are unavailable (if the microphone is allowed) and
 * off when one comes back; the person can switch it either way, which holds
 * until the radios change again.
 *
 * **Listen** on the main screen also uses the microphone, only while that
 * screen is showing.
 */
internal object Modem {
    /** Waits between plays: long enough to leave the air to others, short enough to be useful. */
    private const val FIRST_MS = 8_000L
    private const val EVERY_MS = 45_000L
    private const val JITTER_MS = 30_000L
    /** Starting by itself waits this long for the radios to settle (Wi-Fi reports a moment after start). */
    private const val SETTLE_MS = 4_000L

    private var app: Context? = null
    private val main = Handler(Looper.getMainLooper())
    /** The person's choice, until the radios next change; null: automatic. */
    @Volatile private var manual: Boolean? = null
    @Volatile var on = false
        private set
    @Volatile private var screenListening = false
    /** Playing: what the microphone hears now is us. */
    @Volatile private var deaf = false
    private var mic: Microphone? = null
    private var player: Thread? = null

    val listening: Boolean get() = mic?.isOn == true

    /** No Bluetooth and no Wi-Fi: sound is the only way left. */
    val needed: Boolean get() = !Radios.bluetooth && !Radios.wifi

    /** Would be on, if the microphone were allowed. */
    val wanted: Boolean get() = manual ?: needed

    fun micAllowed(context: Context) =
        context.checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED

    fun attach(context: Context) {
        app = context.applicationContext
    }

    /** The radios changed: back to automatic. */
    fun radiosChanged() {
        manual = null
        evaluate()
    }

    /** The person switched it. */
    fun toggle() {
        manual = !on
        evaluate()
    }

    /** Switch on or off as wanted. Switching on by itself waits for the radios to settle. */
    fun evaluate() {
        main.removeCallbacks(apply)
        if (wanted && !on && manual == null) main.postDelayed(apply, SETTLE_MS) else main.post(apply)
    }

    private val apply = Runnable {
        val a = app ?: return@Runnable
        val want = Mesh.running && wanted && micAllowed(a)
        if (want != on) setOn(a, want)
    }

    @Synchronized
    private fun setOn(context: Context, want: Boolean) {
        on = want
        if (want) {
            // Lets the service keep the microphone with the screen off.
            refreshService(context)
            Mesh.report(
                if (manual == true) {
                    "Sound modem on: posts go out and come in by sound"
                } else {
                    "Sound modem on: no Bluetooth or Wi-Fi, so posts go out and come in by sound"
                },
            )
            warnIfQuiet(context)
            player = Thread(::playLoop, "mesh-modem").also { it.start() }
        } else {
            player?.interrupt()
            player = null
            Mesh.report("Sound modem off")
            refreshService(context)
        }
        updateMic(context)
    }

    /** Media volume as (now, max). */
    fun volume(context: Context): Pair<Int, Int> {
        val am = context.getSystemService(AudioManager::class.java) ?: return 1 to 1
        return am.getStreamVolume(AudioManager.STREAM_MUSIC) to am.getStreamMaxVolume(AudioManager.STREAM_MUSIC).coerceAtLeast(1)
    }

    /** Too quiet for phones a step or two away to hear (below about two thirds). */
    fun tooQuiet(context: Context): Boolean {
        val (now, max) = volume(context)
        return now * 3 < max * 2
    }

    @Volatile private var warnedAt = -1

    /** Say so once per volume level: the modem never changes the person's volume itself. */
    private fun warnIfQuiet(context: Context) {
        val (now, max) = volume(context)
        if (!tooQuiet(context) || warnedAt == now) return
        warnedAt = now
        Mesh.report("Media volume is $now of $max: turn it up, or phones nearby may not hear this one")
    }

    private fun refreshService(context: Context) {
        try {
            context.startService(MeshService.refreshIntent(context))
        } catch (e: RuntimeException) {
            // Not allowed from the background right now; the notification catches up later.
        }
    }

    /** Listen while the main screen shows (Listen button). */
    fun setScreenListening(context: Context, listen: Boolean): Boolean {
        screenListening = listen
        return updateMic(context)
    }

    /** Stop everything (the node stopped). */
    @Synchronized
    fun shutdown() {
        main.removeCallbacks(apply)
        on = false
        manual = null
        screenListening = false
        player?.interrupt()
        player = null
        mic?.stop()
        Mesh.stopHearing()
    }

    /** Start or stop the microphone to match; false if it should be on but can't be. */
    @Synchronized
    private fun updateMic(context: Context): Boolean {
        val want = on || screenListening
        val m = mic ?: run {
            // Debug builds keep the last two minutes heard, to find out why a post wasn't decoded.
            val debuggable = context.applicationInfo.flags and ApplicationInfo.FLAG_DEBUGGABLE != 0
            Microphone(if (debuggable) File(context.filesDir, "last-heard.wav") else null) { pcm, rate ->
                if (!deaf) Mesh.hear(pcm, rate)
            }.also { mic = it }
        }
        if (want && !m.isOn) {
            if (!micAllowed(context) || !m.start()) return false
        } else if (!want && m.isOn) {
            m.stop()
            Mesh.stopHearing()
        }
        return true
    }

    /** Play [pcm] now, deaf to ourselves meanwhile; [done] runs when it ends. */
    fun play(pcm: ByteArray, done: () -> Unit) {
        if (Speaker.isPlaying) return done()
        deaf = true
        Speaker.play(pcm) {
            // A little longer, for the room's echo to die down.
            main.postDelayed({ deaf = false }, 300)
            done()
        }
    }

    private fun playLoop() {
        var nth = 0
        // Spread widely: phones switched on together shouldn't pick the same moment.
        var wait = FIRST_MS + Random.nextLong(3 * FIRST_MS)
        try {
            while (on) {
                Thread.sleep(wait)
                if (!on) break
                // Listen, wait a random moment, listen again: two phones that both
                // found the air free will rarely still both find it free.
                val free = !Speaker.isPlaying && !Mesh.soundBusy() &&
                    run { Thread.sleep(500L + Random.nextLong(2_000L)); !Speaker.isPlaying && !Mesh.soundBusy() }
                if (!on) break
                if (!free) {
                    // Someone else has the air: try again shortly, at a random moment.
                    wait = 3_000L + Random.nextLong(7_000L)
                    continue
                }
                val pcm = try {
                    Mesh.soundOfRecent(nth, false)
                } catch (e: RuntimeException) {
                    null // nothing public to play yet
                }
                if (pcm != null) {
                    app?.let(::warnIfQuiet)
                    nth++
                    val ended = CountDownLatch(1)
                    main.post { play(pcm) { ended.countDown() } }
                    ended.await(5, TimeUnit.MINUTES)
                }
                wait = EVERY_MS + Random.nextLong(JITTER_MS)
            }
        } catch (e: InterruptedException) {
            // switched off
        }
    }
}
