# The CLDR release the i18n payload is cut from, fetched once into the store.
#
# A values file rather than a devenv module, like nix/versions.nix and nix/proto-tools.nix:
# what it provides is a derivation, not options, and nix/ci.nix imports it directly to hand
# `icu4x-datagen` a local `--cldr-root`.
#
# Why this exists: `just i18n::check-data` runs datagen with `--cldr-tag`, which downloads
# this zip from GitHub on every run. That made the gate a host step in the Nix pipeline —
# the only thing about it that needed the network was a fetch of a file whose version is
# pinned. Fetching it here instead makes the whole gate a sandboxed derivation, so:
#
#   - the download happens once per CLDR release per machine, into the store, and every
#     runner that substitutes from ncps gets it for free rather than each fetching its own,
#   - datagen then runs offline, which is what lets the comparison against the committed
#     payload be a derivation at all,
#   - and the input is content-addressed: a bump that changed the payload without
#     regenerating it is a red stage, not a download that happened to differ.
#
# **The version is single-sourced and the hash is self-policing.** The URL is built from
# `versions.toml`'s `tools.cldr-json` through nix/versions.nix — the same value
# `just/i18n.just` reads for `--cldr-tag` — so the two readers cannot name different
# releases. Only the hash lives here, and it cannot go stale silently: bump the version in
# versions.toml and the URL changes, so the fetch fails with the new hash in its error
# rather than quietly serving the old release. That is the property the Swift plugin
# fetches in nix/proto-tools.nix had to be fixed for, where a retag left the store path
# unchanged; naming the file with the version does the same job here.
{pkgs}: let
  versions = import ./versions.nix;
  cldr = versions.tools.cldr-json;
in {
  # The unpacked zip, which is the shape `--cldr-root` wants: a directory of `cldr-*`
  # subtrees, not the archive.
  #
  # `unzip` rather than a Python one-liner because it is the tool that owns the format and
  # it fails loudly on a truncated archive. `-q` keeps a several-thousand-file listing out
  # of the build log.
  root = pkgs.runCommand "cldr-${cldr}-json-full" {
    nativeBuildInputs = [pkgs.unzip];
    # The zip is a fixed-output fetch's result, so this derivation is content-addressed by
    # it: the same release unpacks to the same path on every machine.
    src = pkgs.fetchurl {
      url = "https://github.com/unicode-org/cldr-json/releases/download/${cldr}/cldr-${cldr}-json-full.zip";
      hash = "sha256-ZDWlKcm+L5y8V7qdDld8nErKkMEJu3FWpPBX368qYm8=";
    };
    # Named with the version, for the reason given above: a fixed-output fetch's store path
    # is its name plus its hash, and a generic name would let a bump reuse the old path.
    outputHashMode = "recursive";
  } ''
    mkdir -p $out
    unzip -q "$src" -d $out
    # Fail here rather than in datagen if the archive's shape ever changes: datagen's
    # complaint about a missing locale directory does not say "the zip layout moved".
    test -d $out/cldr-core
    test -d $out/cldr-numbers-full
  '';

  inherit cldr;
}
