"""usage: cisync.py <dir> — like sync.py for CI artefacts: arm = name before -s<shard>; per-run max load."""
import json,glob,sys,os,re
d=sys.argv[1]; R=lambda o:-45<=o<=125
agg={}; loads=[]
for f in sorted(glob.glob(d+'/*/*.json')):
    arm=re.sub(r'-s\d+-r\d+\.json$','',os.path.basename(f))
    try: rep=json.load(open(f))
    except Exception as e: print('BAD',f,e); continue
    lf=f[:-5]+'.load'
    if os.path.exists(lf):
        nums=[float(x) for x in re.findall(r'load averages?: ([\d.]+)',open(lf).read())]
        if nums: loads.append(max(nums))
    for viewer,r in rep.get('viewers',{}).items():
        h=r.get('health') or []
        start=h[0]['at'] if h else (r['pairs'][0]['at'] if r['pairs'] else 0)
        a=agg.setdefault((arm,viewer),{'first':[0,0],'settled':[0,0],'back':[0,0],'off':[],'runs':0,'per':[]})
        a['runs']+=1; s=[0,0]
        for p in r['pairs']:
            k='first' if p['at']<start+12000 else 'settled'
            a[k][0]+=R(p['offsetMs']); a[k][1]+=1
            if k=='settled': a['off'].append(p['offsetMs']); s[0]+=R(p['offsetMs']); s[1]+=1
            if rep.get('soundBack') and p['at']>=rep['soundBack']:
                a['back'][0]+=R(p['offsetMs']); a['back'][1]+=1
        a['per'].append(s[1]-s[0])
if loads: print(f"load max per run: median {sorted(loads)[len(loads)//2]:.1f} max {max(loads):.1f} n {len(loads)}")
for (arm,viewer),a in sorted(agg.items(),key=lambda x:(x[0][1],x[0][0])):
    o=sorted(a['off']); q=lambda x:o[int(x*(len(o)-1))] if o else 0
    back=f" back {a['back'][0]}/{a['back'][1]}" if a['back'][1] else ''
    print(f"{viewer:8} {arm:9} runs {a['runs']:2}  first12 {a['first'][0]}/{a['first'][1]}  settled {a['settled'][0]}/{a['settled'][1]} med {q(.5):+.0f} ({q(.05):+.0f}…{q(.95):+.0f}) miss/run {sorted(a['per'])}{back}")
