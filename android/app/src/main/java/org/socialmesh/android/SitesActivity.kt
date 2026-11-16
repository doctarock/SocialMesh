package org.socialmesh.android

import android.app.Activity
import android.content.Intent
import android.os.Bundle
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import org.json.JSONArray

/** The sites this phone holds, newest first. Tap one to open it. */
class SitesActivity : Activity() {
    private lateinit var list: LinearLayout
    private val listener = Mesh.Listener { line ->
        if (line.startsWith("New site")) runOnUiThread { refresh() }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        title = "Sites"
        val pad = (16 * resources.displayMetrics.density).toInt()
        list = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(pad, pad, pad, pad)
        }
        val publish = android.widget.Button(this).apply {
            text = "Publish a site"
            setOnClickListener { startActivity(Intent(this@SitesActivity, PublishSiteActivity::class.java)) }
        }
        setContentView(
            LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(pad, pad / 2, pad, 0)
                addView(publish, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
                addView(ScrollView(this@SitesActivity).apply { addView(list) }, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
            },
        )
    }

    override fun onStart() {
        super.onStart()
        Mesh.addListener(listener)
        refresh()
    }

    override fun onStop() {
        Mesh.removeListener(listener)
        super.onStop()
    }

    private fun refresh() {
        list.removeAllViews()
        val json = try {
            Mesh.sitesJson()
        } catch (e: RuntimeException) {
            note("Couldn't list sites: ${e.message}")
            return
        }
        if (json == null) {
            note("Start SocialMesh to see sites.")
            return
        }
        val sites = JSONArray(json)
        if (sites.length() == 0) {
            note(
                "No sites yet. Sites are web pages passed around like posts: they arrive from " +
                    "nearby phones and computers running SocialMesh.",
            )
            return
        }
        note("Sites run no scripts and load nothing from outside, so opening one tells no website you read it.")
        for (i in 0 until sites.length()) {
            val s = sites.getJSONObject(i)
            val have = s.getInt("have")
            val files = s.getInt("files")
            val state = if (have < files) "$have of $files files here" else "$files files"
            val who = if (s.getBoolean("ownerless")) " · anonymous" else ""
            val b = Button(this).apply {
                isAllCaps = false
                text = "${s.getString("title")}\nversion ${s.getInt("version")} · $state · ${size(s.getLong("size"))} · ${s.getString("short")}$who"
                setOnClickListener {
                    startActivity(
                        Intent(this@SitesActivity, SiteActivity::class.java)
                            .putExtra(SiteActivity.EXTRA_ADDRESS, s.getString("address"))
                            .putExtra(SiteActivity.EXTRA_TITLE, s.getString("title")),
                    )
                }
            }
            list.addView(b, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
        }
    }

    private fun note(text: String) {
        list.addView(TextView(this).apply { this.text = text })
    }

    private fun size(n: Long): String = when {
        n < 1024 -> "$n B"
        n < 1024 * 1024 -> "${n / 1024} KB"
        else -> String.format("%.1f MB", n / 1048576.0)
    }
}
