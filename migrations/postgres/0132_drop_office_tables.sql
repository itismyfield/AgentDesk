-- The office feature was removed (#6371); nothing reads or writes these any more.
ALTER TABLE departments DROP COLUMN IF EXISTS office_id;
DROP TABLE IF EXISTS office_agents;
DROP TABLE IF EXISTS offices;
