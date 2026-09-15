# A real SNMP agent, for the half of SPEC §M2's acceptance criterion a simulator
# cannot satisfy: "SNMPv3 authPriv (SHA-256 / AES-256) against a real device".
#
# net-snmp built from Alpine's package rather than a third-party image. The same
# reasoning as everywhere else here: one less publisher to trust, a version we pin, and
# a build that works with no network beyond the distribution mirror.
#
# This is a *test fixture*. It is never deployed and it holds credentials that are
# written down in this repository on purpose.
FROM alpine:3.21

RUN apk add --no-cache net-snmp net-snmp-tools

# The v3 user, created before the agent's first start. net-snmp rewrites this file at
# runtime — it hashes createUser into a persistent usmUser line and deletes the
# plaintext — so it is generated at container start rather than baked in, or the second
# start would find an empty createUser and fail.
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
COPY snmpd.conf /etc/snmp/snmpd.conf
RUN chmod +x /usr/local/bin/entrypoint.sh

EXPOSE 161/udp
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
