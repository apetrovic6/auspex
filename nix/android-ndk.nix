# The Android NDK, and the cargo linker variables that go with it.
#
# A values file rather than a devenv module — like nix/versions.nix, nix/proto-tools.nix and
# nix/cldr.nix — because two consumers need the same thing in different forms: the dev shell
# wants `export` lines in its hook (nix/android.nix), and the CI pipeline wants the NDK as a
# build input of a derivation that cross-compiles inside the sandbox (nix/ci.nix). Import with
# `import ./android-ndk.nix {inherit pkgs lib;}`.
#
# What is single-sourced here is the part that can drift silently: the mapping from a Rust
# target triple to the NDK's clang wrapper prefix. The two disagree for 32-bit ARM — Rust says
# `armv7-linux-androideabi`, the NDK says `armv7a-linux-androideabi` — and a wrong prefix does
# not fail where it is written. It fails minutes later as an unreadable linker error, or not at
# all: cargo falls back to the host `cc`, which rejects the cross objects with "Relocations in
# generic ELF (EM: 183) ... file in wrong format".
#
# The COMPOSITION here is deliberately minimal: `includeNDK` and the two version pins, and
# nothing else. nix/android.nix's composition also realises an emulator and a system image —
# several gigabytes, and the only way to get one on a machine whose SDK is a read-only store
# path. A cross-compile needs none of it. Measured, the `ndk-bundle` this produces is the same
# store path the full composition yields, so these are not two NDKs; they are one NDK and two
# views of what else arrives with it.
{
  pkgs,
  lib,
}: let
  versions = import ./versions.nix;

  composition = pkgs.androidenv.composeAndroidPackages {
    includeNDK = true;
    platformVersions = [versions.android.compile-sdk];
    buildToolsVersions = [versions.android.build-tools];
  };

  apiLevel = versions.android.api-level;

  # The directory holding the NDK's single version directory. Only the PARENT is a store path
  # known at evaluation time; which version is inside it moves on `devenv update`, so it is
  # resolved by glob at build or shell entry rather than pinned here — a pinned version silently
  # leaves every path below pointing at a directory that no longer exists. Resolving it with
  # `builtins.readDir` instead would force the NDK to be realised during evaluation, which is
  # the wrong trade for a multi-gigabyte input on a machine that may only be evaluating.
  ndkParent = "${composition.ndk-bundle}/libexec/android-sdk/ndk";

  # Rust target triple -> NDK clang wrapper prefix. See the header for why the ARM one differs.
  linkerTargets = {
    AARCH64_LINUX_ANDROID = "aarch64-linux-android";
    ARMV7_LINUX_ANDROIDEABI = "armv7a-linux-androideabi";
    I686_LINUX_ANDROID = "i686-linux-android";
    X86_64_LINUX_ANDROID = "x86_64-linux-android";
  };
in {
  # What to put in `nativeBuildInputs` (a derivation) or `packages` (a shell).
  packages = [composition.ndk-bundle];

  inherit ndkParent apiLevel linkerTargets;

  # The shell fragment both consumers run: resolve the version directory, validate it, export
  # what cargo needs. One definition, so the dev shell and the sandboxed derivation cannot
  # disagree about which linker a triple gets.
  #
  # Validation is by the file cargo will actually execute, not by the directory existing. That
  # is the check whose absence broke this once before: the glob matched, the exports were
  # skipped, and the failure surfaced much later as a linker error naming nothing.
  #
  # `exit 1` rather than a warning, because a build that proceeds without linkers produces the
  # confusing failure instead of the honest one.
  #
  # `$ndk_dir` and `$ndk_bin` stay shell variables on purpose — only `${...}` is Nix
  # interpolation, and `''${candidate}` is the escape that keeps the glob's own substitution.
  exports =
    ''
      ndk_dir=""
      for candidate in ${ndkParent}/*/; do
        if [ -x "''${candidate}toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android${apiLevel}-clang" ]; then
          ndk_dir="''${candidate%/}"
        fi
      done
      if [ -z "$ndk_dir" ]; then
        echo "no usable NDK under ${ndkParent}" >&2
        echo "  (needs toolchains/llvm/prebuilt/linux-x86_64/bin/*-android${apiLevel}-clang)" >&2
        exit 1
      fi
      export ANDROID_NDK_HOME="$ndk_dir"
      export ANDROID_NDK_ROOT="$ndk_dir"
      ndk_bin="$ndk_dir/toolchains/llvm/prebuilt/linux-x86_64/bin"
    ''
    + lib.concatStringsSep "\n" (
      lib.mapAttrsToList (
        envSuffix: ndkPrefix:
          ''export CARGO_TARGET_${envSuffix}_LINKER="$ndk_bin/${ndkPrefix}${apiLevel}-clang"''
      )
      linkerTargets
    )
    + "\n";
}
