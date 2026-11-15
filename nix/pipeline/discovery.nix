# Workspace discovery: one reader of the manifests, shared by the pipeline library and by
# the devenv modules (nix/web.nix, nix/services.nix, nix/rust.nix). Apps are DISCOVERED,
# never listed: a crate joins CI by carrying `[package.metadata.app]` in its own Cargo.toml
# (see tools/xtask/src/invariants.rs for the check that keeps the keys honest).
#
# This is deliberately plain Nix: it takes `lib` and a `root` path and reads files. Nothing
# here depends on devenv or on any flake input, so the same function evaluates inside a
# devenv module and inside a plain flake.
#
# `root` is a real path on disk (the repository checkout). Reading manifests via
# `builtins.readDir`/`builtins.readFile` is cheap: only the category directories are walked,
# never `target/` or any build output.
#
# Path footgun to keep in mind: `path + "/" + x` drops the slash (the intermediate value is
# a *string*, not a path). Build every path as `toString root + "/" + ...`.
{ lib }:
root:
let
  r = toString root;

  # --- small helpers -----------------------------------------------------------

  toml = p: builtins.fromTOML (builtins.readFile p);

  # The workspace manifest, or `null` when the tree has none.
  rootManifest =
    let p = r + "/Cargo.toml";
    in if builtins.pathExists p then toml p else null;

  # Directories immediately under `dir` (for `*` glob expansion).
  subdirs = dir:
    if !builtins.pathExists dir then [ ]
    else
      lib.attrNames (lib.filterAttrs (_: ty: ty == "directory") (builtins.readDir dir));

  # Expand one `*`-wildcard glob (each `*` matches one directory level, directories only).
  # The workspace globs this repository uses are all of this shape: `crates/*`,
  # `services/*`, `tools/*`, `apps/*/core`, `apps/*/shells/*`, `apps/*/app`,
  # `apps/*/client`.
  expandGlob = pattern:
    let
      segs = lib.splitString "/" pattern;
      walk = dir: remaining:
        if remaining == [ ] then [ dir ]
        else
          let
            h = lib.head remaining;
            t = lib.tail remaining;
          in
          if h == "*" then
            lib.concatMap (n: walk (dir + "/" + n) t) (subdirs dir)
          else
            walk (dir + "/" + h) t;
    in walk r segs;

  # Every workspace member directory (relative to root), from the manifest's own globs.
  memberDirs =
    let
      globs = if rootManifest == null then [ ] else rootManifest.workspace.members or [ ];
      rel = d: lib.removePrefix (r + "/") d;
      isDir = d: builtins.pathExists (r + "/" + d);
    in
    lib.unique (builtins.filter isDir (map rel (lib.concatMap expandGlob globs)));

  # One member: its directory, package name and `[package.metadata.app]` table.
  member = dir:
    let
      manifest = toml (r + "/" + dir + "/Cargo.toml");
      pkg = manifest.package or { };
      meta = pkg.metadata.app or { };
    in
    {
      inherit dir;
      name = pkg.name or (lib.last (lib.splitString "/" dir));
      kind = meta.kind or null;
      # Per-crate facts the pipeline derives its passes from.
      wasm = meta.wasm or false;
      host = meta.host or true;
      trunk = meta.trunk or false;
      nativeFeature = meta.native-feature or null;
      meta = meta;
    };

  members = map member memberDirs;

  byName = lib.foldl' (acc: m: acc // { ${m.name} = m; }) { } members;

  # The app name a member belongs to, or null — derived from its path.
  appOf = m:
    let segs = lib.splitString "/" m.dir;
    in if lib.length segs >= 2 && lib.head segs == "apps" then lib.elemAt segs 1 else null;

  # --- derived sets the pipeline and devenv modules consume --------------------

  # Crates the host `--workspace` passes must exclude (wasm-only shells).
  hostExcluded = lib.filter (m: !m.host) members;

  # Crates that compile for wasm32-unknown-unknown (the wasm deps + wasm clippy passes).
  wasmCrates = lib.filter (m: m.wasm) members;

  # Trunk web shells: a crate built by trunk for wasm32, with its dev-server facts.
  trunkCrates = lib.filter (m: m.trunk) members;

  # Crates with a feature that must additionally be linted OFF.
  nativeOff = lib.filter (m: m.nativeFeature != null) members;

  # Apps (directories under apps/), with their platform projects.
  apps = subdirs (r + "/apps");
  androidApps = lib.filter (a: builtins.pathExists (r + "/apps/" + a + "/platforms/android")) apps;
  appleApps = lib.filter (a: builtins.pathExists (r + "/apps/" + a + "/platforms/apple")) apps;

  # Services (directories under services/), with what devenv's postgres needs and what the
  # delivery outputs derive from.
  serviceDirs = subdirs (r + "/services");
  # The workspace member that implements a service. Usually the directory name, but the
  # manifest is the authority — a directory is a place, a crate name is what `-p` takes.
  memberAt = dir: lib.findFirst (m: m.dir == dir) null members;
  service = name:
    let
      dir = "services/" + name;
      base = r + "/" + dir + "/config/" + name + ".toml";
      overlay = r + "/" + dir + "/config/" + name + ".local.toml";
      baseCfg = if builtins.pathExists base then toml base else { };
      overlayCfg = if builtins.pathExists overlay then toml overlay else { };
      # The facts devenv's Postgres needs are the LOCAL view: the base file deliberately carries
      # no database location (the image bakes it, and a committed location would make every URL
      # deployment refuse to start), so the location lives in the `.local.toml` overlay devenv
      # runs with. Merging is what makes one reader serve both the dev shell and a deployment.
      localCfg = lib.recursiveUpdate baseCfg overlayCfg;
      app = baseCfg.application or { };
      db = localCfg.database or { };
      baseDb = baseCfg.database or { };
      member = memberAt dir;
      meta = if member == null then { } else member.meta;
      manifest = if member == null then { } else toml (r + "/" + dir + "/Cargo.toml");
    in
    {
      inherit dir name;
      # The crate `--locked -p` names, and the binary the image runs.
      pname = if member == null then name else member.name;
      port = app.port or null;
      # A built single-page application INSIDE the service's directory (e.g. `web/dist`), or
      # null. It is the FALLBACK when no web shell is declared; `--drop-example` removes that
      # reference. The required `cargo xtask check` gate refuses dangling or non-Trunk
      # `web-shell` names. Nothing here serves the bundle; the service does, from config.
      spa = meta.spa or null;
      # The web shell crate this service serves, declared by its Cargo package name. The
      # pipeline resolves it to a trunk crate, builds it, and copies its `dist/` in place of
      # the fallback `spa` above.
      webShell = meta.web-shell or null;
      webShellCrate =
        if meta.web-shell or null == null then null
        else
          let crate = byName.${meta.web-shell} or null;
          in if crate == null || !crate.trunk then null else crate;
      # The committed configuration, parsed and verbatim. The module writes the file from the
      # first (so a deployment can override one key) and falls back to the second when nothing
      # was overridden, so the comments survive. `host` is the default bind address.
      settings = baseCfg;
      settingsText = if builtins.pathExists base then builtins.readFile base else "";
      host = app.host or "127.0.0.1";
      # Does this service own a schema at all? A `[database]` section is the signal, and it
      # keeps working across the split because the pool size stays in the base file even when
      # the location does not. This is what a module's `database.enable` defaults to.
      hasDatabase = baseDb != { } || db != { };
      # Does this service verify credentials? An `[auth]` section is the signal, and only
      # `cargo xtask add-auth-oidc` writes one (ADR 0027). Such a service refuses to start without
      # an issuer, which the base file cannot carry (it is the deployment's), so the VM tests
      # have to supply one (`vm-tests.nix`). Read from the base file, as `hasDatabase` is: the
      # overlay is a development convenience and a deployment never sees it.
      hasAuth = (baseCfg.auth or { }) != { };
      # The direct production dependency is authoritative: `scopes` can be omitted when
      # using its serde default, while the BFF still requires its startup settings.
      # Dev-dependency feature unification must not turn a verifier-only service into web auth.
      hasWebAuth = lib.elem "bff" (manifest.dependencies.oidc.features or [ ]);
      # The LOCAL development database — null unless the overlay names a location (host). This
      # is what devenv creates a role and a database from; a deployment does not inherit it.
      database = if db.host or null == null then null else {
        username = db.username or null;
        name = db.database_name or db.username or null;
        port = db.port or 5432;
        host = db.host;
      };
    };
  services = map service serviceDirs;

  # The rust target list for the single toolchain: host is implicit; the cross targets are
  # derived from what the tree actually contains — a web shell implies wasm32, an Android
  # project the four Android triples. The ORDER is load-bearing, not cosmetic: the toolchain
  # is a symlinkJoin whose `paths` order is part of its store hash, so a different order is
  # a different toolchain and a full rebuild of the frozen dependency layers. This is
  # master's order (wasm32 first, then Android in android.nix's own list order).
  #
  # Apple triples are deliberately ABSENT. Master's dev shell never contributed them (no
  # module added `languages.rust.targets` for Apple), so including them would change the
  # toolchain derivation and rebuild depsL0/depsL1. Apple projects build on macOS only, and
  # the `nix` runners are not macOS; reintroducing the three Apple triples is a separate
  # decision, not this restructuring.
  rustTargets = lib.unique (
    lib.optionals (wasmCrates != [ ]) [ "wasm32-unknown-unknown" ]
    ++ lib.optionals (androidApps != [ ]) [
      "aarch64-linux-android"
      "armv7-linux-androideabi"
      "x86_64-linux-android"
      "i686-linux-android"
    ]
  );
in
{
  inherit root memberDirs members byName rustTargets;
  inherit hostExcluded wasmCrates trunkCrates nativeOff;
  inherit apps androidApps appleApps services;
  inherit appOf;
  rootManifest = rootManifest;
}
