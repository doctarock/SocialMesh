package org.socialmesh.android

import android.annotation.SuppressLint
import android.app.Activity
import android.app.AlertDialog
import android.content.ActivityNotFoundException
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.webkit.CookieManager
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
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
    private lateinit var bar: TextView
    private var address = ""
    private var siteTitle = ""
    private var complete = false
    private val handler = Handler(Looper.getMainLooper())

    @SuppressLint("SetJavaScriptEnabled")
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        address = intent.getStringExtra(EXTRA_ADDRESS) ?: return finish()
        siteTitle = intent.getStringExtra(EXTRA_TITLE) ?: "Site"
        title = siteTitle
        val pad = (8 * resources.displayMetrics.density).toInt()
        bar = TextView(this).apply {
            setPadding(pad * 2, pad, pad * 2, pad)
            textSize = 12f
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
                addView(bar)
                addView(web, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
            },
        )
        val index = try {
            Mesh.siteWant(address)
        } catch (e: RuntimeException) {
            Toast.makeText(this, "Site not available: ${e.message}", Toast.LENGTH_LONG).show()
            return finish()
        }
        if (index == null) {
            Toast.makeText(this, "Start SocialMesh to view sites.", Toast.LENGTH_LONG).show()
            return finish()
        }
        web.loadUrl("https://$HOST/$index")
        watchProgress()
    }

    /** Until every file is here: show progress, and reload once complete. */
    private fun watchProgress() {
        val p = Mesh.siteProgress(address)
        if (p == null) {
            bar.text = "$siteTitle · SocialMesh is stopped"
        } else {
            val (have, all) = p
            val state = if (have < all) "$have of $all files here, fetching the rest" else "all $all files here"
            bar.text = "$siteTitle · $state · scripts off, nothing loads from outside"
            if (have == all && !complete) {
                if (have > 0 && waitingForFiles) web.reload()
                complete = true
            }
            waitingForFiles = have < all
        }
        if (!complete) handler.postDelayed(::watchProgress, 2000)
    }

    private var waitingForFiles = false

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
