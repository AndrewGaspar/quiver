#!/bin/bash
# Downloads art referenced in art/art.json into art/raw/
set -u
cd "$(dirname "$0")/.."
python3 -I -c "
import json
for g in json.load(open('art/art.json')):
  for k in ['background_image','thumb_image','logo_image','splash_image']:
    v=g.get(k)
    if v and '.' in v: print(g['product_id'], k, v)
" | while read pid kind file; do
  out="art/raw/${pid}_${kind%_image}.${file##*.}"
  [ -s "$out" ] && continue
  curl -sf -o "$out" "https://ubistatic3-a.akamaihd.net/orbit/uplay_launcher_3_0/assets/$file" || echo "FAIL $pid $kind $file"
done
