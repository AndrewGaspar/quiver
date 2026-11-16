#!/usr/bin/env python3
"""Assemble the single-file mock-up page from src/ + art/web + PROPOSAL.md.

usage: python3 -I design/mockup/build.py [path/to/lucide.min.js]
The optional lucide bundle is only used to check that every icon name exists.
"""
import base64, json, os, re, sys

HERE = os.path.dirname(os.path.abspath(__file__))
DESIGN = os.path.dirname(HERE)
SRC = os.path.join(HERE, 'src')
WEB = os.path.join(DESIGN, 'art', 'web')


def read(p):
    with open(p, encoding='utf-8') as f:
        return f.read()


def data_uri(path):
    ext = path.rsplit('.', 1)[1]
    mime = {'jpg': 'image/jpeg', 'webp': 'image/webp', 'png': 'image/png'}[ext]
    with open(path, 'rb') as f:
        return f'data:{mime};base64,' + base64.b64encode(f.read()).decode()


meta = json.load(open(os.path.join(DESIGN, 'art', 'art.json'), encoding='utf-8'))
games, art = [], {}
for g in meta:
    pid = g['product_id']
    games.append({'id': pid, 'name': g['lib_name'], 'sort': g.get('sort_string', ''), 'dlc_count': g['dlc_count'],
                  'cloud': g['cloud_saves'], 'ach': g['achievements']})
    a = {}
    for kind, fn in [('thumb', f'{pid}_thumb.jpg'), ('hero', f'{pid}_hero.webp'), ('logo', f'{pid}_logo.webp'), ('amb', f'{pid}_amb.jpg')]:
        p = os.path.join(WEB, fn)
        if os.path.exists(p):
            a[kind] = data_uri(p)
    art[pid] = a

js = '\n'.join(read(os.path.join(SRC, f)) for f in ['app1.js', 'app2.js', 'app3.js', 'theme.js', 'page.js'])
data = ('const GAMES = ' + json.dumps(games, ensure_ascii=False) + ';\n'
        'const ART = ' + json.dumps(art) + ';\n'
        'const THEMES_TOML = ' + json.dumps({fn[:-5]: read(os.path.join(DESIGN, 'data', 'themes', fn)) for fn in sorted(os.listdir(os.path.join(DESIGN, 'data', 'themes'))) if fn.endswith('.toml')}, ensure_ascii=False) + ';\n'
        'const PROPOSAL_MD = ' + json.dumps(read(os.path.join(DESIGN, 'PROPOSAL.md')), ensure_ascii=False) + ';\n')

page = read(os.path.join(SRC, 'page.html'))
page = page.replace('/*__STYLES__*/', read(os.path.join(SRC, 'styles.css')))
page = page.replace('/*__DATA__*/', data.replace('</', '<\\/'))
page = page.replace('/*__APP__*/', js)

out = os.path.join(HERE, 'ubi-launcher.html')
with open(out, 'w', encoding='utf-8') as f:
    f.write(page)
print(f'wrote {out}: {os.path.getsize(out) / 1e6:.2f} MB')

if len(sys.argv) > 1:
    bundle = read(sys.argv[1])
    names = sorted(set(re.findall(r"ic\('([a-z0-9-]+)'", js)) | set(re.findall(r'ic\("([a-z0-9-]+)"', js)))
    missing = []
    for n in names:
        pascal = ''.join(p[:1].upper() + p[1:] for p in n.split('-'))
        if not re.search(r'\b' + re.escape(pascal) + r'\b', bundle):
            missing.append(n)
    print('icons:', len(names), 'missing:', missing)
