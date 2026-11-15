//! Enforce that `versions.toml` is actually the single source of truth.
//!
//! Nix reads `versions.toml` directly (`builtins.fromTOML`) and just reads it at recipe
//! runtime, so those two need no checking. Cargo and Gradle cannot read it, so their
//! literals are verified here instead. Without this, a bump lands in two of four places
//! and fails much later with an error that names neither.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use toml::Value;

use crate::repo;

pub struct Report {
    pub checked: usize,
    pub problems: Vec<String>,
    /// What each check contributed to `checked`, in the order they run. See
    /// `invariants::Report::breakdown`; this count is load-bearing the same way, and moves
    /// in a generated product (53 here, 64 after `check-template` scaffolds an app and a
    /// platform), so attributing it matters for the same reason.
    pub breakdown: Vec<(&'static str, usize)>,
}

/// A version check, named so the breakdown can label it.
///
/// The signature is spelled out because the list below coerces each function to it; a check
/// that does not match cannot be added to the list, which is the point.
type Check = fn(&Path, &Value, &mut usize, &mut Vec<String>) -> Result<()>;

fn lookup<'a>(versions: &'a Value, table: &str, key: &str) -> Result<&'a str> {
    versions
        .get(table)
        .and_then(|t| t.get(key))
        .and_then(Value::as_str)
        .with_context(|| format!("versions.toml is missing [{table}].{key}"))
}

/// First run of digits in a line, if any.
fn first_number(line: &str) -> Option<String> {
    let start = line.find(|c: char| c.is_ascii_digit())?;
    Some(
        line[start..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect(),
    )
}

/// The SDK level declared for `key`, or None if this file does not declare it.
///
/// Two DSL shapes have to work. `minSdk`/`targetSdk` are plain assignments:
///
///     minSdk = 34
///
/// while AGP 8.13+ writes the compile SDK as a block:
///
///     compileSdk {
///         version = release(36)
///     }
///
/// so the search looks at a small window of lines rather than just the matched one.
fn kts_number(source: &str, key: &str) -> Option<String> {
    let lines: Vec<&str> = source.lines().map(str::trim).collect();
    let idx = lines.iter().position(|line| {
        !line.starts_with("//")
            && line.starts_with(key)
            && !line[key.len()..].starts_with(|c: char| c.is_alphanumeric())
    })?;
    let end = lines.len().min(idx + 4);
    lines[idx..end].iter().find_map(|line| first_number(line))
}

/// The quoted value assigned to `key`, or None if this file does not assign one.
fn kts_string(source: &str, key: &str) -> Option<String> {
    source.lines().map(str::trim).find_map(|line| {
        let rest = line.strip_prefix(key)?.trim_start();
        let rest = rest.strip_prefix('=')?.trim();
        rest.strip_prefix('"')?
            .split('"')
            .next()
            .map(ToOwned::to_owned)
    })
}

pub fn check() -> Result<Report> {
    let root = repo::root();
    let versions =
        std::fs::read_to_string(root.join("versions.toml")).context("reading versions.toml")?;
    let versions: Value = toml::from_str(&versions).context("parsing versions.toml")?;

    let mut problems = Vec::new();
    let mut checked = 0;

    // Paired with their own names through `stringify!`, so a renamed check cannot keep an
    // old label in the breakdown (RT-16).
    macro_rules! checks {
        ($($f:path),+ $(,)?) => { [$((stringify!($f), $f as Check)),+] };
    }

    let mut breakdown: Vec<(&'static str, usize)> = Vec::new();
    for (name, check) in checks![
        cargo_pins_match,
        rust_toolchain_matches,
        buf_plugins_are_pinned,
        gradle_runtimes_match,
        gradle_wrappers_match,
        xcode_package_matches,
        gradle_sdk_levels_match,
        boltffi_min_sdk_matches,
        datagen_matches_the_icu_crates,
        sea_orm_cli_matches_the_sea_orm_crate,
    ] {
        let before = checked;
        check(&root, &versions, &mut checked, &mut problems)?;
        breakdown.push((name, checked - before));
    }

    if checked == 0 {
        bail!("no version checks ran — this is itself a bug");
    }
    Ok(Report {
        checked,
        problems,
        breakdown,
    })
}

/// Read a single value out of `versions.toml`, for shell callers (`just`, CI).
pub fn get(key: &str) -> Result<String> {
    let root = repo::root();
    let versions =
        std::fs::read_to_string(root.join("versions.toml")).context("reading versions.toml")?;
    let versions: Value = toml::from_str(&versions)?;
    let mut current = &versions;
    for segment in key.split('.') {
        current = current
            .get(segment)
            .with_context(|| format!("versions.toml has no key {key:?}"))?;
    }
    current
        .as_str()
        .map(ToOwned::to_owned)
        .with_context(|| format!("versions.toml key {key:?} is not a string"))
}

fn cargo_pins_match(
    root: &Path,
    versions: &Value,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    // ---- Cargo: exact pins in [workspace.dependencies] -------------------------------
    let manifest =
        std::fs::read_to_string(root.join("Cargo.toml")).context("reading Cargo.toml")?;
    let manifest: Value = toml::from_str(&manifest).context("parsing Cargo.toml")?;
    let deps = manifest
        .get("workspace")
        .and_then(|w| w.get("dependencies"))
        .context("Cargo.toml has no [workspace.dependencies]")?;

    // `exact` mirrors the `=` prefix the manifest uses. boltffi and facet are pinned
    // exactly because a partial bump fails confusingly (see versions.toml); the proto
    // runtimes use ordinary requirements but must still match the buf plugins.
    //
    // The five OpenTelemetry entries are a version-locked set, not five independent pins:
    // tracing-opentelemetry 0.34 requires opentelemetry ^0.33 and the 0.33 SDK/OTLP/API
    // crates require each other, so a partial bump fails at compile time with trait
    // mismatches that name neither crate (see versions.toml [otel]). Checking all five
    // against the one table is what turns that silent failure into a named one. Note the
    // crate name / key mismatch on the SDK: Cargo spells it `opentelemetry_sdk` (an
    // underscore, because it is a Rust identifier) while versions.toml uses the
    // registry's `opentelemetry-sdk`.
    for (dep, table, key, exact) in [
        ("boltffi", "tools", "boltffi", true),
        ("facet", "crates", "facet", true),
        ("buffa", "proto", "buffa", false),
        ("buffa-descriptor", "proto", "buffa", false),
        ("connectrpc", "proto", "connectrpc", false),
        (
            "connectrpc-reflection",
            "proto",
            "connectrpc-reflection",
            false,
        ),
        ("opentelemetry", "otel", "opentelemetry", false),
        ("opentelemetry_sdk", "otel", "opentelemetry-sdk", false),
        ("opentelemetry-otlp", "otel", "opentelemetry-otlp", false),
        (
            "opentelemetry-semantic-conventions",
            "otel",
            "opentelemetry-semantic-conventions",
            false,
        ),
        (
            "tracing-opentelemetry",
            "otel",
            "tracing-opentelemetry",
            false,
        ),
    ] {
        *checked += 1;
        let version = lookup(versions, table, key)?;
        let want = if exact {
            format!("={version}")
        } else {
            version.to_owned()
        };
        let got = deps.get(dep).and_then(|d| {
            d.as_str().map(ToOwned::to_owned).or_else(|| {
                d.get("version")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
        });
        match got {
            Some(actual) if actual == want => {}
            Some(actual) => problems.push(format!(
                "Cargo.toml: [workspace.dependencies].{dep} is {actual:?}, versions.toml says {want:?}"
            )),
            None => problems.push(format!(
                "Cargo.toml: [workspace.dependencies].{dep} is missing (expected {want:?})"
            )),
        }
    }

    Ok(())
}

fn rust_toolchain_matches(
    root: &Path,
    versions: &Value,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    // ---- rust-toolchain.toml ---------------------------------------------------------
    *checked += 1;
    let toolchain_path = root.join("rust-toolchain.toml");
    let toolchain =
        std::fs::read_to_string(&toolchain_path).context("reading rust-toolchain.toml")?;
    let toolchain: Value = toml::from_str(&toolchain).context("parsing rust-toolchain.toml")?;
    let want_channel = lookup(versions, "rust", "channel")?;
    match toolchain
        .get("toolchain")
        .and_then(|t| t.get("channel"))
        .and_then(Value::as_str)
    {
        Some(actual) if actual == want_channel => {}
        Some(actual) => problems.push(format!(
            "rust-toolchain.toml: channel is {actual:?}, versions.toml says {want_channel:?}"
        )),
        None => problems.push("rust-toolchain.toml: [toolchain].channel is missing".to_owned()),
    }

    Ok(())
}

fn buf_plugins_are_pinned(
    root: &Path,
    _versions: &Value,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    // ---- buf.gen.yaml: every plugin is local, and every local is one we know --------
    //
    // Remote plugins are anonymous BSR calls that rate-limit CI — that is the bug this
    // check exists to prevent. Once a plugin is local, the agreement between plugin and
    // runtime that the old check enforced by matching the version literal in a `remote:`
    // line moves to `nix/proto.nix`: connect-kotlin, protoc-gen-swift and
    // protoc-gen-connect-swift are built from versions.toml's pins, and protoc — which
    // comes from nixpkgs — is asserted equal to `protobuf-plugin`, so the shell will not
    // enter on drift. The runtime halves are `gradle_runtimes_match` and
    // `xcode_package_matches`, against the same file.
    *checked += 1;
    let gen_path = root.join("proto/buf.gen.yaml");
    let template = std::fs::read_to_string(&gen_path).context("reading proto/buf.gen.yaml")?;

    let mut found_any_plugin = false;
    for line in template.lines().map(str::trim) {
        // Refuse remote plugins. There are no remote calls left — this check is the
        // gate that keeps the guarantee.
        if let Some(plugin) = line.strip_prefix("- remote: ") {
            found_any_plugin = true;
            problems.push(format!(
                "proto/buf.gen.yaml: remote plugin {plugin:?} — all plugins are local \
                 now and must not be remote; remote plugins are anonymous BSR calls \
                 that rate-limit CI"
            ));
            continue;
        }

        if let Some(plugin) = line.strip_prefix("- protoc_builtin: ") {
            found_any_plugin = true;
            // The names protoc calls its built-in generators, without the protoc-gen-
            // prefix. protoc itself is nixpkgs' protobuf on PATH; `nix/proto.nix` asserts
            // its version is versions.toml's `protobuf-plugin`.
            match plugin.trim() {
                "java" | "kotlin" | "cpp" | "csharp" | "objc" | "php" | "python" | "ruby" => {}
                unknown => problems.push(format!(
                    "proto/buf.gen.yaml: unknown protoc_builtin plugin {unknown:?}"
                )),
            }
            *checked += 1;
        }

        if let Some(plugin) = line.strip_prefix("- local: ") {
            found_any_plugin = true;
            let plugin = plugin.trim();
            match plugin {
                // Rust plugins — from the connectrpc flake, pinned separately
                "protoc-gen-buffa"
                | "protoc-gen-buffa-packaging"
                | "protoc-gen-connect-rust"
                | "protoc-gen-protovalidate-buffa"
                // Shell-language plugins — each built from versions.toml pins
                | "protoc-gen-connect-kotlin"
                | "protoc-gen-swift"
                | "protoc-gen-connect-swift" => {}
                unknown => problems.push(format!(
                    "proto/buf.gen.yaml: unknown local plugin {unknown:?} — add it to \
                     this check and to nix/proto.nix"
                )),
            }
            *checked += 1;
        }
    }

    if !found_any_plugin {
        problems.push(
            "proto/buf.gen.yaml: no plugins found — the file must declare at least one \
             plugin"
                .to_owned(),
        );
    }

    Ok(())
}

/// Every Android project whose pins must agree with versions.toml.
///
/// The apps, and the skeleton `add-platform` falls back to. The skeleton is included for
/// the reason it is the weakest part of this design: nothing builds it, so a pin that drifts
/// there is invisible until the day someone generates from it. This check is what keeps it
/// honest in the meantime — it costs nothing and it covers the versions that actually break
/// a build.
fn android_projects(root: &Path) -> Vec<PathBuf> {
    let mut projects = repo::expand(root, "apps/*/platforms/android");
    let skeleton = root.join("templates/platform-android/apps/__name__/platforms/android");
    if skeleton.is_dir() {
        projects.push(skeleton);
    }
    projects
}

fn gradle_runtimes_match(
    root: &Path,
    versions: &Value,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    // ---- Gradle: the runtimes the generated Kotlin and Java compile against -----------
    for project in android_projects(root) {
        let path = project.join("gradle/libs.versions.toml");
        if !path.is_file() {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", repo::rel(root, &path)))?;
        let catalog: Value = toml::from_str(&source)
            .with_context(|| format!("parsing {}", repo::rel(root, &path)))?;
        for (key, version_key) in [
            ("protobuf", "protobuf-runtime"),
            ("connectrpc", "connect-kotlin"),
        ] {
            *checked += 1;
            let want = lookup(versions, "proto", version_key)?;
            match catalog
                .get("versions")
                .and_then(|v| v.get(key))
                .and_then(Value::as_str)
            {
                Some(actual) if actual == want => {}
                Some(actual) => problems.push(format!(
                    "{}: [versions].{key} = {actual:?}, versions.toml says {want:?}",
                    repo::rel(root, &path)
                )),
                None => problems.push(format!(
                    "{}: [versions].{key} is missing (expected {want:?})",
                    repo::rel(root, &path)
                )),
            }
        }
    }

    Ok(())
}

/// The Gradle every wrapper resolves.
///
/// The wrapper is copied verbatim into each new platform project, so a distribution that
/// drifts between the apps and the skeleton means a generated project quietly builds
/// against a Gradle no CI run has ever used.
fn gradle_wrappers_match(
    root: &Path,
    versions: &Value,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    let want = lookup(versions, "android", "gradle")?;
    let marker = format!("gradle-{want}-bin.zip");
    for project in android_projects(root) {
        let path = project.join("gradle/wrapper/gradle-wrapper.properties");
        if !path.is_file() {
            continue;
        }
        *checked += 1;
        let source = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", repo::rel(root, &path)))?;
        let declared = source
            .lines()
            .find_map(|line| line.trim().strip_prefix("distributionUrl="))
            .unwrap_or_default()
            .to_owned();
        if !declared.contains(&marker) {
            problems.push(format!(
                "{}: distributionUrl is {declared:?}, versions.toml says Gradle {want}",
                repo::rel(root, &path)
            ));
        }
    }
    Ok(())
}

fn xcode_package_matches(
    root: &Path,
    versions: &Value,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    // ---- Xcode: the Connect package the generated Swift compiles against -------------
    for apple in repo::expand(root, "apps/*/platforms/apple") {
        let path = apple.join("project.yml");
        if !path.is_file() {
            continue;
        }
        *checked += 1;
        let source = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", repo::rel(root, &path)))?;
        let want = lookup(versions, "proto", "connect-swift")?;
        // `from: 1.2.3` under the Connect package. Matched loosely on purpose: project.yml
        // is not parsed here, and the alternative is a YAML dependency for one line.
        let declared = source
            .lines()
            .map(str::trim)
            .find_map(|line| line.strip_prefix("from:"))
            .map(|value| value.trim().trim_matches('"').to_owned());
        match declared {
            Some(actual) if actual == want => {}
            Some(actual) => problems.push(format!(
                "{}: the Connect package is `from: {actual}`, versions.toml says {want}",
                repo::rel(root, &path)
            )),
            None => problems.push(format!(
                "{}: no `from:` version for the Connect package (expected {want})",
                repo::rel(root, &path)
            )),
        }
    }

    Ok(())
}

fn gradle_sdk_levels_match(
    root: &Path,
    versions: &Value,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    // ---- Gradle: SDK levels in every Android module ---------------------------------
    // The NDK clang wrapper filename embeds the API level, so a minSdk that drifts from
    // versions.toml means devenv exports a linker path that does not exist. Library
    // modules legitimately omit targetSdk, so each key is only compared where declared —
    // with a floor check below so "declared nowhere" cannot pass silently.
    for android_dir in android_projects(root) {
        let mut gradle_files = Vec::new();
        repo::find_files(&android_dir, "build.gradle.kts", &mut gradle_files);
        gradle_files.sort();
        let mut saw_min_sdk = false;
        for path in gradle_files {
            let source = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", repo::rel(root, &path)))?;
            // buildToolsVersion is a string, not a number, so it is compared separately
            // below. Its agreement matters as much as the SDK levels: AGP installing a
            // build-tools version the Nix SDK lacks is what makes a read-only store path
            // fail, minutes into a build, with a message about writability.
            if let Some(actual) = kts_string(&source, "buildToolsVersion") {
                *checked += 1;
                let want = lookup(versions, "android", "build-tools")?;
                if actual != want {
                    problems.push(format!(
                        "{}: buildToolsVersion = {actual:?}, versions.toml says {want:?}",
                        repo::rel(root, &path)
                    ));
                }
            }
            for (key, version_key) in [
                ("minSdk", "min-sdk"),
                ("compileSdk", "compile-sdk"),
                ("targetSdk", "target-sdk"),
            ] {
                let Some(actual) = kts_number(&source, key) else {
                    continue;
                };
                *checked += 1;
                if key == "minSdk" {
                    saw_min_sdk = true;
                }
                let want = lookup(versions, "android", version_key)?;
                if actual != want {
                    problems.push(format!(
                        "{}: {key} = {actual}, versions.toml says {want}",
                        repo::rel(root, &path)
                    ));
                }
            }
        }
        if !saw_min_sdk {
            problems.push(format!(
                "{}: no Gradle module declares minSdk",
                repo::rel(root, &android_dir)
            ));
        }
    }

    Ok(())
}

fn boltffi_min_sdk_matches(
    root: &Path,
    versions: &Value,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    // ---- boltffi.toml: the same Android API level, a fourth time ---------------------
    for core in repo::expand(root, "apps/*/core") {
        let path = core.join("boltffi.toml");
        if !path.exists() {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", repo::rel(root, &path)))?;
        let config: Value = toml::from_str(&source)
            .with_context(|| format!("parsing {}", repo::rel(root, &path)))?;
        if let Some(min_sdk) = config
            .get("targets")
            .and_then(|t| t.get("android"))
            .and_then(|a| a.get("min_sdk"))
            .and_then(Value::as_integer)
        {
            *checked += 1;
            let want = lookup(versions, "android", "min-sdk")?;
            if min_sdk.to_string() != want {
                problems.push(format!(
                    "{}: [targets.android].min_sdk = {min_sdk}, versions.toml says {want}",
                    repo::rel(root, &path)
                ));
            }
        }
    }

    Ok(())
}

/// `major.minor` of a version or a Cargo version requirement, whichever the string is.
///
/// `2.3`, `=2.3.0` and `^2.3.1` all answer `2.3`; a string with no digits answers None, which
/// the caller turns into a problem rather than a pass.
fn major_minor(spec: &str) -> Option<String> {
    let start = spec.find(|c: char| c.is_ascii_digit())?;
    let numbers: String = spec[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = numbers.split('.');
    let (Some(major), Some(minor)) = (parts.next(), parts.next()) else {
        return None;
    };
    Some(format!("{major}.{minor}"))
}

/// Collect every dependency table in a manifest, including `[target.<cfg>.dependencies]`,
/// which nests one level below the top.
fn collect_dep_tables<'a>(table: &'a toml::Table, out: &mut Vec<&'a toml::Table>) {
    for (key, value) in table {
        let Some(inner) = value.as_table() else {
            continue;
        };
        if key.ends_with("dependencies") {
            out.push(inner);
        } else {
            collect_dep_tables(inner, out);
        }
    }
}

/// Every dependency requirement declared in the manifests the version checks scan.
///
/// The root manifest is read only through `[workspace.dependencies]`; a member is read as a
/// whole manifest, including `[target.<cfg>.dependencies]`. A dependency that inherits its
/// requirement with `workspace = true` is skipped — the root entry it inherits from is in this
/// list already, and counting both would report the same version twice or, worse, report the
/// member as having no requirement. A dependency with no version at all (a path or git
/// dependency) is returned with `None`, so a caller can name it rather than silently skip it.
fn dependency_requirements(root: &Path) -> Result<Vec<(PathBuf, String, Option<String>)>> {
    let mut manifests: Vec<PathBuf> = vec![root.join("Cargo.toml")];
    for glob in crate::invariants::MEMBER_GLOBS {
        manifests.extend(
            repo::expand(root, glob)
                .into_iter()
                .map(|dir| dir.join("Cargo.toml")),
        );
    }

    let mut requirements = Vec::new();
    for path in manifests {
        if !path.is_file() {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", repo::rel(root, &path)))?;
        let manifest: Value = toml::from_str(&source)
            .with_context(|| format!("parsing {}", repo::rel(root, &path)))?;
        let root_manifest = path == root.join("Cargo.toml");
        let mut tables = Vec::new();
        // Members are scanned as a whole manifest; the root one only through
        // `[workspace.dependencies]`, since a member's own `[workspace.dependencies]` (a
        // mistake, but a legal one) is not a list a build ever reads.
        if root_manifest {
            if let Some(deps) = manifest
                .get("workspace")
                .and_then(|w| w.get("dependencies"))
                .and_then(Value::as_table)
            {
                // The table itself, not what `collect_dep_tables` finds inside it: that
                // helper looks for tables *named* `*dependencies`, and a dependency entry
                // is a table named after the dependency.
                tables.push(deps);
            }
        } else if let Some(members) = manifest.as_table() {
            collect_dep_tables(members, &mut tables);
        }

        for table in tables {
            for (dep, spec) in table {
                if spec
                    .get("workspace")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    continue;
                }
                let requirement = spec.as_str().map(ToOwned::to_owned).or_else(|| {
                    spec.get("version")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                });
                requirements.push((path.clone(), dep.clone(), requirement));
            }
        }
    }

    Ok(requirements)
}

/// The `icu4x-datagen` pin and the `icu_*` crates must be the same minor version.
///
/// The tool writes `crates/i18n-data/src/data`, and the `icu_*` crates read it. Their data
/// formats are only guaranteed to agree inside one minor release — markers gain fields, and a
/// marker's struct is named by path in the generated code — so skew between the two halves is
/// a compile error *inside generated code*, naming databake and `src/data/mod.rs` and never
/// mentioning the version that moved. `just proto::check` pins its plugins for the same reason
/// (`buf_plugins_are_pinned`); this is that check for the CLDR slice (ADR 0021).
///
/// Minor, not patch: ICU4X releases data-format changes in minors and patch releases are
/// fixes, and requiring equality on the patch would mean a `cargo update` bump of one `icu_*`
/// crate failing this check while the tool being a patch behind is genuinely harmless.
///
/// Note the check reads requirements, not resolved versions — `2.3` in a manifest against
/// `2.3.0` in versions.toml is a pass. That is deliberate: the manifests state the range they
/// are compatible with, and this asserts that range is the tool's release.
fn datagen_matches_the_icu_crates(
    root: &Path,
    versions: &Value,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    let tool = lookup(versions, "tools", "icu4x-datagen")?;
    let Some(want) = major_minor(tool) else {
        problems.push(format!(
            "versions.toml: [tools].icu4x-datagen = {tool:?} has no major.minor to compare"
        ));
        return Ok(());
    };

    for (path, dep, requirement) in dependency_requirements(root)? {
        if !dep.starts_with("icu_") {
            continue;
        }
        *checked += 1;
        let got = requirement.as_deref().and_then(major_minor);
        let shown = requirement.as_deref().unwrap_or("(no version requirement)");
        match got {
            Some(got) if got == want => {}
            Some(got) => problems.push(format!(
                "{file}: {dep} = {shown:?} is {got}, but versions.toml pins \
                 icu4x-datagen {tool} ({want}) — the tool writes \
                 crates/i18n-data/src/data and this crate reads it, so bump both \
                 together and run `just i18n::generate-data`",
                file = repo::rel(root, &path)
            )),
            None => problems.push(format!(
                "{}: {dep} = {shown:?} has no major.minor to compare against \
                 icu4x-datagen {tool}",
                repo::rel(root, &path)
            )),
        }
    }

    Ok(())
}

/// The `sea-orm-cli` pin and the `sea-orm` crates must be the same minor version.
///
/// The CLI writes `src/entities/*.rs`, and `sea-orm`'s derive macros read them — so this is
/// the datagen relationship with a different pair of halves. `sea-orm-cli` is a binary from
/// crates.io, not a library `Cargo.lock` resolves for the runtime, which is why nothing in
/// Cargo noticed when the CLI moved a whole major ahead of the crate: `Cargo.lock`'s
/// sea-orm-cli 1.1.20 is pulled in by `sea-orm-migration`, and the recipe runs a different
/// binary. Pinning it in `versions.toml` and checking it here is what connects the two.
///
/// Minor, not patch, for the datagen reason: sea-orm spells entity output differently between
/// minors, not between fixes, and a patch bump of either side is harmless.
///
/// `sea-orm-migration` is included because it re-exports `sea-orm` and its own entity shape;
/// its requirement can drift from `sea-orm`'s and this is where that is caught.
fn sea_orm_cli_matches_the_sea_orm_crate(
    root: &Path,
    versions: &Value,
    checked: &mut usize,
    problems: &mut Vec<String>,
) -> Result<()> {
    let tool = lookup(versions, "tools", "sea-orm-cli")?;
    let Some(want) = major_minor(tool) else {
        problems.push(format!(
            "versions.toml: [tools].sea-orm-cli = {tool:?} has no major.minor to compare"
        ));
        return Ok(());
    };

    for (path, dep, requirement) in dependency_requirements(root)? {
        if dep != "sea-orm" && dep != "sea-orm-migration" {
            continue;
        }
        *checked += 1;
        let got = requirement.as_deref().and_then(major_minor);
        let shown = requirement.as_deref().unwrap_or("(no version requirement)");
        match got {
            Some(got) if got == want => {}
            Some(got) => problems.push(format!(
                "{file}: {dep} = {shown:?} is {got}, but versions.toml pins \
                 sea-orm-cli {tool} ({want}) — the CLI writes src/entities and this crate \
                 reads them, so bump both together and re-run `entities` against a real \
                 schema, diffing tools/xtask/fixtures/entities/",
                file = repo::rel(root, &path)
            )),
            None => problems.push(format!(
                "{}: {dep} = {shown:?} has no major.minor to compare against \
                 sea-orm-cli {tool}",
                repo::rel(root, &path)
            )),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    /// The breakdown accounts for the whole count. See the twin of this test in
    /// `invariants`, which explains why it is the assertion that matters: here the list is
    /// an array rather than a sequence of calls, so a check omitted from it does not run at
    /// all — but one that counts outside the loop would still go unattributed.
    #[test]
    fn the_breakdown_accounts_for_every_check() {
        let report = check().expect("the version checks run against this repository");
        let summed: usize = report.breakdown.iter().map(|(_, count)| count).sum();
        assert_eq!(
            summed, report.checked,
            "the breakdown sums to {summed} and the reported total is {}",
            report.checked
        );
    }

    /// Write a buf.gen.yaml into a temp directory and run the check against it.
    fn run_on(dir: &TempDir, template: &str) -> (usize, Vec<String>) {
        let p = dir.path().join("proto/buf.gen.yaml");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, template).unwrap();
        let versions: Value = toml::from_str("[proto]").unwrap();
        let mut checked = 0;
        let mut problems = Vec::new();
        buf_plugins_are_pinned(dir.path(), &versions, &mut checked, &mut problems).unwrap();
        (checked, problems)
    }

    /// `buf_plugins_are_pinned` refuses any `remote:` line, because remote plugins are
    /// anonymous BSR calls that rate-limit CI.
    #[test]
    fn a_remote_plugin_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (_, problems) = run_on(
            &dir,
            "version: v2\nplugins:\n  - remote: buf.build/protocolbuffers/java:v36.2\n",
        );
        assert_eq!(
            problems.len(),
            1,
            "a remote plugin must produce exactly one problem"
        );
        assert!(
            problems[0].contains("remote plugin"),
            "the problem must name the remote plugin: got {}",
            problems[0]
        );
    }

    /// `buf_plugins_are_pinned` refuses an unknown local plugin, because an unrecognised
    /// name has no version gate.
    #[test]
    fn an_unknown_local_plugin_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (_, problems) = run_on(
            &dir,
            "version: v2\nplugins:\n  - local: protoc-gen-nobody\n",
        );
        assert_eq!(
            problems.len(),
            1,
            "an unknown local plugin must produce exactly one problem"
        );
        assert!(
            problems[0].contains("unknown local plugin"),
            "the problem must name the unknown plugin: got {}",
            problems[0]
        );
    }

    /// `buf_plugins_are_pinned` refuses an unknown `protoc_builtin` name, because the
    /// built-in list is the protoc built-in set — anything else is a typo.
    #[test]
    fn an_unknown_protoc_builtin_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (_, problems) = run_on(&dir, "version: v2\nplugins:\n  - protoc_builtin: go\n");
        assert_eq!(
            problems.len(),
            1,
            "an unknown protoc_builtin must produce exactly one problem"
        );
        assert!(
            problems[0].contains("unknown protoc_builtin"),
            "the problem must name the unknown builtin: got {}",
            problems[0]
        );
    }

    /// Known plugins pass cleanly with the right count.
    #[test]
    fn every_known_plugin_passes() {
        let dir = tempfile::tempdir().unwrap();
        let (checked, problems) = run_on(
            &dir,
            concat!(
                "version: v2\n",
                "plugins:\n",
                "  - local: protoc-gen-buffa\n",
                "  - local: protoc-gen-buffa-packaging\n",
                "  - local: protoc-gen-connect-rust\n",
                "  - local: protoc-gen-protovalidate-buffa\n",
                "  - local: protoc-gen-connect-kotlin\n",
                "  - local: protoc-gen-swift\n",
                "  - local: protoc-gen-connect-swift\n",
                "  - protoc_builtin: java\n",
                "  - protoc_builtin: kotlin\n",
            ),
        );
        assert!(
            problems.is_empty(),
            "all known plugins must pass, got: {problems:?}"
        );
        // 1 for the function, 7 local + 2 protoc_builtin = 9 plugin lines
        assert_eq!(checked, 10, "checked must be 10 (1 fn + 9 plugins)");
    }
}
