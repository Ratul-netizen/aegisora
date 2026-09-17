-- A bound on how many credentials one discovery job may name.
--
-- 0017 required at least one and said nothing about the ceiling, because at the time the
-- sweeper used exactly one. Building the credential loop showed why there has to be one,
-- and the reason is not tidiness — it is that the loop cannot be short-circuited.
--
-- `SNMPv2c` has no way to say "wrong community": RFC 3416 agents drop a request they
-- cannot authenticate, so a wrong community string is indistinguishable from an empty
-- address. A sweep must therefore try *every* credential against every address that has
-- not answered, and almost no address answers. The cost is linear and unavoidable:
--
--     one credential over a /16   ~5.5 minutes
--     four                        ~22 minutes
--     ten                         most of an hour, nearly all of it on empty addresses
--
-- Four covers what estates have — the old community string, the new one, an SNMPv3 user,
-- and one spare. An operator who needs more has two populations of equipment and wants
-- two jobs, which also lets them be scheduled apart and audited separately.
--
-- Here as well as in `uops_discover::MAX_CREDENTIALS` for the reason §2.3 gives about
-- every other limit in this feature: a bound that lives only in the application is one a
-- second caller does not have.

ALTER TABLE discovery_job
    ADD CONSTRAINT discovery_job_is_at_most_four_credentials
    CHECK (cardinality(credential_refs) <= 4);
