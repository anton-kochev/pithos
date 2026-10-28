# Inserted into managed_pi's fake Docker dispatch for real BrokerRuntime tests.
elif a[:2]==['image','inspect'] and a[-1]=='sha256:'+'c'*64:
    emit({'id':a[-1], 'user':__USER__, 'volumes':None,
          'env':['HOME=/tmp/browser-home','USER=browser','LOGNAME=browser'],
          'labels':{'io.pithos.broker.browser-fingerprint':__BROWSER_HASH__}})
elif a[0]=='run' and 'sha256:'+'c'*64 in a:
    manifest=json.loads((root/'run/resources.json').read_text())
    journal=json.loads((root/'run/journal.json').read_text())
    r=next(r for r in manifest['resources'] if r['name']==opt('--name'))
    assert next(j for j in journal['records'] if j['request_id']==r['request_id'])['state']=='running'
    assert r['observed_id'] is None and not r['local_reaped']
    def opts(k): return [a[i+1] for i,v in enumerate(a) if v==k]
    labels=dict(v.split('=',1) for v in opts('--label'))
    security=[]
    for v in opts('--security-opt'):
        security.append('seccomp='+pathlib.Path(v[8:]).read_text() if v.startswith('seccomp=') else v)
    if '--security-opt=no-new-privileges' in a: security.insert(0,'no-new-privileges')
    mounts=[]; hosts=[]
    for v in opts('--mount'):
        kv=dict((p.split('=',1)+[''])[:2] for p in next(csv.reader([v])))
        ro='readonly' in kv
        mounts.append({'Type':'bind','Source':kv['source'],'Destination':kv['target'],'RW':not ro,'Propagation':'rprivate'})
        hosts.append({'Type':'bind','Source':kv['source'],'Target':kv['target'],'ReadOnly':ro})
    ports={'6080/tcp':[{'HostIp':'127.0.0.1','HostPort':''}]} if '127.0.0.1::6080' in opts('-p') else {}
    size=lambda k: int(next(v.split('=')[1][:-1] for v in a if v.startswith(k+'=')))
    c={'id':'b'*64,'name':'/'+r['name'],'image':'sha256:'+'c'*64,
       'config':{'Image':'sha256:'+'c'*64,'User':opt('--user'),
                 'Entrypoint':['/usr/bin/tini','--','node','runtime/server.mjs'],'Cmd':None,
                 'Env':[],'Labels':labels,'Volumes':None,'Tty':False,'OpenStdin':False,'WorkingDir':'/opt/pithos-browser'},
       'host':{'NetworkMode':opt('--network'),'ReadonlyRootfs':'--read-only' in a,'Privileged':False,
               'AutoRemove':False,'RestartPolicy':{'Name':'no','MaximumRetryCount':0},'CapDrop':['ALL'],
               'SecurityOpt':security,'CapAdd':None,'GroupAdd':None,'Binds':None,'Devices':None,'PidMode':'',
               'IpcMode':'private','UsernsMode':'','Mounts':hosts,'ExtraHosts':None,
               'Tmpfs':dict(v.split(':',1) for v in opts('--tmpfs')),'PortBindings':ports,'PublishAllPorts':False,
               'ShmSize':size('--shm-size')*1024*1024,'Memory':size('--memory')*1024*1024*1024,
               'PidsLimit':int(next(v.split('=')[1] for v in a if v.startswith('--pids-limit=')))},
       'mounts':mounts,'aliases':opts('--network-alias'),
       'state':{'Status':'running','Running':True,'ExitCode':0,'Error':'','OOMKilled':False,'Dead':False,'Health':{'Status':'healthy'}}}
    (root/'browser-container.json').write_text(json.dumps(c))
    if mode('viewer-write-file') or mode('viewer-write-symlink'):
        target=root/'credential/browser/skills/viewer.json'
        if mode('viewer-write-file'): target.write_text('do not adopt')
        else: target.symlink_to(root/'metadata-canary')
    print(c['id'])
elif a[:2]==['container','ls'] and (root/'browser-container.json').exists() and opt('--filter') in ['id='+'b'*64,'name=^'+json.loads((root/'browser-container.json').read_text())['name']+'$']:
    emit('b'*64)
elif a[:2]==['container','inspect'] and a[-1]=='b'*64:
    c=json.loads((root/'browser-container.json').read_text())
    if 'NetworkSettings.Ports' in a[3]:
        emit({'id':c['id'],'ports':{'6080/tcp':[{'HostIp':'127.0.0.1','HostPort':(root/'viewer-port').read_text()}]} if c['host']['PortBindings'] else {}})
    else: emit(c)
elif a[:2]==['container','rm'] and a[-1]=='b'*64:
    assert (root/'credential/browser/skills/viewer.mjs').exists(), 'files must outlive browser'
    assert a==['container','rm','--force','b'*64]
    (root/'browser-container.json').unlink()
