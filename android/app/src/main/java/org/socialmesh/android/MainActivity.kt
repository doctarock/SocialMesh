package org.socialmesh.android

import android.Manifest
import android.app.Activity
import android.app.AlertDialog
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothManager
import android.content.Intent
import android.content.pm.ApplicationInfo
import android.content.pm.PackageManager
import android.graphics.Typeface
import android.os.Build
import android.os.Bundle
import android.text.InputType
import android.view.Gravity
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import org.json.JSONArray
import java.io.File

/**
 * The main screen: who you are and who you're linked with, writing a post,
 * sharing (sites, sound), and the feed. The node itself runs in
 * [MeshService], so it keeps going with the screen off or the app closed,
 * until stopped here or from the notification.
 */
class MainActivity : Activity() {
    private lateinit var who: TextView
    private lateinit var linked: TextView
    private lateinit var toggle: Button
    private lateinit var input: EditText
    private lateinit var post: Button
    private lateinit var feed: LinearLayout
    private lateinit var log: TextView
    private lateinit var logToggle: TextView
    private lateinit var radios: TextView
    private lateinit var play: Button
    private lateinit var listen: Button
    private lateinit var modem: Button
    /** Asked for the microphone for the sound modem once on this screen: don't nag. */
    private var askedModemMic = false

    private val listener = Mesh.Listener { line -> runOnUiThread { onLine(line) } }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val column = Ui.column(this)

        // You, and who you're linked with.
        who = Ui.text(this, "", 17f, bold = true)
        linked = Ui.note(this, "")
        radios = Ui.note(this, "")
        toggle = Button(this).apply { setOnClickListener { toggleNode() } }
        // Tapping the status offers to turn Bluetooth on, if it's off.
        val status = Ui.card(this, onClick = { offerBluetooth() }).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            addView(
                LinearLayout(this@MainActivity).apply {
                    orientation = LinearLayout.VERTICAL
                    addView(who)
                    addView(linked)
                    addView(radios)
                },
                LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f),
            )
            addView(toggle)
        }
        column.addView(status)

        // Write.
        input = EditText(this).apply {
            hint = "Write a post"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_MULTI_LINE or InputType.TYPE_TEXT_FLAG_CAP_SENTENCES
            minLines = 2
            maxLines = 8
            gravity = Gravity.TOP or Gravity.START
        }
        post = Button(this).apply {
            text = "Post"
            setOnClickListener { publish() }
        }
        val compose = Ui.card(this).apply {
            addView(input, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(
                LinearLayout(this@MainActivity).apply {
                    orientation = LinearLayout.HORIZONTAL
                    gravity = Gravity.CENTER_VERTICAL
                    addView(
                        Ui.note(this@MainActivity, "Signed by you. It reaches people as you meet them."),
                        LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f),
                    )
                    addView(post)
                },
                LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT),
            )
        }
        column.addView(compose)

        // Share.
        column.addView(Ui.heading(this, "Share"))
        val sites = Button(this).apply {
            text = "Sites"
            setOnClickListener { startActivity(Intent(this@MainActivity, SitesActivity::class.java)) }
        }
        val hosts = Button(this).apply {
            text = "Hosts"
            setOnClickListener { startActivity(Intent(this@MainActivity, HostsActivity::class.java)) }
        }
        column.addView(Ui.row(this, sites, hosts))

        // Sound: one post now, listening while this screen shows, or the modem.
        column.addView(Ui.heading(this, "Sound"))
        play = Button(this).apply {
            text = PLAY
            setOnClickListener { playNewest() }
        }
        listen = Button(this).apply {
            text = LISTEN
            setOnClickListener { toggleListening() }
        }
        modem = Button(this).apply { setOnClickListener { toggleModem() } }
        column.addView(Ui.row(this, play, listen, modem))

        // Posts.
        column.addView(Ui.heading(this, "Posts"))
        feed = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        column.addView(feed, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))

        // What the node is doing, for the curious.
        logToggle = Ui.note(this, "").apply {
            setPadding(Ui.dp(this@MainActivity, 4), Ui.dp(this@MainActivity, 16), 0, Ui.dp(this@MainActivity, 8))
            setOnClickListener { setLogShown(log.visibility != View.VISIBLE) }
        }
        log = TextView(this).apply {
            typeface = Typeface.MONOSPACE
            textSize = 11f
            setTextColor(Ui.secondary(this@MainActivity))
        }
        column.addView(logToggle)
        column.addView(log)
        setLogShown(false)

        setContentView(ScrollView(this).apply { addView(column) })
        if (!Mesh.running) requestPermissionsThenStart()
    }

    override fun onStart() {
        super.onStart()
        Mesh.addListener(listener)
        log.text = Mesh.recentLines().asReversed().joinToString("\n")
        Radios.update()
        showState()
        maybeAskModemMic()
    }

    override fun onStop() {
        stopListening()
        Mesh.removeListener(listener)
        super.onStop()
    }

    private fun setLogShown(shown: Boolean) {
        log.visibility = if (shown) View.VISIBLE else View.GONE
        logToggle.text = if (shown) "Hide activity ▴" else "Show activity ▾"
    }

    private fun say(text: String) = Toast.makeText(this, text, Toast.LENGTH_LONG).show()

    // ---- sound ----------------------------------------------------------------

    private fun playNewest() {
        if (Speaker.isPlaying) return
        val pcm = try {
            Mesh.soundOfNewest(false)
        } catch (e: RuntimeException) {
            say("Can't play: ${e.message}")
            return
        }
        if (pcm == null) {
            say("Start SocialMesh to play posts.")
            return
        }
        val seconds = pcm.size / 2 / 8000
        Mesh.report("Playing the newest post as sound (${seconds}s)")
        say("Playing for about $seconds seconds. Turn the volume up and hold it near the other phone.")
        play.text = "Playing…"
        play.isEnabled = false
        Modem.play(pcm) {
            runOnUiThread {
                play.text = PLAY
                play.isEnabled = true
            }
        }
    }

    private fun toggleListening() {
        if (listen.text != LISTEN) {
            stopListening()
        } else if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(arrayOf(Manifest.permission.RECORD_AUDIO), REQ_MIC)
        } else {
            startListening()
        }
    }

    private fun startListening() {
        if (!Mesh.running) {
            say("Start SocialMesh to listen.")
            return
        }
        if (Modem.setScreenListening(this, true)) {
            listen.text = "Stop"
            Mesh.report("Listening for posts by sound")
            say("Listening. Posts played nearby will appear in the feed.")
            showLinks()
        } else {
            say("Couldn't open the microphone.")
        }
    }

    private fun stopListening() {
        if (listen.text == LISTEN) return
        Modem.setScreenListening(this, false)
        listen.text = LISTEN
        showLinks()
    }

    private fun toggleModem() {
        if (!Mesh.running) {
            say("Start SocialMesh first.")
            return
        }
        if (!Modem.on && !Modem.micAllowed(this)) {
            requestPermissions(arrayOf(Manifest.permission.RECORD_AUDIO), REQ_MODEM_MIC)
            return
        }
        if (!Modem.on) {
            say("Sound modem on: this phone listens, and plays its newest posts every minute or so.")
        }
        Modem.toggle()
    }

    /** No Bluetooth or Wi-Fi: the modem wants the microphone. Ask once per visit. */
    private fun maybeAskModemMic() {
        if (!Mesh.running || !Modem.wanted || Modem.micAllowed(this) || askedModemMic) return
        askedModemMic = true
        AlertDialog.Builder(this)
            .setTitle("No Bluetooth or Wi-Fi")
            .setMessage(
                "SocialMesh can still pass posts on by sound: this phone listens, and every minute or so plays " +
                    "its newest public posts as tones for phones nearby. That needs the microphone.",
            )
            .setPositiveButton("Allow microphone") { _, _ ->
                requestPermissions(arrayOf(Manifest.permission.RECORD_AUDIO), REQ_MODEM_MIC)
            }
            .setNegativeButton("Not now", null)
            .show()
    }

    /** Offer to turn Bluetooth on (it's never forced). */
    @Suppress("MissingPermission")
    private fun offerBluetooth() {
        if (!Mesh.running || Radios.bluetooth) return
        val adapter = getSystemService(BluetoothManager::class.java)?.adapter
        when {
            adapter == null -> say("This phone has no Bluetooth.")
            !Radios.bluetoothAllowed(this) -> requestPermissions(neededPermissions().toTypedArray(), REQ_PERMISSIONS)
            else -> try {
                @Suppress("DEPRECATION")
                startActivityForResult(Intent(BluetoothAdapter.ACTION_REQUEST_ENABLE), REQ_ENABLE)
            } catch (e: RuntimeException) {
                say("Turn Bluetooth on in Settings.")
            }
        }
    }

    // ---- starting and stopping ---------------------------------------------------

    private fun toggleNode() {
        if (Mesh.running) {
            AlertDialog.Builder(this)
                .setTitle("Stop SocialMesh?")
                .setMessage("Nothing is sent or received until you start it again, and nearby people won't get your posts from this phone.")
                .setPositiveButton("Stop") { _, _ -> startService(MeshService.stopIntent(this)) }
                .setNegativeButton("Keep running", null)
                .show()
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
        if (missing.isEmpty()) startNode() else requestPermissions(missing.toTypedArray(), REQ_PERMISSIONS)
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, results: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, results)
        if (requestCode == REQ_MODEM_MIC) {
            if (Modem.micAllowed(this)) {
                if (Modem.needed) Modem.evaluate() else Modem.toggle()
            } else {
                say("Without the microphone, the sound modem can't hear posts.")
            }
            return
        }
        if (requestCode == REQ_MIC) {
            if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED) {
                startListening()
            } else {
                say("Microphone permission is needed to hear posts.")
            }
            return
        }
        if (requestCode != REQ_PERMISSIONS) return
        // Without Bluetooth the node still runs: Wi-Fi, hosts and sound work without it.
        if (Mesh.running) Radios.update() else startNode()
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        @Suppress("DEPRECATION")
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode == REQ_ENABLE) Radios.update()
    }

    private fun startNode() {
        if (Mesh.running) return
        linked.text = "Starting…"
        startForegroundService(MeshService.startIntent(this))
    }

    // ---- state ------------------------------------------------------------------

    private fun onLine(line: String) {
        log.text = (line + "\n" + log.text).take(4000)
        when {
            line == "Started" || line == "Stopped" || line.startsWith("Could not start") -> showState()
            line.startsWith("New post") -> refreshFeed()
            line.startsWith("New link key") -> showState()
            line.startsWith("Connected") || line.startsWith("Disconnected") -> showLinks()
            line.startsWith("Bluetooth ") || line.startsWith("Sound modem") || line.startsWith("Media volume") -> {
                showLinks()
                maybeAskModemMic()
            }
        }
        if (line.startsWith("Could not start")) say(line)
    }

    private fun showState() {
        if (Mesh.running) {
            val ids = Mesh.ids()?.split(" ")
            who.text = "You are ${ids?.getOrNull(0) ?: "…"}"
            toggle.text = "Stop"
            showLinks()
        } else {
            who.text = "SocialMesh is off"
            linked.text = "Nothing is sent or received."
            radios.text = ""
            toggle.text = "Start"
            modem.text = "Modem"
        }
        refreshFeed()
    }

    /** "Linked with 2 nearby: Bluetooth, Wi-Fi" from the node's live links. */
    private fun showLinks() {
        if (!Mesh.running) return
        val links = (Mesh.links() ?: "").lines().filter { it.isNotBlank() }.map { it.split(" ") }
        val peers = links.map { it[0] }.toSet().size
        val media = links.mapNotNull { it.getOrNull(1) }.toSet().map(Ui::medium).sorted()
        linked.text = when (peers) {
            0 -> "Looking for people nearby…"
            1 -> "Linked with 1 nearby · ${media.joinToString(", ")}"
            else -> "Linked with $peers nearby · ${media.joinToString(", ")}"
        }
        val sound = when {
            Modem.on && Modem.tooQuiet(this) -> " · sound modem on, volume low"
            Modem.on -> " · sound modem on"
            Modem.listening -> " · listening"
            else -> ""
        }
        radios.text = when {
            Radios.bluetooth || Modem.on -> Mesh.radioLine() + sound
            else -> "${Mesh.radioLine()}$sound (tap to turn Bluetooth on)"
        }
        modem.text = if (Modem.on) "Modem: on" else "Modem: off"
    }

    private fun publish() {
        val text = input.text.toString().trim()
        if (text.isEmpty()) {
            input.error = "Write something first"
            return
        }
        try {
            if (Mesh.publish(text) == null) {
                say("Start SocialMesh to post.")
                return
            }
            input.setText("")
            refreshFeed()
            Toast.makeText(this, "Posted", Toast.LENGTH_SHORT).show()
        } catch (e: RuntimeException) {
            say("Post failed: ${e.message}")
        }
    }

    private fun refreshFeed() {
        feed.removeAllViews()
        val json = Mesh.feedJson(50)
        if (json == null) {
            feed.addView(Ui.note(this, "Start SocialMesh to see posts."))
            return
        }
        val posts = JSONArray(json)
        if (posts.length() == 0) {
            feed.addView(Ui.note(this, "No posts yet. Write one, or bring another phone running SocialMesh close by."))
            return
        }
        for (i in 0 until posts.length()) {
            val p = posts.getJSONObject(i)
            val author = when {
                p.getBoolean("mine") -> "You"
                p.isNull("author") -> "Anonymous"
                else -> p.getString("author")
            }
            feed.addView(
                Ui.card(this).apply {
                    addView(Ui.note(this@MainActivity, "$author · ${Ui.ago(p.getLong("created_ms"))}"))
                    addView(Ui.text(this@MainActivity, p.getString("body"), 16f).apply { setTextIsSelectable(true) })
                },
            )
        }
    }

    companion object {
        private const val REQ_PERMISSIONS = 1
        private const val REQ_ENABLE = 2
        private const val REQ_MIC = 3
        private const val REQ_MODEM_MIC = 4
        private const val PLAY = "Play"
        private const val LISTEN = "Listen"
    }
}
