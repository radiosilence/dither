# dither: an image host

# Run it on http://127.0.0.1:8080 with no sign-in and everything under ./data.
dev:
    INSECURE_NO_AUTH=1 cargo run

# What CI checks, short of the audit and the release builds.
check:
    actionlint
    git diff --quiet Cargo.lock || { echo 'Cargo.lock has uncommitted changes'; exit 1; }
    cargo fmt --all -- --check
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

# Install the shared theme (@radiosilence/theme) the stylesheet imports, writing the registry token first if missing.
theme:
    @test -s .npmrc.auth || just auth
    aube install --frozen-lockfile

# Write the GitHub Packages token .npmrc reads: GH_TOKEN in CI, the gh login otherwise. A token in .npmrc itself would
# be an ${ENV} aube refuses to expand in a project's auth settings.
auth:
    #!/usr/bin/env bash
    set -euo pipefail
    token="${GH_TOKEN:-$(gh auth token 2>/dev/null || true)}"
    [ -n "$token" ] || { echo "no GitHub token: run 'gh auth login', or set GH_TOKEN" >&2; exit 1; }
    printf '//npm.pkg.github.com/:_authToken=%s\n' "$token" > .npmrc.auth

# Compile styles/ into assets/app.css and copy the theme's typeface beside it, where the stylesheet's relative font URL
# points. Both are committed because the server embeds them; CI fails a build where either is stale.
css: theme
    tailwindcss --optimize --input styles/app.css --output assets/app.css
    cp node_modules/@radiosilence/theme/geist-mono.woff2 assets/geist-mono.woff2

# Fail when the committed stylesheet or typeface is not what the sources and the installed theme give.
css-fresh: theme
    #!/usr/bin/env bash
    set -euo pipefail
    css=$(mktemp)
    trap 'rm -f "$css"' EXIT
    tailwindcss --optimize --input styles/app.css --output "$css"
    cmp -s "$css" assets/app.css || { echo 'assets/app.css is stale: run `just css`'; exit 1; }
    cmp -s node_modules/@radiosilence/theme/geist-mono.woff2 assets/geist-mono.woff2 || { echo 'assets/geist-mono.woff2 is stale: run `just css`'; exit 1; }

# Typecheck the browser's script. Cargo strips its types as it builds (build.rs); this checks them.
js:
    tsc -p scripts

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

fmt:
    cargo fmt --all

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
