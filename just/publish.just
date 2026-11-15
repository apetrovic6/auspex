# Publishing service images: validate on every push, push on master only.
#
# The template never publishes its own images (decision 5). What it does on every push is
# prove the archive parses and carries the contract a deployment depends on — that is the
# `<svc>-image-validate` derivation in `nix/pipeline/pipeline.nix`, which ci-pure builds. The
# push is the shared part: a product configures its own registry project (`[publish]` in
# versions.toml, which `cli new` sets) and the same step runs from this recipe.
#
# One spelling for the template, a product and a person. A second copy in a workflow is how a
# tag rule and a credential rule drift apart.
#
# Import with: mod publish '<relative-path-to-repo-root>/just/publish.just'

# The repository root. `source_directory()` is this file's directory even when the module is
# imported from the root Justfile, which is what makes it the root from here.
root := source_directory() / ".."

# validate every service image archive — what CI does, with no registry and no credentials
#
# Every service that has an image, from the flake's own packages rather than a `services/*`
# scan: a worker has no image, and a directory would try to publish one. Each archive is
# streamed to a tar and inspected with `skopeo inspect` — entrypoint, user, exposed port, and
# the environment the binary needs — by the same derivation ci-pure builds, so this is the
# local spelling of a check that already ran.
[script('bash')]
validate:
    set -euo pipefail
    cd '{{ root }}'

    system=$(nix eval --raw --impure --expr builtins.currentSystem)
    names=$(nix eval --json ".#packages.$system" --apply builtins.attrNames \
        | jq -r '.[] | select(endswith("-image-validate")) | sub("-image-validate$"; "")')
    if [ -z "$names" ]; then
        echo "publish::validate: no service has an image — nothing to check" >&2
        exit 0
    fi

    for name in $names; do
        nix build ".#$name-image-validate" --no-link --print-out-paths >/dev/null
        echo "  ✓ $name: archive parses and carries its contract"
    done

# push every service image to the configured registry, tagged with the git revision
#
# The archives are validated inside this recipe rather than through a just dependency: the
# gate checker resolves a dependency by its qualified name, and a module naming itself is a
# self-reference it refuses. Nothing is published before `nix build .#<svc>-image-validate`
# succeeds, which is the same derivation ci-pure built.
#
# Master only, and that is the workflow's decision rather than this recipe's: a person may
# deliberately push a branch build, and a recipe that guessed would be wrong half the time.
#
# Unconfigured is a skip, HALF-configured is an error. A product whose registry secrets are
# not set must not have a red pipeline for the one step that is supposed to skip; a product
# with one of the two set has a misconfiguration, and skipping it would hide exactly the
# mistake this step exists to catch — so it fails and names the missing variable.
#
# The workflow maps both from `secrets.HARBOR_*`: Forgejo does not put secrets in a step's
# environment unless the workflow says so, and before that line existed `push` saw two empty
# strings and reported success forever (`cargo xtask check` now asserts the mapping).
[script('bash')]
push *names:
    set -euo pipefail
    cd '{{ root }}'

    have_username=0
    [ -n "${HARBOR_USERNAME:-}" ] && have_username=1
    have_password=0
    [ -n "${HARBOR_PASSWORD:-}" ] && have_password=1

    if [ "$have_username" -eq 0 ] && [ "$have_password" -eq 0 ]; then
        echo "publish: HARBOR_USERNAME/HARBOR_PASSWORD are unset — nothing was pushed."
        echo "         A product sets both to publish; the template never pushes its own images."
        exit 0
    fi
    if [ "$have_username" -eq 0 ] || [ "$have_password" -eq 0 ]; then
        missing=HARBOR_USERNAME
        [ "$have_username" -eq 1 ] && missing=HARBOR_PASSWORD
        echo "publish: $missing is unset while the other registry credential is set." >&2
        echo "         Set both, or neither (neither means skip)." >&2
        exit 1
    fi

    # Where the images go. An empty one is a misconfiguration, not a skip: the credentials say
    # "publish", so the target has to be named — and the message names the versions.toml key
    # rather than the shell variable it lands in.
    registry=$(just version publish.registry)
    project=$(just version publish.project)
    if [ -z "$registry" ]; then
        echo "publish: versions.toml [publish].registry is empty — set it to the registry host." >&2
        exit 1
    fi
    if [ -z "$project" ]; then
        echo "publish: versions.toml [publish].project is empty — set it to this product's project." >&2
        exit 1
    fi
    rev=$(git rev-parse HEAD)

    names='{{ names }}'
    if [ -z "$names" ]; then
        system=$(nix eval --raw --impure --expr builtins.currentSystem)
        names=$(nix eval --json ".#packages.$system" --apply builtins.attrNames \
            | jq -r '.[] | select(endswith("-image-validate")) | sub("-image-validate$"; "")')
    fi

    # Never publish an archive that has not been shown to parse and carry its contract.
    for name in $names; do
        nix build ".#$name-image-validate" --no-link --print-out-paths >/dev/null
    done

    scratch=$(mktemp -d)
    trap 'rm -rf "$scratch"' EXIT

    # The password goes in an auth file, not on the command line: `--dest-creds` puts it in the
    # process arguments, where every user on the machine can read it from `ps`.
    authfile="$scratch/auth.json"
    printf '{"auths":{"%s":{"auth":"%s"}}}' \
        "$registry" \
        "$(printf '%s:%s' "$HARBOR_USERNAME" "$HARBOR_PASSWORD" | base64 -w0)" \
        > "$authfile"

    for name in $names; do
        image=$(nix build ".#$name-image" --no-link --print-out-paths)
        # `streamLayeredImage` is a script that writes the archive to stdout, so the tar never
        # has to live in the store.
        "$image" > "$scratch/$name.tar"
        target="$registry/$project/$name:$rev"
        # The name and the destination before the copy, because skopeo's own failure names
        # neither: the first failing push reported a store path for the image and nothing about
        # where the bytes were going.
        echo "  → $name: $target"
        # `--insecure-policy` skips containers/image's SIGNATURE policy and nothing else: TLS to
        # the registry is still verified, and the credentials are still the ones in the authfile.
        # It is needed because `skopeo copy` loads a policy even for an archive that nothing
        # signed, and a host with no policy.json at all — a CI runner, which is where a product's
        # first push happens — dies before the copy starts:
        #
        #   Error loading trust policy: config file not found: no policy.json file found
        #
        # The source is a docker-archive built here from the derivation validated two lines up.
        # There is no signature to check and no policy under which one could appear, so supplying
        # one would only be a longer spelling of the same decision.
        skopeo --insecure-policy copy --dest-authfile "$authfile" \
            "docker-archive:$scratch/$name.tar" "docker://$target"
        echo "  ✓ $name: $target"
    done
