// What the panel keeps, on the appliance's data disk: its user, the
// sessions, the hosts it manages and what each is to it, and the history of
// their numbers -- an hour a second at a time, thirty days a minute at a
// time. SQLite, Node's own (`node:sqlite`): nothing native to build, and a
// file a crash leaves whole.

import fs from 'node:fs';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';

/** Where the data is: the data disk's folder, or `PANEL_DATA`. */
export const DATA = process.env.PANEL_DATA || path.join(process.cwd(), 'data');

let db: DatabaseSync | null = null;

/** The database, made and brought to the schema on first use. */
export function store(): DatabaseSync {
  if (db) {
    return db;
  }
  fs.mkdirSync(/*turbopackIgnore: true*/ DATA, { recursive: true, mode: 0o700 });
  const d = new DatabaseSync(/*turbopackIgnore: true*/ path.join(DATA, 'panel.db'));
  d.exec(`
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    CREATE TABLE IF NOT EXISTS users (
      name TEXT PRIMARY KEY,
      hash TEXT NOT NULL,
      changed_ms INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS sessions (
      id TEXT PRIMARY KEY,
      user TEXT NOT NULL,
      created_ms INTEGER NOT NULL,
      seen_ms INTEGER NOT NULL,
      ip TEXT NOT NULL,
      agent TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS hosts (
      id TEXT PRIMARY KEY,
      name TEXT NOT NULL,
      url TEXT NOT NULL,
      pin TEXT NOT NULL,
      added_ms INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS identity (
      id INTEGER PRIMARY KEY CHECK (id = 1),
      cert TEXT NOT NULL,
      key TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS samples (
      host TEXT NOT NULL,
      ms INTEGER NOT NULL,
      fine INTEGER NOT NULL,
      cpu_busy_permille INTEGER,
      mem_used_bytes INTEGER,
      rx_bytes_per_s INTEGER,
      tx_bytes_per_s INTEGER,
      PRIMARY KEY (host, fine, ms)
    );
    CREATE TABLE IF NOT EXISTS settings (
      key TEXT PRIMARY KEY,
      value TEXT NOT NULL
    );
  `);
  db = d;
  return d;
}

export function setting(key: string): string | undefined {
  const row = store().prepare('SELECT value FROM settings WHERE key = ?').get(key) as { value: string } | undefined;
  return row?.value;
}

export function setSetting(key: string, value: string) {
  store().prepare('INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value')
    .run(key, value);
}
