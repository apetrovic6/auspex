CREATE TABLE actions (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('deploy', 'build', 'reboot', 'rollback', 'freeze', 'unfreeze')),
    machine TEXT NOT NULL CHECK (machine <> '' AND length(CAST(machine AS BLOB)) <= 255),
    fleet_commit TEXT CHECK (fleet_commit IS NULL OR (length(fleet_commit) BETWEEN 4 AND 64 AND fleet_commit NOT GLOB '*[^0-9a-f]*')),
    fleet_dirty INTEGER CHECK (fleet_dirty IS NULL OR fleet_dirty IN (0, 1)),
    started_at TEXT NOT NULL CHECK (started_at GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9]Z'),
    ended_at TEXT CHECK (ended_at IS NULL OR ended_at GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9]Z'),
    result TEXT CHECK (result IS NULL OR result IN ('succeeded', 'failed', 'cancelled', 'timed_out', 'interrupted')),
    exit_code INTEGER,
    who TEXT NOT NULL CHECK (who <> ''),
    host TEXT NOT NULL CHECK (host <> ''),
    detail TEXT CHECK (detail IS NULL OR length(CAST(detail AS BLOB)) <= 4096),
    auspex_version TEXT NOT NULL,
    pid INTEGER NOT NULL,
    pid_start TEXT,
    CHECK ((ended_at IS NULL) = (result IS NULL)),
    CHECK (exit_code IS NULL OR ended_at IS NOT NULL)
);
CREATE INDEX actions_machine_started ON actions (machine, started_at DESC, id DESC);
CREATE INDEX actions_started ON actions (started_at, id);
