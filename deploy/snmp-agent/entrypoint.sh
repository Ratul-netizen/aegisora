#!/bin/sh
# Create the v3 user, then run the agent in the foreground.
#
# Two things here are the result of getting them wrong first.
#
# The user is created in the *persistent* config (/var/lib/net-snmp/snmpd.conf), which
# net-snmp reads separately from /etc/snmp/snmpd.conf. Starting snmpd with `-C` — "use
# only the files I name" — excludes it, and the agent comes up with no v3 user at all.
# The symptom is "Unknown user name" on every request, which reads like a credential
# problem rather than a configuration one.
#
# And `-c FILE` *adds* to the default search path rather than replacing it, so naming
# /etc/snmp/snmpd.conf explicitly makes snmpd read it twice and bind port 161 twice.
# That fails with "Error opening specified endpoint", which reads like a permissions
# problem and is not one. Both files are in the default path; neither is named.
#
# net-snmp consumes `createUser` on startup: it derives the keys, writes a `usmUser`
# line in its place, and deletes the plaintext. So this file is written fresh every
# start; a stale createUser on the second start is an error.
set -eu

PERSIST=/var/lib/net-snmp
mkdir -p "$PERSIST"

# SHA-256 and AES-256 — SPEC §M2's pair. AES-192/256 are the Blumenthal draft and a
# compile-time option that not every distribution enables, so this asserts rather than
# assumes: if this build cannot do it, fail loudly at start instead of producing an
# agent that quietly refuses every authPriv request.
if ! snmpget --help 2>&1 | grep -q 'AES-256'; then
  echo "this net-snmp build has no AES-256; SPEC M2 asks for it" >&2
  exit 1
fi

cat > "$PERSIST/snmpd.conf" <<EOF
createUser uops-v3 SHA-256 uops-auth-passphrase AES-256 uops-priv-passphrase
EOF

echo "uops test agent: v3 user uops-v3 with SHA-256 / AES-256"

# Neither -C nor -c: the defaults already cover /etc/snmp/snmpd.conf and the
# persistent file, and naming either one breaks the other. See above.
exec snmpd -f -Lo
