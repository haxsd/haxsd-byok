CREATE TABLE devin_session_models (
    session_id TEXT PRIMARY KEY NOT NULL CHECK (length(session_id) > 0),
    model_uid TEXT NOT NULL CHECK (length(model_uid) > 0),
    model_spec_json TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
