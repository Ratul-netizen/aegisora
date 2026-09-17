# M5 — Discovery: specification

PLAN's roadmap gives this milestone one line: *"CIDR · SNMP · LLDP/CDP · ARP · device
classification."* SPEC covers M0–M4 and says M5+ is deliberately absent, and its own rule
is that *"where a decision is still open it is marked **[OPEN]** and must be closed before
the milestone starts — not during."* This closes them.

| | |
|---|---|
| **Problem** | an operator installs this and has to type their estate in by hand. The demo built on 2026-09-17 needed eight `curl` calls to create eight devices |
| **Depends on** | everything it needs already exists: `uops-snmp` (walk, GETBULK, simulator), `uops-profile` (classification by `sysObjectID`), `uops-identity` (resolution with a review queue), `uops-store-pg` (`DiscoveredChild`, relationships), the poller's scheduler |
| **Produces** | resources that resolve to real identities, `connected_to` edges between them, and candidates for the ones it could not decide about |
| **Does not produce** | credentials, a port scanner, or anything that reaches outside the ranges an operator wrote down |

---

## 1. The three things discovery does

**Sweep.** Given a CIDR an operator entered, probe each address and find out what is
there. Produces *candidates*.

**Classify.** Turn a probe response into a vendor, a model, an OS and a monitoring
profile — machinery `uops-profile` already has, reached by `sysObjectID`.

**Walk neighbours.** On a device that is already known, read LLDP, CDP and ARP to find
*adjacent* devices and the links between them. Produces edges, and more candidates.

The third is the one that matters most and is easiest to underrate: an operator enters one
core switch, and neighbour discovery finds the rest of the estate from it. A CIDR sweep is
the fallback for the parts of a network that nothing points at.

---

## 2. Decisions

### 2.1 Discovery probes SNMP directly. ICMP is not a prerequisite.

The obvious design pings first and probes what answers. It is wrong here: a device that
drops ICMP while answering SNMP is common — it is the default on several firewall
platforms and on any host with a restrictive local policy — and a sweep that skips them
silently discovers less than the operator's own network diagram.

So the probe is an SNMP `GET` of three OIDs in one request: `sysObjectID`, `sysName`,
`sysDescr`. That is the liveness test and the classification in a single round trip.
Nothing answers on 161 unless it is an agent, so a response *is* a device.

ICMP stays available as a cheap pre-filter for a large range where the operator says it is
safe — a flag on the job, off by default, and named for what it costs: `skip_silent_hosts`.

### 2.2 Credentials are supplied, never guessed.

A discovery job names the credentials it may use, in order, from the vault. It tries each
once per address.

**It never tries a list of likely community strings.** That is what a scanner does, and it
is wrong for three reasons that are not about taste: it is indistinguishable from an
attack in the customer's own IDS logs, SNMPv3 authentication failures lock accounts on
several platforms, and a product that ships with `public` in a wordlist is a product that
teaches its users that guessing credentials is normal.

An address that answers nothing with the supplied credentials is recorded as
`unreachable`, which is a fact an operator can act on, rather than retried with more
guesses.

### 2.3 A sweep is bounded, and the bound is in the schema.

A discovery job holds CIDRs, not "the network". The rules:

- **A /16 is the largest single range** — 65 536 addresses. Larger is refused when the job
  is written, with the sentence saying to split it. An operator who means a /8 means
  something else.
- **Addresses per job are capped at 65 536** across all its ranges, for the same reason.
- **Concurrency is capped** at `IN_FLIGHT` probes, and the rate at `PROBES_PER_SECOND`. A
  discovery run must not be the reason a customer's network monitoring alerts.
- **Network and broadcast addresses are skipped** in any range of /30 or wider. Not for
  the two addresses: the broadcast address makes every host on the segment answer at
  once, which looks like a tool that has found a great many devices and is one being
  shouted at by the same device several hundred times. A /31 is exempt (RFC 3021 — both
  addresses of a point-to-point link are usable) and a /32 is one host.

The caps are constants in one place, and the schema enforces the first two — a limit that
lives only in the application is one a second caller does not have.

### 2.4 Discovery resolves identity; interface discovery does not.

`uops_store_pg::discovery` explains why an interface bypasses the resolver: its parent is
not in question. A swept device is the opposite case — it is exactly *"a resource that
turned up"*, which is what `create_provisional` and the review queue exist for.

So each probe response becomes an `ObservedIdentity` carrying what it proved:

| Identifier | Tier | Available |
|---|---|---|
| `MgmtIp` — the address probed | 3 | the probe |
| `Hostname` — `sysName` | 4 | the probe |
| `Serial` — `entPhysicalSerialNum` | 1 | the first poll |
| `SnmpEngineId` — `snmpEngineID` (v3) | 1 | the first poll |

The last column is the one that decides the design. **A probe proves nothing globally
unique.** A serial is a table column and needs an index to `GET`; an engine ID belongs to
the v3 session rather than to the MIB. Reading either costs a second conversation with
every address in the range, most of which are empty — so both wait for the first poll,
which is a conversation with something already known to exist, and *upgrade* the
resolution when they arrive.

That is not a limitation to work around; it is why the review queue exists. An address
and a hostname are tier 3 and tier 4, so a sweep lands most results below
`AUTO_MERGE_THRESHOLD` by construction, and a device is identified precisely when it
becomes worth polling.

Each sighting goes through `uops_identity::classify`. Above `AUTO_MERGE_THRESHOLD` it merges into the
existing resource. Below `REVIEW_FLOOR` it creates a new one. Between them it becomes a
review-queue entry and **not** a resource — which is the whole point of that queue, and
the case a sweep produces constantly: the same hostname in two sites, a device that
changed address, a spare that was racked with a clone's configuration.

### 2.5 A neighbour that is not known is a candidate, not a resource.

LLDP and CDP report a neighbour's chassis ID, port and platform. That is enough to create
an edge *if both ends exist*, and not enough to create the far end: a chassis ID is an
identifier, not a device, and inventing a resource from one produces an inventory full of
half-devices that never get polled because nothing knows how to reach them.

So: an edge is written only between two resources that exist. An unknown neighbour becomes
a candidate carrying its chassis ID and management address, and the next sweep — or an
operator pressing *probe* — turns it into a device. Same rule for ARP, which is weaker
still: an ARP entry is a MAC and an IP, and most of them are laptops.

### 2.6 Edges are `connected_to`, and they are not blast radius.

`RelationshipKind::ConnectedTo` is already excluded from dependency traversal, and the
comment in `resource.rs` says why: L2 adjacency is not causation. Discovery produces
`connected_to` and nothing else. `depends_on` is a judgement about services, and M9's
correlation engine is what will infer it.

### 2.7 Every run is a row, and every scan is audited.

Scanning a network is a sensitive operation — it is the thing a customer's security team
will ask about first. A discovery run records who started it, which ranges, when it
finished, how many addresses were probed, how many answered, and what it created. The
audit entry carries the ranges, because "who scanned 10.0.0.0/16 on Tuesday" is the
question that gets asked.

Manual runs are `Operator`. Editing a job's ranges is `Operator`. Reading is `Viewer`.

---

## 3. Schema

```sql
discovery_job       -- what to scan, how often, with which credentials
discovery_run       -- one execution: when, by whom, what it found
discovery_candidate -- an address or a neighbour that is not yet a resource
```

`discovery_candidate` is the table that keeps this honest. It is where an address that
answered but could not be identified goes, where an LLDP neighbour with no matching
resource goes, and where an operator looks to see what discovery found and did not act on.
A product that silently drops what it cannot classify is one whose inventory an operator
cannot trust.

---

## 4. Acceptance criteria

- [ ] A /24 sweep against the SNMP simulator finds every agent in it and creates one
      resource per agent, classified onto a profile
- [ ] Re-running the same sweep creates nothing new — the second run resolves every
      address to the resource the first one created
- [ ] A device that answers with a hostname matching an existing resource in another site
      produces a **review-queue entry**, not a second resource and not a silent merge
- [ ] A CIDR larger than /16 is refused when the job is written, with a sentence saying
      what to do instead
- [ ] An LLDP walk between two known devices produces exactly one `connected_to` edge, and
      re-walking produces no duplicate
- [ ] An LLDP neighbour with no matching resource produces a candidate, not a resource
- [ ] A sweep of 65 536 addresses stays within its concurrency and rate caps, measured
- [ ] No code path anywhere tries a credential that was not named by the job

---

## 5. What M5 does not do

**Nmap.** No port scanning, no service fingerprinting, no OS detection by TCP stack
behaviour. The product discovers what answers SNMP and what its neighbours say; a network
scanner is a different product with a different security posture.

**WMI, SSH or agent push.** M5 is SNMP and neighbours. Hosts arrive through the OTel
Collector, which M3 already ships.

**Automatic polling of everything it finds.** A discovered device is created and
classified; whether it is polled is a decision — a profile and a credential assignment —
and doing it automatically is how a discovery run turns into a thousand new SNMP
conversations nobody asked for. The UI offers it as one action on a list.

**An IPv6 sweep.** A /64 is 18 quintillion addresses, so sweeping one is not a slow
version of sweeping a /24, it is a different thing that does not work. The schema refuses
an IPv6 range outright rather than accepting one and timing out. IPv6 devices arrive
through neighbour discovery, which does not enumerate anything.

**Topology layout.** M5 produces edges. Drawing them is M6.
