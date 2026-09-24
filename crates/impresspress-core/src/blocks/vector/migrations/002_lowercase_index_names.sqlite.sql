-- Mirrored to 002_lowercase_index_names.postgres.sql.
--
-- Index names are lowercase `[a-z0-9_]`: the database layer refuses any other
-- table name, so an index registered as `impresspress__vector__Docs` could no
-- longer be opened, queried or deleted. Its tables live in the SQLite vector
-- database, which folds identifier case, so `impresspress__vector__docs_meta`
-- already names the same table: only the registry row needs its lowercase
-- spelling.
--
-- Two rows can fold to one name (`Docs` beside `docs`, or `Docs` beside
-- `DOCS`); both already addressed the same tables. The lowercase row is kept
-- when there is one, otherwise the lowest-sorting spelling, and the rest are
-- deleted first so the rename below cannot collide on the primary key.
-- Idempotent: a second run finds no row to change.

DELETE FROM impresspress__vector__registry
WHERE prefixed_name <> lower(prefixed_name)
  AND EXISTS (
    SELECT 1 FROM impresspress__vector__registry AS other
    WHERE lower(other.prefixed_name) = lower(impresspress__vector__registry.prefixed_name)
      AND other.prefixed_name <> impresspress__vector__registry.prefixed_name
      AND (other.prefixed_name = lower(other.prefixed_name)
           OR other.prefixed_name < impresspress__vector__registry.prefixed_name)
  );

UPDATE impresspress__vector__registry
SET prefixed_name = lower(prefixed_name)
WHERE prefixed_name <> lower(prefixed_name);
