#!/usr/bin/env bash
# Generate THIRD_PARTY_LICENSES.txt from Rust and npm production dependencies.
set -euo pipefail

OUT="THIRD_PARTY_LICENSES.txt"

echo "Generating Rust dependency licenses..."
cargo about generate about.hbs -o "$OUT"

echo "Appending npm production dependency licenses..."
cd web
npx --yes license-checker@25 --production --json 2>/dev/null | python -c "
import json, sys
data = json.load(sys.stdin)
for name, info in sorted(data.items()):
    if name.startswith('web@'):
        continue
    lic = info.get('licenses', 'UNKNOWN')
    repo = info.get('repository', '')
    publisher = info.get('publisher', '')
    print('-' * 70)
    print(name)
    print(lic)
    if repo:
        print(repo)
    if publisher:
        print(f'Copyright {publisher}')
" >> "../$OUT"
cd ..

lines=$(wc -l < "$OUT")
echo "Done. $OUT generated ($lines lines)."
