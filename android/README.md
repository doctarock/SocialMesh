# SocialMesh for Android (test app)

A minimal app for trying Bluetooth meshing between real phones: start it on
two Android phones near each other and they find each other, link up and
sync posts, with no accounts, no internet and no setup.

- `app/.../BleDriver.kt`, `Link.kt`, `GattPipe.kt`: the Bluetooth driver.
  Radio work only (advertise, scan, L2CAP channels and the GATT pipe);
  everything else is the Rust node.
- `app/.../Native.kt`: the node (`apps/mesh-mobile`, built as
  `libsocialmesh.so`); `Mesh.kt` owns the one running node.
- `app/.../MeshService.kt`: keeps the node running in the background, with
  a notification showing the number of links and a Stop button.
- `app/.../MainActivity.kt`: post, feed, and a log of what the radio does.

Needs Android 10 (API 29) or newer: that's when Android got L2CAP
connection-oriented channels.

## Build

1. Install [Android Studio](https://developer.android.com/studio). In its
   SDK Manager, also install **NDK (Side by side)**.
2. Rust targets and cargo-ndk:

   ```sh
   rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
   cargo install cargo-ndk
   ```

3. Build the node library into the app (from the repository root; set
   `ANDROID_NDK_HOME` if cargo-ndk can't find the NDK):

   ```sh
   cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 -o android/app/src/main/jniLibs build --release -p mesh-mobile
   ```

   Without cargo-ndk, point cargo at the NDK's clang directly (Windows
   shown; `.cmd` drops on other hosts):

   ```sh
   TC=$ANDROID_HOME/ndk/<version>/toolchains/llvm/prebuilt/windows-x86_64/bin
   CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$TC/aarch64-linux-android29-clang.cmd    CC_aarch64_linux_android=$TC/aarch64-linux-android29-clang.cmd AR_aarch64_linux_android=$TC/llvm-ar.exe      cargo build --release -p mesh-mobile --target aarch64-linux-android
   cp target/aarch64-linux-android/release/libsocialmesh.so android/app/src/main/jniLibs/arm64-v8a/
   ```

4. Open the `android/` folder in Android Studio and run on a phone (twice:
   two phones). Android Studio creates the Gradle wrapper on first sync.
   Or from the command line, with Gradle 8.7+ and JDK 17:
   `gradle assembleDebug testDebugUnitTest`, then
   `adb install -r app/build/outputs/apk/debug/app-debug.apk`.

The emulator has no Bluetooth radio; use real phones.

## What to expect

- Grant the Bluetooth permissions when asked. Android 10/11 calls it
  location permission; the app never reads your location.
- Within a few seconds of both apps being open and in range, the log shows
  `Found a node via bluetooth` and `Connected … over ble`.
- Post on one phone; it appears on the other.
- Phones on the same Wi-Fi also link over the LAN; the log names each
  medium.

## Known limits

- Android rotates the Bluetooth address itself, on its own schedule (about
  every 15 minutes). The app restarts advertising when the link key
  rotates, but can't force a new address at that moment.
- The node keeps running with the screen off, but Android may still pause
  its timers in deep sleep (Doze), slowing syncs; Bluetooth adverts and
  incoming data wake it.
- Turning Bluetooth off and on while the node runs stops it accepting
  links until the node is restarted.
- The GATT pipe (for Windows and iOS peers) is written but has only been
  checked against the Android API, not run; Android-to-Android uses L2CAP.
  iOS needs its own driver.
