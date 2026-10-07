import re, sys, glob, statistics as st
U={'ns':1,'µs':1e3,'ms':1e6,'s':1e9,'ps':1e-3}
def parse(path):
    out={}; stack=[]
    for line in open(path, encoding='utf-8'):
        line=line.rstrip('\n')
        if line.startswith('Timer'): continue
        m=re.match(r'^([│ ├╰─]*)(\S+)\s*(.*)$', line)
        if not m: continue
        depth=len(m.group(1))//3; name=m.group(2); rest=m.group(3)
        stack=stack[:depth]+[name]
        if 'fastest' in rest: continue
        vals=re.findall(r'([\d.]+) (ns|µs|ms|s|ps)\b', rest)
        if len(vals)>=4:
            out['/'.join(stack[1:])]=[float(v)*U[u] for v,u in vals[:4]]
    return out
d=sys.argv[1]; labels=sys.argv[2:]
data={}
for l in labels:
    runs=[parse(p) for p in sorted(glob.glob(f'{d}/log_{l}_*.txt'))]
    data[l]=(runs,len(runs))
keys=[]
for l in labels:
    for r in data[l][0]:
        for k in r:
            if k not in keys: keys.append(k)
print('rounds:', {l:data[l][1] for l in labels})
base=labels[0]
def stat(l,k):
    f=[r[k][0] for r in data[l][0] if k in r]; m=[r[k][2] for r in data[l][0] if k in r]
    if not f: return None
    return min(f), st.median(m), (max(m)/min(m)-1)*100
print('min = least "fastest" of the rounds; med = median of the round medians; ns')
hdr=f'{"bench":40}'
for l in labels: hdr+=f' | {l+" min":>11} {l+" med":>11}'+('' if l==base else f' {"dmin":>7} {"dmed":>7}')
print(hdr)
for k in keys:
    b=stat(base,k); row=f'{k:40}'
    for l in labels:
        s=stat(l,k)
        if s is None: row+=f' | {"-":>11} {"-":>11}'+('' if l==base else f' {"":>7} {"":>7}'); continue
        row+=f' | {s[0]:11.2f} {s[1]:11.2f}'
        if l!=base:
            row+= f' {(s[0]/b[0]-1)*100:+6.1f}% {(s[1]/b[1]-1)*100:+6.1f}%' if b else f' {"":>7} {"":>7}'
    print(row)
