# The SNMP test agent

SPEC §M2 asks for `SNMPv3 authPriv (SHA-256 / AES-256) **against a real device**`. A
simulator cannot satisfy that by definition, and a switch on a desk cannot run in CI.
This is the middle: a real `net-snmp` agent, speaking real USM, in a container.

```bash
docker compose -f deploy/docker-compose.yml --profile test up -d snmp-agent
```

It is behind a `test` profile, so `docker compose up` does not start it. It is a
fixture, not part of the product.

## What it is configured with

| | |
|---|---|
| v3 user | `uops-v3`, authPriv |
| auth | SHA-256, passphrase `uops-auth-passphrase` |
| privacy | AES-256, passphrase `uops-priv-passphrase` |
| v2c community | `uops-test`, read-only |
| port | `16100/udp` on the host |

Every one of those is written down here on purpose. Nothing this agent holds is worth
protecting, and a test fixture whose credentials are a secret is a test fixture nobody
can run.

## What it does not prove

It is Linux running `net-snmp`, which is the most standards-compliant agent there is.
Real network hardware is less so, and the interesting failures — an agent that answers
`tooBig` to everything, one that returns the same OID forever — come from firmware this
will never reproduce. Those are simulated instead, in `uops-snmp::sim`, which is the
only way to get them on demand.

So the two layers test different things, and neither replaces the other:

* the simulator: every failure mode, deterministically, in milliseconds;
* this agent: that the wire format, the USM key derivation and the crypto are right.

## Things that were surprising

Both cost an hour, and both are the sort of thing that reads like a different problem:

* `snmpd -c FILE` **adds** to the default config search path rather than replacing it.
  Naming `/etc/snmp/snmpd.conf` explicitly makes the agent read it twice and bind port
  161 twice, which fails with `Error opening specified endpoint` — a message that reads
  like a permissions problem and is not one.
* `snmpd -C` ("use only the files I name") excludes the *persistent* config at
  `/var/lib/net-snmp/snmpd.conf`, which is where `createUser` lives. The agent then
  starts with no v3 user and answers `Unknown user name` to everything, which reads
  like a credential problem and is not one.

The entrypoint names neither flag, and the comments there say why.

AES-192 and AES-256 are the Blumenthal draft — a compile-time option that not every
distribution enables. The entrypoint asserts the build supports AES-256 and exits if it
does not, rather than creating a user that silently fails every request.
