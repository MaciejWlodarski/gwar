"""The CI subset of a sweep file (seed 1, the 8 drops of drop_matrix_matches_main_baseline), as SWEEP lines."""
import re,sys
want={(20000000,5000000,'false','0.0','0ns',0),(20000000,5000000,'true','0.01','0ns',0),(20000000,5000000,'false','0.01','10ms',1000000),
(6000000,2000000,'false','0.0','0ns',0),(6000000,2000000,'false','0.0','5ms',1000000),(6000000,2000000,'false','0.01','5ms',1000000),
(20000000,4000000,'false','0.01','5ms',0),(6000000,2000000,'false','0.0','2ms',1000000)}
for l in open(sys.argv[1]):
    m=re.search(r'SWEEP s(\d+) c\d+ LinkDrop \{ from: (\d+), to: (\d+), ramp: (\w+), loss: ([\d.]+), aggregate: (\w+), cross: (\d+) \}',l)
    s,f,t,r,lo,ag,cr=m.groups()
    if s=='1' and (int(f),int(t),r,lo,ag,int(cr)) in want: print(l,end='')
