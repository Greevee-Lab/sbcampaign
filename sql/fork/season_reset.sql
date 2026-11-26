-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Season reset: empty the campaign of this instance, keep the core.
--
--   SELECT * FROM campaign_reset(false);                         -- dry run
--   SELECT * FROM campaign_reset(true, '2026-12-02 18:45+00');   -- apply
--
-- Campaign = every table in schema public except fork_migrations: pilots and
-- accounts, garage, graveyard, leaderboard, war tide, awards, results ledger,
-- game audit, turns (settings). The shop catalog stays; its stock is refilled
-- and serial numbers start again at 1, exactly like a new catalog
-- (fork/shop_catalog.sql). settings gets one row: round 1, turn 1, ending at
-- p_turn1_end (default: one week from now, like fork/preseason.sql).
--
-- Core = every table in schema core (players, identities, display names,
-- reservations, change limits, accesses, core audit). Not touched: the
-- function fingerprints it (row count and md5 per table) before and after
-- and aborts if anything differs. It refuses to run on a database that is
-- not split yet, because there the players still live in public.
--
-- Note: accounts.banned is campaign data and goes with the reset; a ban that
-- must outlast the season is an identity suspension (core).
--
-- Returns one row per table: part ('campaign' = emptied, 'shop' = refilled,
-- 'core' = kept), table name, rows before. Run by deploy/server/maint.sh (backup
-- first, service stopped) and by the server's tests.
CREATE OR REPLACE FUNCTION campaign_reset(
    p_apply      boolean,
    p_turn1_end  timestamptz DEFAULT NULL,
    p_source     text        DEFAULT 'reset'
) RETURNS TABLE (part text, table_name text, rows_before bigint)
LANGUAGE plpgsql AS $$
DECLARE
    r          record;
    n          bigint;
    emptied    text[] := '{}';
    summary    text   := '';
    fp_before  text;
    fp_after   text;
BEGIN
    IF to_regclass('core.player') IS NULL OR to_regclass('public.player') IS NOT NULL THEN
        RAISE EXCEPTION 'campaign_reset: database is not split (fork/core_split.sql); a reset would delete the players';
    END IF;

    fp_before := campaign_reset_core_fingerprint();

    FOR r IN
        SELECT c.relname
        FROM pg_class c JOIN pg_namespace ns ON ns.oid = c.relnamespace
        WHERE ns.nspname = 'public' AND c.relkind IN ('r', 'p') AND c.relname <> 'fork_migrations'
        ORDER BY c.relname
    LOOP
        EXECUTE format('SELECT count(*) FROM public.%I', r.relname) INTO n;
        IF r.relname = 'shop' THEN
            part := 'shop';
        ELSE
            part := 'campaign';
            emptied := emptied || format('public.%I', r.relname);
        END IF;
        table_name := r.relname;
        rows_before := n;
        summary := summary || format('%s=%s ', r.relname, n);
        RETURN NEXT;
    END LOOP;

    FOR r IN
        SELECT c.relname
        FROM pg_class c JOIN pg_namespace ns ON ns.oid = c.relnamespace
        WHERE ns.nspname = 'core' AND c.relkind IN ('r', 'p')
        ORDER BY c.relname
    LOOP
        EXECUTE format('SELECT count(*) FROM core.%I', r.relname) INTO n;
        part := 'core';
        table_name := r.relname;
        rows_before := n;
        RETURN NEXT;
    END LOOP;

    IF NOT p_apply THEN
        RETURN;
    END IF;

    IF cardinality(emptied) > 0 THEN
        -- No CASCADE: a foreign key from outside the campaign aborts the reset.
        EXECUTE 'TRUNCATE ' || array_to_string(emptied, ', ') || ' RESTART IDENTITY';
    END IF;
    UPDATE public.shop SET vtstock = vtlimit, "serial" = 1;
    INSERT INTO public.settings (round, turn, "period")
    VALUES (1, 1, coalesce(p_turn1_end, now() + INTERVAL '1 WEEK'));
    -- First row of the new season's game audit: who reset, what was there.
    INSERT INTO public.audit_log (source, action, detail)
    VALUES (left(p_source, 64), 'campaign_reset', left(trim(summary), 1000));

    fp_after := campaign_reset_core_fingerprint();
    IF fp_after IS DISTINCT FROM fp_before THEN
        RAISE EXCEPTION 'campaign_reset: the core changed during the reset, nothing was reset';
    END IF;
END;
$$;

-- Row count and md5 of every core table, as one text (used by campaign_reset).
CREATE OR REPLACE FUNCTION campaign_reset_core_fingerprint() RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
    r    record;
    line text;
    fp   text := '';
BEGIN
    FOR r IN
        SELECT c.relname
        FROM pg_class c JOIN pg_namespace ns ON ns.oid = c.relnamespace
        WHERE ns.nspname = 'core' AND c.relkind IN ('r', 'p')
        ORDER BY c.relname
    LOOP
        EXECUTE format('SELECT %L || '' '' || count(*) || '' '' || md5(coalesce(string_agg(x::text, chr(10) ORDER BY x::text), '''')) FROM core.%I x',
                       r.relname, r.relname) INTO line;
        fp := fp || line || chr(10);
    END LOOP;
    RETURN fp;
END;
$$;
