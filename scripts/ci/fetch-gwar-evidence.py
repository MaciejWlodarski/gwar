import subprocess,sys
# Keep credentials out of argv, logs and files; curl uses the Mac's system resolver.
token=subprocess.check_output(['gh','auth','token'],text=True).strip()
url='https://api.github.com/repos/MaciejWlodarski/gwar/'+sys.argv[1]
subprocess.run(['curl','--fail','--location','--silent','--show-error','--retry','2','--connect-timeout','10','--max-time','90','--config','-','--output',sys.argv[2],url],input='header = "Authorization: Bearer '+token+'"\n',text=True,check=True)
