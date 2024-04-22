-- Add migration script here
CREATE TABLE IF NOT EXISTS premints
(
    id                 TEXT    NOT NULL,
    kind               TEXT    NOT NULL,
    version            INTEGER NOT NULL,
    signer             TEXT    NOT NULL,
    chain_id           INTEGER NOT NULL,
    collection_address TEXT,
    token_id           TEXT, -- may be u256, ensure we can store
    token_uri          TEXT,
    json               JSONB   NOT NULL,
    seen_on_chain      BOOLEAN NOT NULL DEFAULT false,
    created_at         DATETIME         DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (kind, id)
)
