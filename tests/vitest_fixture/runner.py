#!/usr/bin/env python3
import json, pathlib, re, sys
root=pathlib.Path.cwd()
args=sys.argv[1:]
if args == ['--version']:
    print('vitest/3.2.4 fixture'); sys.exit(0)
filters=[]
i=1
while i<len(args):
    if args[i]=='--outputFile': i+=2; continue
    if not args[i].startswith('--'): filters.append(args[i])
    i+=1
rows=[]
for path in sorted(root.rglob('*.test.js')):
    if filters and not any(f in path.relative_to(root).as_posix() for f in filters): continue
    for name in re.findall(r"test\('([^']+)'",path.read_text()):
        rows.append({'file':str(path),'name':name,'projectName':''})
if args[0]=='list':
    if (root/'.git/bad-list').exists(): print('{}')
    else: print(json.dumps(rows))
    sys.exit(0)
with (root/'.git/calls.jsonl').open('a') as f: f.write(json.dumps(args)+'\n')
red=any(t['name']=='RED' for t in rows)
if '--outputFile' in args and not (root/'.git/no-report').exists():
    suites=[]
    for file in sorted({t['file'] for t in rows}):
        suites.append({'name':file,'status':'failed' if red else 'passed',
          'assertionResults':[{'fullName':t['name'],'status':'failed' if t['name']=='RED' else 'passed'} for t in rows if t['file']==file]})
    report={'success':not red,'numFailedTests':int(red),'numFailedTestSuites':int(red),'numTotalTests':len(rows),'testResults':suites}
    if (root/'.git/mismatch').exists(): report['testResults']=[]
    pathlib.Path(args[args.index('--outputFile')+1]).write_text(json.dumps(report))
sys.exit(1 if red else 0)
