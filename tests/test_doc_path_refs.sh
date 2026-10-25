#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
"${PYTHON:-python3}" scripts/check_doc_path_refs.py
"${PYTHON:-python3}" -m unittest tests.test_doc_path_refs
