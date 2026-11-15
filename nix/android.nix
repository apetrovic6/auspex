# Everything Android: the SDK, the NDK, the linkers cargo cross-compiles with, and the
# machine-specific files Gradle reads.
#
# Self-contained on purpose. A product that ships no Android app deletes this file and its
# line in devenv.nix; nothing else in the environment mentions an SDK.
{
  pkgs,
  lib,
  config,
  ...
}: let
  versions = import ./versions.nix;

  # AGP downloads its own aapt2 from Maven; that binary is linked against a
  # generic glibc and cannot run on NixOS (stub-ld refuses it), which makes every
  # AarResourcesCompilerTransform fail with "Daemon startup failed". Point Gradle
  # at the Nix-built aapt2 instead -- pkgs.aapt ships the same 8.13.2-14304508.
  aapt2 = "${pkgs.aapt}/bin/aapt2";

  # The NDK shipped in the user-managed SDK is a generic Linux binary that NixOS
  # cannot execute (stub-ld refuses it), so Rust cross-compilation uses a Nix-built
  # NDK instead.
  #
  # This MUST come from androidenv.composeAndroidPackages with includeNDK: `pkgs.androidsdk`
  # ships no NDK at all, so the old `${pkgs.androidsdk}/libexec/android-sdk/ndk` base
  # matched nothing, the glob below silently produced an empty ndk_dir, no
  # CARGO_TARGET_*_LINKER was ever exported, and cargo fell back to the host `cc` or to
  # the unusable NDK in ~/Android/Sdk. The symptom was every Android link failing with
  # "Could not start dynamically linked executable ... stub-ld".
  #
  # Only the parent directory is pinned; the version directory under it is resolved by
  # glob at shell entry, because the NDK version moves on `devenv update` and a pinned
  # version silently leaves ANDROID_NDK_HOME pointing at a directory that no longer
  # exists. That resolution is now VALIDATED below rather than assumed.
  # One composition, used for the NDK and -- on a machine with no SDK of its own -- for the
  # SDK too. Composing twice would realise two store paths that have to agree about
  # versions, and nothing would check that they did.
  androidComposition = pkgs.androidenv.composeAndroidPackages {
    includeNDK = true;
    platformVersions = [versions.android.compile-sdk];
    buildToolsVersions = [versions.android.build-tools];

    # The emulator and one system image, because Android Studio cannot install them
    # itself here. Its SDK Manager works by writing into the SDK root, and this SDK is a
    # /nix/store path -- read-only by construction -- so every package it offers fails
    # with "Failed to read or create install properties file", whatever is ticked. The
    # only place an emulator can come from on this machine is the composition itself.
    #
    # (The `Observed package id 'ndk;...' in inconsistent location .../ndk-bundle` warning
    # that appears alongside it is unrelated and harmless: nixpkgs installs the NDK at
    # `ndk-bundle` where a newer sdkmanager expects `ndk/<version>`. Both directories are
    # present and Gradle builds fine through them.)
    includeEmulator = true;
    includeSystemImages = true;

    # One image type and one ABI, not the defaults. The defaults are two types
    # (google_apis and google_apis_playstore) across four ABIs, which is eight system
    # images to realise for a host that can only run one fast: x86_64 is the only ABI
    # that gets KVM here, and playstore is the one with Play Services. Multiply that by
    # a couple of GB each and the default is a very long first shell entry in exchange
    # for images nothing starts.
    #
    # All three coordinates come from versions.toml -- the API level implicitly, via
    # `platformVersions` above, and the other two explicitly -- so the image this realises
    # and the image `android-emulator` boots cannot name different things.
    systemImageTypes = [versions.android.system-image-type];
    abiVersions = [versions.android.system-image-abi];
  };

  androidNdkParent = "${androidComposition.ndk-bundle}/libexec/android-sdk/ndk";

  # The one image the composition realises, in sdkmanager's coordinates, and the AVD
  # `android-emulator` keeps. Derived rather than written out so that bumping
  # [android].compile-sdk cannot leave the script booting an image that is no longer
  # installed -- the failure would be `Package path is not valid`, several layers from
  # the edit that caused it.
  systemImagePath = "system-images;android-${versions.android.compile-sdk};${versions.android.system-image-type};${versions.android.system-image-abi}";
  avdName = "api${versions.android.compile-sdk}-${versions.android.system-image-abi}";

  # The hardware profile the AVD is cut from. Not a version, so not versions.toml -- but
  # not omittable either: see the `-d` comment in the script below for what happens
  # without it.
  avdDevice = "pixel_10";

  # The SDK -- the only one. Read-only, which is fine as long as AGP never needs to
  # install anything into it, hence the pinned buildToolsVersion on the Gradle side.
  androidSdkRoot = "${androidComposition.androidsdk}/libexec/android-sdk";

  # Cargo needs the NDK clang wrapper as the linker for every Android target.
  # Without it cargo falls back to the host `cc`, and the host ld.bfd rejects
  # the cross objects with "Relocations in generic ELF (EM: 183) ... file in
  # wrong format" -- which is what `just android/build` used to die on.
  # The wrapper filename embeds the API level, so keep this in sync with
  # `minSdk` in apps/*/platforms/android/app/build.gradle.kts.
  androidApiLevel = versions.android.api-level;

  # Rust target triple -> NDK clang wrapper prefix. They differ for 32-bit ARM:
  # Rust says armv7-linux-androideabi, the NDK says armv7a-linux-androideabi.
  androidLinkerTargets = {
    AARCH64_LINUX_ANDROID = "aarch64-linux-android";
    ARMV7_LINUX_ANDROIDEABI = "armv7a-linux-androideabi";
    I686_LINUX_ANDROID = "i686-linux-android";
    X86_64_LINUX_ANDROID = "x86_64-linux-android";
  };

  # Emitted into enterShell as `export CARGO_TARGET_<TRIPLE>_LINKER=...` lines,
  # with $ndk_bin filled in at shell entry once the NDK glob has resolved.
  androidLinkerExports = lib.concatStringsSep "\n" (
    lib.mapAttrsToList (
      envSuffix: ndkPrefix: "    export CARGO_TARGET_${envSuffix}_LINKER=\"$ndk_bin/${ndkPrefix}${androidApiLevel}-clang\""
    )
    androidLinkerTargets
  );

in {
  # One SDK on PATH, and it is the composition above -- not devenv's `android` module,
  # which is deliberately not enabled. That module composes a SECOND SDK from its own
  # defaults (API 32 and 34, no system images) and puts *that* on PATH, while this file's
  # composition is what ANDROID_HOME points at. Both existed until this commit, and the
  # only symptom was `avdmanager` reporting `Valid system image paths are: null` -- true
  # of the SDK it was reading, which was never the SDK anything else used.
  #
  # The `androidComposition` comment above already forbids this ("Composing twice would
  # realise two store paths that have to agree about versions, and nothing would check
  # that they did"); enabling the module broke that rule from the outside. Dropping it
  # also removes the three `lib.mkForce`es and the enterShell branch that existed purely
  # to overrule it.
  #
  # android-studio is a plain package with no SDK of its own, so it is unaffected.
  #
  # The JDK is here for the same reason: devenv's android module supplied Gradle's JVM as
  # well as the SDK, so dropping the module took `java` with it and every Gradle build
  # died with "JAVA_HOME is not set and no 'java' command could be found in your PATH" --
  # after the Rust half had already succeeded, which is the confusing way round. jdk21 is
  # not a guess: it resolves to the identical store path the module was supplying
  # (openjdk-21.0.12.1+1), so builds run on the JVM they always ran on. AGP 8.13's
  # documented floor is 17; matching what was actually in use beats matching the floor.
  packages = [pkgs.aapt androidComposition.androidsdk pkgs.android-studio pkgs.jdk21];

  # Gradle finds a JVM through JAVA_HOME before it looks at PATH, and the Gradle daemon
  # does not inherit an interactive shell's PATH at all -- so the package above is not
  # on its own enough.
  env.JAVA_HOME = pkgs.jdk21.home;

  # The Rust side of Android: the four cross targets come from the single shared toolchain
  # (nix/toolchain.nix), which derives them from discovery — an app with an Android
  # project implies the four triples. Deleting this file no longer needs to also stop a
  # target list being contributed somewhere else.

  # Nothing chooses an SDK at runtime any more: this is it, in both spellings, set
  # once. The previous arrangement preferred ~/Android/Sdk and fell back to the store,
  # which meant the toolchain a build used depended on whether a directory existed in
  # somebody's home -- reproducible only by coincidence. It also forced the whole file
  # through `builtins.getEnv "HOME"`, an impurity that yields "" when HOME is unset and
  # would have pointed ANDROID_HOME at "/Android/Sdk" rather than failing.
  #
  # The cost, stated because it is real: Android Studio's SDK Manager can no longer
  # install anything (a store path is read-only), so every SDK component must be
  # declared in `androidComposition` above. That is the trade this file now makes on
  # purpose -- see `includeEmulator` for the same argument.
  env.ANDROID_HOME = androidSdkRoot;
  env.ANDROID_SDK_ROOT = androidSdkRoot;

  # Writable state, kept beside the project rather than in ~/.android. These were set by
  # devenv's android module; dropping it means setting them here or silently moving every
  # AVD back into $HOME. Gitignored, and the reason the emulator can run at all against a
  # read-only SDK: the images are in the store, the AVDs are not.
  env.ANDROID_USER_HOME = "${config.devenv.root}/.android";
  env.ANDROID_AVD_HOME = "${config.devenv.root}/.android/avd";

  # AGP downloads its own aapt2 from Maven and that binary cannot run on NixOS. A
  # `-Dorg.gradle.project.*` system property outranks every gradle.properties, so the
  # override goes here; enterShell additionally writes it into gradle.properties, which
  # is the only channel Android Studio reads (it never sees this shell's GRADLE_OPTS).
  env.GRADLE_OPTS =
    "-Dorg.gradle.project.android.aapt2FromMavenOverride=${aapt2}";

  # `android-emulator [emulator args...]` -- boot the image the SDK ships, creating the
  # AVD on first use. `-no-window -no-audio` for headless.
  #
  # Both binaries are named by absolute store path rather than found on PATH. That is not
  # caution for its own sake: reading the wrong `avdmanager` is precisely the bug this
  # commit fixes, and a script that depended on PATH order would reintroduce it the moment
  # anything else put an SDK in front.
  scripts.android-emulator.exec = ''
    set -euo pipefail
    avdmanager="${androidComposition.androidsdk}/bin/avdmanager"
    emulator="${androidComposition.androidsdk}/bin/emulator"

    mkdir -p "$ANDROID_AVD_HOME"
    # Tested by looking for the directory, not by parsing `avdmanager list avd`: that
    # takes seconds to load the repository, and the obvious `list avd | grep -q` is a
    # trap this repository has already been bitten by -- grep exits at the first match,
    # avdmanager dies of SIGPIPE, and `set -o pipefail` reports failure exactly when the
    # AVD *does* exist. See check-template in the Justfile for the other one.
    if [ ! -d "$ANDROID_AVD_HOME/${avdName}.avd" ]; then
      echo "android-emulator: creating AVD ${avdName} from ${systemImagePath}"
      # `no` declines the custom-hardware-profile prompt.
      #
      # `-d` matters far more than it looks. Without it the AVD inherits avdmanager's
      # built-in default profile, which is 320x640 at 160dpi -- a screen from about 2010.
      # Android 16 renders into it perfectly happily and merely looks wrong, fonts first,
      # which is the kind of defect that ships because nothing fails.
      #
      # The two "Could not load devices from .../devices.xml" errors this prints are
      # noise, and were misread once already: nixpkgs' system images carry no per-image
      # device file, avdmanager complains twice and then uses its built-in list anyway.
      # The obvious reading -- lookup failed, so `-d` was ignored -- is wrong, and the
      # config.ini settles it: hw.device.name matching, and a real density and resolution.
      echo no | "$avdmanager" create avd -n "${avdName}" -k "${systemImagePath}" -d "${avdDevice}"
    fi

    exec "$emulator" -avd "${avdName}" "$@"
  '';

  enterShell = ''
    # Keep the aapt2 override pointing at the current Nix store path. Rewritten
    # on every shell entry, so a `devenv update` that bumps pkgs.aapt cannot
    # leave a stale path behind. See the `aapt2` binding above for the why.
    # $HOME at runtime, not `builtins.getEnv "HOME"` at eval time -- that was this
    # file's last impurity. Guarded, because a shell with neither variable set would
    # otherwise write to "/.gradle" and fail somewhere less obvious.
    gradle_home="''${GRADLE_USER_HOME:-''${HOME:+$HOME/.gradle}}"
    if [ -z "$gradle_home" ]; then
      echo "devenv: neither GRADLE_USER_HOME nor HOME is set -> Android Studio will not see the aapt2 override" >&2
    elif ! grep -qxF "android.aapt2FromMavenOverride=${aapt2}" "$gradle_home/gradle.properties" 2>/dev/null; then
      gradle_props="$gradle_home/gradle.properties"
      mkdir -p "$gradle_home"
      touch "$gradle_props"
      ${pkgs.gnused}/bin/sed -i '/^android\.aapt2FromMavenOverride=/d' "$gradle_props"
      printf '%s\n' "android.aapt2FromMavenOverride=${aapt2}" >> "$gradle_props"
      echo "devenv: pointed Gradle's aapt2 at ${aapt2}"
    fi

    # Resolve the Nix-built NDK and point cargo's Android linkers at it. Done
    # here rather than in `env.` so a `devenv update` that bumps the bundled NDK
    # version cannot leave a stale path behind. See androidNdkParent above.
    ndk_dir=""
    for candidate in ${androidNdkParent}/*/; do
      # Accept a candidate only if the clang wrapper cargo will actually invoke is
      # present. An unvalidated path here is how this broke before: the directory
      # looked plausible, the exports were skipped, and the failure only surfaced
      # much later as an unreadable linker error.
      if [ -x "''${candidate}toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android${androidApiLevel}-clang" ]; then
        ndk_dir="''${candidate%/}"
      fi
    done
    if [ -z "$ndk_dir" ]; then
      echo "devenv: ERROR: no usable NDK under ${androidNdkParent}" >&2
      echo "devenv:        (needs toolchains/llvm/prebuilt/linux-x86_64/bin/*-android${androidApiLevel}-clang)" >&2
      echo "devenv:        Android Rust targets will NOT link until this is fixed." >&2
    else
      export ANDROID_NDK_HOME="$ndk_dir"
      export ANDROID_NDK_ROOT="$ndk_dir"
      ndk_bin="$ndk_dir/toolchains/llvm/prebuilt/linux-x86_64/bin"
${androidLinkerExports}
      echo "devenv: cargo Android linkers -> $ndk_bin (API ${androidApiLevel})"
    fi

    # ANDROID_HOME is set by `env.` above and exported again here, because the
    # local.properties writer below reads it and one spelling in one place is what stops
    # the file and the environment disagreeing.
    #
    # A store SDK used to fail here with "Failed to install ... The SDK directory is
    # not writable", because AGP picked a build-tools version the SDK did not have and
    # tried to install it. That is why [android].build-tools is pinned in versions.toml
    # and asserted against every build.gradle.kts: with the version AGP wants already
    # present, it never writes, and read-only stops mattering.
    # There is no longer a choice to make here: one SDK, named once, whatever machine
    # this is. A CI container and a developer laptop now run the same toolchain, which
    # the old ~/Android/Sdk preference could not promise.
    export ANDROID_HOME="${androidSdkRoot}"
    export ANDROID_SDK_ROOT="$ANDROID_HOME"
    # Every Android platform project in the monorepo gets its local.properties
    # rewritten, discovered by glob rather than listed, so adding an app needs no edit
    # here. local.properties is gitignored: it holds absolute, machine-specific paths.
    shopt -s nullglob
    for android_dir in "''${DEVENV_ROOT}"/apps/*/platforms/android; do
      {
        echo "# Generated on devenv shell entry; see enterShell in nix/android.nix."
        echo "sdk.dir=$ANDROID_HOME"
        [ -n "$ndk_dir" ] && echo "ndk.dir=$ndk_dir"
      } > "$android_dir/local.properties"
    done
    shopt -u nullglob
  '';
}
