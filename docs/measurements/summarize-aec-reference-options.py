"""Rebuild the reference-option CSVs from the committed raw measurements.

Run with --speech --echo --right. Energy is summed in linear FS^2 seconds;
threshold comparisons reproduce f32 dB subtraction. Historical controls must
match every original window exactly. This script does not build or play audio.
"""
import csv,json,math,re,struct
from pathlib import Path
OUT=Path(__file__).resolve().parent
OLD=list(csv.DictReader((OUT/'aec-paired-echo-cases.csv').open()))
BASE={r['case_window']:r for r in OLD if r['profile']=='1'}
P7={r['case_window']:r for r in OLD if r['profile']=='7'}

def speech(path,mapping):
 s=Path(path).read_text();controls={};out=[]
 for g in re.findall(r'speech-matrix-control,lead=(\d+),echo=(false|true),noise=(\d+),cold-hold=(\d+),cold-kept=([^,]+),no-aec-hold=(\d+),no-aec-kept=([^\n]+)',s):
  controls[g[:3]]=g[3:]
 for g in re.findall(r'speech-matrix,lead=(\d+),echo=(false|true),noise=(\d+),mode=(\d+),hold=(\d+),kept=([^,]+),pass=(false|true)',s):
  lead,echo,noise,mode,hold,kept,passed=g
  c=controls[lead,echo,noise]
  out.append(dict(arm=mapping[int(mode)],lead_s=int(lead),speakers=echo,noise_percent=int(noise),cold_hold=int(c[0]),cold_kept=float(c[1]),held=int(hold),kept=float(kept),passes=passed=='true'))
 return out

def echo(path,old_check=False):
 rows=[]
 for line in Path(path).read_text().splitlines():
  if 'native-option-echo\t' not in line:continue
  arm,case,n,db,energy=line.split('native-option-echo\t',1)[1].split('\t')
  rows.append(dict(arm=arm,case=case,samples=int(n),dbfs=float(db),energy_fs2_s=float(energy)))
 base={r['case']:r for r in rows if r['arm']=='left-warm-p0'}
 assert len(base)==(549 if old_check else 108),len(base)
 for r in rows:
  r['baseline_dbfs']=base[r['case']]['dbfs'];r['delta_db']=struct.unpack('f',struct.pack('f',r['dbfs']-r['baseline_dbfs']))[0]
  if old_check and r['arm'] in ['left-warm-p0','left-cold-p0','left-warm-p7']:
   expected=(P7[r['case']]['candidate_warm_dbfs'] if r['arm']=='left-warm-p7' else BASE[r['case']]['current_warm_dbfs' if r['arm']=='left-warm-p0' else 'plain_cold_dbfs'])
   assert r['dbfs']==float(expected),(r['arm'],r['case'],r['dbfs'],expected)
 return rows

def write(name,rows):
 with (OUT/name).open('w',newline='') as f:
  w=csv.DictWriter(f,fieldnames=list(rows[0]),lineterminator='\n');w.writeheader();w.writerows(rows)

def aggregate(rows):
 base=sum(r['energy_fs2_s'] for r in rows if r['arm']=='left-warm-p0')
 out=[]
 for arm in dict.fromkeys(r['arm'] for r in rows):
  v=[r for r in rows if r['arm']==arm];energy=sum(r['energy_fs2_s'] for r in v)
  worst=max(v,key=lambda r:r['delta_db'])
  out.append(dict(arm=arm,windows=len(v),over_1db=sum(r['delta_db']>1 for r in v),over_3db=sum(r['delta_db']>3 for r in v),over_6db=sum(r['delta_db']>6 for r in v),worst_db=worst['delta_db'],energy_fs2_s=energy,baseline_energy_fs2_s=base,total_energy_delta_db=10*math.log10(energy/base),total_energy_delta_percent=100*(energy/base-1),worst_case=worst['case']))
 return out

if __name__=='__main__':
 import argparse
 p=argparse.ArgumentParser();p.add_argument('--speech',action='store_true');p.add_argument('--echo',action='store_true');p.add_argument('--right',action='store_true');a=p.parse_args()
 if a.speech:
  rows=speech(OUT/'aec-profile7-full-speech.txt',{90:'left-warm-p7'})+speech(OUT/'aec-two-reference-speech.txt',{100:'two-warm-p7',101:'two-cold-p7',102:'two-warm-p1',103:'two-cold-p1'})
  assert len(rows)==60,len(rows)
  write('aec-reference-speech.csv',rows)
  for arm in dict.fromkeys(r['arm'] for r in rows):
   v=[r for r in rows if r['arm']==arm];print(arm,'speech',sum(r['passes'] for r in v),'/12')
   for r in v:
    if r['speakers']=='true' and r['noise_percent']==100:print(r)
 if a.echo:
  rows=echo(OUT/'aec-reference-full-echo.txt',True);assert len(rows)==7*549
  write('aec-reference-echo.csv',rows);summary=aggregate(rows);write('aec-reference-echo-summary.csv',summary)
  print(json.dumps(summary,indent=2))
 if a.right:
  rows=echo(OUT/'aec-reference-right-echo.txt');assert len(rows)==7*108
  write('aec-reference-right-echo.csv',rows);summary=aggregate(rows);write('aec-reference-right-summary.csv',summary)
  print(json.dumps(summary,indent=2))
