#!/usr/bin/env bash
# matmetrics — Raspberry Pi installer and updater.
#
# First install (on the Pi, Raspberry Pi OS 64-bit):
#   curl -fsSL https://raw.githubusercontent.com/zlaja237/MatHelper/HEAD/deploy/pi/install.sh | sudo bash
#
# What it does (safe to re-run any time):
#   - installs Chromium + a virtual display (Xvfb) for the Willys/Hemköp login
#   - adds swap on small-memory Pis (Pi 3 has 1 GB)
#   - downloads the prebuilt binary from the GitHub release "pi-latest"
#   - runs it as a systemd service on port 7878, started at boot
#   - installs a nightly timer that pulls new builds automatically
#   - turns on automatic OS security updates
#
# `matmetrics-update` (installed by this script) re-runs only the download
# step; the nightly timer calls it.
set -euo pipefail
# Everything sits inside main() so bash reads the whole file before running
# any of it; with `curl | bash` a command reading stdin could otherwise
# swallow the rest of the script.
main() {

REPO="${MATMETRICS_REPO:-zlaja237/MatHelper}"
PORT="${MATMETRICS_PORT:-7878}"
RELEASE_URL="${MATMETRICS_RELEASE_URL:-https://github.com/${REPO}/releases/download/pi-latest}"
APP_USER="matmetrics"
APP_DIR="/opt/matmetrics"        # binary + installed version
DATA_DIR="/var/lib/matmetrics"   # working dir: data/ (receipts), out/ (report)
SWAPFILE="/var/swap.matmetrics"

log() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31mfel:\033[0m %s\n' "$*" >&2; exit 1; }

[[ $EUID -eq 0 ]] || die "kör med sudo"
[[ "$(uname -m)" == "aarch64" ]] || die "kräver 64-bitars OS (uname -m = $(uname -m)). Installera Raspberry Pi OS (64-bit)."

# ---------------------------------------------------------------------------
# Download the newest build if it differs from what's installed.
# Prints "updated" or "unchanged".
# ---------------------------------------------------------------------------
fetch_release() (   # subshell: the EXIT trap only cleans up this call
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT

    remote_sha="$(curl -fsSL --retry 3 "${RELEASE_URL}/matmetrics-aarch64.sha256")" \
        || die "kunde inte hämta ${RELEASE_URL}/matmetrics-aarch64.sha256 — har GitHub Actions byggt en release än?"
    local_sha="$(cat "${APP_DIR}/installed.sha256" 2>/dev/null || true)"
    if [[ "$remote_sha" == "$local_sha" && -x "${APP_DIR}/matmetrics" ]]; then
        echo unchanged
        exit 0
    fi

    curl -fsSL --retry 3 -o "$tmp/m.tar.gz" "${RELEASE_URL}/matmetrics-aarch64.tar.gz"
    echo "${remote_sha}  $tmp/m.tar.gz" | sha256sum -c --quiet - || die "checksumman stämmer inte"
    tar -C "$tmp" -xzf "$tmp/m.tar.gz"

    install -d -m 755 "$APP_DIR"
    install -m 755 "$tmp/matmetrics/matmetrics" "${APP_DIR}/matmetrics.new"
    mv -f "${APP_DIR}/matmetrics.new" "${APP_DIR}/matmetrics"
    install -m 755 "$tmp/matmetrics/install.sh" /usr/local/sbin/matmetrics-install
    # Category rules are shared, repo-managed; personal fixes live in
    # data/category_overrides.json, which is never touched.
    install -d -o "$APP_USER" -g "$APP_USER" "${DATA_DIR}/data"
    install -m 644 -o "$APP_USER" -g "$APP_USER" "$tmp/matmetrics/data/categories.toml" "${DATA_DIR}/data/categories.toml"
    cp "$tmp/matmetrics/VERSION" "${APP_DIR}/VERSION"
    echo "$remote_sha" > "${APP_DIR}/installed.sha256"
    echo updated
)

do_update() {
    local result
    result="$(fetch_release)"
    if [[ "$result" == updated ]]; then
        log "ny version $(cut -c1-7 "${APP_DIR}/VERSION") installerad"
        # Re-categorize existing receipts with the new rules/parsers.
        (cd "$DATA_DIR" && runuser -u "$APP_USER" -- "${APP_DIR}/matmetrics" reparse) || true
        systemctl try-restart matmetrics.service
    else
        log "redan senaste versionen"
    fi
}

if [[ "${1:-}" == "--update" ]]; then
    do_update
    return 0
fi

# ---------------------------------------------------------------------------
# Full install
# ---------------------------------------------------------------------------
log "installerar paket (Chromium, Xvfb, automatiska säkerhetsuppdateringar)"
export DEBIAN_FRONTEND=noninteractive
apt-get update -q </dev/null
CHROMIUM_PKG=chromium
apt-cache show chromium >/dev/null 2>&1 || CHROMIUM_PKG=chromium-browser
apt-get install -y -q "$CHROMIUM_PKG" xvfb xauth curl ca-certificates fonts-dejavu-core unattended-upgrades </dev/null
systemctl enable --now unattended-upgrades >/dev/null 2>&1 || true

mem_kb="$(awk '/MemTotal/ {print $2}' /proc/meminfo)"
swap_kb="$(awk '/SwapTotal/ {print $2}' /proc/meminfo)"
if (( mem_kb < 2000000 && swap_kb < 1000000 )) && [[ ! -f "$SWAPFILE" ]]; then
    log "lite minne ($((mem_kb / 1024)) MB) — lägger till 1 GB swap för Chromium"
    fallocate -l 1G "$SWAPFILE" || dd if=/dev/zero of="$SWAPFILE" bs=1M count=1024 status=none
    chmod 600 "$SWAPFILE"
    mkswap "$SWAPFILE" >/dev/null
    swapon "$SWAPFILE"
    grep -q "$SWAPFILE" /etc/fstab || echo "$SWAPFILE none swap sw 0 0" >> /etc/fstab
fi

if ! id "$APP_USER" >/dev/null 2>&1; then
    log "skapar systemanvändaren $APP_USER"
    useradd --system --home-dir "$DATA_DIR" --create-home --shell /usr/sbin/nologin "$APP_USER"
fi
install -d -m 750 -o "$APP_USER" -g "$APP_USER" "$DATA_DIR"   # receipts are private
install -d -o "$APP_USER" -g "$APP_USER" "${DATA_DIR}/data" "${DATA_DIR}/out"

log "hämtar senaste bygget från github.com/${REPO}"
fetch_release >/dev/null

cat > /usr/local/sbin/matmetrics-update <<EOF
#!/bin/sh
MATMETRICS_REPO="${REPO}" exec /usr/local/sbin/matmetrics-install --update
EOF
chmod 755 /usr/local/sbin/matmetrics-update

log "installerar systemd-tjänst och nattlig uppdatering"
cat > /etc/systemd/system/matmetrics.service <<EOF
[Unit]
Description=matmetrics dashboard
Wants=network-online.target
After=network-online.target

[Service]
User=${APP_USER}
Group=${APP_USER}
WorkingDirectory=${DATA_DIR}
Environment=HOME=${DATA_DIR}
# Xvfb gives Chromium a (virtual) screen: the Willys/Hemköp login must run
# in a non-headless browser, but nobody needs to look at it.
ExecStart=/usr/bin/xvfb-run --auto-servernum --server-args="-screen 0 1280x800x24" ${APP_DIR}/matmetrics serve --port ${PORT}
Restart=always
RestartSec=5
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF

cat > /etc/systemd/system/matmetrics-update.service <<EOF
[Unit]
Description=Update matmetrics from the latest GitHub build
Wants=network-online.target
After=network-online.target

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/matmetrics-update
EOF

cat > /etc/systemd/system/matmetrics-update.timer <<EOF
[Unit]
Description=Nightly matmetrics update

[Timer]
OnCalendar=*-*-* 04:00
RandomizedDelaySec=30min
Persistent=true

[Install]
WantedBy=timers.target
EOF

systemctl daemon-reload
systemctl enable --now matmetrics-update.timer >/dev/null
systemctl enable matmetrics.service >/dev/null
systemctl restart matmetrics.service

ip_addr="$(hostname -I | awk '{print $1}')"
log "klart! version $(cut -c1-7 "${APP_DIR}/VERSION")"
cat <<EOF

  Inköpslista:  http://$(hostname).local:${PORT}/
           el:  http://${ip_addr}:${PORT}/
  Kvitton:      http://$(hostname).local:${PORT}/report.html

  Status:      systemctl status matmetrics
  Loggar:      journalctl -u matmetrics -f
  Uppdatera:   sudo matmetrics-update   (sker även automatiskt varje natt)
  Kvittodata:  ${DATA_DIR}/data
  Flera profiler: kopiera config.example.toml till ${DATA_DIR}/config.toml

EOF
}

main "$@"
