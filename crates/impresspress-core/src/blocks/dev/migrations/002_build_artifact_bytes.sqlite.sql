-- The stored size of a build's artifact.
--
-- `dev_status` reports what the artifact store holds, and reporting it from a
-- folder listing meant walking the store on every ~300 ms poll (O(folder) on
-- OPFS). The builds table already indexes every artifact this block stores —
-- the garbage collector deletes a build row with the artifact it names — so
-- the size belongs beside the hash that identifies it.
--
-- Defaulted rather than back-filled: a row written before this migration
-- describes an artifact whose size nothing recorded, and 0 is the honest
-- answer for it. The dev sandbox re-seeds from its bundle, so no long-lived
-- instance carries such rows.
ALTER TABLE impresspress__dev__builds
    ADD COLUMN artifact_bytes INTEGER NOT NULL DEFAULT 0;
