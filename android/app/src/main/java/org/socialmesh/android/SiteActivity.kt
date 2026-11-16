package org.socialmesh.android

import android.app.Activity
import android.app.AlertDialog
import android.content.ActivityNotFoundException
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.Menu
import android.view.MenuItem
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.webkit.CookieManager
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import org.json.JSONArray
import java.io.ByteArrayInputStream
import java.nio.ByteBuffer
import java.nio.charset.StandardCharsets

/**
 * Shows one site (SPEC.md §10.4). Every request the page makes is answered
 * here from the node: nothing is loaded from the network, scripts are off,
 * and links to other websites open only after the reader agrees, in their
 * own browser. Pages still downloading appear when they arrive.
 */
class SiteActivity : Activity() {
    private lateinit var web: WebView
    private lateinit var crumbs: TextView
    private lateinit var status: TextView
    private var address = ""
    private var siteTitle = ""
    private var index = "index.html"
    private var complete = false
    private var waitingForFiles = false
    private val handler = Handler(Looper.getMainLooper())

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        address = intent.getStringExtra(EXTRA_ADDRESS) ?: return finish()
        siteTitle = intent.getStringExtra(EXTRA_TITLE) ?: "Site"
        title = siteTitle
        actionBar?.setDisplayHomeAsUpEnabled(true)
        crumbs = Crumbs.view(this)
        status = Ui.note(this, "").apply {
            textSize = 12f
            val p = Ui.dp(this@SiteActivity, 16)
            setPadding(p, Ui.dp(this@SiteActivity, 4), p, Ui.dp(this@SiteActivity, 6))
        }
        web = WebView(this).apply {
            settings.javaScriptEnabled = false
            settings.allowFileAccess = false
            settings.allowContentAccess = false
            settings.blockNetworkLoads = true
            settings.domStorageEnabled = false
            settings.setGeolocationEnabled(false)
            settings.mediaPlaybackRequiresUserGesture = true
            settings.safeBrowsingEnabled = false // no URL lookups sent to Google
            settings.builtInZoomControls = true
            settings.displayZoomControls = false
            webViewClient = Client()
            setDownloadListener { _, _, _, _, _ -> } // downloads are not offered from sites yet
        }
        CookieManager.getInstance().setAcceptThirdPartyCookies(web, false)
        setContentView(
            LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                addView(crumbs, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
                addView(status, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
                addView(web, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
            },
        )
        showCrumbs(null)
        val first = try {
            Mesh.siteWant(address)
        } catch (e: RuntimeException) {
            Toast.makeText(this, "Site not available: ${e.message}", Toast.LENGTH_LONG).show()
            return finish()
        }
        if (first == null) {
            Toast.makeText(this, "Start SocialMesh to view sites.", Toast.LENGTH_LONG).show()
            return finish()
        }
        index = first
        web.loadUrl(url(index))
        watchProgress()
    }

    private fun url(path: String) = "https://$HOST/$path"

    /** `SocialMesh › Sites › <site> › <page>`; the page crumb is left out on the first page. */
    private fun showCrumbs(page: String?) {
        val trail = mutableListOf(
            Crumbs.Crumb("SocialMesh", Crumbs.home(this)),
            Crumbs.Crumb("Sites", Crumbs.sites(this)),
            Crumbs.Crumb(siteTitle) { web.loadUrl(url(index)) },
        )
        if (page != null) trail += Crumbs.Crumb(page, null)
        Crumbs.set(crumbs, trail)
    }

    /** Until every file is here: show progress, and reload once complete. */
    private fun watchProgress() {
        val p = Mesh.siteProgress(address)
        if (p == null) {
            status.text = "SocialMesh is stopped: pages already here still show."
        } else {
            val (have, all) = p
            status.text = if (have < all) {
                "$have of $all files here, fetching the rest from nearby. Scripts off; nothing loads from outside."
            } else {
                "Scripts off; nothing loads from outside this site."
            }
            if (have == all && !complete) {
                if (have > 0 && waitingForFiles) web.reload()
                complete = true
            }
            waitingForFiles = have < all
        }
        if (!complete) handler.postDelayed(::watchProgress, 2000)
    }

    override fun onCreateOptionsMenu(menu: Menu): Boolean {
        menu.add(0, MENU_RELOAD, 0, "Reload")
        menu.add(0, MENU_ABOUT, 1, "About this site")
        menu.add(0, MENU_COPY, 2, "Copy address")
        return true
    }

    override fun onOptionsItemSelected(item: MenuItem): Boolean {
        when (item.itemId) {
            android.R.id.home -> finish()
            MENU_RELOAD -> web.reload()
            MENU_ABOUT -> about()
            MENU_COPY -> Ui.copy(this, "Site address", address)
            else -> return super.onOptionsItemSelected(item)
        }
        return true
    }

    private fun about() {
        val s = try {
            Mesh.sitesJson()?.let { j -> JSONArray(j).let { a -> (0 until a.length()).map { a.getJSONObject(it) } } }
                ?.firstOrNull { it.getString("address") == address }
        } catch (e: RuntimeException) {
            null
        } ?: return
        val who = if (s.getBoolean("ownerless")) {
            "Anonymous: published with no key, so it can never be updated or withdrawn."
        } else {
            "Signed by its own site key: only whoever holds that key can publish new versions. " +
                "The title proves nothing; the address is the site."
        }
        AlertDialog.Builder(this)
            .setTitle(s.getString("title"))
            .setMessage(
                "Version ${s.getInt("version")} · ${s.getInt("files")} files · ${Ui.size(s.getLong("size"))}\n\n" +
                    "$who\n\nAddress:\n$address",
            )
            .setPositiveButton("Copy address") { _, _ -> Ui.copy(this, "Site address", address) }
            .setNegativeButton("Close", null)
            .show()
    }

    override fun onDestroy() {
        handler.removeCallbacksAndMessages(null)
        web.destroy()
        super.onDestroy()
    }

    @Deprecated("Deprecated in Java")
    override fun onBackPressed() {
        if (web.canGoBack()) web.goBack() else @Suppress("DEPRECATION") super.onBackPressed()
    }

    private inner class Client : WebViewClient() {
        override fun shouldInterceptRequest(view: WebView, request: WebResourceRequest): WebResourceResponse {
            val url = request.url
            if (url.scheme != "https" || url.host != HOST) return blocked()
            val path = (url.encodedPath ?: "/").removePrefix("/")
            val packed = Mesh.siteFile(address, path) ?: return blocked()
            return unpack(packed)
        }

        override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean {
            val url = request.url
            if (url.scheme == "https" && url.host == HOST) return false
            if (url.scheme in setOf("http", "https", "mailto")) {
                confirmLeaving(url)
            } else {
                Toast.makeText(this@SiteActivity, "Sites can't open that kind of link.", Toast.LENGTH_SHORT).show()
            }
            return true
        }

        override fun onPageFinished(view: WebView, url: String) {
            val path = Uri.parse(url).encodedPath?.removePrefix("/") ?: ""
            if (path == index || path.isEmpty()) {
                showCrumbs(null)
            } else {
                // The page's own <title> if it has one, otherwise its file name.
                val t = view.title?.takeIf { it.isNotBlank() && !it.startsWith(HOST) && !it.startsWith("https://") }
                showCrumbs(t ?: path.substringAfterLast('/'))
            }
        }
    }

    private fun confirmLeaving(url: Uri) {
        AlertDialog.Builder(this)
            .setTitle("Leave SocialMesh?")
            .setMessage(
                "This link goes to\n\n$url\n\nOpening it connects to that website directly, " +
                    "like any browser: it will see your IP address.",
            )
            .setPositiveButton("Open") { _, _ ->
                try {
                    startActivity(Intent(Intent.ACTION_VIEW, url))
                } catch (e: ActivityNotFoundException) {
                    Toast.makeText(this, "No app can open that link.", Toast.LENGTH_SHORT).show()
                }
            }
            .setNegativeButton("Stay", null)
            .show()
    }

    companion object {
        const val EXTRA_ADDRESS = "address"
        const val EXTRA_TITLE = "title"
        private const val MENU_RELOAD = 1
        private const val MENU_ABOUT = 2
        private const val MENU_COPY = 3

        /** Pages appear to come from here; it never resolves (.invalid), and every request is answered locally. */
        private const val HOST = "site.socialmesh.invalid"

        private fun blocked() =
            WebResourceResponse("text/plain", null, 403, "Blocked", emptyMap(), ByteArrayInputStream(ByteArray(0)))

        /** `u16 status | str media_type | u16 n | n × (str, str) | body` (see Native.siteFile). */
        private fun unpack(b: ByteArray): WebResourceResponse {
            val r = ByteBuffer.wrap(b)
            fun str(): String {
                val n = r.short.toInt() and 0xffff
                val out = ByteArray(n)
                r.get(out)
                return String(out, StandardCharsets.UTF_8)
            }
            val status = r.short.toInt() and 0xffff
            val type = str()
            val headers = HashMap<String, String>()
            repeat(r.short.toInt() and 0xffff) {
                val k = str()
                headers[k] = str()
            }
            headers.remove("Content-Type")
            val body = ByteArray(r.remaining())
            r.get(body)
            val mime = type.substringBefore(';').trim()
            val charset = type.substringAfter("charset=", "").trim().ifEmpty { null }
            val reason = when (status) {
                200 -> "OK"
                404 -> "Not Found"
                503 -> "Service Unavailable"
                else -> "Error"
            }
            return WebResourceResponse(mime, charset, status, reason, headers, ByteArrayInputStream(body))
        }
    }
}
