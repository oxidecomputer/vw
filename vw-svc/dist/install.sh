#!/bin/bash
#
# Install vw-svc as a systemd service.
#
# Safe to re-run: the binary and the unit are replaced, the configuration file
# is not. A first install leaves the service enabled but stopped, because the
# configuration it was just given is an example and starting on it would only
# produce a confusing failure.
#
#   ./install.sh                          # from ../../target/release/vw-svc
#   ./install.sh --binary /path/to/vw-svc
#   ./install.sh --commit <sha>           # from that commit's buildomat build
#   ./install.sh --restart                # and restart a running service

set -euo pipefail

BUILDOMAT=https://buildomat.eng.oxide.computer/public/file/oxidecomputer/vw
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

BINARY=""
COMMIT=""
RESTART=false

while [ $# -gt 0 ]; do
	case "$1" in
	--binary) BINARY="$2"; shift 2 ;;
	--commit) COMMIT="$2"; shift 2 ;;
	--restart) RESTART=true; shift ;;
	-h | --help) sed -n '2,13p' "${BASH_SOURCE[0]}" | sed 's/^# \?//'; exit 0 ;;
	*) echo "unknown argument: $1" >&2; exit 2 ;;
	esac
done

[ "$(id -u)" -eq 0 ] || { echo "install.sh must run as root" >&2; exit 1; }

# Resolve what is being installed before touching anything, so a bad path or
# an unreachable buildomat fails before the unit has been replaced.
scratch=""
if [ -n "$COMMIT" ]; then
	[ -z "$BINARY" ] || { echo "--binary and --commit are exclusive" >&2; exit 2; }
	scratch="$(mktemp -d)"
	trap 'rm -rf "$scratch"' EXIT
	BINARY="$scratch/vw-svc"
	echo "Fetching vw-svc from vw $COMMIT"
	curl --proto '=https' --tlsv1.2 -fL -o "$BINARY" "$BUILDOMAT/linux/$COMMIT/vw-svc"
	chmod +x "$BINARY"
fi
: "${BINARY:=$HERE/../../target/release/vw-svc}"

[ -x "$BINARY" ] || {
	echo "no vw-svc binary at $BINARY" >&2
	echo "build one with 'cargo build --release -p vw-svc', or pass --binary/--commit" >&2
	exit 1
}

# Run it once here. A binary that cannot start on this machine should say so
# now rather than as a restart loop after the unit is in place.
"$BINARY" serve --help >/dev/null

echo "Installing /usr/local/bin/vw-svc"
install -o root -g root -m 0755 "$BINARY" /usr/local/bin/vw-svc

install -d -o root -g root -m 0755 /etc/vw-svc

# Never overwritten. It holds the rack token and everything about how this
# machine is configured, and an upgrade has no business resetting either.
fresh=false
if [ -e /etc/vw-svc/vw-svc.env ]; then
	echo "Keeping /etc/vw-svc/vw-svc.env"
else
	echo "Installing /etc/vw-svc/vw-svc.env"
	install -o root -g root -m 0600 \
		"$HERE/vw-svc.env.example" /etc/vw-svc/vw-svc.env
	fresh=true
fi

echo "Installing /etc/systemd/system/vw-svc.service"
install -o root -g root -m 0644 \
	"$HERE/vw-svc.service" /etc/systemd/system/vw-svc.service

systemctl daemon-reload
systemctl enable vw-svc.service >/dev/null

if $fresh; then
	cat <<-EOF

		vw-svc is installed and enabled, and has not been started.

		Edit /etc/vw-svc/vw-svc.env first. As shipped it names a
		certificate that does not exist, and configures no rack -- so
		starting on it would stop at the missing certificate, and
		fixing only that would give you a service that records
		environments and provisions nothing. Then:

		    systemctl start vw-svc
		    journalctl -fu vw-svc
	EOF
	exit 0
fi

if systemctl is-active --quiet vw-svc.service; then
	if $RESTART; then
		echo "Restarting vw-svc"
		systemctl restart vw-svc.service
	else
		# Deliberately not automatic. This service relays the connections
		# builds run over, so a restart ends whatever synthesis runs, REPL
		# sessions and downloads are in flight. Picking the moment for that
		# is the operator's call.
		cat <<-EOF

			The new binary is installed; the running service is still the
			old one. Restarting ends any build, REPL session or download
			currently being relayed, so it is left to you:

			    systemctl restart vw-svc

			Or re-run this with --restart.
		EOF
	fi
else
	echo
	echo "vw-svc is installed and enabled. Start it with: systemctl start vw-svc"
fi
