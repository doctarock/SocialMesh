package org.socialmesh.android

import android.Manifest
import android.app.Activity
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothManager
import android.content.Intent
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
 * The node only runs while this screen is open; background running (a
 * foreground service) comes later.
 */
class MainActivity : Activity() {
    private var handle = 0L
    private var driver: BleDriver? = null
    @Volatile private var watching = false

    private lateinit var status: TextView
    private lateinit var input: EditText
    private lateinit var feed: TextView
    private lateinit var log: TextView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val pad = (16 * resources.displayMetrics.density).toInt()
        status = TextView(this).apply { text = "Starting…" }
        input = EditText(this).apply { hint = "Write a post" }
        val post = Button(this).apply {
            text = "Post"
            setOnClickListener { publish() }
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
            addView(input, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(post)
            addView(feed)
            addView(log)
        }
        setContentView(ScrollView(this).apply { addView(column) })
        requestPermissionsThenStart()
    }

    private fun neededPermissions(): Array<String> =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            arrayOf(
                Manifest.permission.BLUETOOTH_SCAN,
                Manifest.permission.BLUETOOTH_ADVERTISE,
                Manifest.permission.BLUETOOTH_CONNECT,
            )
        } else {
            arrayOf(Manifest.permission.ACCESS_FINE_LOCATION)
        }

    private fun requestPermissionsThenStart() {
        val missing = neededPermissions().filter { checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }
        if (missing.isEmpty()) ensureBluetoothOn() else requestPermissions(missing.toTypedArray(), REQ_PERMISSIONS)
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, results: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, results)
        if (requestCode != REQ_PERMISSIONS) return
        if (results.isNotEmpty() && results.all { it == PackageManager.PERMISSION_GRANTED }) {
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
        if (handle != 0L) return
        val home = File(filesDir, "mesh").absolutePath
        Thread {
            val h = try {
                Native.start(home, true, true)
            } catch (e: RuntimeException) {
                runOnUiThread { status.text = "Could not start: ${e.message}" }
                return@Thread
            }
            runOnUiThread {
                handle = h
                driver = BleDriver(this, h).also { it.start() }
                val ids = Native.ids(h)?.split(" ")
                status.text = "You are ${ids?.getOrNull(0)} · link key ${ids?.getOrNull(1)} (changes every run)"
                refreshFeed()
                watchEvents()
            }
        }.start()
    }

    private fun watchEvents() {
        watching = true
        val h = handle
        Thread({
            while (watching) {
                val line = Native.nextEvent(h, 500) ?: continue
                runOnUiThread {
                    log.text = (line + "\n" + log.text).take(4000)
                    if (line.startsWith("New post")) refreshFeed()
                }
            }
        }, "mesh-events").start()
    }

    private fun publish() {
        val text = input.text.toString().trim()
        if (text.isEmpty() || handle == 0L) return
        try {
            Native.publish(handle, text)
            input.setText("")
            refreshFeed()
        } catch (e: RuntimeException) {
            log.text = "Post failed: ${e.message}\n" + log.text
        }
    }

    private fun refreshFeed() {
        val json = Native.feedJson(handle, 50) ?: return
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

    override fun onDestroy() {
        watching = false
        val h = handle
        handle = 0
        val d = driver
        driver = null
        if (h != 0L) {
            // Off the UI thread: stopping waits for links and threads to wind down.
            Thread {
                d?.stop()
                Native.stop(h)
                Thread.sleep(600) // let the event watcher's last poll return
                Native.free(h)
            }.start()
        }
        super.onDestroy()
    }

    companion object {
        private const val REQ_PERMISSIONS = 1
        private const val REQ_ENABLE = 2
    }
}
