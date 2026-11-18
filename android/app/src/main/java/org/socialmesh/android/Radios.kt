package org.socialmesh.android

import android.Manifest
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothManager
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.os.Build

/**
 * Which ways of reaching people are available right now: Bluetooth (on, and
 * allowed) and a local network (Wi-Fi or Ethernet). Nothing is switched on
 * for the person; the app works with whatever is there, and falls back to
 * sound when there's neither (see [Modem]).
 */
internal object Radios {
    @Volatile var bluetooth = false
        private set
    @Volatile var wifi = false
        private set

    private var app: Context? = null
    private var onChange: (() -> Unit)? = null
    private val networks = HashSet<Network>()

    private val bluetoothState = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) = update()
    }

    private val wifiState = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            synchronized(networks) { networks += network }
            update()
        }

        override fun onLost(network: Network) {
            synchronized(networks) { networks -= network }
            update()
        }
    }

    /** Start watching; [changed] runs (on any thread) whenever either changes. */
    @Synchronized
    fun start(context: Context, changed: () -> Unit) {
        if (app != null) return
        val a = context.applicationContext
        app = a
        onChange = changed
        bluetooth = bluetoothUsable(a)
        a.registerReceiver(bluetoothState, IntentFilter(BluetoothAdapter.ACTION_STATE_CHANGED))
        val request = NetworkRequest.Builder()
            .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
            .addTransportType(NetworkCapabilities.TRANSPORT_ETHERNET)
            // A router with no internet (a local mesh, a field hotspot) counts.
            .removeCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .build()
        try {
            a.getSystemService(ConnectivityManager::class.java).registerNetworkCallback(request, wifiState)
        } catch (e: RuntimeException) {
            // Can't watch networks: assume there's one, so sound isn't switched on needlessly.
            wifi = true
        }
    }

    @Synchronized
    fun stop() {
        val a = app ?: return
        app = null
        onChange = null
        try {
            a.unregisterReceiver(bluetoothState)
        } catch (e: IllegalArgumentException) {
        }
        try {
            a.getSystemService(ConnectivityManager::class.java).unregisterNetworkCallback(wifiState)
        } catch (e: IllegalArgumentException) {
        }
        synchronized(networks) { networks.clear() }
    }

    /** Re-read the state now (after a permission was granted, say). */
    fun update() {
        val a = app ?: return
        val bt = bluetoothUsable(a)
        val w = synchronized(networks) { networks.isNotEmpty() }
        if (bt == bluetooth && w == wifi) return
        bluetooth = bt
        wifi = w
        onChange?.invoke()
    }

    fun bluetoothAllowed(context: Context): Boolean =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            listOf(Manifest.permission.BLUETOOTH_SCAN, Manifest.permission.BLUETOOTH_ADVERTISE, Manifest.permission.BLUETOOTH_CONNECT)
                .all { context.checkSelfPermission(it) == PackageManager.PERMISSION_GRANTED }
        } else {
            context.checkSelfPermission(Manifest.permission.ACCESS_FINE_LOCATION) == PackageManager.PERMISSION_GRANTED
        }

    fun bluetoothUsable(context: Context): Boolean {
        val adapter = context.getSystemService(BluetoothManager::class.java)?.adapter ?: return false
        return try {
            adapter.isEnabled && bluetoothAllowed(context)
        } catch (e: SecurityException) {
            false
        }
    }
}
