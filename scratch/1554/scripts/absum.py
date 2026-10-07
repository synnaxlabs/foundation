import sys, collections
runs = sys.argv[1:]
D = collections.OrderedDict()
for i, p in enumerate(runs):
    for line in open(p):
        f = line.strip().split('|')
        if f[0] == 'name': continue
        D.setdefault(f[0], {}).setdefault(f[1], []).append((float(f[3]), float(f[5]), float(f[7]), float(f[8]), float(f[9])))
def pct(x): return f'{(x-1)*100:+5.1f}'
print(f'{"bench":34} {"base min":>9} {"head min":>9} {"dmin":>6} | head pair per run      | base2 pair per run     | rnd1 pair per run')
for name, v in D.items():
    first = 'base' if 'base' in v else 'head'
    bm = min(x[0] for x in v[first])
    row = f'{name:34} {bm:9.3f}'
    if 'base' in v:
        hm = min(x[0] for x in v['head'])
        row += f' {hm:9.3f} {pct(hm/bm)}%'
        for lab in ['head', 'base2', 'rnd1']:
            row += ' | ' + ' '.join(pct(x[2]) for x in v[lab])
    else:
        rm = min(x[0] for x in v['rnd1'])
        row += f' (head)  rnd1 min {rm:9.3f} {pct(rm/bm)}% | rnd1 pair ' + ' '.join(pct(x[2]) for x in v['rnd1'])
    print(row)
