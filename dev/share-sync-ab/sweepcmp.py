"""usage: sweepcmp.py <sweep.txt>... — per file and metric: sum, mean, p90, p99, worst of per-case deltas vs main."""
import re,sys
def q(xs,p): xs=sorted(xs); return xs[min(len(xs)-1,int(p*len(xs)))]
print(f"{'run':10} {'n':>5} | {'metric':6} {'sum':>7} {'mean':>6} {'p90':>5} {'p99':>5} {'worst':>6} {'best':>6} {'changed':>7}")
for f in sys.argv[1:]:
    d=[tuple(map(int,re.search(r'above (\d+) was (\d+) hurt (\d+) was (\d+)',l).groups())) for l in open(f)]
    name=f.split('sweep-')[-1][:-4]
    for m,(i,j) in (('above',(0,1)),('voice',(2,3))):
        x=[r[i]-r[j] for r in d]
        print(f"{name:10} {len(d):5} | {m:6} {sum(x):7} {sum(x)/len(x):6.1f} {q(x,.9):5} {q(x,.99):5} {max(x):6} {min(x):6} {sum(1 for v in x if v):7}")
