import re, sys, glob, statistics as st, os
U = {'ns': 1, 'µs': 1e3, 'ms': 1e6, 's': 1e9, 'ps': 1e-3}
def parse(path):
    out = {}; stack = []
    for line in open(path, encoding='utf-8'):
        m = re.match(r'^([│ ├╰─]*)(\S+)\s*(.*)$', line.rstrip('\n'))
        if line.startswith('Timer'): continue
        if 'fastest' in line: stack = ['log']; continue
        if not m: continue
        depth = len(m.group(1)) // 3; name = m.group(2); rest = m.group(3)
        stack = stack[:depth] + [name]
        vals = re.findall(r'([\d.]+) (ns|µs|ms|s|ps)\b', rest)
        if len(vals) >= 4:
            key = '/'.join(s for s in stack[1:] if s != 'shim')
            out[key] = [float(v) * U[u] for v, u in vals[:4]]
    return out
d = sys.argv[1]; labels = sys.argv[2:]
data = {}
for l in labels:
    data[l] = {}
    for p in sorted(glob.glob(f'{d}/log_{l}_*.txt')):
        r = os.path.basename(p)[len(f'log_{l}_'):-4]
        try: data[l][r] = parse(p)
        except Exception as e: print('bad', p, e)
rounds = sorted(set.intersection(*[set(data[l]) for l in labels]))
rounds = [r for r in rounds if all(len(data[l][r]) >= 20 for l in labels)]
print('complete rounds:', len(rounds))
keys = []
for l in labels:
    for r in rounds:
        for k in data[l][r]:
            if k not in keys: keys.append(k)
base = labels[0]
def fmt(v):
    return f'{v:.2f}' if v < 1000 else (f'{v:.0f}' if v < 1e5 else f'{v/1e6:.3f}ms')
print('min = least "fastest" over the rounds; pair = median over rounds of (round median of label / round median of base)')
h = f'{"bench":36} {base+" min":>10}'
for l in labels[1:]: h += f' | {l+" min":>10} {"dmin":>6} {"pair":>6} {"q1":>6} {"q3":>6}'
print(h)
for k in keys:
    if not all(k in data[base][r] for r in rounds): bm = None
    else: bm = min(data[base][r][k][0] for r in rounds)
    row = f'{k:36} {fmt(bm) if bm else "-":>10}'
    for l in labels[1:]:
        if not all(k in data[l][r] for r in rounds): row += f' | {"-":>10} {"":>6} {"":>6} {"":>6} {"":>6}'; continue
        lm = min(data[l][r][k][0] for r in rounds)
        if bm is None: row += f' | {fmt(lm):>10} {"":>6} {"":>6} {"":>6} {"":>6}'; continue
        ratios = sorted(data[l][r][k][2] / data[base][r][k][2] for r in rounds)
        q = st.quantiles(ratios, n=4) if len(ratios) >= 4 else [ratios[0], st.median(ratios), ratios[-1]]
        row += f' | {fmt(lm):>10} {(lm/bm-1)*100:+5.1f}% {(q[1]-1)*100:+5.1f}% {(q[0]-1)*100:+5.1f}% {(q[2]-1)*100:+5.1f}%'
    print(row)
# e2e
E = {}
for l in labels:
    E[l] = {}
    for p in sorted(glob.glob(f'{d}/e2e_{l}_*.txt')):
        t = open(p).read()
        g = {}
        m = re.search(r'commit: [\d.]+ ns per commit of \d+ entries, ([\d.]+) ns per entry', t)
        if m: g['commit per entry'] = (float(m.group(1)), float(m.group(1)))
        for name in ['read_whole_path', 'read_at_tail', 'control_pool']:
            m = re.search(name + r':.*?min ([\d.]+), median\s+([\d.]+)', t, re.S)
            if m: g[name] = (float(m.group(1)), float(m.group(2)))
        if len(g) == 4: E[l][os.path.basename(p)[len(f'e2e_{l}_'):-4]] = g
el = [l for l in labels if E[l]]
if el:
    runs = sorted(set.intersection(*[set(E[l]) for l in el]))
    print('\ne2e complete runs:', len(runs))
    h = f'{"bench":20} {base+" min":>10}'
    for l in el[1:]: h += f' | {l+" min":>10} {"dmin":>6} {"pair":>6} {"q1":>6} {"q3":>6}'
    print(h)
    for k in ['commit per entry', 'read_whole_path', 'read_at_tail', 'control_pool']:
        bm = min(E[base][r][k][0] for r in runs); row = f'{k:20} {bm:10.2f}'
        for l in el[1:]:
            lm = min(E[l][r][k][0] for r in runs)
            ratios = sorted(E[l][r][k][1] / E[base][r][k][1] for r in runs)
            q = st.quantiles(ratios, n=4)
            row += f' | {lm:10.2f} {(lm/bm-1)*100:+5.1f}% {(q[1]-1)*100:+5.1f}% {(q[0]-1)*100:+5.1f}% {(q[2]-1)*100:+5.1f}%'
        print(row)
