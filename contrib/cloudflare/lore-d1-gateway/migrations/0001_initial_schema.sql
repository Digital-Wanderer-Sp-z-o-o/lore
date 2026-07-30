-- SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
-- SPDX-License-Identifier: MIT

PRAGMA foreign_keys = ON;

CREATE TABLE lore_items (
    table_name TEXT NOT NULL,
    partition_key TEXT NOT NULL,
    sort_key TEXT NOT NULL DEFAULT '',
    item_json TEXT NOT NULL CHECK (json_valid(item_json)),
    PRIMARY KEY (table_name, partition_key, sort_key)
) WITHOUT ROWID;

CREATE TABLE lore_item_attributes (
    table_name TEXT NOT NULL,
    partition_key TEXT NOT NULL,
    sort_key TEXT NOT NULL DEFAULT '',
    attribute_name TEXT NOT NULL,
    attribute_kind TEXT NOT NULL,
    attribute_value TEXT NOT NULL,
    PRIMARY KEY (table_name, partition_key, sort_key, attribute_name),
    FOREIGN KEY (table_name, partition_key, sort_key)
        REFERENCES lore_items (table_name, partition_key, sort_key)
        ON DELETE CASCADE
) WITHOUT ROWID;

CREATE INDEX lore_item_attributes_query
    ON lore_item_attributes (
        table_name,
        attribute_name,
        attribute_kind,
        attribute_value,
        partition_key,
        sort_key
    );

-- A failed CHECK aborts and rolls back the complete D1 batch. The gateway uses
-- this singleton as an atomic condition guard for CAS and lock transactions.
CREATE TABLE lore_condition_guard (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    value INTEGER NOT NULL CHECK (value = 1)
);
