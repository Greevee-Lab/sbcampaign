-- SPDX-License-Identifier: AGPL-3.0-only
--
-- W531: which VT a pilot took into a battle (additive).
--
-- The game names the VTs of a sortie in VtSortie (0x1047: u16 count, then
-- count x (u16 type, u32 serial)) when the mission starts; the result
-- (AddUserPoint 0x5026) carries no VT. The Steelbound gateway writes one row
-- here per accepted sortie and links it to the pilot's next user_points
-- result in gateway_result (result_id), so that every battle of a pilot can
-- name the VT type it was fought with.
--
-- Contents: VT types the campaign stores anyway (garage), the side and the
-- times. No serials, no names, no addresses. Like gateway_result this is
-- campaign data: a season reset empties it (fork/season_reset.sql empties
-- every table in schema public together).
--
-- The core is referred to by value only (identity_id, player_id), as in
-- gateway_result (fork/core_split.sql).

CREATE TABLE battle_vt (
    battle_vt_id bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    sortied_at   timestamptz NOT NULL DEFAULT now(),
    identity_id  uuid        NOT NULL,
    player_id    uuid        NOT NULL,
    faction      faction     NOT NULL,
    -- The VT the pilot drives: the first VT of the sortie.
    vt_type      smallint    NOT NULL,
    -- Every VT type of the sortie in the game's order (normally one).
    vt_types     smallint[]  NOT NULL CHECK (cardinality(vt_types) >= 1),
    -- The pilot's result of this battle; NULL until it arrives (or for ever,
    -- when the mission was aborted).
    result_id    bigint      UNIQUE REFERENCES gateway_result (result_id) ON DELETE CASCADE,
    linked_at    timestamptz
);

-- The gateway looks for the newest open sortie of a player.
CREATE INDEX battle_vt_open_idx ON battle_vt (player_id, sortied_at DESC) WHERE result_id IS NULL;
