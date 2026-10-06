#!/usr/bin/env python3
"""Read-only proof that the synthetic project memory has a valid local 384-D embedding."""

import argparse
import hashlib
import json
import math
import sqlite3
import struct
import sys
from pathlib import Path

DIMENSIONS = 384
EXPECTED_BYTES = DIMENSIONS * 4


def project_user_id(cwd: str) -> str:
    normalized = cwd.replace("\\", "/").rstrip("/").lower()
    return f"proj:{normalized}"


def inspect_embedding(db_path: Path, text: str, cwd: str, category: str) -> dict:
    uri = db_path.resolve().as_uri() + "?mode=ro"
    try:
        connection = sqlite3.connect(uri, uri=True, timeout=5)
    except sqlite3.Error as exc:
        raise RuntimeError(f"could not open the memory database read-only: {exc}") from exc
    try:
        rows = connection.execute(
            "SELECT user_id, scope, memory, category, embedding FROM memories "
            "WHERE user_id = ? AND scope = 'project' AND memory = ? AND category = ?",
            (project_user_id(cwd), text, category),
        ).fetchall()
    except sqlite3.Error as exc:
        raise RuntimeError(f"could not query the expected memory row: {exc}") from exc
    finally:
        connection.close()

    if len(rows) != 1:
        raise RuntimeError(f"expected exactly one matching project memory row; found {len(rows)}")
    user_id, scope, _memory, stored_category, blob = rows[0]
    if not isinstance(blob, (bytes, bytearray)):
        raise RuntimeError("embedding column is not a byte buffer")
    blob = bytes(blob)
    if len(blob) != EXPECTED_BYTES:
        raise RuntimeError(f"embedding has {len(blob)} bytes; expected {EXPECTED_BYTES} bytes for {DIMENSIONS} float32 values")

    values = struct.unpack("<384f", blob)
    finite = all(math.isfinite(value) for value in values)
    if not finite:
        raise RuntimeError("embedding contains a non-finite float")
    l2_norm = math.sqrt(sum(value * value for value in values))
    if abs(l2_norm - 1.0) > 0.0001:
        raise RuntimeError(f"embedding is not L2-normalized (norm={l2_norm:.9f})")

    return {
        "database": str(db_path),
        "scope": scope,
        "user_id": user_id,
        "expected_user_id": project_user_id(cwd),
        "category": stored_category,
        "dimensions": DIMENSIONS,
        "embedding_bytes": len(blob),
        "finite": finite,
        "l2_norm": l2_norm,
        "embedding_sha256": hashlib.sha256(blob).hexdigest(),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", required=True, type=Path)
    parser.add_argument("--text", required=True)
    parser.add_argument("--cwd", required=True)
    parser.add_argument("--category", required=True)
    args = parser.parse_args()
    try:
        proof = inspect_embedding(args.db, args.text, args.cwd, args.category)
    except (OSError, RuntimeError) as exc:
        print(f"embedding verification failed: {exc}", file=sys.stderr)
        return 1
    print(json.dumps(proof, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
