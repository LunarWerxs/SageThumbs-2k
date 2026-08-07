#!/usr/bin/env python3
"""Regenerate tests/fixtures/sqlite/sample.db — the database the Quick preview viewer's
`dbdoc` reader is tested against.

Written by a REAL SQLite (Python's stdlib module), not hand-assembled, so the test exercises
the actual on-disk layout: a rowid table whose INTEGER PRIMARY KEY is stored as NULL in the
record, an empty table, a BLOB column, an index and a view. Deliberately tiny (one page size,
a few KB) so it is a reasonable thing to commit.

    python scripts/make-sqlite-fixture.py
"""

import os
import sqlite3
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "..", "tests", "fixtures", "sqlite", "sample.db")


def main() -> int:
    out = os.path.normpath(OUT)
    os.makedirs(os.path.dirname(out), exist_ok=True)
    if os.path.exists(out):
        os.remove(out)

    con = sqlite3.connect(out)
    con.executescript(
        """
        PRAGMA page_size = 1024;
        CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            email TEXT,
            score REAL,
            active INT,
            -- Exact-integer REAL values: SQLite stores these with an INTEGER serial type and
            -- relies on the column affinity to read them back as reals ("IntReal"), which the
            -- preview reader has to reproduce or every such column prints as an integer.
            weight REAL
        );
        CREATE TABLE notes (note_id INTEGER PRIMARY KEY, user INTEGER, body TEXT, attachment BLOB);
        CREATE TABLE empty_tbl (a TEXT, b INT, c BLOB);
        CREATE INDEX idx_users_name ON users(name);
        CREATE VIEW active_users AS SELECT id, name FROM users WHERE active = 1;
        """
    )
    for i in range(1, 61):
        con.execute(
            "INSERT INTO users (id, name, email, score, active, weight) VALUES (?,?,?,?,?,?)",
            (i, f"user{i}", f"u{i}@example.com", i + 0.5, i % 2, float(i * 10)),
        )
    # A value that is markdown syntax: the renderer must escape it, never render a live link.
    # Also the NULL case, in a REAL column, where affinity must not invent a value.
    con.execute(
        "INSERT INTO users (id, name, email, score, active, weight) VALUES (?,?,?,?,?,?)",
        (61, "[click](http://example.invalid)", None, None, 0, None),
    )
    for i in range(1, 5):
        con.execute(
            "INSERT INTO notes (note_id, user, body, attachment) VALUES (?,?,?,?)",
            (i, i, f"note {i}", bytes([i]) * (400 * i)),
        )
    con.commit()
    con.execute("VACUUM")  # smallest possible file, stable byte-for-byte
    con.close()

    print(f"{out}  {os.path.getsize(out)} bytes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
