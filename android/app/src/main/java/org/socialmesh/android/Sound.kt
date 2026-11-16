package org.socialmesh.android

import android.annotation.SuppressLint
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.AudioTrack
import android.media.MediaRecorder
import android.util.Log
import java.io.File
import java.io.IOException
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * Posts by sound: the node turns the newest public post into tones
 * ([Speaker]) and decodes what the microphone hears ([Microphone]). The
 * modem, verification and storing are all in Rust; this is just audio I/O.
 */
internal object Speaker {
    /** The rate the node produces (`Mobile::SOUND_RATE`). */
    private const val RATE = 8000

    @Volatile private var playing: Thread? = null

    val isPlaying: Boolean get() = playing != null

    /** Play 16-bit mono PCM at 8 kHz on a thread; [done] runs when it ends. */
    fun play(pcm: ByteArray, done: () -> Unit) {
        if (playing != null) return
        val t = Thread({
            var track: AudioTrack? = null
            try {
                val min = AudioTrack.getMinBufferSize(RATE, AudioFormat.CHANNEL_OUT_MONO, AudioFormat.ENCODING_PCM_16BIT)
                track = AudioTrack.Builder()
                    .setAudioAttributes(
                        AudioAttributes.Builder()
                            .setUsage(AudioAttributes.USAGE_MEDIA)
                            .setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION)
                            .build(),
                    )
                    .setAudioFormat(
                        AudioFormat.Builder()
                            .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                            .setSampleRate(RATE)
                            .setChannelMask(AudioFormat.CHANNEL_OUT_MONO)
                            .build(),
                    )
                    .setTransferMode(AudioTrack.MODE_STREAM)
                    .setBufferSizeInBytes(maxOf(min, RATE)) // half a second
                    .build()
                track.play()
                var off = 0
                while (off < pcm.size) {
                    val n = track.write(pcm, off, minOf(4096, pcm.size - off)) // blocks while the buffer is full
                    if (n <= 0) break
                    off += n
                }
                // Let what's buffered finish playing.
                val frames = pcm.size / 2
                val deadline = System.currentTimeMillis() + 3000
                while (track.playbackHeadPosition < frames && System.currentTimeMillis() < deadline) Thread.sleep(20)
            } catch (e: Exception) {
                Log.w(TAG, "playback failed", e)
            } finally {
                try {
                    track?.stop()
                } catch (e: IllegalStateException) {
                }
                track?.release()
                playing = null
                done()
            }
        }, "mesh-play")
        playing = t
        t.start()
    }

    private const val TAG = "SocialMeshSound"
}

/**
 * Records while on, handing each piece (16-bit mono PCM, and its rate) to
 * [onPcm]. With [capture] set, the last two minutes are also written there
 * as a WAV file, for working out why something wasn't heard (debug builds).
 */
internal class Microphone(private val capture: File? = null, private val onPcm: (ByteArray, Int) -> Unit) {
    @Volatile private var running = false

    val isOn: Boolean get() = running

    /** Needs RECORD_AUDIO. False if no microphone setting works. */
    @SuppressLint("MissingPermission")
    fun start(): Boolean {
        if (running) return true
        val (rec, rate) = open() ?: return false
        running = true
        Thread({
            val buf = ShortArray(rate / 4) // a quarter second per piece
            val kept = if (capture != null) Capture(rate) else null
            try {
                rec.startRecording()
                while (running) {
                    val n = rec.read(buf, 0, buf.size)
                    if (n < 0) break
                    if (n == 0) continue
                    val bytes = ByteArray(2 * n)
                    for (i in 0 until n) {
                        val v = buf[i].toInt()
                        bytes[2 * i] = v.toByte()
                        bytes[2 * i + 1] = (v shr 8).toByte()
                    }
                    kept?.add(bytes)
                    onPcm(bytes, rate)
                }
            } catch (e: Exception) {
                Log.w(TAG, "recording failed", e)
            } finally {
                running = false
                try {
                    rec.stop()
                } catch (e: IllegalStateException) {
                }
                rec.release()
                if (capture != null) kept?.save(capture)
            }
        }, "mesh-listen").start()
        return true
    }

    fun stop() {
        running = false
    }

    /** 8 kHz if the phone offers it (the modem's own rate), otherwise a common one to resample from. */
    @SuppressLint("MissingPermission")
    private fun open(): Pair<AudioRecord, Int>? {
        for (rate in intArrayOf(8000, 16000, 48000, 44100)) {
            val min = AudioRecord.getMinBufferSize(rate, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT)
            if (min <= 0) continue
            val r = try {
                // Voice recognition: the least processed source most phones have
                // (no noise suppression to eat the tones).
                AudioRecord(
                    MediaRecorder.AudioSource.VOICE_RECOGNITION, rate,
                    AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT, maxOf(min, 2 * rate),
                )
            } catch (e: IllegalArgumentException) {
                continue
            }
            if (r.state == AudioRecord.STATE_INITIALIZED) return r to rate
            r.release()
        }
        return null
    }

    /** The newest two minutes of audio, kept in memory and written out as WAV when listening stops. */
    private class Capture(private val rate: Int) {
        private val pieces = ArrayDeque<ByteArray>()
        private var bytes = 0

        fun add(b: ByteArray) {
            pieces.addLast(b)
            bytes += b.size
            while (bytes > rate * 2 * 120) bytes -= pieces.removeFirst().size
        }

        fun save(f: File) {
            try {
                f.outputStream().use { out ->
                    val h = ByteBuffer.allocate(44).order(ByteOrder.LITTLE_ENDIAN)
                    h.put("RIFF".toByteArray()).putInt(36 + bytes).put("WAVE".toByteArray())
                    h.put("fmt ".toByteArray()).putInt(16).putShort(1).putShort(1)
                    h.putInt(rate).putInt(rate * 2).putShort(2).putShort(16)
                    h.put("data".toByteArray()).putInt(bytes)
                    out.write(h.array())
                    for (p in pieces) out.write(p)
                }
            } catch (e: IOException) {
                Log.w(TAG, "couldn't save the capture", e)
            }
        }
    }

    companion object {
        private const val TAG = "SocialMeshSound"
    }
}
