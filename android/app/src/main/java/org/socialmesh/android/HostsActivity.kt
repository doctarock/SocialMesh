package org.socialmesh.android

import android.app.Activity
import android.app.AlertDialog
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.view.MenuItem
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.view.inputmethod.EditorInfo
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import org.json.JSONObject

/**
 * Hosts: nodes reached by address rather than found by radio, such as a
 * computer at home or a relay someone runs. Type an address or paste an
 * invitation; nodes announcing themselves on this Wi-Fi are listed so they
 * can be kept for later. Saved hosts are redialled whenever there's a
 * network (Wi-Fi or mobile data).
 */
class HostsActivity : Activity() {
    private lateinit var input: EditText
    private lateinit var add: Button
    private lateinit var result: TextView
    private lateinit var list: LinearLayout
    private val handler = Handler(Looper.getMainLooper())
    private val tick = object : Runnable {
        override fun run() {
            refresh()
            handler.postDelayed(this, 3000)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        title = "Hosts"
        actionBar?.setDisplayHomeAsUpEnabled(true)
        val crumbs = Crumbs.view(this)
        Crumbs.set(crumbs, listOf(Crumbs.Crumb("SocialMesh", Crumbs.home(this)), Crumbs.Crumb("Hosts", null)))

        val column = Ui.column(this)
        column.addView(
            Ui.note(
                this,
                "Phones nearby are found by themselves. A host is a node you reach by its address: a computer " +
                    "running `mesh run`, or a relay someone shares. Everything sent is encrypted, and the node " +
                    "that answers is remembered, so a different one at that address later is refused.",
            ),
        )
        column.addView(Ui.heading(this, "Add a host"))
        input = EditText(this).apply {
            hint = "192.168.1.20, relay.example.org:7842, or mesh1:…"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_URI
            isSingleLine = true
            imeOptions = EditorInfo.IME_ACTION_GO
            setOnEditorActionListener { _, _, _ ->
                addTyped()
                true
            }
        }
        add = Button(this).apply {
            text = "Add"
            setOnClickListener { addTyped() }
        }
        column.addView(
            LinearLayout(this).apply {
                orientation = LinearLayout.HORIZONTAL
                addView(input, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
                addView(add)
            },
        )
        result = Ui.note(this, "")
        column.addView(result)
        list = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        column.addView(list)

        setContentView(
            LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                addView(crumbs, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
                addView(ScrollView(this@HostsActivity).apply { addView(column) }, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
            },
        )
    }

    override fun onOptionsItemSelected(item: MenuItem): Boolean {
        if (item.itemId == android.R.id.home) {
            finish()
            return true
        }
        return super.onOptionsItemSelected(item)
    }

    override fun onStart() {
        super.onStart()
        handler.post(tick)
    }

    override fun onStop() {
        handler.removeCallbacks(tick)
        super.onStop()
    }

    private fun addTyped() {
        val text = input.text.toString().trim()
        if (text.isEmpty()) {
            input.error = "Type an address or paste an invitation"
            return
        }
        save(text) { input.setText("") }
    }

    /** Try the address (takes a few seconds), save it, and say what happened. */
    private fun save(what: String, then: () -> Unit = {}) {
        add.isEnabled = false
        result.text = "Trying $what…"
        Thread {
            val r = try {
                Result.success(Mesh.addHost(what) ?: throw RuntimeException("Start SocialMesh first"))
            } catch (e: RuntimeException) {
                Result.failure(e)
            }
            runOnUiThread {
                add.isEnabled = true
                r.onSuccess {
                    result.text = it
                    then()
                }
                r.onFailure { result.text = it.message ?: "Couldn't add that" }
                refresh()
            }
        }.start()
    }

    private fun refresh() {
        list.removeAllViews()
        val json = try {
            Mesh.hostsJson()
        } catch (e: RuntimeException) {
            list.addView(Ui.note(this, "Couldn't list hosts: ${e.message}"))
            return
        }
        if (json == null) {
            list.addView(Ui.note(this, "Start SocialMesh to see hosts."))
            return
        }
        val o = JSONObject(json)
        val saved = o.getJSONArray("saved")
        list.addView(Ui.heading(this, "Saved"))
        if (saved.length() == 0) list.addView(Ui.note(this, "None yet."))
        for (i in 0 until saved.length()) {
            val h = saved.getJSONObject(i)
            val addr = h.getString("addr")
            val state = when {
                h.getBoolean("linked") -> "Linked now"
                h.isNull("last_seen_ms") -> "Not reached yet; trying when there's a network"
                else -> "Last linked ${Ui.ago(h.getLong("last_seen_ms"))}"
            }
            val id = if (h.isNull("id")) "" else " · ${h.getString("id")}"
            list.addView(
                Ui.card(this, onClick = { confirmRemove(addr) }, onLongClick = { Ui.copy(this, "Address", addr) }).apply {
                    addView(Ui.text(this@HostsActivity, addr, 16f, bold = true))
                    addView(Ui.note(this@HostsActivity, "$state$id"))
                },
            )
        }

        val found = o.getJSONArray("found")
        list.addView(Ui.heading(this, "On this network"))
        list.addView(
            Ui.note(
                this,
                if (found.length() == 0) {
                    "None found. Nodes on the same Wi-Fi announce themselves and are linked with by themselves."
                } else {
                    "Linked with by themselves while you're on this network. Tap one to keep it for later."
                },
            ),
        )
        for (i in 0 until found.length()) {
            val h = found.getJSONObject(i)
            val addr = h.getString("addr")
            val id = if (h.isNull("id")) "" else " · ${h.getString("id")}"
            list.addView(
                Ui.card(this, onClick = { save(addr) }).apply {
                    addView(Ui.text(this@HostsActivity, addr, 16f, bold = true))
                    addView(Ui.note(this@HostsActivity, (if (h.getBoolean("linked")) "Linked now" else "Seen just now") + id))
                },
            )
        }
    }

    private fun confirmRemove(addr: String) {
        AlertDialog.Builder(this)
            .setTitle("Forget $addr?")
            .setMessage("This phone stops connecting to it. Posts already here stay.")
            .setPositiveButton("Forget") { _, _ ->
                try {
                    Mesh.removeHost(addr)
                } catch (e: RuntimeException) {
                    result.text = e.message
                }
                refresh()
            }
            .setNegativeButton("Keep", null)
            .show()
    }
}
