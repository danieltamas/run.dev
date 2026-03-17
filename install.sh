#!/bin/bash
set -e

# run.dev installer — macOS and Linux
# Usage: curl -fsSL https://getrun.dev/install.sh | bash

BOLD='\033[1m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
YELLOW='\033[0;33m'
RED='\033[0;31m'
DIM='\033[2m'
NC='\033[0m'

RUNDEV_VERSION="${RUNDEV_VERSION:-latest}"
INSTALL_DIR="/usr/local/bin"
HELPER_PATH="/usr/local/bin/rundev-hosts-helper"

# ── Helpers ───────────────────────────────────────────────────────────────────

print_header() {
    echo ""
    echo -e "${CYAN}${BOLD}  ██████╗ ██╗   ██╗███╗   ██╗   ██████╗ ███████╗██╗   ██╗${NC}"
    echo -e "${CYAN}${BOLD}  ██╔══██╗██║   ██║████╗  ██║   ██╔══██╗██╔════╝██║   ██║${NC}"
    echo -e "${CYAN}${BOLD}  ██████╔╝██║   ██║██╔██╗ ██║   ██║  ██║█████╗  ██║   ██║${NC}"
    echo -e "${CYAN}${BOLD}  ██╔══██╗██║   ██║██║╚██╗██║██╗██║  ██║██╔══╝  ╚██╗ ██╔╝${NC}"
    echo -e "${CYAN}${BOLD}  ██║  ██║╚██████╔╝██║ ╚████║╚═╝██████╔╝███████╗ ╚████╔╝ ${NC}"
    echo -e "${CYAN}${BOLD}  ╚═╝  ╚═╝ ╚═════╝ ╚═╝  ╚═══╝   ╚═════╝ ╚══════╝  ╚═══╝  ${NC}"
    echo ""
    echo -e "${DIM}  AI-native local dev environment${NC}"
    echo ""
}

ok()   { echo -e "  ${GREEN}✓${NC}  $1"; }
info() { echo -e "  ${CYAN}→${NC}  $1"; }
warn() { echo -e "  ${YELLOW}!${NC}  $1"; }
fail() { echo -e "  ${RED}✗${NC}  $1"; exit 1; }

spinner() {
    local pid=$1
    local label=$2
    local frames=("⠋" "⠙" "⠹" "⠸" "⠼" "⠴" "⠦" "⠧" "⠇" "⠏")
    local i=0
    while kill -0 "$pid" 2>/dev/null; do
        printf "\r  ${CYAN}%s${NC}  %s..." "${frames[$((i % 10))]}" "$label"
        i=$((i + 1))
        sleep 0.08
    done
    printf "\r"
}

# ── Platform detection ────────────────────────────────────────────────────────

detect_os() {
    OS="$(uname -s)"
    ARCH="$(uname -m)"

    case "$OS" in
        Darwin) OS="macos" ;;
        Linux)  OS="linux" ;;
        *) fail "Unsupported OS: $OS. run.dev supports macOS and Linux." ;;
    esac

    case "$ARCH" in
        x86_64 | amd64)  ARCH="x86_64" ;;
        arm64 | aarch64) ARCH="aarch64" ;;
        *) fail "Unsupported architecture: $ARCH" ;;
    esac

    ok "Platform: $OS / $ARCH"
}

# ── Dependencies ──────────────────────────────────────────────────────────────

install_mkcert() {
    if command -v mkcert &>/dev/null; then
        ok "mkcert"
        mkcert -install &>/dev/null 2>&1 &
        spinner $! "Trusting local CA"
        wait $! 2>/dev/null || warn "mkcert -install failed — run it manually if you see cert warnings"
        return
    fi

    if [[ "$OS" == "macos" ]]; then
        if command -v brew &>/dev/null; then
            brew install mkcert nss &>/dev/null &
            spinner $! "Installing mkcert"
            wait $! 2>/dev/null || fail "brew install mkcert failed"
        else
            fail "Homebrew not found. Install it first: https://brew.sh"
        fi
    else
        # Linux: try apt, then direct download
        MKCERT_ARCH="$ARCH"
        case "$ARCH" in
            x86_64)  MKCERT_ARCH="amd64" ;;
            aarch64) MKCERT_ARCH="arm64" ;;
        esac

        if command -v apt-get &>/dev/null; then
            sudo apt-get install -y libnss3-tools &>/dev/null
        fi

        curl -fsSL "https://dl.filippo.io/mkcert/latest?for=linux/${MKCERT_ARCH}" -o /tmp/mkcert &
        spinner $! "Downloading mkcert"
        if wait $! 2>/dev/null; then
            sudo install -m 0755 /tmp/mkcert /usr/local/bin/mkcert
            rm -f /tmp/mkcert
        else
            rm -f /tmp/mkcert
            warn "Could not install mkcert — HTTPS will use built-in certificates instead"
            return
        fi
    fi

    ok "mkcert installed"
    mkcert -install &>/dev/null 2>&1 &
    spinner $! "Trusting local CA (enter password if prompted)"
    wait $! 2>/dev/null || warn "mkcert -install failed — run it manually if you see cert warnings"
    ok "Local CA trusted — HTTPS will work without browser warnings"
}

install_rust() {
    if command -v cargo &>/dev/null; then
        ok "Rust/cargo"
        return
    fi
    info "Installing Rust via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path &>/dev/null &
    spinner $! "Installing Rust"
    wait $! 2>/dev/null || fail "Rust installation failed"
    # Source cargo env for the rest of this script
    # shellcheck source=/dev/null
    source "$HOME/.cargo/env" 2>/dev/null || export PATH="$HOME/.cargo/bin:$PATH"
    ok "Rust installed"
}

# ── rundev binary ─────────────────────────────────────────────────────────────

install_rundev_binary() {
    BINARY_URL="https://getrun.dev/releases/${RUNDEV_VERSION}/rundev-${OS}-${ARCH}"

    # Try pre-built binary first
    if curl -fsSL --head "$BINARY_URL" &>/dev/null 2>&1; then
        curl -fsSL "$BINARY_URL" -o /tmp/rundev-bin &
        spinner $! "Downloading run.dev"
        wait $! 2>/dev/null || fail "Download failed"
        chmod +x /tmp/rundev-bin
        sudo mv /tmp/rundev-bin "$INSTALL_DIR/rundev"
        sudo ln -sf "$INSTALL_DIR/rundev" "$INSTALL_DIR/run.dev"
        ok "run.dev installed"
        return
    fi

    # Build from source — works whether inside the repo or not
    install_rust

    BUILD_DIR=""
    CLEANUP_BUILD=0
    if [[ -f "Cargo.toml" ]]; then
        BUILD_DIR="$(pwd)"
    else
        # Clone the repo to a temp directory
        CLONE_PARENT="$(mktemp -d)"
        BUILD_DIR="$CLONE_PARENT/run.dev"
        CLEANUP_BUILD=1

        if command -v git &>/dev/null; then
            git clone --depth 1 https://github.com/danieltamas/run.dev.git "$BUILD_DIR" 2>&1 &
            spinner $! "Downloading source"
            wait $!
            if [[ $? -ne 0 ]] || [[ ! -f "$BUILD_DIR/Cargo.toml" ]]; then
                fail "git clone failed — check your network connection"
            fi
            ok "Source downloaded"
        else
            TARBALL="$CLONE_PARENT/source.tar.gz"
            curl -fsSL "https://github.com/danieltamas/run.dev/archive/refs/heads/main.tar.gz" -o "$TARBALL" 2>&1 &
            spinner $! "Downloading source"
            wait $!
            if [[ $? -ne 0 ]] || [[ ! -f "$TARBALL" ]]; then
                fail "Source download failed — check your network connection"
            fi
            mkdir -p "$BUILD_DIR"
            tar -xzf "$TARBALL" -C "$BUILD_DIR" --strip-components=1
            rm -f "$TARBALL"
            if [[ ! -f "$BUILD_DIR/Cargo.toml" ]]; then
                fail "Source download corrupt — Cargo.toml not found"
            fi
            ok "Source downloaded"
        fi
    fi

    BUILD_LOG="$(mktemp)"
    (cd "$BUILD_DIR" && cargo build --release) >"$BUILD_LOG" 2>&1 &
    BUILD_PID=$!
    spinner $BUILD_PID "Building run.dev — this takes ~60s on first run"
    wait $BUILD_PID
    STATUS=$?

    if [[ $STATUS -ne 0 ]]; then
        echo ""
        echo -e "  ${RED}Build output:${NC}"
        tail -20 "$BUILD_LOG" | sed 's/^/    /'
        rm -f "$BUILD_LOG"
        fail "Build failed"
    fi
    rm -f "$BUILD_LOG"

    if [[ ! -f "$BUILD_DIR/target/release/rundev" ]]; then
        fail "Build succeeded but binary not found at $BUILD_DIR/target/release/rundev"
    fi

    sudo cp "$BUILD_DIR/target/release/rundev" "$INSTALL_DIR/rundev"
    sudo ln -sf "$INSTALL_DIR/rundev" "$INSTALL_DIR/run.dev"

    # Clean up temp clone if we created one
    if [[ $CLEANUP_BUILD -eq 1 ]]; then
        rm -rf "$CLONE_PARENT"
    fi

    ok "run.dev built and installed"
}

# ── Privileged helper ─────────────────────────────────────────────────────────

install_privileged_helper() {
    echo ""
    echo -e "  ${YELLOW}${BOLD}One-time password prompt${NC}"
    echo -e "  ${DIM}run.dev manages /etc/hosts so your custom domains resolve locally.${NC}"
    echo -e "  ${DIM}We need to install a tiny helper — enter your password ${BOLD}once${NC}${DIM} and it will never ask again.${NC}"
    echo ""

    CURRENT_USER="$(whoami)"

    # This must stay in sync with HELPER_SCRIPT in src/core/hosts.rs
    HELPER_SCRIPT='#!/bin/sh
# rundev-hosts-helper — write /etc/hosts and flush DNS cache
cat > /etc/hosts
# Flush DNS so changes take effect immediately
if command -v dscacheutil >/dev/null 2>&1; then
    dscacheutil -flushcache 2>/dev/null
    killall -HUP mDNSResponder 2>/dev/null || true
elif command -v resolvectl >/dev/null 2>&1; then
    resolvectl flush-caches 2>/dev/null || true
elif command -v systemd-resolve >/dev/null 2>&1; then
    systemd-resolve --flush-caches 2>/dev/null || true
elif command -v nscd >/dev/null 2>&1; then
    nscd -i hosts 2>/dev/null || true
fi
'

    # Platform-specific sudoers line — must match sudoers_line() in src/main.rs
    if [[ "$OS" == "macos" ]]; then
        SUDOERS_RULE="# rundev sudoers
${CURRENT_USER} ALL=(ALL) NOPASSWD: ${HELPER_PATH}, /sbin/pfctl"
    else
        SUDOERS_RULE="# rundev sudoers
${CURRENT_USER} ALL=(ALL) NOPASSWD: ${HELPER_PATH}, /sbin/iptables, /usr/sbin/iptables, /sbin/iptables-save"
    fi

    TMP_HELPER="$(mktemp)"
    TMP_SUDOERS="$(mktemp)"
    printf '%s' "$HELPER_SCRIPT" > "$TMP_HELPER"
    printf '%s\n' "$SUDOERS_RULE" > "$TMP_SUDOERS"

    sudo sh -c "
        cp '$TMP_HELPER' '$HELPER_PATH' && \
        chmod 755 '$HELPER_PATH' && \
        cp '$TMP_SUDOERS' /etc/sudoers.d/rundev && \
        chmod 440 /etc/sudoers.d/rundev
    " || fail "Failed to install privileged helper"

    rm -f "$TMP_HELPER" "$TMP_SUDOERS"

    # Write sentinel so rundev knows setup has been run
    RUNDEV_CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/rundev"
    if [[ "$OS" == "macos" ]]; then
        RUNDEV_CONFIG_DIR="$HOME/Library/Application Support/rundev"
    fi
    mkdir -p "$RUNDEV_CONFIG_DIR"
    touch "$RUNDEV_CONFIG_DIR/setup_done"

    ok "Done — /etc/hosts updates will be silent from now on"
}

# ── PORT forwarding ───────────────────────────────────────────────────────────

setup_port_forwarding() {
    if [[ "$OS" == "macos" ]]; then
        # Write pfctl anchor for 80→8080 and 443→8443
        PF_ANCHOR="/etc/pf.anchors/rundev"
        PF_RULES="rdr pass on lo0 proto tcp from any to any port 80 -> 127.0.0.1 port 1111
rdr pass on lo0 proto tcp from any to any port 443 -> 127.0.0.1 port 1112"

        TMP_ANCHOR="$(mktemp)"
        printf '%s\n' "$PF_RULES" > "$TMP_ANCHOR"
        sudo sh -c "cp '$TMP_ANCHOR' '$PF_ANCHOR' && chmod 644 '$PF_ANCHOR'" 2>/dev/null || true
        rm -f "$TMP_ANCHOR"

        # Ensure pf.conf references the rundev anchor
        if ! sudo grep -q 'rdr-anchor "rundev"' /etc/pf.conf 2>/dev/null; then
            sudo sh -c 'printf "\n# run.dev port forwarding\nrdr-anchor \"rundev\"\nanchor \"rundev\"\n" >> /etc/pf.conf' 2>/dev/null || true
        fi

        sudo pfctl -ef "$PF_ANCHOR" &>/dev/null 2>&1 || true
        ok "Port forwarding: pfctl rules installed (80→1111, 443→1112)"
    else
        # Linux: iptables redirect
        sudo iptables -t nat -C OUTPUT -p tcp --dport 80  -j REDIRECT --to-port 1111 2>/dev/null || \
            sudo iptables -t nat -A OUTPUT -p tcp --dport 80  -j REDIRECT --to-port 1111 2>/dev/null || true
        sudo iptables -t nat -C OUTPUT -p tcp --dport 443 -j REDIRECT --to-port 1112 2>/dev/null || \
            sudo iptables -t nat -A OUTPUT -p tcp --dport 443 -j REDIRECT --to-port 1112 2>/dev/null || true

        # Persist via iptables-save if available
        if command -v iptables-save &>/dev/null; then
            sudo mkdir -p /etc/iptables
            sudo sh -c 'iptables-save > /etc/iptables/rules.v4' 2>/dev/null || true
        fi

        ok "Port forwarding: iptables rules installed (80→1111, 443→1112)"
    fi
}

# ── PATH ──────────────────────────────────────────────────────────────────────

configure_path() {
    if echo "$PATH" | grep -q "$INSTALL_DIR"; then
        return
    fi

    EXPORT_LINE="export PATH=\"${INSTALL_DIR}:\$PATH\""
    ADDED=0

    # Detect shell and pick the right RC file
    case "$SHELL" in
        */zsh)
            RC="$HOME/.zshrc"
            echo "$EXPORT_LINE" >> "$RC" && ADDED=1
            ;;
        */bash)
            # Prefer .bash_profile on macOS, .bashrc on Linux
            if [[ "$OS" == "macos" ]]; then
                RC="$HOME/.bash_profile"
            else
                RC="$HOME/.bashrc"
            fi
            echo "$EXPORT_LINE" >> "$RC" && ADDED=1
            ;;
        */fish)
            fish -c "fish_add_path $INSTALL_DIR" 2>/dev/null && ADDED=1
            ;;
    esac

    if [[ $ADDED -eq 1 ]]; then
        ok "Added $INSTALL_DIR to PATH"
        info "Run: source \$RC  (or open a new terminal)"
    else
        warn "$INSTALL_DIR not in PATH — add it manually: $EXPORT_LINE"
    fi
}

# ── Done ──────────────────────────────────────────────────────────────────────

print_done() {
    echo ""
    echo -e "  ${GREEN}${BOLD}✨ run.dev is ready!${NC}"
    echo ""
    echo -e "  Run ${BOLD}rundev${NC} or ${BOLD}run.dev${NC} to open the dashboard."
    echo ""
}

# ── Main ──────────────────────────────────────────────────────────────────────

print_header
detect_os
install_mkcert
install_rundev_binary
install_privileged_helper
setup_port_forwarding
configure_path
print_done
