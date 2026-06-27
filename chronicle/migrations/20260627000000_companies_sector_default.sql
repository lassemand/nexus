-- Allow chronicle's companies table to be populated without a sector.
-- The sector is resolved by signal via Polygon; chronicle only needs the ticker.
ALTER TABLE companies ALTER COLUMN sector SET DEFAULT '';
