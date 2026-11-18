package org.socialmesh.android

import android.app.Activity
import android.content.Intent
import android.os.Bundle
import android.view.MenuItem
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import org.json.JSONArray
import org.json.JSONObject

/** The sites this phone holds: yours first, then everyone else's. Tap to open; long-press to copy the address. */
class SitesActivity : Activity() {
    private lateinit var crumbs: TextView
    private lateinit var list: LinearLayout
    private val listener = Mesh.Listener { line ->
        if (line.startsWith("New site") || line.startsWith("Published")) runOnUiThread { refresh() }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        title = "Sites"
        actionBar?.setDisplayHomeAsUpEnabled(true)
        crumbs = Crumbs.view(this)
        Crumbs.set(crumbs, listOf(Crumbs.Crumb("SocialMesh", Crumbs.home(this)), Crumbs.Crumb("Sites", null)))
        list = Ui.column(this)
        val publish = Button(this).apply {
            text = "Publish a site"
            setOnClickListener { startActivity(Intent(this@SitesActivity, PublishSiteActivity::class.java)) }
        }
        setContentView(
            LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                addView(crumbs, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
                addView(ScrollView(this@SitesActivity).apply { addView(list) }, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
                addView(
                    LinearLayout(this@SitesActivity).apply {
                        val p = Ui.dp(this@SitesActivity, 12)
                        setPadding(p, 0, p, p)
                        addView(publish, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
                    },
                )
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
            list.addView(Ui.note(this, "Couldn't list sites: ${e.message}"))
            return
        }
        if (json == null) {
            list.addView(Ui.note(this, "Start SocialMesh to see sites."))
            return
        }
        val mine = HashSet<String>()
        try {
            Mesh.mySitesJson()?.let { j ->
                val a = JSONArray(j)
                for (i in 0 until a.length()) mine += a.getJSONObject(i).getString("address")
            }
        } catch (e: RuntimeException) {
        }
        val all = JSONArray(json)
        val sites = (0 until all.length()).map { all.getJSONObject(it) }
        val (yours, others) = sites.partition { it.getString("address") in mine }
        list.addView(
            Ui.note(
                this,
                "Sites are web pages passed around like posts, with no server. They run no scripts and " +
                    "load nothing from outside, so opening one tells no website you read it.",
            ),
        )
        if (yours.isNotEmpty()) {
            list.addView(Ui.heading(this, "Yours"))
            for (s in yours) list.addView(card(s, true))
        }
        list.addView(Ui.heading(this, if (yours.isEmpty()) "Sites" else "From others"))
        if (others.isEmpty()) {
            list.addView(Ui.note(this, "None yet. They arrive from nearby phones and computers running SocialMesh."))
        }
        for (s in others) list.addView(card(s, false))
    }

    private fun card(s: JSONObject, yours: Boolean): LinearLayout {
        val have = s.getInt("have")
        val files = s.getInt("files")
        val title = s.getString("title")
        val address = s.getString("address")
        val state = if (have < files) "$have of $files files here" else "$files files"
        val kind = (if (s.optBoolean("app")) " · app" else "") + when {
            s.getBoolean("ownerless") -> " · anonymous"
            yours -> " · yours"
            else -> ""
        }
        return Ui.card(
            this,
            onClick = {
                startActivity(
                    Intent(this, SiteActivity::class.java)
                        .putExtra(SiteActivity.EXTRA_ADDRESS, address)
                        .putExtra(SiteActivity.EXTRA_TITLE, title),
                )
            },
            onLongClick = { Ui.copy(this, "Site address", address) },
        ).apply {
            addView(Ui.text(this@SitesActivity, title, 17f, bold = true))
            addView(
                Ui.note(
                    this@SitesActivity,
                    "Version ${s.getInt("version")} · $state · ${Ui.size(s.getLong("size"))} · ${s.getString("short")}$kind",
                ),
            )
        }
    }
}
