#!/usr/bin/env bash
set -euo pipefail

git init
git add .
git commit -m "Initial extraction of turnkey-mcp"

cat <<'MSG'

Next:
  git remote add origin git@github.com:YOURNAME/turnkey-mcp.git
  git push -u origin main
MSG
