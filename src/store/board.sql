CREATE TABLE IF NOT EXISTS board_projects (
  project_id   TEXT PRIMARY KEY,
  enabled      INTEGER NOT NULL DEFAULT 0,
  priority     INTEGER NOT NULL DEFAULT 100,
  confidential INTEGER NOT NULL DEFAULT 1,
  deadline_ms  INTEGER,
  key_prefix   TEXT NOT NULL,
  machines_json TEXT,
  next_ticket  INTEGER NOT NULL DEFAULT 1,
  next_epic    INTEGER NOT NULL DEFAULT 1,
  updated_at   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS board_milestones (
  id          TEXT PRIMARY KEY,
  project_id  TEXT NOT NULL,
  title       TEXT NOT NULL,
  target_ms   INTEGER,
  status      TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS board_epics (
  id                 TEXT PRIMARY KEY,
  project_id         TEXT NOT NULL,
  milestone_id       TEXT,
  key                TEXT NOT NULL,
  title              TEXT NOT NULL,
  question           TEXT NOT NULL,
  status             TEXT NOT NULL,
  priority           INTEGER NOT NULL DEFAULT 100,
  criteria_version   INTEGER NOT NULL DEFAULT 0,
  rules_json         TEXT NOT NULL,
  budget_gpu_hours   REAL,
  budget_deadline_ms INTEGER,
  conclusion         TEXT,
  proposed_by        TEXT NOT NULL,
  created_at         INTEGER NOT NULL,
  updated_at         INTEGER NOT NULL,
  UNIQUE(project_id, key)
);

CREATE TABLE IF NOT EXISTS board_epic_criteria (
  epic_id     TEXT NOT NULL,
  version     INTEGER NOT NULL,
  body_md     TEXT NOT NULL,
  checks_json TEXT NOT NULL,
  approved_at INTEGER,
  created_at  INTEGER NOT NULL,
  PRIMARY KEY(epic_id, version)
);

CREATE TABLE IF NOT EXISTS board_tickets (
  id                 TEXT PRIMARY KEY,
  project_id         TEXT NOT NULL,
  epic_id            TEXT,
  key                TEXT NOT NULL,
  kind               TEXT NOT NULL,
  batch_id           TEXT,
  title              TEXT NOT NULL,
  hypothesis         TEXT,
  change_spec        TEXT,
  status             TEXT NOT NULL,
  priority           INTEGER NOT NULL DEFAULT 100,
  base_experiment_id TEXT,
  experiment_id      TEXT,
  needs_gpus         INTEGER NOT NULL DEFAULT 0,
  est_minutes        INTEGER,
  validation_json    TEXT,
  harness            TEXT,
  session_id         TEXT,
  machine_id         TEXT,
  slot_lease_at      INTEGER,
  repair_count       INTEGER NOT NULL DEFAULT 0,
  outcome            TEXT,
  core_hypothesis    INTEGER NOT NULL DEFAULT 0,
  result_json        TEXT,
  conclusion         TEXT,
  tainted            INTEGER NOT NULL DEFAULT 0,
  tainted_reason     TEXT,
  pinned_machine_id  TEXT,
  proposed_by        TEXT NOT NULL,
  started_at         INTEGER,
  finished_at        INTEGER,
  created_at         INTEGER NOT NULL,
  updated_at         INTEGER NOT NULL,
  UNIQUE(project_id, key)
);
CREATE INDEX IF NOT EXISTS idx_board_tickets_status ON board_tickets(status, priority);
CREATE UNIQUE INDEX IF NOT EXISTS uidx_board_tickets_experiment
  ON board_tickets(experiment_id) WHERE experiment_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS board_relations (
  from_id    TEXT NOT NULL,
  to_id      TEXT NOT NULL,
  kind       TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY(from_id, to_id, kind)
);

CREATE TABLE IF NOT EXISTS board_decisions (
  id              TEXT PRIMARY KEY,
  project_id      TEXT NOT NULL,
  epic_id         TEXT,
  kind            TEXT NOT NULL,
  ticket_ids_json TEXT NOT NULL,
  payload_json    TEXT NOT NULL,
  status          TEXT NOT NULL,
  resolution_json TEXT,
  dedupe_key      TEXT NOT NULL UNIQUE,
  created_at      INTEGER NOT NULL,
  resolved_at     INTEGER
);
CREATE INDEX IF NOT EXISTS idx_board_decisions_open ON board_decisions(status, created_at);

CREATE TABLE IF NOT EXISTS board_events (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  at          INTEGER NOT NULL,
  project_id  TEXT NOT NULL,
  subject_id  TEXT NOT NULL,
  actor       TEXT NOT NULL,
  kind        TEXT NOT NULL,
  detail_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_board_events_subject ON board_events(subject_id, id);

CREATE TABLE IF NOT EXISTS board_artifact_refs (
  ticket_id  TEXT NOT NULL,
  path       TEXT NOT NULL,
  role       TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY(ticket_id, path, role)
);

CREATE TABLE IF NOT EXISTS board_machines (
  id         TEXT PRIMARY KEY,
  name       TEXT NOT NULL UNIQUE,
  backend    TEXT NOT NULL,
  gpu_slots  INTEGER NOT NULL,
  own        INTEGER NOT NULL DEFAULT 1,
  enabled    INTEGER NOT NULL DEFAULT 1,
  updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS board_settings (
  k TEXT PRIMARY KEY,
  v TEXT NOT NULL
);

CREATE TRIGGER IF NOT EXISTS delete_board_project AFTER DELETE ON local_projects BEGIN
  DELETE FROM board_projects WHERE project_id = OLD.id;
  DELETE FROM board_milestones WHERE project_id = OLD.id;
  DELETE FROM board_epic_criteria WHERE epic_id IN (SELECT id FROM board_epics WHERE project_id = OLD.id);
  DELETE FROM board_epics WHERE project_id = OLD.id;
  DELETE FROM board_relations WHERE from_id IN (SELECT id FROM board_tickets WHERE project_id = OLD.id)
                                  OR to_id IN (SELECT id FROM board_tickets WHERE project_id = OLD.id);
  DELETE FROM board_artifact_refs WHERE ticket_id IN (SELECT id FROM board_tickets WHERE project_id = OLD.id);
  DELETE FROM board_tickets WHERE project_id = OLD.id;
  DELETE FROM board_decisions WHERE project_id = OLD.id;
  DELETE FROM board_events WHERE project_id = OLD.id;
END;

-- A deleted experiment leaves its ticket as the record of what was tried.
CREATE TRIGGER IF NOT EXISTS unlink_board_experiment AFTER DELETE ON local_experiments BEGIN
  UPDATE board_tickets SET experiment_id = NULL WHERE experiment_id = OLD.id;
END;
