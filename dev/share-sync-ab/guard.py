"""Kills the e2e run if any process it started opens an audio output (pmset audio-out assertion).
Assertions present before the run, and those of processes outside its tree, are ignored.
usage: guard.py <node pid>"""
import os,re,subprocess,sys,time,signal
target=int(sys.argv[1])
def blocks():
    out=subprocess.run(['pmset','-g','assertions'],capture_output=True,text=True).stdout
    found=[]
    for b in re.split(r'\n(?=\s+pid \d+\()',out):
        if 'audio-out' not in b: continue
        i=re.search(r'\[(0x[0-9a-f]+)\]',b); p=re.search(r'Created for PID: (\d+)',b)
        if i: found.append((i[1],int(p[1]) if p else None,' '.join(b.split())))
    return found
def tree(root):
    ps=subprocess.run(['ps','-Ao','pid=,ppid='],capture_output=True,text=True).stdout.split('\n')
    kids={}
    for l in ps:
        f=l.split()
        if len(f)==2: kids.setdefault(int(f[1]),[]).append(int(f[0]))
    seen,stack={root},[root]
    while stack:
        for c in kids.get(stack.pop(),[]):
            if c not in seen: seen.add(c); stack.append(c)
    return seen
before={i for i,_,_ in blocks()}
while True:
    try: os.kill(target,0)
    except OSError: sys.exit(0)
    mine=tree(target)
    for i,pid,line in blocks():
        if i in before or pid not in mine: continue
        print(time.strftime('%H:%M:%S'),'AUDIO OUT from run pid',pid,':',line[:300],flush=True)
        for p in sorted(mine,reverse=True):
            try: os.kill(p,signal.SIGKILL)
            except OSError: pass
        sys.exit(1)
    time.sleep(0.25)
