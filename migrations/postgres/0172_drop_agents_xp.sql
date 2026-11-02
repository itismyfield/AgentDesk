-- XP is gone and no binary on either node reads or writes agents.xp any more.
ALTER TABLE agents DROP COLUMN IF EXISTS xp;
