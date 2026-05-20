-- ============================================================
-- Provision noetl_ro — read-only access for noetl-doctor
-- Database: noetl-prod  |  Schema: noetl
-- Run as:  superuser (postgres / cloudsqlsuperuser)
--
-- Usage:
--   psql "postgresql://postgres:<admin-pass>@<host>:5432/noetl-prod" \
--     -f scripts/provision_noetl_ro.sql
--
-- Or from a pod:
--   kubectl exec -n noetl deploy/noetl-server -- \
--     psql "$NOETL_PG_DSN_ADMIN" -f /tmp/provision_noetl_ro.sql
--
-- The script is idempotent: re-running it only rotates the password.
-- Pass the password via the env var to avoid it appearing in ps output:
--
--   PGPASSWORD_RO=<secret> psql ... \
--     -v ro_password="'$PGPASSWORD_RO'" \
--     -f scripts/provision_noetl_ro.sql
--
--   -- then replace 'CHANGE_ME' below with :'ro_password'
-- ============================================================

-- 1. Create role (idempotent)
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'noetl_ro') THEN
    CREATE ROLE noetl_ro
      LOGIN
      PASSWORD 'CHANGE_ME'       -- replace or pass via -v ro_password=...
      CONNECTION LIMIT 5
      NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION;
    RAISE NOTICE 'Role noetl_ro created.';
  ELSE
    ALTER ROLE noetl_ro PASSWORD 'CHANGE_ME';
    RAISE NOTICE 'Role noetl_ro already exists — password updated.';
  END IF;
END
$$;

-- 2. Allow connecting to this database
GRANT CONNECT ON DATABASE "noetl-prod" TO noetl_ro;

-- 3. Allow reading the noetl schema
GRANT USAGE ON SCHEMA noetl TO noetl_ro;

-- 4. SELECT on exactly the three tables doctor queries
--    detect_stuck_executions  → noetl.event, noetl.command, noetl.runtime
--    reachability_smoke       → noetl.runtime
--    inspect_stale_commands   → noetl.command, noetl.event
GRANT SELECT ON noetl.event   TO noetl_ro;
GRANT SELECT ON noetl.command TO noetl_ro;
GRANT SELECT ON noetl.runtime TO noetl_ro;

-- 5. Future-proof: new tables added to the noetl schema also get SELECT
ALTER DEFAULT PRIVILEGES IN SCHEMA noetl
  GRANT SELECT ON TABLES TO noetl_ro;

-- 6. Verify
SELECT
  r.rolname,
  r.rolcanlogin,
  r.rolconnlimit,
  has_table_privilege('noetl_ro', 'noetl.event',   'SELECT') AS can_select_event,
  has_table_privilege('noetl_ro', 'noetl.command', 'SELECT') AS can_select_command,
  has_table_privilege('noetl_ro', 'noetl.runtime', 'SELECT') AS can_select_runtime
FROM pg_roles r
WHERE r.rolname = 'noetl_ro';
