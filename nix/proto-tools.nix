# The wire contract's toolchain: buf, protoc, and the codegen plugins `buf generate` runs.
#
# Deliberately *not* a devenv module — a values file, like nix/versions.nix — because two
# consumers need the same derivations: the dev shell (nix/proto.nix puts them on PATH) and
# the CI pipeline (nix/ci.nix runs `buf generate` inside a sandboxed derivation, which
# needs every plugin as a build input rather than as shell content). Import with
# `import ./proto-tools.nix {inherit pkgs inputs;}`.
{
  pkgs,
  inputs,
}: let
  versions = import ./versions.nix;

  # buf.build/protocolbuffers/java and kotlin are protoc's own built-in generators;
  # buf runs them with `protoc_builtin: java` / `protoc_builtin: kotlin`, which needs
  # protoc on PATH. nixpkgs carries protobuf 36.2, which matches the pinned plugin
  # version — no override, but an assertion: protoc comes from nixpkgs, so a
  # `devenv update` can move it, and generated Java newer than the Gradle runtime fails
  # only at class initialisation (versions.toml's [proto] comment). The shell refuses to
  # enter instead. If it fires, either pin the runtimes to the new protoc together or
  # override protobuf back to the pin.
  protobuf =
    assert pkgs.lib.assertMsg
    (pkgs.protobuf.version == versions.proto.protobuf-plugin)
    "nixpkgs' protobuf is ${pkgs.protobuf.version}, but versions.toml pins protobuf-plugin ${versions.proto.protobuf-plugin}: the generated Java and Kotlin would disagree with the Gradle runtime";
      pkgs.protobuf;
  #
  # buf.build/connectrpc/kotlin ships as a self-contained fat jar on Maven Central.
  # The jar bundles everything it needs (protobuf-java, kotlinpoet, moshi, connect-rpc
  # itself), so wrapping it as `exec java -jar <jar>` is enough to run it as a protoc
  # plugin. Its version is read from versions.toml the way boltffi and icu4x-datagen
  # are, so `cargo xtask check-versions` can still refuse a disagreement.
  connectKotlinJar = pkgs.fetchurl {
    url =
      "https://repo1.maven.org/maven2/com/connectrpc/protoc-gen-connect-kotlin/"
      + "${versions.proto.connect-kotlin}/protoc-gen-connect-kotlin-${versions.proto.connect-kotlin}.jar";
    hash = "sha256-55RZkNOdwmz0C4SJybYaOjI6+LGR9p0tLeM5aQ9fKqs=";
  };
  protoc-gen-connect-kotlin = pkgs.writeShellApplication {
    name = "protoc-gen-connect-kotlin";
    runtimeInputs = [pkgs.jdk];
    text = ''
      exec java -jar ${connectKotlinJar} "$@"
    '';
  };

  # buf.build/apple/swift is swift-protobuf's protoc-gen-swift binary, built at the
  # pinned version. nixpkgs carries a newer release (1.39.0), so the version and source
  # are overridden; everything else (build system, install phase) stays as nixpkgs wrote
  # it. Both the version and the tag fetched are versions.toml's swift-protobuf-plugin,
  # so a bump there changes the source and fails on the hash below rather than
  # relabelling the old release. There is no Swift runtime pin to check it against:
  # SwiftPM resolves swift-protobuf through connect-swift (versions.toml says why).
  #
  # The source is *named* with the version, here and for connect-swift below. A
  # fixed-output fetch's store path is its name plus its hash, and fetchFromGitHub's
  # default name is "source": bumping only the tag left the path unchanged, so Nix reused
  # the old release from the store and never fetched the new one, and the hash check
  # never ran. Measured, 2026-11-03: swift-protobuf-plugin set to 1.38.0 entered the
  # shell on 1.38.1's source.
  protoc-gen-swift = pkgs.protoc-gen-swift.overrideAttrs (_final: _prev: {
    version = versions.proto.swift-protobuf-plugin;
    src = pkgs.fetchFromGitHub {
      name = "swift-protobuf-${versions.proto.swift-protobuf-plugin}-source";
      owner = "apple";
      repo = "swift-protobuf";
      rev = versions.proto.swift-protobuf-plugin;
      hash = "sha256-/FuqnLJm7x0tVvGmno6fHbXoQlWuVClWMKOnqgCllYw=";
      fetchSubmodules = true;
    };
  });

  # buf.build/connectrpc/swift is connect-swift's protoc-gen-connect-swift binary.
  # It has external SwiftPM dependencies (swift-nio, swift-nio-http2, swift-nio-ssl,
  # swift-protobuf) that the Nix sandbox cannot download, so they are vendored with
  # fetchSwiftPMDeps from the committed Package.resolved. Its version is read from
  # versions.toml the same way connect-kotlin's is.
  protoc-gen-connect-swift = pkgs.stdenv.mkDerivation (finalAttrs: {
    pname = "protoc-gen-connect-swift";
    version = versions.proto.connect-swift;

    src = pkgs.fetchFromGitHub {
      name = "connect-swift-${finalAttrs.version}-source";
      owner = "connectrpc";
      repo = "connect-swift";
      rev = finalAttrs.version;
      hash = "sha256-ZiVHkVYdbb4DKTcNM3zNYE/KthJKs8WBWTnm5VAq/B0=";
    };

    nativeBuildInputs = [
      pkgs.swift
      pkgs.swiftpm
    ];

    # No Foundation or Dispatch in buildInputs: Swift 6's default SDK includes both, and
    # naming them is an evaluation warning on every shell entry since the nixpkgs bump.

    swiftpmDeps = pkgs.fetchSwiftPMDeps {
      inherit (finalAttrs) pname version src;
      hash = "sha256-gcHsOBLUe0cX0c4MN5xqtDB+1ATeWZ/XAW0mQw9/8CI=";
    };

    swiftpmFlags = ["--product" "protoc-gen-connect-swift"];

    installPhase = ''
      runHook preInstall
      install -Dm755 "$(swiftpmBinPath)/protoc-gen-connect-swift" \
        "$out/bin/protoc-gen-connect-swift"
      runHook postInstall
    '';

    meta = with pkgs.lib; {
      description = "Connect RPC code generator for Swift";
      homepage = "https://github.com/connectrpc/connect-swift";
      license = with licenses; [licenses.asl20];
      mainProgram = "protoc-gen-connect-swift";
      inherit (pkgs.swift.meta) platforms badPlatforms;
    };
  });
in {
  packages =
    [
      pkgs.buf
      protobuf
      protoc-gen-connect-kotlin
      protoc-gen-swift
      protoc-gen-connect-swift
    ]
    # The flake pins protoc-gen-buffa, protoc-gen-connect-rust, protoc-gen-buffa-packaging
    # and protoc-gen-protovalidate-buffa together; the RUNTIME crates they generate against
    # are pinned in versions.toml and checked by `cargo xtask check-versions`, because a
    # plugin bump without a matching crate bump produces stubs that compile against the
    # wrong API.
    ++ inputs.connectrpc.lib.plugins pkgs;
}
