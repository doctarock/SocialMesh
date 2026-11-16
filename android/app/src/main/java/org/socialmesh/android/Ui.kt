package org.socialmesh.android

import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.res.ColorStateList
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.graphics.drawable.RippleDrawable
import android.text.SpannableStringBuilder
import android.text.Spanned
import android.text.TextPaint
import android.text.method.LinkMovementMethod
import android.text.style.ClickableSpan
import android.text.style.ForegroundColorSpan
import android.text.style.StyleSpan
import android.util.TypedValue
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast

/**
 * The few building blocks every screen shares, drawn in code (the app has no
 * layout files): breadcrumbs, cards, headings, and friendly sizes and times.
 * Colours come from the phone's theme, so light and dark mode both work.
 */
internal object Ui {
    fun dp(ctx: Context, v: Int): Int = (v * ctx.resources.displayMetrics.density).toInt()

    fun attrColor(ctx: Context, attr: Int): Int {
        val tv = TypedValue()
        if (!ctx.theme.resolveAttribute(attr, tv, true)) return Color.GRAY
        return if (tv.resourceId != 0) ctx.getColor(tv.resourceId) else tv.data
    }

    fun primary(ctx: Context) = attrColor(ctx, android.R.attr.textColorPrimary)
    fun secondary(ctx: Context) = attrColor(ctx, android.R.attr.textColorSecondary)
    fun accent(ctx: Context) = attrColor(ctx, android.R.attr.colorAccent)

    /** `color` at `alpha` (0–255): a tint that shows on light and dark backgrounds alike. */
    fun tint(color: Int, alpha: Int) = Color.argb(alpha, Color.red(color), Color.green(color), Color.blue(color))

    fun column(ctx: Context, padding: Int = 16): LinearLayout = LinearLayout(ctx).apply {
        orientation = LinearLayout.VERTICAL
        val p = dp(ctx, padding)
        setPadding(p, p, p, p)
    }

    fun row(ctx: Context, vararg views: View): LinearLayout = LinearLayout(ctx).apply {
        orientation = LinearLayout.HORIZONTAL
        for (v in views) addView(v, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
    }

    /** A small section title. */
    fun heading(ctx: Context, text: String): TextView = TextView(ctx).apply {
        this.text = text.uppercase()
        textSize = 12f
        letterSpacing = 0.08f
        setTypeface(typeface, Typeface.BOLD)
        setTextColor(secondary(ctx))
        setPadding(dp(ctx, 4), dp(ctx, 18), 0, dp(ctx, 6))
    }

    fun text(ctx: Context, text: CharSequence, size: Float = 15f, bold: Boolean = false): TextView = TextView(ctx).apply {
        this.text = text
        textSize = size
        setTextColor(primary(ctx))
        if (bold) setTypeface(typeface, Typeface.BOLD)
    }

    fun note(ctx: Context, text: CharSequence): TextView = TextView(ctx).apply {
        this.text = text
        textSize = 13f
        setTextColor(secondary(ctx))
    }

    /** A rounded panel; tappable (with a ripple) if [onClick] is given. */
    fun card(ctx: Context, onClick: (() -> Unit)? = null, onLongClick: (() -> Unit)? = null): LinearLayout =
        LinearLayout(ctx).apply {
            orientation = LinearLayout.VERTICAL
            val p = dp(ctx, 14)
            setPadding(p, p, p, p)
            val shape = GradientDrawable().apply {
                cornerRadius = dp(ctx, 12).toFloat()
                setColor(tint(primary(ctx), 0x12))
            }
            background = if (onClick != null) {
                RippleDrawable(ColorStateList.valueOf(tint(primary(ctx), 0x30)), shape, shape)
            } else {
                shape
            }
            onClick?.let { f -> setOnClickListener { f() } }
            onLongClick?.let { f -> setOnLongClickListener { f(); true } }
            layoutParams = LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT).apply { bottomMargin = dp(ctx, 8) }
        }

    fun copy(ctx: Context, label: String, text: String) {
        ctx.getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText(label, text))
        Toast.makeText(ctx, "$label copied", Toast.LENGTH_SHORT).show()
    }

    fun size(n: Long): String = when {
        n < 1024 -> "$n B"
        n < 1024 * 1024 -> "${(n + 512) / 1024} KB"
        else -> String.format("%.1f MB", n / 1048576.0)
    }

    fun ago(ms: Long, now: Long = System.currentTimeMillis()): String {
        val s = ((now - ms) / 1000).coerceAtLeast(0)
        return when {
            s < 60 -> "just now"
            s < 3600 -> "${s / 60} min ago"
            s < 86_400 -> "${s / 3600} h ago"
            s < 7 * 86_400 -> "${s / 86_400} d ago"
            else -> java.text.DateFormat.getDateInstance(java.text.DateFormat.MEDIUM).format(java.util.Date(ms))
        }
    }

    /** How a link's medium reads to people. */
    fun medium(m: String): String = when (m) {
        "ble" -> "Bluetooth"
        "tcp" -> "Wi-Fi"
        else -> m
    }
}

/**
 * The breadcrumb trail at the top of a screen, e.g. `SocialMesh › Sites ›
 * Street meeting › details.html`. Every crumb but the last is a link back up.
 */
internal object Crumbs {
    class Crumb(val label: String, val go: (() -> Unit)?)

    fun view(ctx: Context): TextView = TextView(ctx).apply {
        textSize = 14f
        movementMethod = LinkMovementMethod.getInstance()
        highlightColor = Color.TRANSPARENT
        setPadding(Ui.dp(ctx, 16), Ui.dp(ctx, 10), Ui.dp(ctx, 16), Ui.dp(ctx, 10))
        background = GradientDrawable().apply { setColor(Ui.tint(Ui.primary(ctx), 0x0c)) }
    }

    fun set(view: TextView, crumbs: List<Crumb>) {
        val ctx = view.context
        val accent = Ui.accent(ctx)
        val out = SpannableStringBuilder()
        crumbs.forEachIndexed { i, c ->
            if (i > 0) {
                val s = out.length
                out.append("  ›  ")
                out.setSpan(ForegroundColorSpan(Ui.secondary(ctx)), s, out.length, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
            }
            val s = out.length
            out.append(c.label)
            val go = c.go
            if (go != null && i < crumbs.size - 1) {
                out.setSpan(
                    object : ClickableSpan() {
                        override fun onClick(widget: View) = go()
                        override fun updateDrawState(ds: TextPaint) {
                            ds.color = accent
                            ds.isUnderlineText = false
                        }
                    },
                    s, out.length, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE,
                )
            } else {
                out.setSpan(StyleSpan(Typeface.BOLD), s, out.length, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
                out.setSpan(ForegroundColorSpan(Ui.primary(ctx)), s, out.length, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
            }
        }
        view.text = out
    }

    /** Back to the main screen, closing everything above it. */
    fun home(a: Activity): () -> Unit = {
        a.startActivity(Intent(a, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP))
    }

    /** Back to the list of sites. */
    fun sites(a: Activity): () -> Unit = {
        a.startActivity(Intent(a, SitesActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP))
    }
}
