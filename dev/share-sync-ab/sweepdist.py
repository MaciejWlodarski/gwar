"""usage: sweepdist.py <sweep>... — per metric: sum, p50/p90/p99/max of the values themselves, minus main's."""
import re,sys
def q(xs,p): xs=sorted(xs); return xs[min(len(xs)-1,int(p*len(xs)))]
print(f"{'run':8} {'metric':6} {'sum':>7} {'p50':>5} {'p90':>5} {'p95':>5} {'p99':>5} {'max':>5}   (value minus main's)")
for f in sys.argv[1:]:
    d=[tuple(map(int,re.search(r'above (\d+) was (\d+) hurt (\d+) was (\d+)',l).groups())) for l in open(f)]
    name=f.split('/')[-1][:-4]
    for m,(i,j) in (('above',(0,1)),('voice',(2,3))):
        now=[r[i] for r in d]; was=[r[j] for r in d]
        print(f"{name:8} {m:6} {sum(now)-sum(was):7} "+" ".join(f"{q(now,p)-q(was,p):5}" for p in (.5,.9,.95,.99,1)))
