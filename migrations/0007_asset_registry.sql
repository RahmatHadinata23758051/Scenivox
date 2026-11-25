-- P2-02B Commit 1: Content-addressed local asset registry (additive).
CREATE TABLE assets (
    id TEXT PRIMARY KEY NOT NULL,
    sha256 TEXT NOT NULL UNIQUE CHECK(length(sha256)=64 AND sha256 NOT GLOB '*[^0-9a-f]*'),
    kind TEXT NOT NULL CHECK(kind IN ('narration_wav', 'sfx_wav')),
    provenance TEXT NOT NULL CHECK(provenance IN ('user_supplied', 'synthetic_test_fixture')),
    byte_size INTEGER NOT NULL CHECK(byte_size > 0 AND byte_size <= 67108864),
    storage_rel_path TEXT NOT NULL UNIQUE,
    metadata_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX assets_created_order ON assets(created_at, id);

CREATE TABLE asset_import_idempotency (
    idempotency_key TEXT PRIMARY KEY NOT NULL,
    request_sha256 TEXT NOT NULL CHECK(length(request_sha256)=64 AND request_sha256 NOT GLOB '*[^0-9a-f]*'),
    asset_id TEXT NOT NULL REFERENCES assets(id),
    response_json TEXT NOT NULL,
    created_flag INTEGER NOT NULL CHECK(created_flag IN (0,1))
);

CREATE TRIGGER assets_immutable_identity
BEFORE UPDATE ON assets
BEGIN
    SELECT RAISE(ABORT, 'asset records are immutable');
END;

CREATE TRIGGER assets_no_delete
BEFORE DELETE ON assets
BEGIN
    SELECT RAISE(ABORT, 'asset records are durable');
END;
