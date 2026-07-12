#!/usr/bin/env bash
set -Eeuo pipefail

# Provision Ubuntu under WSL for building, testing, and auditing dora.
# Run as your normal WSL user; the script uses sudo where required.

readonly DB_USER="${DORA_DB_USER:-dora}"
readonly DB_PASSWORD="${DORA_DB_PASSWORD:-dora}"
readonly DB_NAME="${DORA_DB_NAME:-dora}"
readonly DATABASE_URL="postgres://${DB_USER}:${DB_PASSWORD}@localhost:5432/${DB_NAME}"

log() {
    printf '\n==> %s\n' "$*"
}

if [[ "$(uname -s)" != "Linux" ]] || ! grep -qi microsoft /proc/version 2>/dev/null; then
    printf 'Warning: this script is intended for Ubuntu under WSL.\n' >&2
fi

if ! command -v sudo >/dev/null 2>&1; then
    printf 'sudo is required. Install it or run the package commands as root.\n' >&2
    exit 1
fi

log "Installing Ubuntu build, audit, database, and network tools"
sudo apt-get update
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    clang \
    cmake \
    curl \
    gdb \
    git \
    iproute2 \
    jq \
    libcap2-bin \
    libclang-dev \
    libpq-dev \
    libssl-dev \
    lld \
    llvm \
    pkg-config \
    postgresql \
    postgresql-client \
    python3 \
    ripgrep \
    shellcheck \
    strace \
    tcpdump

log "Installing rustup when it is not already available"
if ! command -v rustup >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile default
fi

# rustup modifies shell startup files, but this invocation needs Cargo now.
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"

log "Installing the repository-pinned Rust toolchain and components"
if [[ -f rust-toolchain.toml ]]; then
    rustup show active-toolchain >/dev/null
else
    rustup toolchain install 1.95.0 \
        --component rustfmt,clippy,llvm-tools-preview
    rustup default 1.95.0
fi

log "Installing Rust audit helpers"
# --locked favors the dependency versions tested by each published tool.
command -v cargo-audit >/dev/null 2>&1 \
    || cargo install cargo-audit --locked
command -v cargo-nextest >/dev/null 2>&1 \
    || cargo install cargo-nextest --locked
command -v cargo-llvm-cov >/dev/null 2>&1 \
    || cargo install cargo-llvm-cov --locked

log "Starting PostgreSQL"
if command -v systemctl >/dev/null 2>&1 && systemctl is-system-running >/dev/null 2>&1; then
    sudo systemctl enable --now postgresql
else
    sudo service postgresql start
fi

log "Creating the local dora PostgreSQL role and database"
sudo -u postgres psql --set ON_ERROR_STOP=1 \
    --set=db_user="$DB_USER" \
    --set=db_password="$DB_PASSWORD" <<'SQL'
SELECT format('CREATE ROLE %I LOGIN PASSWORD %L CREATEDB', :'db_user', :'db_password')
WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = :'db_user') \gexec
SELECT format('ALTER ROLE %I WITH LOGIN PASSWORD %L CREATEDB', :'db_user', :'db_password') \gexec
SQL

if ! sudo -u postgres psql --tuples-only --no-align --set=db_name="$DB_NAME" \
    --command "SELECT 1 FROM pg_database WHERE datname = :'db_name'" | grep -qx 1; then
    sudo -u postgres createdb --owner="$DB_USER" "$DB_NAME"
fi

log "Writing a reusable environment file"
mkdir -p "$HOME/.config/dora"
printf 'export DATABASE_URL=%q\n' "$DATABASE_URL" \
    > "$HOME/.config/dora/audit-env.sh"

log "Fetching dependencies and compiling the workspace"
export DATABASE_URL
cargo fetch --locked
cargo test --workspace --all-targets --no-run

cat <<EOF

Setup complete.

For each new shell, run:
  source ~/.config/dora/audit-env.sh

Then, from the dora repository:
  cargo nextest run --workspace --all-targets
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo audit
  cargo llvm-cov nextest --workspace --all-targets --html

Some ICMP/DHCP integration tests may need raw-socket privileges. Prefer running
only those tests with sudo while preserving DATABASE_URL and PATH, for example:
  sudo --preserve-env=DATABASE_URL,PATH cargo nextest run -p integration-tests

Database URL:
  ${DATABASE_URL}
EOF
