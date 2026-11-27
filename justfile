# dither: an image host

# Run it on http://127.0.0.1:8080 with the images in memory and a state
# directory of its own, so each run starts empty: its first page makes the
# admin, with the setup code it logs. A laptop's disk is
# usually fuller than a server's, so uploads stop at 1 GB free here rather
# than at the default share. A worker beside it makes uploads' copies,
# reaching it over the worker API on loopback.
dev:
    #!/usr/bin/env bash
    set -euo pipefail
    # Images in memory, and a state directory made for this run, so a restart
    # starts empty rather than with rows naming images that are gone.
    export MIN_FREE_BYTES=1000000000 STORAGE=memory:///
    export STATE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/dither-dev.XXXXXX")"
    cargo build
    cargo run -q -- worker &
    trap 'kill %1 2>/dev/null || true; rm -rf "$STATE_DIR"' EXIT
    cargo run -q -- serve

# What CI checks, short of the audit and the release builds. Format covers the
# Rust and the templates.
check:
    actionlint
    git diff --quiet Cargo.lock || { echo 'Cargo.lock has uncommitted changes'; exit 1; }
    cargo fmt --all -- --check
    askama-analyzer fmt --check
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --locked
    cd deploy/pulumi && aube install --frozen-lockfile && aube run check && aube run build
    just css-fresh
    just js
    python3 -m unittest scripts/test_changelog.py
    python3 scripts/changelog.py --lint
    just versions

# The checks that take seconds, run before every push; CI runs the rest.
fast:
    git diff --quiet Cargo.lock || { echo 'Cargo.lock has uncommitted changes'; exit 1; }
    cargo fmt --all -- --check
    askama-analyzer fmt --check

# Point styles/kit at koan-kit-web's source, the version Cargo.lock names, so the stylesheet is compiled from the same
# kit release as the markup its components write. The link is gitignored.
kit:
    #!/usr/bin/env bash
    set -euo pipefail
    manifest=$(cargo metadata --format-version 1 --locked | python3 -c 'import json, sys; print(next(p["manifest_path"] for p in json.load(sys.stdin)["packages"] if p["name"] == "koan-kit-web"))')
    ln -sfn "$(dirname "$manifest")" styles/kit

# Fail on classes outside the kōan look (rounded corners, shadows, Tailwind's palette and type sizes, inline styles) in
# the templates and the code that writes markup. Most compile to nothing, so a leftover would lose its style silently.
kit-check: kit
    node styles/kit/kit-check.mjs templates src browser

# Compile styles/ and the kit's stylesheet into assets/app.css. Committed because the server embeds it; the kit's
# typeface is served from the crate. CI fails a build where the stylesheet is stale.
css: kit-check
    tailwindcss --optimize --input styles/app.css --output assets/app.css

# Fail when the committed stylesheet is not what the sources and the kit give.
css-fresh: kit-check
    #!/usr/bin/env bash
    set -euo pipefail
    css=$(mktemp)
    trap 'rm -f "$css"' EXIT
    tailwindcss --optimize --input styles/app.css --output "$css"
    cmp -s "$css" assets/app.css || { echo 'assets/app.css is stale: run `just css`'; exit 1; }

# Typecheck the browser's script. Cargo strips its types as it builds (build.rs); this checks them.
js:
    tsc -p browser

# Rewrite the message catalogues from the templates, keeping translations. A test fails while they are stale.
i18n:
    UPDATE_CATALOGUES=1 cargo test --quiet the_catalogues_are_up_to_date

# Fail unless the crate, the Pulumi package and the image it deploys share one version.
versions:
    #!/usr/bin/env bash
    set -euo pipefail
    crate=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
    chart=$(node -p "require('./deploy/pulumi/package.json').version")
    app=$(sed -n 's/^export const APP_VERSION = "\(.*\)";/\1/p' deploy/pulumi/src/versions.ts)
    if [ "$crate" != "$chart" ] || [ "$crate" != "$app" ]; then
        echo "versions disagree: Cargo.toml $crate, package.json $chart, versions.ts $app"
        exit 1
    fi

# Format the Rust and the templates
fmt:
    cargo fmt --all
    askama-analyzer fmt

# Install the git hooks
hooks:
    lefthook install

# Write CHANGELOG.md from the fragments in changelog.d/. `--stdout` prints it instead.
changelog *args:
    python3 scripts/changelog.py {{args}}

# Open the release PR: `just release patch|minor|major`, or an exact version.
# Merging it publishes the image, the Pulumi package and the GitHub release; tags are never pushed by hand.
release version *args:
    scripts/release.sh {{version}} {{args}}
