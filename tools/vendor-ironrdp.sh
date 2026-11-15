#!/bin/sh
# Puts IronRDP into vendor/IronRDP for the RDP server (crates/hyperwin-rdp): keel-rdp's pin
# with ironrdp-keel.patch and ironrdp-hypermac.patch applied, the same sources hyperMAC builds
# on (application-defined dynamic channels for keelOS::Video, Hybrid security that also takes
# TLS-only clients, the password or its NT hash looked up by user name). Those two patches are
# copies of hyperMAC's; take new ones from there, do not edit them here. hyperWIN's own changes
# go into ironrdp-hyperwin.patch, applied last.
# And a second one for the guests' helper, further down.
# Re-made only when the pin or a patch changed. Needs git and sh (Git for Windows has both):
#   sh tools/vendor-ironrdp.sh
set -e
D=$(cd "$(dirname "$0")/.." && pwd)
REV=8f9bf5b8102dd3c4cdb4841939d4dbba35c7bff8
V=$D/vendor/IronRDP
SUM=$(cat "$D/ironrdp-keel.patch" "$D/ironrdp-hypermac.patch" "$D/ironrdp-hyperwin.patch" | sha256sum | cut -d' ' -f1)
if [ "$(cat "$V/.keel-rev" 2>/dev/null)" != "$REV $SUM" ]; then
	rm -rf "$V" && mkdir -p "$D/vendor"
	git clone -q https://github.com/Devolutions/IronRDP.git "$V"
	git -C "$V" -c advice.detachedHead=false checkout -q "$REV"
	git -C "$V" apply --whitespace=nowarn "$D/ironrdp-keel.patch"
	git -C "$V" apply --whitespace=nowarn "$D/ironrdp-hypermac.patch"
	git -C "$V" apply --whitespace=nowarn "$D/ironrdp-hyperwin.patch"	# hyperWIN's own, on top (its head says what)
	echo "$REV $SUM" > "$V/.keel-rev"
	echo "vendor/IronRDP: $REV with the three patches"
fi

# A second IronRDP, for the helper that runs in the guests (guest/): kRDPclient's pin with
# kRDPclient's patch, because that one has the Windows clipboard with files (copy and paste
# of files in Explorer), which the pin above does not. ironrdp-krdp.patch is a copy of
# kRDPclient's; take a new one from there, do not edit it here. The helper is a workspace of
# its own (two IronRDPs do not resolve in one build), and the two only meet on the wire.
GREV=d5e70f02b670dbe79ad7ef47203bfa82d32c16f4
G=$D/vendor/IronRDP-guest
GSUM=$(sha256sum < "$D/ironrdp-krdp.patch" | cut -d' ' -f1)
if [ "$(cat "$G/.krdp-rev" 2>/dev/null)" != "$GREV $GSUM" ]; then
	rm -rf "$G" && mkdir -p "$D/vendor"
	git clone -q https://github.com/Devolutions/IronRDP.git "$G"
	git -C "$G" -c advice.detachedHead=false checkout -q "$GREV"
	git -C "$G" apply --whitespace=nowarn "$D/ironrdp-krdp.patch"
	echo "$GREV $GSUM" > "$G/.krdp-rev"
	echo "vendor/IronRDP-guest: $GREV with ironrdp-krdp.patch"
fi
