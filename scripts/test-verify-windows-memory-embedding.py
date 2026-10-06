import json
import os
import sqlite3
import struct
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("verify-windows-memory-embedding.py")
MEMORY = "dotz Windows installer acceptance synthetic memory"
CATEGORY = "windows-installer-acceptance"
CWD = r"C:\Users\runneradmin\AppData\Local\Temp\dotz-native-test-project"
USER_ID = "proj:" + CWD.replace("\\", "/").rstrip("/").lower()


def make_db(path, *, user_id=USER_ID, scope="project", embedding=None, text=MEMORY):
    if embedding is None:
        values = [0.0] * 384
        values[0] = 1.0
        embedding = struct.pack("<384f", *values)
    conn = sqlite3.connect(path)
    conn.execute("CREATE TABLE memories (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, scope TEXT NOT NULL, memory TEXT NOT NULL, category TEXT, folder TEXT, embedding BLOB NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER)")
    conn.execute("INSERT INTO memories VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)", ("fixture", user_id, scope, text, CATEGORY, None, embedding, 1, None))
    conn.commit()
    conn.close()


class MemoryEmbeddingProbeTests(unittest.TestCase):
    def run_probe(self, db_path):
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--db", str(db_path), "--text", MEMORY,
             "--cwd", CWD, "--category", CATEGORY],
            capture_output=True, text=True, check=False,
        )

    def test_reports_exact_normalized_project_embedding(self):
        with tempfile.TemporaryDirectory(prefix="dotz-memory-probe-", dir=os.environ.get("TMPDIR")) as tmp:
            db = Path(tmp) / "memory.db"
            make_db(db)
            result = self.run_probe(db)
            self.assertEqual(result.returncode, 0, result.stderr)
            proof = json.loads(result.stdout)
            self.assertEqual(proof["dimensions"], 384)
            self.assertEqual(proof["embedding_bytes"], 1536)
            self.assertEqual(proof["scope"], "project")
            self.assertEqual(proof["user_id"], USER_ID)
            self.assertTrue(proof["finite"])
            self.assertAlmostEqual(proof["l2_norm"], 1.0, places=6)
            self.assertEqual(len(proof["embedding_sha256"]), 64)

    def test_rejects_missing_synthetic_project_memory(self):
        with tempfile.TemporaryDirectory(prefix="dotz-memory-probe-", dir=os.environ.get("TMPDIR")) as tmp:
            db = Path(tmp) / "memory.db"
            make_db(db, text="different memory")
            result = self.run_probe(db)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("exactly one", result.stderr.lower())

    def test_rejects_wrong_project_scope(self):
        with tempfile.TemporaryDirectory(prefix="dotz-memory-probe-", dir=os.environ.get("TMPDIR")) as tmp:
            db = Path(tmp) / "memory.db"
            make_db(db, user_id="proj:c:/wrong", scope="project")
            result = self.run_probe(db)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("exactly one", result.stderr.lower())

    def test_rejects_malformed_embedding_length(self):
        with tempfile.TemporaryDirectory(prefix="dotz-memory-probe-", dir=os.environ.get("TMPDIR")) as tmp:
            db = Path(tmp) / "memory.db"
            make_db(db, embedding=b"short")
            result = self.run_probe(db)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("1536", result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
