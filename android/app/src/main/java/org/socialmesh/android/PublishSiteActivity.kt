package org.socialmesh.android

import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.RadioButton
import android.widget.RadioGroup
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import org.json.JSONArray
import java.io.ByteArrayOutputStream
import java.io.DataOutputStream
import java.io.IOException
import java.io.InputStream
import java.util.zip.ZipInputStream

/**
 * Publish a folder or a .zip of web pages as a site (SPEC.md §10): a new
 * site, a new version of one of yours, or an anonymous one that can never
 * be updated. Photos lose their hidden location data on the way out.
 */
class PublishSiteActivity : Activity() {
    private lateinit var target: RadioGroup
    private lateinit var titleInput: EditText
    private lateinit var indexInput: EditText
    private lateinit var picked: TextView
    private lateinit var publish: Button
    private var files: List<Pair<String, ByteArray>> = emptyList()
    /** radio button id → site key name (null: new site; "": anonymous). */
    private val targets = HashMap<Int, String?>()
    private val titles = HashMap<String, String>()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        title = "Publish a site"
        val pad = (16 * resources.displayMetrics.density).toInt()
        target = RadioGroup(this)
        titleInput = EditText(this).apply { hint = "Title" }
        indexInput = EditText(this).apply {
            hint = "First page"
            setText("index.html")
        }
        picked = TextView(this).apply {
            text = "No files chosen yet."
            setPadding(0, pad / 2, 0, pad / 2)
        }
        val folder = Button(this).apply {
            text = "Choose a folder"
            setOnClickListener {
                startActivityForResult(Intent(Intent.ACTION_OPEN_DOCUMENT_TREE), REQ_FOLDER)
            }
        }
        val zip = Button(this).apply {
            text = "Choose a .zip"
            setOnClickListener {
                val i = Intent(Intent.ACTION_OPEN_DOCUMENT).addCategory(Intent.CATEGORY_OPENABLE).setType("*/*")
                    .putExtra(Intent.EXTRA_MIME_TYPES, arrayOf("application/zip", "application/x-zip-compressed"))
                startActivityForResult(i, REQ_ZIP)
            }
        }
        publish = Button(this).apply {
            text = "Publish"
            isEnabled = false
            setOnClickListener { doPublish(false) }
        }
        val column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(pad, pad, pad, pad)
            addView(
                TextView(this@PublishSiteActivity).apply {
                    text = "A site is a set of web pages that travels like a post, with no server. " +
                        "Only you can publish new versions of your site. Scripts never run."
                },
            )
            addView(target)
            addView(titleInput, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(indexInput, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(folder, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(zip, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(picked)
            addView(publish, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
        }
        setContentView(ScrollView(this).apply { addView(column) })
        fillTargets()
    }

    private fun fillTargets() {
        fun option(label: String, key: String?) {
            val b = RadioButton(this).apply { text = label }
            target.addView(b)
            targets[b.id] = key
        }
        option("A new site", null)
        val json = try {
            Mesh.mySitesJson()
        } catch (e: RuntimeException) {
            null
        }
        if (json != null) {
            val mine = JSONArray(json)
            for (i in 0 until mine.length()) {
                val s = mine.getJSONObject(i)
                val key = s.getString("key")
                val t = if (s.isNull("title")) key else s.getString("title")
                if (!s.isNull("title")) titles[key] = t
                val v = if (s.isNull("version")) "" else " (now version ${s.getInt("version")})"
                option("Update: $t$v", key)
            }
        }
        option("Anonymous: no name attached, can never be updated", "")
        target.setOnCheckedChangeListener { _, id ->
            val key = targets[id]
            if (!key.isNullOrEmpty()) titles[key]?.let { titleInput.setText(it) }
        }
        target.check(target.getChildAt(0).id)
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        @Suppress("DEPRECATION")
        super.onActivityResult(requestCode, resultCode, data)
        val uri = data?.data
        if (resultCode != RESULT_OK || uri == null) return
        picked.text = "Reading…"
        publish.isEnabled = false
        Thread {
            val result = try {
                Result.success(if (requestCode == REQ_FOLDER) readFolder(uri) else readZip(uri))
            } catch (e: Exception) {
                Result.failure(e)
            }
            runOnUiThread { onPicked(result) }
        }.start()
    }

    private fun onPicked(result: Result<List<Pair<String, ByteArray>>>) {
        result.onFailure {
            files = emptyList()
            picked.text = "Couldn't read that: ${it.message}"
            return
        }
        files = result.getOrThrow()
        val paths = files.map { it.first }
        if (indexInput.text.toString() !in paths) {
            // No index.html: if there's exactly one page, that's the first page.
            paths.singleOrNull { it.endsWith(".html") || it.endsWith(".htm") }?.let { indexInput.setText(it) }
        }
        val size = files.sumOf { it.second.size.toLong() }
        val index = indexInput.text.toString()
        val note = if (index in paths) "First page: $index" else "No $index among them: set the first page above."
        picked.text = "${files.size} files, ${size / 1024} KB\n$note"
        publish.isEnabled = files.isNotEmpty()
    }

    private fun doPublish(keepMetadata: Boolean) {
        val key = targets[target.checkedRadioButtonId]
        val ownerless = key == ""
        val siteTitle = titleInput.text.toString().trim()
        val index = indexInput.text.toString().trim()
        if (siteTitle.isEmpty()) {
            titleInput.error = "Give the site a title"
            return
        }
        publish.isEnabled = false
        publish.text = "Publishing…"
        val packed = pack(files)
        Thread {
            val result = try {
                Result.success(Mesh.publishSite(if (ownerless) null else key, ownerless, siteTitle, index, packed, keepMetadata))
            } catch (e: RuntimeException) {
                Result.failure(e)
            }
            runOnUiThread {
                publish.text = "Publish"
                publish.isEnabled = true
                result.onSuccess { line ->
                    if (line == null) {
                        Toast.makeText(this, "Start SocialMesh first.", Toast.LENGTH_LONG).show()
                        return@onSuccess
                    }
                    Mesh.report(line)
                    Toast.makeText(this, line, Toast.LENGTH_LONG).show()
                    finish()
                }
                result.onFailure { e ->
                    val msg = e.message ?: "failed"
                    if (msg.startsWith("METADATA ")) {
                        AlertDialog.Builder(this)
                            .setTitle("Hidden details")
                            .setMessage(msg.removePrefix("METADATA ") + ".\n\nPublish it anyway? Anyone could read them.")
                            .setPositiveButton("Publish anyway") { _, _ -> doPublish(true) }
                            .setNegativeButton("Cancel", null)
                            .show()
                    } else {
                        AlertDialog.Builder(this).setTitle("Couldn't publish").setMessage(msg).setPositiveButton("OK", null).show()
                    }
                }
            }
        }.start()
    }

    // ---- reading files ------------------------------------------------------

    private var total = 0L

    private fun readLimited(input: InputStream): ByteArray {
        val out = ByteArrayOutputStream()
        val buf = ByteArray(64 * 1024)
        while (true) {
            val n = input.read(buf)
            if (n < 0) break
            total += n
            if (total > MAX_BYTES) throw IOException("more than ${MAX_BYTES / 1048576} MB; a site must be smaller")
            out.write(buf, 0, n)
        }
        return out.toByteArray()
    }

    private fun add(out: MutableList<Pair<String, ByteArray>>, path: String, data: ByteArray) {
        if (out.size >= MAX_FILES) throw IOException("more than $MAX_FILES files; a site holds at most $MAX_FILES")
        out += path to data
    }

    private fun readFolder(tree: Uri): List<Pair<String, ByteArray>> {
        total = 0
        val out = mutableListOf<Pair<String, ByteArray>>()
        fun walk(docId: String, prefix: String) {
            val children = DocumentsContract.buildChildDocumentsUriUsingTree(tree, docId)
            val cols = arrayOf(Document.COLUMN_DOCUMENT_ID, Document.COLUMN_DISPLAY_NAME, Document.COLUMN_MIME_TYPE)
            contentResolver.query(children, cols, null, null, null)?.use { c ->
                while (c.moveToNext()) {
                    val id = c.getString(0)
                    val name = c.getString(1) ?: continue
                    if (name.startsWith(".")) continue
                    if (c.getString(2) == Document.MIME_TYPE_DIR) {
                        walk(id, "$prefix$name/")
                    } else {
                        val doc = DocumentsContract.buildDocumentUriUsingTree(tree, id)
                        val data = contentResolver.openInputStream(doc)?.use(::readLimited) ?: continue
                        add(out, "$prefix$name", data)
                    }
                }
            }
        }
        walk(DocumentsContract.getTreeDocumentId(tree), "")
        if (out.isEmpty()) throw IOException("the folder has no files")
        return out
    }

    private fun readZip(uri: Uri): List<Pair<String, ByteArray>> {
        total = 0
        val out = mutableListOf<Pair<String, ByteArray>>()
        val input = contentResolver.openInputStream(uri) ?: throw IOException("can't open it")
        ZipInputStream(input).use { z ->
            while (true) {
                val e = z.nextEntry ?: break
                val name = e.name.replace('\\', '/')
                val hidden = name.split('/').any { it.startsWith(".") } || name.startsWith("__MACOSX/")
                if (e.isDirectory || hidden) continue
                add(out, name, readLimited(z))
            }
        }
        if (out.isEmpty()) throw IOException("the zip has no files")
        // Zips often hold everything inside one folder: publish its contents.
        val first = out[0].first.substringBefore('/', "")
        if (first.isNotEmpty() && out.all { it.first.startsWith("$first/") }) {
            return out.map { it.first.removePrefix("$first/") to it.second }
        }
        return out
    }

    companion object {
        private const val REQ_FOLDER = 1
        private const val REQ_ZIP = 2
        private const val MAX_FILES = 64
        private const val MAX_BYTES = 32L * 1024 * 1024

        /** Repeated `u16 path_len | path | u32 len | bytes` (see Native.publishSite). */
        private fun pack(files: List<Pair<String, ByteArray>>): ByteArray {
            val bytes = ByteArrayOutputStream()
            DataOutputStream(bytes).use { d ->
                for ((path, data) in files) {
                    val p = path.toByteArray(Charsets.UTF_8)
                    d.writeShort(p.size)
                    d.write(p)
                    d.writeInt(data.size)
                    d.write(data)
                }
            }
            return bytes.toByteArray()
        }
    }
}
