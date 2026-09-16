-- 0009 — where a site is.
--
-- A map of the estate is the view an MSP opens first: forty customer sites across a
-- country, and the one that is red. Nothing in the schema could answer "where" until now.
--
-- # Why the coordinates are on the site and not on the device
--
-- A device's location is a fact about the rack it is in, and the rack is the site. Fifty
-- switches in one building are not fifty places; storing a coordinate per device would
-- be storing the same two numbers fifty times and inviting them to disagree.
--
-- # Why they are not derived from an IP address
--
-- Geo-IP answers a different question, and for this product it usually answers nothing:
-- a management address is RFC 1918 and resolves to no location at all, and a public
-- address resolves to the registrant's office rather than to the equipment. An operator
-- typing a coordinate once per site is both more accurate and less work than a lookup
-- that is wrong in a way nobody can see.
--
-- # Nullable, and that is the normal state
--
-- A site with no coordinate is not an error: most customers will place a handful of
-- sites and leave the rest. The map shows what it has, and the inventory does not care.

ALTER TABLE site
    -- Signed degrees, WGS 84 — what every map, GPS and phone reports. Not a PostGIS
    -- geography type: this is two numbers that are read whole and never queried
    -- spatially, and a spatial extension is a deployment dependency an on-premise
    -- customer would have to install for a pin on a map.
    ADD COLUMN latitude  double precision,
    ADD COLUMN longitude double precision,

    -- Out-of-range coordinates are a typo, and a typo that reaches the map puts a site
    -- in the sea. Checked here because a constraint is the only place a check cannot be
    -- forgotten by a future caller.
    ADD CONSTRAINT site_latitude_is_a_latitude
        CHECK (latitude IS NULL OR latitude BETWEEN -90 AND 90),
    ADD CONSTRAINT site_longitude_is_a_longitude
        CHECK (longitude IS NULL OR longitude BETWEEN -180 AND 180),

    -- Half a coordinate is not a location. Permitting one without the other would put
    -- the decision about what to do with it in every reader.
    ADD CONSTRAINT site_location_is_whole
        CHECK ((latitude IS NULL) = (longitude IS NULL));
