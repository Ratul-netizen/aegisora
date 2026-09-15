-- 0008 — what makes interface discovery repeatable.
--
-- Discovery runs every fifteen minutes for the life of a device. Without a key to match
-- an already-discovered interface on, every run creates a fresh child: after a day a
-- 48-port switch has 4 608 interfaces, every one of them with a fraction of the
-- telemetry, and no way to tell which is the real `GigabitEthernet0/1`.
--
-- # Why the name and not the index
--
-- `ifIndex` is the obvious key and it is the wrong one. It is an arbitrary integer the
-- agent may renumber across a reboot, a module insertion, or a firmware upgrade — the
-- MIB says only that it is stable "between re-initializations of the network management
-- system". A key that changes when a switch reboots would re-create every interface on
-- every device in a power cut.
--
-- `ifName` is what the operator calls it, what the vendor prints on the chassis, and
-- what survives a reboot. `generic-snmp` reads it from `ifXTable::ifName` for exactly
-- this reason — see the comment there about `ifDescr` being a sentence on some agents
-- and identical across every port on others.
--
-- # Why partial
--
-- Only children. Two top-level devices with the same name are a real situation — the
-- same hostname in two sites, before identity resolution has decided anything about
-- them — and this index must not be the thing that refuses that. Under one parent, two
-- interfaces with one name is not ambiguity, it is an agent contradicting itself, and
-- keeping the first is the right answer.
--
-- The `kind` column is in the key because a parent may one day hold children of more
-- than one kind — a host with interfaces and containers — and `eth0` the interface is
-- not `eth0` the container.

CREATE UNIQUE INDEX resource_child_name_key
    ON resource (tenant_id, parent_id, kind, name)
 WHERE parent_id IS NOT NULL;
