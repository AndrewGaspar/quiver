import json, sys, re, os
base = sys.argv[1]
lib = json.load(open(os.path.join(base, 'library.json')))
out = []
for g in lib:
    pid = g['product_id']
    txt = open(os.path.join(base, 'configs', f'{pid}.yaml')).read()
    # crude: parse root-level art fields and default localizations
    fields = {}
    for k in ['background_image','thumb_image','logo_image','splash_image','icon_image','name','sort_string']:
        m = re.search(r'^  %s: (.+)$' % k, txt, re.M)
        if m: fields[k] = m.group(1).strip().strip("'\"")
    loc = {}
    m = re.search(r'^localizations:\n  default:\n((?:    .+\n)+)', txt, re.M)
    if m:
        for line in m.group(1).splitlines():
            mm = re.match(r'    ([A-Za-z0-9_]+): (.+)$', line)
            if mm: loc[mm.group(1)] = mm.group(2).strip().strip("'\"")
    res = {k: loc.get(v, v) for k, v in fields.items()}
    res['product_id'] = pid
    res['lib_name'] = g['name']
    res['dlc_count'] = len(g['associations'])
    res['cloud_saves'] = bool(re.search(r'cloud_saves: yes', txt))
    res['achievements'] = bool(re.search(r'achievements: ', txt))
    exes = re.findall(r'relative: (\S+\.exe)', txt)
    res['exes'] = sorted(set(exes))
    out.append(res)
json.dump(out, sys.stdout, indent=1, ensure_ascii=False)
