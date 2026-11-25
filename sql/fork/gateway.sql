-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Steelbound gateway: ledger of reported results (additive).
--
-- Every result a player reports through a gateway (AddUserPoint ->
-- leaderboard, AddMissionPoint -> wartide) is also written here, with the
-- identity that reported it. The upstream tables only hold sums, so this
-- ledger is what makes "annul the results of one identity" possible
-- (function annul_identity_results in fork/gateway_annul.sql).
--
-- Contents: numbers the campaign stores anyway (round, turn, map, faction,
-- points). No pilot data, no names, no addresses.

CREATE TABLE gateway_result (
    result_id    bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    at           timestamptz NOT NULL DEFAULT now(),
    identity_id  uuid        NOT NULL REFERENCES identity (identity_id),
    player_id    uuid        NOT NULL REFERENCES player (player_id),
    kind         text        NOT NULL CHECK (kind IN ('user_points', 'mission_points')),
    round        smallint    NOT NULL,
    turn         smallint    NOT NULL,
    "map"        smallint    NOT NULL,
    faction      faction     NOT NULL,
    points       bigint[]    NOT NULL CHECK (cardinality(points) IN (1, 10)),
    annulled_at  timestamptz
);

CREATE INDEX gateway_result_open_idx ON gateway_result (identity_id) WHERE annulled_at IS NULL;
