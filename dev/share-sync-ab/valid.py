"""Prints which runs are void: load above 6 at start or end, a sleep between start and end, or no meta."""
import glob,subprocess,sys,datetime,re
log=subprocess.run(['pmset','-g','log'],capture_output=True,text=True).stdout
sleeps=[]
for l in log.splitlines():
    m=re.match(r'(\d{4}-\d\d-\d\d \d\d:\d\d:\d\d) [+-]\d{4} +(Sleep|Wake|DarkWake) ',l)
    if m: sleeps.append(datetime.datetime.strptime(m[1],'%Y-%m-%d %H:%M:%S').timestamp())
def void(json_path):
    try: lines=dict((l.split()[0],l.split()[1:]) for l in open(json_path[:-5]+'.meta'))
    except FileNotFoundError: return 'no meta'
    if 'end' not in lines: return 'unfinished'
    s,ls=float(lines['start'][0]),float(lines['start'][1]); e,le=float(lines['end'][0]),float(lines['end'][1])
    if ls>6 or le>6: return f'load {ls}/{le}'
    if any(s-5<=t<=e+5 for t in sleeps): return 'sleep'
    return None
if __name__=='__main__':
    for p in sys.argv[1:]:
        for f in sorted(glob.glob(p)):
            v=void(f)
            if v: print(f,v)
