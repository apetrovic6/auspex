# The CI pipeline as a library. `mkPipeline` is plain Nix: it takes pkgs, a crane
# library already overridden with the toolchain, the source path and options, and returns
# plain derivations. It evaluates with no devenv involved — devenv is only one consumer.
#
# THE CALLER owns the pins. `pkgs`, `craneLib` (and therefore the toolchain `craneLib` was
# overridden with) and `inputs` are all arguments; nothing here imports a flake lock or a
# devenv config. A product passes its own pkgs/toolchain, so its dev shell and CI use the
# product's pins, never the template's.
#
# Apps are DISCOVERED, not listed: `nix/pipeline/discovery.nix` reads every workspace
# member's manifest and derives which crates get the wasm pass, the native-OFF
# clippy/deps pass, trunk, and which apps have platforms/android|apple. A hand-made crate
# joins CI by carrying the same `[package.metadata.app]` metadata.
#
# The library owns:
#   - the crane layering (L0/L1, the fixed vendor path, the git-source gate, the
#     "registry crates" count);
#   - the stage definitions.
# The product owns: Justfiles, gates.toml, versions.toml, nix/deps-base (decision 8).
{ lib }:
{
  # The source tree — a real path the manifests are read from (not a fileset).
  src,
  pkgs,
  # crane.lib.<system>, already overridden with the shared toolchain (see
  # nix/pipeline/toolchain.nix).
  craneLib,
  # The frozen dependency snapshot (nix/deps-base). Optional: a missing snapshot falls
  # back to a full (slow) deps build, never a wrong one.
  depsBase ? null,
  # The toolchain the bottom layer (L0) is built with: the shared toolchain without its cross
  # targets (nix/toolchain.nix, `hostOnly`). L0 compiles for the host only, and the target list
  # is derived from which shells and platforms a tree has, so with the full toolchain L0 was a
  # different derivation in a product with another app set, over the same snapshot (RT-76).
  # Every layer above keeps `craneLib`'s toolchain: the rustc and the host std are the same
  # component either way, and cargo keys a unit by `rustc -vV`, not by the sysroot it lives in —
  # measured upstream, the template's L1 over this L0 recompiles only the lock's drift.
  # `null` builds L0 with `craneLib`.
  depsToolchain ? null,
  # Name used for the crane derivations (a workspace manifest has no package.name).
  pname ? (builtins.baseNameOf (toString src)),
  # The product this tree is, from `[publish].project` in versions.toml — the value `cli new`
  # writes and the registry project the images are pushed under. Passed through to the NixOS
  # modules, which derive every host-level name from it: two products both ship a service called
  # `api`, so their modules would otherwise define `services.api`, `api.service` and `/etc/api`
  # twice on one host. It is an argument, defaulted from that one place rather than from a second
  # spelling here.
  product ? null,
  # Flake inputs the stage definitions need (e.g. `connectrpc` for the proto stage).
  # Plain Nix: any attrset the caller passes; only the stages that use it force it.
  inputs ? { },
  # Toggle built-in stages: `stages.<name>.enable = false` drops one. Defaults to "on when
  # the tree has what the stage needs" (a trunk build needs a trunk crate, etc.).
  stages ? { },
  # Product/template-added stages that reuse the dependency layers. Each entry is a
  # function `{ hostArgs, depsL0, depsL1, wasmArtifacts, commonArgs, src, srcSet, discovery } -> drv`.
  # They are part of the `ci-pure` aggregate unless marked `pure = false`.
  extraStages ? { },
  # Forge-specific surface (decision 8): where CI pushes to and substitutes from. Not read
  # by any derivation today; threaded through so the workflow and docs have one home for it.
  cache ? { substituter = null; push = null; },
  # Native build inputs the whole workspace needs (the gpui/desktop closure). Kept as a
  # single list today; gating it on the presence of a desktop shell is a later refinement.
  nativeBuildInputs ? (with pkgs; [
    pkg-config
    openssl
    zstd
    fontconfig
    freetype
    libxkbcommon
    libglvnd
    vulkan-loader
    wayland
    libX11
    libXcursor
    libXi
    libxcb
    libxkbfile
  ]),
}:
let
  inherit (lib) optional optionals optionalAttrs optionalString concatStrings
    concatStringsSep concatMap concatMapStringsSep filter foldl' mapAttrs mapAttrs' unique
    hasAttr listToAttrs splitString length head tail elemAt last removePrefix;

  discovery = import ./discovery.nix { inherit lib; } src;
  disc = discovery;

  r = toString src;

  # The product name every host-level artefact is namespaced by. Read from versions.toml when the
  # caller does not name one: that file's `[publish].project` is already the product's identity
  # (`cli new` fills it in, `just publish::push` tags with it), and a second spelling of the
  # product name here would be a second thing to keep in step. Missing means the tree is not this
  # template's, which is worth naming rather than defaulting to something plausible-but-colliding.
  productName =
    if product != null then
      product
    else
      (builtins.fromTOML (builtins.readFile (r + "/versions.toml"))).publish.project
      or (throw "mkPipeline: pass `product`, or set [publish].project in versions.toml — \
                the NixOS modules derive every host-level name from it");

  # --- source fileset ---------------------------------------------------------

  # `maybe rel` — a fileset for a path that may not exist (template-only files are absent
  # in a product).
  maybe = rel: lib.fileset.maybeMissing (src + rel);

  crateDirs = map (d: src + "/${d}") disc.memberDirs;

  # The non-cargo assets a trunk web shell needs: index.html, Trunk.toml, styles/.
  trunkAssets = concatMap (c: [
    (src + "/${c.dir}/index.html")
    (src + "/${c.dir}/Trunk.toml")
    (src + "/${c.dir}/styles")
  ]) disc.trunkCrates;

  # Per-service config, read at test time through CARGO_MANIFEST_DIR.
  serviceConfigs = map (s: src + "/${s.dir}/config") disc.services;

  # A service's built single-page application, when its manifest declares one. A build input
  # like any other, so it is part of the source fileset: `commonCargoSources` only takes
  # `.rs`/`.toml`/build scripts, and a bundle is none of those.
  serviceSpas = map (s: src + "/${s.dir}/${s.spa}")
    (filter (s: s.spa != null) disc.services);

  # Platform projects as their TRACKED files only, so a dev checkout's `.gradle/`,
  # `app/build/`, `generated/` and `.android/` never change the input hash. In a flake
  # source (no `.git`) the tree is already clean, so the whole directory is safe.
  trackedPlatforms = concatMap (app:
    # `maybeMissing`: an app has no `platforms/` until `add platform-*` gives it one — a
    # terminal-only app (`add crux-app --shells tui`) never does — and a bare path that does not
    # exist fails `lib.fileset.unions` at eval, before a single stage is built.
    let dir = lib.fileset.maybeMissing (src + "/apps/${app}/platforms"); in
    if lib.pathExists (src + "/.git") then
      [ (lib.fileset.intersection (lib.fileset.gitTracked src) dir) ]
    else
      [ dir ]
  ) disc.apps;

  # Build junk that lives beside the sources but belongs to no crate.
  buildJunk = lib.fileset.unions (
    [
      (maybe "/nix/deps-base")
    ]
    ++ map (c: craneLib.fileset.commonCargoSources (src + "/${c.dir}/dist"))
      (filter (c: lib.pathExists (src + "/${c.dir}/dist")) disc.trunkCrates)
  );

  srcSet = lib.fileset.toSource {
    root = src;
    fileset = srcFileset;
  };

  # The fileset `srcSet` is made from, so a stage that needs a DIFFERENT slice of the same
  # tree can subtract from it before the copy is made — a store path cannot be filtered
  # afterwards. `ci-archetypes` is the one stage that does (nix/ci.nix says why it can).
  srcFileset = lib.fileset.difference (lib.fileset.unions (
    [
      # Workspace manifest and lock.
      (maybe "/Cargo.toml")
      (maybe "/Cargo.lock")

      # The `xtask` alias every `cargo xtask` invocation resolves through.
      (maybe "/.cargo/config.toml")

      # Root files the build/test/check read.
      (maybe "/README.md")
      # Generated products keep the persistent recipe lock out of version control.
      (maybe "/.gitignore")
      (maybe "/AGENTS.md")
      (maybe "/clippy.toml")
      (maybe "/rustfmt.toml")
      (maybe "/gates.toml")
      (maybe "/versions.toml")
      (maybe "/rust-toolchain.toml")
      (maybe "/deny.toml")
      (maybe "/typos.toml")
      (maybe "/taplo.toml")
      (maybe "/devenv.nix")
      # `cargo xtask check` reads flake.nix/flake.lock to tell a product from the template (the
      # `template` input is the origin record), and devenv.lock is the second lock
      # `tools/xtask/src/locks.rs` compares against flake.lock. The origin record itself is the
      # `template` node in flake.lock — see nix/template-pin.nix.
      #
      # The flake files are also build INPUTS: `check-archetypes` runs `cli new` inside a
      # derivation with this fileset as `TEMPLATE_SOURCE`, and `new` writes the product's
      # `template` input into flake.nix. Without them the product it generates has no flake at
      # all, and the pin step fails on a missing file rather than on the change under test.
      (maybe "/flake.nix")
      (maybe "/flake.lock")
      (maybe "/devenv.yaml")
      (maybe "/devenv.lock")
      # Product invariants read the permanent generator recipe.
      (maybe "/.template/recipe.toml")

      # Justfiles — discovered, not listed: the app-level Justfile per app, plus every
      # workspace member's Justfile (an app's core/client/shell Justfiles, a service's
      # Justfile). `maybe` so a member with no Justfile (a plain crate) contributes nothing.
      (maybe "/Justfile")
    ]
    ++ map maybe (map (a: "/apps/" + a + "/Justfile") disc.apps)
    ++ map maybe (map (m: "/" + m.dir + "/Justfile") disc.members)
    # Every `.rs`, `.toml` and build script in each crate directory.
    ++ map craneLib.fileset.commonCargoSources crateDirs
    # A client crate's test CA and leaf are PEM, which `commonCargoSources` does not carry
    # (the same reason `crates/oidc/src/testing` is listed below). Derived from the members,
    # not hardcoded, so `rename app` rewrites it.
    ++ map maybe (map (m: "/" + m.dir + "/src/testing")
      (lib.filter (m: lib.hasSuffix "/client" m.dir) disc.members))
    # Golden fixtures a crate's tests read at run time through CARGO_MANIFEST_DIR (probe
    # output, JSON lines, expected snapshots). None of them is `.rs` or `.toml`, so without
    # this they are absent in the sandbox and the tests fail with "No such file or
    # directory", while passing in every checkout. Derived from the members, not listed per
    # crate, so the next crate with fixtures needs no edit here.
    ++ map maybe (map (m: "/" + m.dir + "/tests/fixtures") disc.members)
    ++ [
      (maybe "/just")
      (maybe "/docs")
      (maybe "/proto")
      (maybe "/.claude/skills")
      (maybe "/templates")
      (maybe "/nix")
      (maybe "/.forgejo")
      (maybe "/tools/xtask/fixtures")
      # The template's generated-product gates assert exact ordered recipe steps.
      (maybe "/tools/cli/check-recipe.py")
      (maybe "/crates/i18n-data/src/data")
      # The test-only keys and CA `oidc::testing` embeds with `include_str!` — PEM, which
      # `commonCargoSources` does not carry. A product without `crates/oidc` contributes nothing.
      (maybe "/crates/oidc/src/testing")
      (maybe "/crates/README.md")
      (maybe "/crates/identity/README.md")
      (maybe "/services/README.md")
      (maybe "/tools/README.md")
    ]
    ++ trunkAssets
    ++ serviceConfigs
    ++ serviceSpas
    ++ trackedPlatforms
  )) buildJunk;

  # --- app-specific flags, derived from the manifests -------------------------

  hostExcludedArgs = concatMapStringsSep " " (m: "--exclude ${m.name}") disc.hostExcluded;
  cargoExtraArgs = "--locked --workspace${optionalString (disc.hostExcluded != [ ]) " ${hostExcludedArgs}"}";

  wasmPkgArgs = concatMapStringsSep " " (m: "-p ${m.name}") disc.wasmCrates;
  nativeOffCheck = concatMapStringsSep "\n" (m: "cargo check --locked -p ${m.name} --all-targets") disc.nativeOff;
  nativeOffClippy = concatMapStringsSep "\n"
    (m: "cargo clippy --locked -p ${m.name} --all-targets -- --no-deps -D warnings")
    disc.nativeOff;

  # --- shared cargo arguments --------------------------------------------------

  commonArgs = {
    # The fileset is what crane's derivations build from; `src` here is the fileset, not
    # the path mkPipeline was given (which lives in the `src` argument and in `srcSet`).
    src = srcSet;
    pname = pname;
    strictDeps = false;
    CARGO_PROFILE = "";
    inherit cargoExtraArgs;
    inherit nativeBuildInputs;
  };

  # --- layered dependency artifacts -------------------------------------------

  depsLock = builtins.fromTOML (builtins.readFile (r + "/Cargo.lock"));
  depsHasGitSource =
    builtins.any (p: lib.hasPrefix "git+" (p.source or "")) (depsLock.package or [ ]);

  # The fixed vendor path. See [ADR 0025](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/8aaabf5c1f90d6a725b2f82526b55cf057a7d3da/docs/adr/0025-dependency-artifacts-are-a-frozen-layer-and-a-delta-layer.md): a REGISTRY crate's per-crate path depends only on
  # its name/version/checksum, so remapping the vendor dir to a fixed sandbox path lets L0
  # and L1 (vendored from two different locks) reuse each other's units. Git crates are not
  # content-addressed, so the remap is dropped entirely when the live lock pins any.
  stableVendorDir =
    if depsHasGitSource then { } else {
      postPatch = ''
        mkdir -p "$NIX_BUILD_TOP/cargo-vendor"
        # One `cp -a` per registry, not one `ln` per crate. The registry entry in the vendor
        # directory is itself a symlink, and copying it as one would hand cargo a path that
        # resolves back to the per-lock store path this hook exists to get rid of — so it is
        # dereferenced into a real directory whose crate entries stay symlinks. Written
        # as a loop over crates first, and measured: ~3 400 forks per derivation, +13 s to
        # +23 s on every stage that consumes the artifacts. Not `cp -al`: the crate entries
        # are symlinks into the store, and a hard link across the sandbox's tmpfs/store
        # boundary is EXDEV, so `-l` dereferences and fails on all of them.
        for registry in "$cargoVendorDir"/*/; do
          cp -a "$(readlink -f "$registry")" "$NIX_BUILD_TOP/cargo-vendor/$(basename "$registry")"
        done
      '';
      postConfigure = ''
        sed -i "s|$cargoVendorDir|$NIX_BUILD_TOP/cargo-vendor|g" "$CARGO_HOME/config.toml"
        # The point of the rewrite is that no layer's fingerprints name the store path, so a
        # surviving reference is the bug this hook exists to prevent, not a cosmetic miss.
        if grep -qF "$cargoVendorDir" "$CARGO_HOME/config.toml"; then
          echo "vendor path $cargoVendorDir survived the rewrite:" >&2
          cat "$CARGO_HOME/config.toml" >&2
          exit 1
        fi
        echo "vendored sources are read from $NIX_BUILD_TOP/cargo-vendor"
      '';
    };

  # Every stage that consumes the layered artifacts has to agree on the vendored source
  # path, or cargo sees a different package set and recompiles all of it.
  hostArgs = commonArgs // stableVendorDir // {
    # Formatting does not vendor sources; keep this out of commonArgs so cargoFmt
    # does not try offline resolution with an empty registry.
    # A lock that does not cover the workspace fails as "no matching package named X
    # found", which names a crate rather than the stale Cargo.lock — auspex's first ci-pure
    # (RT-76): `add crux-app` wrote a member whose dependencies the committed lock lacked.
    # Upstream generators now refresh the lock (`xtask::cargo_lock`); the pinned external
    # CLI here predates that fix. Say what to do when a stale lock is committed anyway.
    # This must reach every live-lock build, not just L1: auspex's release package builds
    # its own dependencies and can fail before L1 gets to report the diagnosis.
    preBuild = ''
      if ! cargo metadata --locked --offline --format-version 1 >/dev/null 2>"$NIX_BUILD_TOP/metadata.err"; then
        cat "$NIX_BUILD_TOP/metadata.err" >&2
        echo "Cargo.lock does not cover the workspace: run \`cargo metadata\` (or any cargo command) and commit Cargo.lock" >&2
        exit 1
      fi
    '';
  };

  # The bottom layer's source: the committed snapshot, wrapped so it unpacks to the same
  # build directory name (`source`) as the real tree.
  hasDepsBase = depsBase != null && lib.pathExists depsBase;

  depsBaseSrc =
    if hasDepsBase then
      lib.fileset.toSource { root = depsBase; fileset = depsBase; }
    else
      lib.warn "nix/deps-base is missing; falling back to a full (slow) deps build" srcSet;

  depsVersionOf = root:
    (builtins.fromTOML (builtins.readFile (toString root + "/Cargo.toml"))).workspace.package.version or "0.0.1";
  depsVersion = depsVersionOf src;

  depsCommonArgs = hostArgs // {
    pname = pname;
    version = depsVersion;
  };

  nativeOffCheckTee = concatMapStringsSep "\n"
    (m: "cargo check --locked -p ${m.name} --all-targets 2>&1 | tee -a \"$NIX_BUILD_TOP/cargo.log\" >&2")
    disc.nativeOff;

  # L0's cargo command is read from the SNAPSHOT's manifests, never the live tree's (RT-76). The
  # snapshot is the only thing L0 builds, so it is the only thing L0 may be keyed by: derived from
  # the live tree, the `--exclude` list and the native-off `-p` passes made L0 a different
  # derivation in every product even over a byte-identical snapshot, and no product could
  # substitute the layer the template's CI had already pushed. Upstream `cli new` keeps that
  # snapshot byte for byte; auspex has its own rebased snapshot instead. What still reaches
  # L0 from the product: the pins, `pname`, and the live lock's git-source gate
  # (`stableVendorDir`), each of which makes L0 product-private, never wrong.
  # Upstream `check-template` asserts the two `drvPath`s agree; auspex has no such gate.
  depsBaseDisc = if hasDepsBase then import ./discovery.nix { inherit lib; } depsBase else disc;
  depsL0ExtraArgs =
    let excluded = concatMapStringsSep " " (m: "--exclude ${m.name}") depsBaseDisc.hostExcluded;
    in "--locked --workspace${optionalString (depsBaseDisc.hostExcluded != [ ]) " ${excluded}"}";
  depsL0NativeOffCheck = concatMapStringsSep "\n"
    (m: "cargo check --locked -p ${m.name} --all-targets") depsBaseDisc.nativeOff;

  depsCraneLib =
    if depsToolchain == null then craneLib else craneLib.overrideToolchain (_: depsToolchain);

  # L0 — the frozen snapshot.
  # The preflight is for the live workspace, not the frozen snapshot. Keep it out of
  # L0's inputs so changing the diagnosis does not rebuild the bottom dependency layer.
  depsL0 = depsCraneLib.buildDepsOnly ((if hasDepsBase then
    builtins.removeAttrs depsCommonArgs [ "preBuild" ] else depsCommonArgs) // {
    src = depsBaseSrc;
    version = if hasDepsBase then depsVersionOf depsBase else depsVersion;
    cargoExtraArgs = depsL0ExtraArgs;
    buildPhaseCargoCommand = ''
      cargoWithProfile check ${depsL0ExtraArgs} --all-targets
      cargoWithProfile build ${depsL0ExtraArgs}
      ${depsL0NativeOffCheck}
    '';
  });

  # L1 — the delta.
  depsL1Args = depsCommonArgs // {
    src = craneLib.mkDummySrc depsCommonArgs;
    pnameSuffix = "-deps-l1";
    cargoArtifacts = depsL0;
    cargoVendorDir = craneLib.vendorCargoDeps depsCommonArgs;
    doCheck = true;
    doInstallCargoArtifacts = true;
    env.CRANE_BUILD_DEPS_ONLY = 1;
    buildPhaseCargoCommand = ''
      cargoWithProfile check ${cargoExtraArgs} --all-targets 2>&1 | tee "$NIX_BUILD_TOP/cargo.log" >&2
      cargoWithProfile build ${cargoExtraArgs} 2>&1 | tee -a "$NIX_BUILD_TOP/cargo.log" >&2
      ${nativeOffCheckTee}
    '';
    checkPhaseCargoCommand = ''
      cargoWithProfile test ${cargoExtraArgs} --no-run 2>&1 | tee -a "$NIX_BUILD_TOP/cargo.log" >&2
      # Distinct registry crates, not a raw `Compiling` count: a raw count double-counts
      # the workspace stubs across the check and build passes and misses the `Checking`
      # units cargo only metadata-checks. Workspace members are the lines carrying a
      # `(/build/source/...)` path; every other `Compiling`/`Checking` line is a registry
      # crate (this repo has no git sources — see `depsHasGitSource`). Deduped across the
      # four passes, so a crate recompiled by check, build and the client pass counts
      # once. The `|| true`
      # is load-bearing: `set -o pipefail` turns a no-match `grep` into a failed pipeline,
      # and a no-change commit legitimately compiles nothing.
      compiled=$(
        sed -E 's/\x1b\[[0-9;]*m//g' "$NIX_BUILD_TOP/cargo.log" |
          grep -E '^ *(Compiling|Checking) ' |
          grep -vF '(' |
          sed -E 's/^ *(Compiling|Checking) +([^ ]+) v([^ ]+).*/\2 \3/' |
          sort -u |
          wc -l
      ) || true
      echo "layered deps: L1 compiled $compiled registry crates (see just deps::drift)"
    '';
  };

  depsL1 = craneLib.mkCargoDerivation depsL1Args;

  cargoArtifacts = depsL1;

  # --- services: one build, everything else derived from it --------------------
  #
  # Every service is built ONCE, here, and the image (and, from PR B, the NixOS module and its
  # VM checks) is derived from that one build. The alternative — one `cargo build` per artefact
  # — is how a template ends up with four spellings of the same binary and no guarantee they
  # are the same bytes.
  #
  # `cargoArtifacts = depsL1` is what makes this a build rather than a cold rebuild: the same
  # dependency layer every other stage consumes, so adding a service costs its own crates and
  # nothing else. `--locked -p <crate>` builds that one crate's binaries; a service that needs a
  # second binary adds a `[[bin]]`, not a second entry here.
  servicePackage = svc: craneLib.buildPackage (commonArgs // {
    preBuild = hostArgs.preBuild;
    pname = svc.pname;
    version = depsVersion;
    cargoArtifacts = depsL1;
    cargoExtraArgs = "--locked -p ${svc.pname}";
    # The tests are `ci-test`'s job, over the whole workspace and in one derivation. Here the
    # question is "does this package build", and re-running the suite per service would make a
    # package build depend on a database-shaped world it does not have.
    doCheck = false;
    # The bundle: the web shell's Trunk `dist/` when a `web-shell` is declared and its crate
    # has a trunk derivation, otherwise the fallback `spa` (the committed demo bundle).
    # `web.dir` is still configuration — nothing is served unless the deployment says so.
    postInstall =
      let
        hasWebShell = svc.webShellCrate != null
          && builtins.hasAttr svc.webShellCrate.name trunkDerivations;
        bundle = if hasWebShell
          then "${trunkDerivations.${svc.webShellCrate.name}}"
          else if svc.spa != null
          then src + "/${svc.dir}/${svc.spa}"
          else null;
      in
      optionalString (bundle != null) ''
        install -d $out/share/${svc.name}
        # `--no-preserve=mode`: the bundle is a store path, so a plain `cp -r` carries 444 out of
        # it and crane's `stripReferencesToRustToolchain` hook — which rewrites every text file
        # with `sed -i` — then fails on a read-only file in `$out` with an error naming neither
        # the copy nor the hook.
        cp -r --no-preserve=mode ${bundle} $out/share/${svc.name}/web
      '';
  });

  # The image, as a stream rather than a tarball: `dockerTools.streamLayeredImage` emits a
  # script that writes the archive to stdout, so a push pipes it into skopeo and a VM hands it
  # to podman without ever materialising a multi-hundred-megabyte file in the store.  #
  # The shape is decision 4: a fixed unprivileged uid with passwd/group entries (a numeric
  # `User` with no entry makes glibc's lookups fail in ways that name neither the uid nor the
  # image), CA roots and zoneinfo (a scratch image has neither, and a TLS handshake or a
  # `Local::now()` then fails at runtime), no shell and no coreutils (nothing in the image needs
  # them; the entrypoint is the binary), the base config only, and the variables that make the
  # binary find them.
  serviceImage = svc: package:
    let
      uid = "10001";
      caBundle = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
      # The bundle, at the FHS path rather than where the package keeps it. `contents` merges a
      # store path's top-level directories into the image root, so the package's
      # `$out/share/...` would land at `/share/...` — and copying it again in `extraCommands`
      # would put the whole bundle in the image twice. A one-file tree is the small layer that
      # gets it to `/usr/share` without that.
      hasWebBundle = svc.webShellCrate != null || svc.spa != null;
      webTree =
        if !hasWebBundle then
          null
        else
          pkgs.runCommand "${svc.name}-web" { } ''
            mkdir -p $out/usr/share/${svc.name}
            cp -r --no-preserve=mode ${package}/share/${svc.name}/web $out/usr/share/${svc.name}/web
          '';
    in
    pkgs.dockerTools.streamLayeredImage {
      name = svc.name;
      tag = "latest";
      contents = [ package pkgs.cacert pkgs.tzdata ] ++ optionals (webTree != null) [ webTree ];
      config = {
        User = "${uid}:${uid}";
        WorkingDir = "/";
        Entrypoint = [ "/bin/${svc.pname}" "serve" ];
        Env = [
          # The product's directory, like `nix/pipeline/module.nix`: the same file a host and a
          # container read is `/etc/<product>/<service>.toml`. The path is private to the image
          # either way; agreeing on the shape is what stops the two from drifting apart.
          "APP_CONFIG_DIR=/etc/${productName}"
          "SSL_CERT_FILE=${caBundle}"
          "TZDIR=${pkgs.tzdata}/share/zoneinfo"
          # The committed config binds loopback, which is right for a devenv process and
          # useless in a container: nothing outside the container can reach a socket on its
          # 127.0.0.1. One variable, and the deployment does not have to patch the file.
          "APP__APPLICATION__HOST=0.0.0.0"
        ] ++ optionals (hasWebBundle) [
          # Where the package put the bundle. The service serves nothing until `web.dir` is
          # set, which is why this is here and not a default in the binary.
          "APP__WEB__DIR=/usr/share/${svc.name}/web"
        ];
        ExposedPorts = optionalAttrs (svc.port != null) {
          "${toString svc.port}/tcp" = { };
        };
      };
      extraCommands = ''
        mkdir -p etc/${productName} tmp
        chmod 1777 tmp

        # The uid needs a name for the same reason it needs to be numeric at all: `User` in an
        # OCI config is resolved by the runtime, and anything that calls `getpwuid` — a logging
        # crate filling in a username, an allocator naming an arena — fails with "no such user"
        # rather than with the uid. No shell: `/sbin/nologin` does not exist in this image, and
        # nothing may log in anyway.
        echo 'app:x:${uid}:${uid}:app:/nonexistent:/sbin/nologin' > etc/passwd
        echo 'app:x:${uid}:' > etc/group

        # The base configuration only. `<name>.local.toml` is a devenv overlay (reflection on,
        # a service name for the local collector) and a deployment that baked it would publish
        # its schema to anyone who can reach the port.
        cp ${src + "/${svc.dir}/config/${svc.name}.toml"} etc/${productName}/${svc.name}.toml
      '';
    };

  # The archive, checked rather than pushed: the template never publishes its own images
  # (decision 5). What it does on every push is prove the archive parses and carries the
  # contract a deployment depends on — the entrypoint, the user, the exposed port, the variables
  # that make the binary find its configuration — so a broken image fails here, in ci-pure,
  # instead of at a registry the product configured.
  #
  # No network and no docker daemon: `streamLayeredImage` writes the archive to stdout, skopeo's
  # `docker-archive:` transport reads the file, and `--config --raw` prints the OCI config as
  # stored in it. The assertions say what they mean on failure, because "jq exit 1" in a
  # derivation names neither the image nor the field.
  serviceImageValidation = svc: image:
    pkgs.runCommand "${svc.name}-image-validate" {
      nativeBuildInputs = [ pkgs.skopeo pkgs.jq ];
    } ''
      ${image} > image.tar
      skopeo inspect --config --raw docker-archive:image.tar > config.json

      fail() {
        echo "services.${svc.name}: the image archive does not carry its contract: $*" >&2
        echo "--- image config ---" >&2
        cat config.json >&2
        exit 1
      }

      jq -e '.config.Entrypoint == [ "/bin/${svc.pname}", "serve" ]' config.json >/dev/null \
        || fail "the entrypoint is not [/bin/${svc.pname} serve]"
      jq -e '.config.User == "10001:10001"' config.json >/dev/null \
        || fail "the user is not 10001:10001"

      ${optionalString (svc.port != null) ''
        jq -e '.config.ExposedPorts | has("${toString svc.port}/tcp")' config.json >/dev/null \
          || fail "port ${toString svc.port}/tcp is not exposed"
      ''}

      # The three paths the binary needs at runtime, plus the config directory. `TZDIR` and
      # `SSL_CERT_FILE` are the ones whose absence surfaces as a runtime failure rather than a
      # startup one: a TLS handshake with no roots, a `Local::now()` with no zoneinfo.
      for variable in APP_CONFIG_DIR=/etc/${productName} TZDIR SSL_CERT_FILE; do
        jq -e --arg prefix "$variable" '.config.Env | any(startswith($prefix))' config.json >/dev/null \
          || fail "$variable is not in the environment"
      done

      ${optionalString (svc.webShellCrate != null || svc.spa != null) ''
        jq -e '.config.Env | any(startswith("APP__WEB__DIR=/usr/share/${svc.name}/web"))' config.json >/dev/null \
          || fail "APP__WEB__DIR is not in the environment, so the baked bundle would never be served"
      ''}

      echo "${svc.name}-image: archive parses, and carries its entrypoint, user, port and environment" > $out
    '';

  # The discovery entry for a service, by name. `services` is keyed by name and carries the
  # delivery surface (package, image, module, checks); the facts a check needs — the crate and
  # the bundle — come from discovery, so they are looked up rather than copied into the surface.
  discovered = listToAttrs (map (svc: {
    name = svc.name;
    value = svc;
  }) disc.services);

  # One service's NixOS module, built for a named product. Called once with this tree's own
  # product for the delivery outputs, and again by `twoProductsCheck` below under a second name —
  # which is how "two products on one host" is proved without generating a second product.
  serviceModule = forProduct: svc: package:
    import ./module.nix { inherit lib; } {
      product = forProduct;
      inherit (svc) name pname port hasDatabase settings settingsText host;
      hasWeb = svc.webShellCrate != null || svc.spa != null;
      inherit package;
    };

  # The delivery surface the flake maps into its outputs — one entry per discovered service,
  # named by the service. Everything in it derives from the ONE package build above: the image
  # from the package, the NixOS module's default package from it, and the VM checks from both.
  # Adding a service adds an entry; nothing here or in the flake learns its name.
  services = listToAttrs (map (svc:
    let
      package = servicePackage svc;
      image = serviceImage svc package;
      # The NixOS module: one generic module, called with the facts discovery read out of the
      # service's own manifest and configuration, and with the product whose names it carries.
      module = serviceModule productName svc package;
    in
    {
      name = svc.name;
      value = {
        inherit package image module;
        # The product the module is namespaced by; the flake needs it to name the option it
        # defaults the package of, without importing anything that depends on `pkgs`.
        product = productName;
        # The two checks that prove the artefacts run. A NixOS test is a Linux-only derivation,
        # and guarding here keeps `packages` meaningful on every system the flake declares.
        vmTests = optionalAttrs pkgs.stdenv.hostPlatform.isLinux (
          import ./vm-tests.nix { inherit lib pkgs; } {
            inherit (svc) name pname port hasAuth hasWebAuth;
            servesWebShell = serviceHasWebShell svc;
            inherit package image module;
            product = productName;
          }
        );
      };
    }) disc.services);

  # `<name>`, `<name>-image` and the two VM checks in `packages`, so a deployment can
  # `nix build .#<svc>`, a push can pipe `.#<svc>-image` into skopeo, and ci-pure runs the checks
  # — all without knowing about `services`.
  servicePackages = lib.concatMapAttrs (name: svc:
    {
      ${name} = svc.package;
      "${name}-image" = svc.image;
    }
    // lib.concatMapAttrs (kind: drv: { "${name}-${kind}-vm" = drv; }) (svc.vmTests or { })
    // optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
      "${name}-settings-merge" = settingsMergeCheck name svc.module;
      "${name}-image-validate" = serviceImageValidation discovered.${name} svc.image;
    }
  ) services;

  # The eval-level check that a deployment's `settings` MERGE. Two modules each set one key
  # under `settings.database`; with an attrset option the later definition would replace
  # `database` wholesale, the written file would lose one key, and the service would start with
  # half its configuration. `settings` is a freeform submodule precisely so that cannot happen,
  # and this is what holds it: evaluate a system with both definitions and read the file the
  # module generated. Evaluation is the whole test — the property is option merging, and the
  # file is generated at evaluation rather than built.
  settingsMergeCheck = name: module:
    let
      option = "services.${productName}.${name}";
      evaluated = pkgs.nixos [
        module
        {
          system.stateVersion = lib.trivial.release;
          services.${productName}.${name} = {
            enable = true;
            # Two definitions of the same option, one key each.
            settings.database = { host = "merge-check"; };
          };
        }
        { services.${productName}.${name}.settings.database = { port = 6543; }; }
      ];
      file = evaluated.config.environment.etc."${productName}/${name}.toml".source;
    in
    pkgs.runCommand "${name}-settings-merge" { } ''
      # Named failures, not bare `grep -q` exits: the regression this guards is a lost key, and a
      # bare exit code sends the reader to the module to work out which one went missing.
      grep -q 'host = "merge-check"' ${file} || {
        echo "${option}.settings: the host key was lost, so two definitions of settings.database did not merge — the generated file is:" >&2
        cat ${file} >&2
        exit 1
      }
      grep -q '^port = 6543' ${file} || {
        echo "${option}.settings: the port key was lost — the generated file is:" >&2
        cat ${file} >&2
        exit 1
      }
      echo "${option}.settings: two definitions merged into one file" > $out
    '';

  # Two products on one host — the check that every name the module creates is namespaced by the
  # product and not by the service's crate.
  #
  # Every scaffolded service is called `api` (that is what `just add service api` makes), so two
  # products' modules imported into one system are one system with two `api` services in it.
  # Before the module namespaced what it creates, that was a conflict on `services.api`,
  # `api.service` and `/etc/api/api.toml` at evaluation — or, where two identical definitions
  # merged, one product quietly running the other's unit.
  #
  # The second product is the same service's module under another name, which is what two
  # products ARE: the same template, the same crate names, a different `[publish].project`.
  # Evaluating the system is the whole test; nothing is built.
  #
  # The accounts are asserted too, which is why both services run with `dynamicUser = false`: a
  # dynamic uid would hide an account collision, and the account is the third of the three names
  # that collided.
  twoProductsCheck =
    if disc.services == [ ] then null
    else
      let
        svc = builtins.head disc.services;
        package = servicePackage svc;
        other = "second-product";
        system = pkgs.nixos [
          (serviceModule productName svc package)
          (serviceModule other svc package)
          {
            system.stateVersion = lib.trivial.release;
            services.${productName}.${svc.name} = {
              enable = true;
              dynamicUser = false;
            };
            services.${other}.${svc.name} = {
              enable = true;
              dynamicUser = false;
            };
          }
        ];
        units = builtins.attrNames system.config.systemd.services;
        configs = builtins.attrNames system.config.environment.etc;
        accounts = builtins.attrNames system.config.users.users;
        mine = "${productName}-${svc.name}";
        theirs = "${other}-${svc.name}";
        expect = ok: message: if ok then "" else "fail ${lib.escapeShellArg message}";
        expectUnit = unit:
          expect (lib.elem unit units)
            "${unit}.service is missing: the module did not namespace its unit by the product";
        expectConfig = path:
          expect (lib.elem path configs)
            "${path} is missing: the module did not namespace its configuration by the product";
        expectAccount = account:
          expect (lib.elem account accounts)
            "the account ${account} is missing: the module did not namespace it by the product";
      in
      pkgs.runCommand "two-products-one-system" { } ''
        fail() { echo "two products on one host: $*" >&2; exit 1; }

        # The option tree a deployment writes: `services.<product>.<service>`, both products in
        # one system, neither one a conflict. `or false` rather than a bare path, so a module
        # that stopped declaring the tree fails with the message below instead of a Nix trace
        # about a missing attribute.
        ${expect (system.config.services.${productName}.${svc.name}.enable or false)
          "services.${productName}.${svc.name}.enable is not the option the module declares"}
        ${expect (system.config.services.${other}.${svc.name}.enable or false)
          "services.${other}.${svc.name}.enable is not the option the module declares"}

        ${expectUnit mine}
        ${expectUnit theirs}
        ${expectConfig "${productName}/${svc.name}.toml"}
        ${expectConfig "${other}/${svc.name}.toml"}
        ${expectAccount mine}
        ${expectAccount theirs}

        echo "two products on one host: both units exist, both configurations are written, and both accounts are their own" > $out
      '';

  # A service that verifies credentials must be given an issuer by the VM tests, or both of its
  # checks fail at config load. The template cannot notice: its own `services/api` has no
  # `[auth]` (ADR 0027 — verification is a generator, not a default), so its VM tests never
  # exercise the path, and the breakage surfaced only in a product that had run
  # `cargo xtask add-auth-oidc`. This is the cheap check that would have caught it. It
  # evaluates the two VM tests' nodes and reads the environment they give the service; no VM is
  # built, which is also why it can run in `ci-pure` rather than in a stage of its own.
  #
  # Two properties. The mechanism: forced auth/web nodes carry all required settings, while
  # forced verifier-only nodes still carry just the issuer. The wiring: each REAL service
  # gets exactly what its parsed configuration and direct OIDC feature selection require.
  # Read the raw settings/manifest rather than discovery's booleans, so broken discovery or
  # a dropped VM-test argument cannot agree with itself. The template's wiring case is
  # vacuous for auth; a generated product exercises it. No VM or service binary is built.
  authIssuerCheck =
    if disc.services == [ ] || !pkgs.stdenv.hostPlatform.isLinux then null
    else
      let
        expected = hasAuth: hasWebAuth: {
          APP__AUTH__ISSUER = hasAuth;
          APP__AUTH__PUBLIC_URL = hasWebAuth;
          APP__AUTH__COOKIE_KEY = hasWebAuth;
        };
        testsFor = svc: hasAuth: hasWebAuth:
          let package = servicePackage svc;
          in import ./vm-tests.nix { inherit lib pkgs; } {
            inherit (svc) name pname port;
            servesWebShell = serviceHasWebShell svc;
            inherit hasAuth hasWebAuth package;
            image = serviceImage svc package;
            module = serviceModule productName svc package;
            product = productName;
          };
        # The environment each running thing is given, by node.
        environments = svc: tests: {
          "module-vm dynamic node" =
            tests.module.nodes.dynamic.services.${productName}.${svc.name}.environment;
          "module-vm peer node" =
            tests.module.nodes.peer.services.${productName}.${svc.name}.environment;
          "image-vm container" =
            tests.image.nodes.machine.virtualisation.oci-containers.containers.${svc.name}.environment;
        };
        failures = svc: tests: want:
          lib.concatMap
            (node: lib.concatMap
              (key:
                let has = (environments svc tests).${node} ? ${key};
                in lib.optional (has != want.${key})
                  "${svc.name} ${node}: ${key} is ${if has then "present" else "absent"}, expected ${
                    if want.${key} then "present" else "absent"}")
              (builtins.attrNames want))
            (builtins.attrNames (environments svc tests));
        sample = builtins.head disc.services;
        forced =
          failures sample (testsFor sample true true) (expected true true)
          ++ failures sample (testsFor sample true false) (expected true false);
        wired = lib.concatMap
          (svc:
            let
              manifest = builtins.fromTOML (builtins.readFile (src + "/${svc.dir}/Cargo.toml"));
              web = lib.elem "bff" (manifest.dependencies.oidc.features or [ ]);
            in failures svc (services.${svc.name}.vmTests) (expected (svc.settings ? auth) web))
          disc.services;
        problems = forced ++ wired;
      in
      pkgs.runCommand "auth-issuer-vm-environment" { } ''
        ${if problems == [ ] then "" else ''
          echo "the VM tests do not give services exactly their required auth environment:" >&2
          ${lib.concatMapStringsSep "\n" (p: "echo ${lib.escapeShellArg ("  " + p)} >&2") problems}
          exit 1
        ''}
        echo "VM auth environments: issuer for auth; public URL and test cookie key only for web login" > $out
      '';

  # The wasm closure gets the fixed vendor path but not the two-layer split ([ADR 0025](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/8aaabf5c1f90d6a725b2f82526b55cf057a7d3da/docs/adr/0025-dependency-artifacts-are-a-frozen-layer-and-a-delta-layer.md)).
  wasmArtifacts =
    if disc.wasmCrates == [ ] then null
    else
      craneLib.buildDepsOnly (hostArgs // {
        cargoExtraArgs = "--locked ${wasmPkgArgs} --target wasm32-unknown-unknown";
        doCheck = false;
      });

  clippyCargoVendorDir = craneLib.vendorCargoDeps hostArgs;

  # --- stage: enabled-ness -----------------------------------------------------

  # A stage is enabled by default when the tree has what it needs.
  autoEnabled = {
    "ci-fmt" = true;
    "ci-clippy" = true;
    "ci-build" = true;
    "ci-test" = true;
    "ci-typos" = true;
    "ci-xtask-check" = true;
    "ci-proto-check" = lib.pathExists (r + "/proto/buf.yaml");
    "ci-i18n-data" = lib.pathExists (r + "/crates/i18n");
    "ci-trunk-build" = disc.trunkCrates != [ ];
    "ci-android-generated" = disc.androidApps != [ ];
  };

  enabled = name:
    let s = stages.${name} or { }; in
    if hasAttr "enable" s then s.enable else autoEnabled.${name} or true;

  # --- stages ------------------------------------------------------------------

  # proto: committed generated code against a fresh `buf generate`.
  ciProtoCheck =
    if !(enabled "ci-proto-check") then null
    else
      let protoTools = (import ../proto-tools.nix { inherit pkgs inputs; }).packages;
      in
      pkgs.runCommand "ci-proto-check" {
        nativeBuildInputs = protoTools ++ [ pkgs.diffutils ];
      } ''
        # Nix sets `HOME` to /homeless-shelter in every build, so a
        # writable one has to be exported here.
        export HOME="$NIX_BUILD_TOP"
        cp -r --no-preserve=mode,ownership ${srcSet} work
        cd work
        buf lint proto
        buf format --diff --exit-code proto
        snapshot="$(mktemp -d)"
        cp -r crates/proto/src/gen "$snapshot/rust"
        cp -r proto/gen "$snapshot/shells"
        buf generate proto --template proto/buf.gen.yaml
        diff -r "$snapshot/rust" crates/proto/src/gen
        diff -r "$snapshot/shells" proto/gen
        echo "proto: generated code matches the .proto files" > $out
      '';

  # i18n: the committed CLDR slice against a fresh icu4x-datagen run.
  ciI18nData =
    if !(enabled "ci-i18n-data") then null
    else
      let
        cldr = import ../cldr.nix { inherit pkgs; };
        datagen = (import ../icu4x-datagen-tool.nix { inherit pkgs; }).datagen;
      in
      pkgs.runCommand "ci-i18n-data" {
        nativeBuildInputs = with pkgs; [ just datagen bash coreutils findutils diffutils gnugrep ];
        env.ICU4X_CLDR_ROOT = cldr.root;
      } ''
        cp -r --no-preserve=mode,ownership ${srcSet} work
        cd work
        just i18n::check-data
        echo "i18n: the CLDR slice matches its generator" > $out
      '';

  ciFmt =
    if !(enabled "ci-fmt") then null
    else
      craneLib.cargoFmt (commonArgs // {
        cargoExtraArgs = "--all";
        doInstallCargoArtifacts = false;
      });

  # The web shell, built by trunk for wasm32. One derivation per trunk crate; keyed by crate
  # name so a service declaring `web-shell` can look up its bundle.
  buildTrunk = crate:
    let
      depth = length (splitString "/" crate.dir);
      up = concatStrings (lib.replicate depth "../");
    in
    craneLib.buildTrunkPackage (hostArgs // {
      cargoArtifacts = wasmArtifacts;
      wasm-bindgen-cli = pkgs.wasm-bindgen-cli_0_2_126;
      nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ pkgs.tailwindcss_4 ];
      # Nix sets `HOME` to /homeless-shelter in every build (the old `env.HOME` never reached
      # the derivation), and trunk creates its cache directory under it whenever a tool it finds on
      # PATH is not the version it requires, to download the pinned one. A writable HOME and
      # XDG_CACHE_HOME turn that into a clean, visible download attempt; `--offline` below turns
      # it into an error naming the tool; and the Trunk.toml tool pins are dropped so the
      # nixpkgs tools are used whatever version nixpkgs ships (the pins are for devenv users).
      trunkExtraBuildArgs = "--offline=true";
      preBuild = hostArgs.preBuild + ''
        export HOME="$NIX_BUILD_TOP/home"
        export XDG_CACHE_HOME="$HOME/.cache"
        mkdir -p "$XDG_CACHE_HOME"
        cd ${crate.dir}
        if [ -f Trunk.toml ]; then
          sed -i '/^\[tools\]$/,/^\[/{/^\[tools\]$/d;/^[A-Za-z_-]* *=/d}' Trunk.toml
        fi
      '';
      postBuild = ''
        mv ./dist ${up}
        cd ${up}
      '';
    });

  # Trunk derivations keyed by crate name, so a service's `webShellCrate.name` resolves to its
  # built `dist/`. The `trunkStages` list is kept for `ciTrunkBuild`'s aggregate.
  trunkDerivations = builtins.listToAttrs (map (crate: {
    name = crate.name;
    value = buildTrunk crate;
  }) disc.trunkCrates);
  trunkStages = map buildTrunk disc.trunkCrates;

  # Does this service bake a real web shell, rather than the fallback bundle? The VM tests assert
  # a hashed `.wasm` only when it does, and a product with no web shell (`--drop-example`, a
  # terminal-only Crux app) has none to find.
  serviceHasWebShell = svc:
    svc.webShellCrate != null && builtins.hasAttr svc.webShellCrate.name trunkDerivations;

  ciTrunkBuild =
    if !(enabled "ci-trunk-build") then null
    else if disc.trunkCrates == [ ] then null
    else if length disc.trunkCrates == 1 then head trunkStages
    else pkgs.symlinkJoin { name = "ci-trunk-build"; paths = trunkStages; };

  # The Android app's generated tree. Loops over every app with an Android project.
  ciAndroidGenerated =
    if !(enabled "ci-android-generated") then null
    else
      let
        ndk = import ../android-ndk.nix { inherit pkgs lib; };
        boltffi = (import ../boltffi-tool.nix { inherit pkgs; }).cli;
      in
      craneLib.mkCargoDerivation (hostArgs // {
        inherit cargoArtifacts;
        pnameSuffix = "-android-generated";
        nativeBuildInputs = commonArgs.nativeBuildInputs ++ ndk.packages ++ [ boltffi pkgs.jdk ];
        cargoExtraArgs = "";
        buildPhaseCargoCommand = ''
          export HOME="$NIX_BUILD_TOP"
          ${ndk.exports}
        '' + concatMapStringsSep "\n" (app:
          let
            core = "${app}-core";
            generated = "apps/${app}/platforms/android/generated";
          in
          ''
            cargo run --locked --package ${core} --bin codegen \
              --features codegen,facet_typegen -- \
              --language kotlin --output-dir "$PWD/${generated}"
            (cd apps/${app}/core && boltffi pack android)
            cargo run --quiet --locked -p xtask -- i18n-resources ${app} android
          ''
        ) disc.androidApps;
        installPhaseCommand = ''
        '' + concatMapStringsSep "\n" (app:
          let generated = "apps/${app}/platforms/android/generated"; in
          ''
            test -d ${generated}
            mkdir -p "$out/$(dirname ${generated})"
            cp -r ${generated} "$out/$(dirname ${generated})/generated"
          ''
        ) disc.androidApps;
        doInstallCargoArtifacts = false;
      });

  ciXtaskCheck =
    if !(enabled "ci-xtask-check") then null
    else
      craneLib.mkCargoDerivation (hostArgs // {
        inherit cargoArtifacts;
        pnameSuffix = "-xtask-check";
        buildPhaseCargoCommand = "cargo xtask check";
        installPhaseCommand = ''echo "xtask check passed" > $out'';
        doInstallCargoArtifacts = false;
      });

  ciClippy =
    if !(enabled "ci-clippy") then null
    else
      craneLib.mkCargoDerivation (hostArgs // {
        inherit cargoArtifacts;
        cargoVendorDir = clippyCargoVendorDir;
        buildPhaseCargoCommand = ''
          ${optionalString (wasmArtifacts != null) ''
            mkdir -p "''${CARGO_TARGET_DIR:-target}"
            zstd -d ${wasmArtifacts}/target.tar.zst --stdout \
              | tar -x --skip-old-files -C "''${CARGO_TARGET_DIR:-target}"
            test -d "''${CARGO_TARGET_DIR:-target}/wasm32-unknown-unknown"
          ''}
          cargo clippy --locked --workspace ${hostExcludedArgs} --all-targets -- --no-deps -D warnings
          ${nativeOffClippy}
          ${optionalString (disc.wasmCrates != [ ]) ''
            cargo clippy --locked ${wasmPkgArgs} --target wasm32-unknown-unknown -- --no-deps -D warnings
          ''}
        '';
        installPhaseCommand = ''echo "clippy passed" > $out'';
        doInstallCargoArtifacts = false;
      });

  ciBuild =
    if !(enabled "ci-build") then null
    else
      craneLib.mkCargoDerivation (hostArgs // {
        inherit cargoArtifacts;
        buildPhaseCargoCommand = "cargo build --locked --workspace${optionalString (disc.hostExcluded != [ ]) " ${hostExcludedArgs}"}";
        installPhaseCommand = ''echo "build succeeded" > $out'';
        doInstallCargoArtifacts = false;
      });

  ciTest =
    if !(enabled "ci-test") then null
    else
      craneLib.cargoNextest (hostArgs // {
        inherit cargoArtifacts;
        cargoNextestExtraArgs = "--no-tests=warn";
        nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ pkgs.cargo-deny ];
        # `git`, for the one test whose SUBJECT is git: `tools/cli` asks `git rev-parse
        # --is-inside-work-tree` whether a directory is inside a checkout, and that answer is
        # what decides how `nix flake lock` is told about a product's flake. The only test that
        # proves it runs the real binary against a real repository, because a stub of git would
        # only prove the stub. Without this the test passes in the dev shell, where git is on
        # PATH, and dies in the sandbox with `Os { code: 2, kind: NotFound }` — exactly the
        # false green it exists to prevent.
        #
        # `nativeCheckInputs` rather than `nativeBuildInputs`: it is a test-time tool, stdenv
        # adds it only when `doCheck` runs, and nothing in the build itself spawns git. No other
        # test in the workspace needs it — `repo::tests` and the rename tests write `.git` FILES
        # as fixtures and never execute git.
        # `util-linux` (auspex-effects): its `prlimit` puts git under a memory limit before exec.
        nativeCheckInputs = [ pkgs.git ] ++ lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.util-linux ];
        env = lib.optionalAttrs (disc.services != [ ]) {
          APP_CONFIG_DIR = (builtins.head disc.services).dir + "/config";
        };
        doInstallCargoArtifacts = false;
      });

  ciTypos =
    if !(enabled "ci-typos") then null
    else
      pkgs.runCommand "ci-typos" {
        nativeBuildInputs = [ pkgs.typos ];
      } ''
        cd ${srcSet}
        typos --config typos.toml
        echo "typos passed" > $out
      '';

  # --- extra stages ------------------------------------------------------------

  extraStageDrvs = mapAttrs (name: spec:
    let
      fn = if lib.isFunction spec then spec else spec.drv;
      pure = if lib.isFunction spec then true else (spec.pure or true);
    in
    {
      drv = fn {
        inherit hostArgs depsL0 depsL1 wasmArtifacts commonArgs src srcSet srcFileset discovery;
        inherit pkgs craneLib;
      };
      inherit pure;
    }
  ) extraStages;

  # --- the aggregate -----------------------------------------------------------

  pureStages = [
    (optionalAttrs (enabled "ci-fmt") { "ci-fmt" = ciFmt; })
    (optionalAttrs (enabled "ci-clippy") { "ci-clippy" = ciClippy; })
    (optionalAttrs (enabled "ci-build") { "ci-build" = ciBuild; })
    (optionalAttrs (enabled "ci-test") { "ci-test" = ciTest; })
    (optionalAttrs (enabled "ci-typos") { "ci-typos" = ciTypos; })
    (optionalAttrs (enabled "ci-xtask-check") { "ci-xtask-check" = ciXtaskCheck; })
    (optionalAttrs (enabled "ci-proto-check") { "ci-proto-check" = ciProtoCheck; })
    (optionalAttrs (enabled "ci-i18n-data") { "ci-i18n-data" = ciI18nData; })
    (optionalAttrs (enabled "ci-trunk-build") { "ci-trunk-build" = ciTrunkBuild; })
  ];

  pureExtra = mapAttrs' (name: e: lib.nameValuePair name e.drv)
    (lib.filterAttrs (_: e: e.pure) extraStageDrvs);

  nonPureExtra = lib.filterAttrs (_: e: !e.pure) extraStageDrvs;

  pureSet = lib.foldl' (a: b: a // b) { } (pureStages ++ [ pureExtra servicePackages ])
    // optionalAttrs (twoProductsCheck != null) { "two-products-one-system" = twoProductsCheck; }
    // optionalAttrs (authIssuerCheck != null) { "auth-issuer-vm-environment" = authIssuerCheck; };

  # `ci-pure` — one aggregate derivation the workflow builds with a single step. Each
  # stage is still its own derivation (so each is cached/substituted individually); the
  # aggregate references each one as an input, so Nix builds them in parallel and the body
  # runs only after all succeed.
  ciPure =
    let
      names = builtins.attrNames pureSet;
      echo = n: "echo \"  ✓ ${n}: ${pureSet.${n}}\"";
    in
    pkgs.runCommand "ci-pure" { } (
      "echo \"ci-pure: all pure stages passed\"\n"
      + concatMapStringsSep "\n" echo names
      + "\necho \"ci-pure\" > $out\n"
    );

  packages = pureSet // {
    "ci-pure" = ciPure;
    # Not a stage: upstream exposes this so `check-template` can assert a generated product's
    # bottom layer is the template's derivation (RT-76) — `devenv eval outputs.deps-l0.drvPath`
    # in both trees. Auspex has no such gate; this remains available for manual inspection.
    "deps-l0" = depsL0;
    "ci-android-generated" = ciAndroidGenerated;
  } // nonPureExtra;

in
{
  inherit packages discovery services serviceModule;
  # The internals, for `extraStages` authors and for the delivery outputs to build on.
  inherit commonArgs hostArgs depsL0 depsL1 cargoArtifacts wasmArtifacts srcSet srcFileset;
  inherit depsHasGitSource;
}
