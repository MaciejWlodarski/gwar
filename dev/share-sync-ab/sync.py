"""usage: sync.py <dir> — per arm and viewer: bursts in range, first 12 s and settled (after 12 s
of the viewer's record), and after sound came back (soundBack) when the run has one."""
import json,glob,sys,os,statistics as st
sys.path.insert(0,os.path.dirname(os.path.abspath(__file__))); from valid import void
d=sys.argv[1]; R=lambda o:-45<=o<=125
agg={}
for f in sorted(glob.glob(d+'/*.json')):
    arm=os.path.basename(f).rsplit('-',1)[0]
    v=void(f)
    if v: print('VOID',os.path.basename(f),v); continue
    rep=json.load(open(f))
    for viewer,r in rep['viewers'].items():
        h=r.get('health') or []
        start=h[0]['at'] if h else (r['pairs'][0]['at'] if r['pairs'] else 0)
        a=agg.setdefault((arm,viewer),{'first':[0,0],'settled':[0,0],'back':[0,0],'off':[],'runs':0})
        a['runs']+=1
        for p in r['pairs']:
            k='first' if p['at']<start+12000 else 'settled'
            a[k][0]+=R(p['offsetMs']); a[k][1]+=1
            if k=='settled': a['off'].append(p['offsetMs'])
            if rep.get('soundBack') and p['at']>=rep['soundBack']:
                a['back'][0]+=R(p['offsetMs']); a['back'][1]+=1
for (arm,viewer),a in sorted(agg.items(),key=lambda x:(x[0][1],x[0][0])):
    o=sorted(a['off']); q=lambda x:o[int(x*(len(o)-1))] if o else 0
    back=f" back {a['back'][0]}/{a['back'][1]}" if a['back'][1] else ''
    print(f"{viewer:8} {arm:9} runs {a['runs']:2}  first12 {a['first'][0]}/{a['first'][1]}  settled {a['settled'][0]}/{a['settled'][1]} med {q(.5):+.0f} ({q(.05):+.0f}…{q(.95):+.0f}){back}")
