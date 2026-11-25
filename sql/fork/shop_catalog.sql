-- SPDX-License-Identifier: AGPL-3.0-only
--
-- VT catalog of the reinforcement shop.
--
-- Upstream fills the table "shop" with a one-off import from its old MySQL
-- database (sql/shop_migration.sql), which is not part of the migration list.
-- Without rows a new pilot sees an empty shop: the game cannot buy a VT, the
-- garage stays empty and MISSION SELECT stays disabled.
--
-- The rows below are the catalog of sql/shop_migration.sql (type, faction,
-- cost, limit, turn of release, name). A new season starts with full stock
-- (vtstock = vtlimit, NULL = unlimited) and serial numbers from 1. Existing
-- rows are left alone, so applying this to a running campaign changes
-- nothing that the service already tracks.
--
-- Faction: hsd = Hai Shi Dao, prf = Pacific Rim Forces, rb = guerrilla forces,
-- jar = Jaralaccs. turn = first turn of a round in which the VT is sold.

INSERT INTO shop ("type", faction, "cost", vtstock, vtlimit, "serial", turn, name) VALUES
    ( 0, 'hsd',   12, NULL, NULL, 1, 1, 'Vitzh'),
    ( 1, 'hsd',   14,   30,   30, 1, 3, 'm-Vitzh'),
    ( 2, 'hsd',   10, NULL, NULL, 1, 1, 'Vortex'),
    ( 3, 'hsd', 3000,    1,    1, 1, 8, 'Juggernaut'),
    ( 5, 'prf',   22,   25,   25, 1, 1, 'Falchion'),
    ( 6, 'prf',  100,   15,   15, 1, 4, 'Blade'),
    ( 7, 'prf',   25, NULL, NULL, 1, 1, 'Decider'),
    ( 8, 'hsd',   30, NULL, NULL, 1, 2, 'Scare Face'),
    ( 9, 'hsd',   25,   25,   25, 1, 3, 'Scare Face A1'),
    (10, 'prf',  110, NULL, NULL, 1, 4, 'Prominence M1'),
    (11, 'prf',  400,    6,    6, 1, 5, 'Prominence M2'),
    (12, 'prf',  600,    3,    3, 1, 6, 'Prominence M3'),
    (13, 'hsd',  120, NULL, NULL, 1, 4, 'Scare Face II'),
    (14, 'prf',  400,    4,    4, 1, 5, 'Rapier'),
    (15, 'prf', 2600,    2,    2, 1, 7, 'Quasar'),
    (16, 'jar',  170, NULL, NULL, 1, 5, 'Jaralaccs C'),
    (17, 'jar',  400,    4,    4, 1, 6, 'Jaralaccs NS-R'),
    (18, 'jar',  160, NULL, NULL, 1, 5, 'Jaralaccs N'),
    (19, 'hsd',  800,    2,    2, 1, 7, 'Regal Dress N'),
    (20, 'hsd',  800,    2,    2, 1, 7, 'Regal Dress A'),
    (21, 'hsd',  550,    3,    3, 1, 5, 'Behemoth'),
    (22, 'hsd',  120,   15,   15, 1, 5, 'Maelstrom'),
    (23, 'rb',   300,    4,    4, 1, 5, 'Sheepdog'),
    (24, 'hsd',  260,    4,    4, 1, 6, 'Garpike'),
    (25, 'rb',   300,    6,    6, 1, 6, 'Siegeszug'),
    (26, 'rb',    18,   25,   25, 1, 3, 'Colt Executive'),
    (27, 'rb',    15, NULL, NULL, 1, 3, 'Colt'),
    (28, 'jar', 1200,    2,    2, 1, 7, 'Jaralaccs Macabre'),
    (29, 'rb',  4000,    1,    1, 1, 7, 'Earth Shaker'),
    (30, 'rb',    90, NULL, NULL, 1, 4, 'Yellow Jacket'),
    (31, 'prf',   20,   20,   20, 1, 2, 'Decider Volcanic')
ON CONFLICT ("type") DO NOTHING
