package org.socialmesh.android

import android.Manifest
import android.app.Activity
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothManager
import android.content.Intent
import android.content.pm.ApplicationInfo
import android.content.pm.PackageManager
import android.graphics.Typeface
import android.os.Build
import android.os.Bundle
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import org.json.JSONArray
import java.io.File
import java.text.DateFormat
import java.util.Date

/**
 * A minimal test app: start the node, post, see the feed and what the radio
 * is doing. Two phones running it in range should find each other and sync
 * with no setup.
 *
 * The node runs in [MeshService], so it keeps going with the screen off or
 * the app closed, until stopped here or from the notification.
 */
class MainActivity : Activity() {
    private lateinit var status: TextView
    private lateinit var toggle: Button
    private lateinit var input: EditText
    private lateinit var feed: TextView
    private lateinit var log: TextView
    private lateinit var play: Button
    private lateinit var listen: Button

    /** Only while this screen is visible: the microphone is never on in the background. */
    private val mic by lazy {
        // Debug builds keep the last two minutes heard, to find out why a post wasn't decoded.
        val debuggable = applicationInfo.flags and ApplicationInfo.FLAG_DEBUGGABLE != 0
        Microphone(if (debuggable) File(filesDir, "last-heard.wav") else null) { pcm, rate -> Mesh.hear(pcm, rate) }
    }

    private val listener = Mesh.Listener { line -> runOnUiThread { onLine(line) } }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val pad = (16 * resources.displayMetrics.density).toInt()
        status = TextView(this)
        toggle = Button(this).apply { setOnClickListener { toggleNode() } }
        input = EditText(this).apply { hint = "Write a post" }
        val post = Button(this).apply {
            text = "Post"
            setOnClickListener { publish() }
        }
        play = Button(this).apply {
            text = PLAY
            setOnClickListener { playNewest() }
        }
        listen = Button(this).apply {
            text = LISTEN
            setOnClickListener { toggleListening() }
        }
        val sound = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            addView(play, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
            addView(listen, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
        }
        feed = TextView(this).apply { setPadding(0, pad, 0, pad) }
        log = TextView(this).apply {
            typeface = Typeface.MONOSPACE
            textSize = 11f
        }
        val column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(pad, pad, pad, pad)
            addView(status)
            addView(toggle)
            addView(input, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(post)
            addView(sound, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(feed)
            addView(log)
        }
        setContentView(ScrollView(this).apply { addView(column) })
        if (!Mesh.running) requestPermissionsThenStart()
    }

    override fun onStart() {
        super.onStart()
        Mesh.addListener(listener)
        log.text = Mesh.recentLines().asReversed().joinToString("\n")
        showState()
    }

    override fun onStop() {
        stopListening()
        Mesh.removeListener(listener)
        super.onStop()
    }

    private fun playNewest() {
        if (Speaker.isPlaying) return
        val pcm = try {
            Mesh.soundOfNewest(false)
        } catch (e: RuntimeException) {
            log.text = "Can't play: ${e.message}\n" + log.text
            return
        }
        if (pcm == null) {
            log.text = "Start SocialMesh to play posts.\n" + log.text
            return
        }
        val seconds = pcm.size / 2 / 8000
        Mesh.report("Playing the newest post as sound (${seconds}s)")
        play.text = "Playing…"
        play.isEnabled = false
        Speaker.play(pcm) {
            runOnUiThread {
                play.text = PLAY
                play.isEnabled = true
            }
        }
    }

    private fun toggleListening() {
        if (mic.isOn) {
            stopListening()
        } else if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(arrayOf(Manifest.permission.RECORD_AUDIO), REQ_MIC)
        } else {
            startListening()
        }
    }

    private fun startListening() {
        if (!Mesh.running) {
            log.text = "Start SocialMesh to listen.\n" + log.text
            return
        }
        if (mic.start()) {
            listen.text = "Stop listening"
            Mesh.report("Listening for posts by sound")
        } else {
            log.text = "Couldn't open the microphone.\n" + log.text
        }
    }

    private fun stopListening() {
        if (!mic.isOn) return
        mic.stop()
        Mesh.stopHearing()
        listen.text = LISTEN
    }

    private fun toggleNode() {
        if (Mesh.running) {
            startService(MeshService.stopIntent(this))
        } else {
            requestPermissionsThenStart()
        }
    }

    private fun neededPermissions(): List<String> {
        val p = mutableListOf<String>()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            p += Manifest.permission.BLUETOOTH_SCAN
            p += Manifest.permission.BLUETOOTH_ADVERTISE
            p += Manifest.permission.BLUETOOTH_CONNECT
        } else {
            p += Manifest.permission.ACCESS_FINE_LOCATION
        }
        return p
    }

    private fun requestPermissionsThenStart() {
        val wanted = neededPermissions().toMutableList()
        // Optional: without it the node still runs, but its notification is hidden.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) wanted += Manifest.permission.POST_NOTIFICATIONS
        val missing = wanted.filter { checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }
        if (missing.isEmpty()) ensureBluetoothOn() else requestPermissions(missing.toTypedArray(), REQ_PERMISSIONS)
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, results: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, results)
        if (requestCode == REQ_MIC) {
            if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED) {
                startListening()
            } else {
                log.text = "Microphone permission is needed to hear posts.\n" + log.text
            }
            return
        }
        if (requestCode != REQ_PERMISSIONS) return
        if (neededPermissions().all { checkSelfPermission(it) == PackageManager.PERMISSION_GRANTED }) {
            ensureBluetoothOn()
        } else {
            status.text = "Bluetooth permission is needed to find people nearby."
        }
    }

    @Suppress("MissingPermission")
    private fun ensureBluetoothOn() {
        val adapter = getSystemService(BluetoothManager::class.java).adapter
        if (adapter == null) {
            status.text = "This device has no Bluetooth."
        } else if (!adapter.isEnabled) {
            @Suppress("DEPRECATION")
            startActivityForResult(Intent(BluetoothAdapter.ACTION_REQUEST_ENABLE), REQ_ENABLE)
        } else {
            startNode()
        }
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        @Suppress("DEPRECATION")
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode == REQ_ENABLE) {
            if (resultCode == RESULT_OK) startNode() else status.text = "Bluetooth is off."
        }
    }

    private fun startNode() {
        status.text = "Starting…"
        startForegroundService(MeshService.startIntent(this))
    }

    private fun onLine(line: String) {
        log.text = (line + "\n" + log.text).take(4000)
        when {
            line == "Started" || line == "Stopped" || line.startsWith("Could not start") -> showState()
            line.startsWith("New post") -> refreshFeed()
            line.startsWith("New link key") -> showState()
        }
    }

    private fun showState() {
        if (Mesh.running) {
            val ids = Mesh.ids()?.split(" ")
            status.text = "You are ${ids?.getOrNull(0)} · link key ${ids?.getOrNull(1)} (changes every run and daily)"
            toggle.text = "Stop"
        } else {
            status.text = "Off. Nothing is sent or received."
            toggle.text = "Start"
        }
        refreshFeed()
    }

    private fun publish() {
        val text = input.text.toString().trim()
        if (text.isEmpty()) return
        try {
            if (Mesh.publish(text) == null) {
                log.text = "Start SocialMesh to post.\n" + log.text
                return
            }
            input.setText("")
            refreshFeed()
        } catch (e: RuntimeException) {
            log.text = "Post failed: ${e.message}\n" + log.text
        }
    }

    private fun refreshFeed() {
        val json = Mesh.feedJson(50)
        if (json == null) {
            feed.text = ""
            return
        }
        val posts = JSONArray(json)
        val time = DateFormat.getDateTimeInstance(DateFormat.SHORT, DateFormat.SHORT)
        val sb = StringBuilder()
        for (i in 0 until posts.length()) {
            val p = posts.getJSONObject(i)
            val who = when {
                p.getBoolean("mine") -> "you"
                p.isNull("author") -> "anonymous"
                else -> p.getString("author")
            }
            sb.append(who).append(" · ").append(time.format(Date(p.getLong("created_ms")))).append('\n')
            sb.append(p.getString("body")).append("\n\n")
        }
        feed.text = if (sb.isEmpty()) "No posts yet. Bring another phone running SocialMesh close by." else sb
    }

    companion object {
        private const val REQ_PERMISSIONS = 1
        private const val REQ_ENABLE = 2
        private const val REQ_MIC = 3
        private const val PLAY = "Play newest post"
        private const val LISTEN = "Listen"
    }
}
