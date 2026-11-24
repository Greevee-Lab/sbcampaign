-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Base schema for sbcampaign, RECONSTRUCTED for local testing and CI.
--
-- The upstream repository ships data migration scripts and stored procedures
-- (sql/*.sql) but no CREATE TABLE statements. This file was derived from the
-- queries in src/ and the column lists in the upstream migration scripts.
-- It is not an official dump of the live database. Replace it with an
-- upstream schema dump (pg_dump --schema-only) as soon as one is available.
--
-- Apply order on an empty database:
--   1. sql/fork/base_schema.sql      (this file)
--   2. sql/set_award_rank.sql, sql/give_turn_award.sql,
--      sql/give_round_award.sql, sql/shop_drop.sql   (upstream procedures)
--   3. optional: sql/fork/test_seed.sql (synthetic data only)

CREATE TYPE faction AS ENUM ('none', 'hsd', 'prf', 'rb', 'jar');
CREATE TYPE language AS ENUM ('ja', 'en');

CREATE TABLE accounts (
    xuid       uuid        PRIMARY KEY,
    xname      varchar(16) DEFAULT NULL,
    faction    faction     NOT NULL DEFAULT 'none',
    pilot      bytea       DEFAULT NULL,
    "language" language    DEFAULT NULL,
    banned     boolean     NOT NULL DEFAULT false
);

CREATE TABLE settings (
    round    smallint    NOT NULL,
    turn     smallint    NOT NULL,
    "period" timestamptz NOT NULL,
    PRIMARY KEY (round, turn)
);

CREATE TABLE shop (
    "type"   smallint    PRIMARY KEY,
    faction  faction     NOT NULL,
    "cost"   integer     NOT NULL,
    vtstock  smallint    DEFAULT NULL,
    vtlimit  smallint    DEFAULT NULL,
    "serial" bigint      NOT NULL DEFAULT 1,
    turn     smallint    NOT NULL,
    name     varchar(32) NOT NULL
);

CREATE TABLE garage (
    "type"   smallint NOT NULL,
    "serial" bigint   NOT NULL,
    xuid     uuid     NOT NULL,
    faction  faction  NOT NULL,
    sortied  boolean  NOT NULL DEFAULT false,
    PRIMARY KEY ("type", "serial")
);
CREATE INDEX garage_xuid_faction ON garage (xuid, faction);

CREATE TABLE leaderboard (
    xuid    uuid     NOT NULL,
    faction faction  NOT NULL,
    round   smallint NOT NULL,
    turn    smallint NOT NULL,
    "map"   smallint NOT NULL,
    stat1   integer  NOT NULL DEFAULT 0,
    stat2   integer  NOT NULL DEFAULT 0,
    stat3   integer  NOT NULL DEFAULT 0,
    stat4   integer  NOT NULL DEFAULT 0,
    stat5   integer  NOT NULL DEFAULT 0,
    stat6   integer  NOT NULL DEFAULT 0,
    stat7   integer  NOT NULL DEFAULT 0,
    stat8   integer  NOT NULL DEFAULT 0,
    stat9   integer  NOT NULL DEFAULT 0,
    stat10  integer  NOT NULL DEFAULT 0,
    PRIMARY KEY (xuid, faction, round, turn, "map")
);

CREATE TABLE wartide (
    round smallint NOT NULL,
    "map" smallint NOT NULL,
    hsd   integer  NOT NULL DEFAULT 0,
    prf   integer  NOT NULL DEFAULT 0,
    rb    integer  NOT NULL DEFAULT 0,
    PRIMARY KEY (round, "map")
);

CREATE TABLE awards (
    xuid  uuid     NOT NULL,
    round smallint NOT NULL,
    turn  smallint NOT NULL,
    rank1 integer  DEFAULT NULL,
    rank2 integer  DEFAULT NULL,
    rank3 integer  DEFAULT NULL,
    rank4 integer  DEFAULT NULL,
    PRIMARY KEY (xuid, round, turn)
);

CREATE TABLE graveyard (
    id    bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    xuid  uuid        NOT NULL,
    pilot bytea       DEFAULT NULL,
    tod   timestamptz NOT NULL DEFAULT now()
);
