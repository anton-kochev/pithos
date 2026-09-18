#!/usr/bin/python3
"""Fake the external Docker boundary; never start containers or read secrets."""
import fcntl
import hashlib
import json
import os
import re
import sys

args = sys.argv[1:]
file = os.environ['BROWSER_FAKE_STATE']
with open(file, 'a+') as stream:
    fcntl.flock(stream, fcntl.LOCK_EX)
    stream.seek(0)
    data = json.loads(stream.read() or '{"resources":{},"calls":[]}')
    data['calls'].append(args)
    resources = data['resources']
    ids = data.setdefault('ids', {})
    def allocate(name):
        data['sequence'] = data.get('sequence', 0) + 1
        ids[name] = hashlib.sha256(str(data['sequence']).encode()).hexdigest()
    code = 0
    text = ''
    def option(name):
        return args[args.index(name)+1]
    if args[0] == 'test-set-fault':
        data['fault'] = args[1]
    elif data.get('fault') == 'engine':
        code = 1
    elif data.get('fault') == 'inspect' and args[:2] in [['container','inspect'], ['network','inspect']]:
        code = 1
    elif data.get('fault') == 'remove' and args[:2] in [['container','rm'], ['network','rm']]:
        code = 1
    elif data.get('fault') == 'remove-transient' and args[:2] == ['network','rm']:
        # Docker can report a failed network removal while the teardown it
        # started completes anyway: endpoints detach asynchronously after a
        # forced container removal. Drop the resource and still report failure.
        data.pop('fault')
        target = args[-1]
        name = next((name for name, value in ids.items() if value == target), target)
        resources.pop(name, None)
        ids.pop(name, None)
        code = 1
    elif args[0] == 'test-arm-bad-id':
        data['bad_id'] = True
    elif args[0] == 'test-arm-swap':
        data['swap'] = args[1]
    elif args[0] == 'test-alter-owner':
        resources[args[1]] = args[2]
    elif args[:2] == ['network', 'create']:
        resources[args[-1]] = option('--label').split('=', 1)[1]
        allocate(args[-1])
        text = args[-1]
    elif args[0] == 'run':
        if '--name' in args:
            resources[option('--name')] = option('--label').split('=', 1)[1]
            allocate(option('--name'))
            text = option('--name')
        if '--entrypoint' in args and option('--entrypoint') == '/usr/bin/node':
            text = os.environ.get('BROWSER_FAKE_PI_VERSION', '0.85.1')
        if '--rm' in args and '--name' in args:
            resources.pop(option('--name'), None)
            ids.pop(option('--name'), None)
    elif args[0] == 'inspect':
        name = args[-1]
        if name in resources:
            text = 'false unhealthy' if os.environ.get('BROWSER_FAKE_FAIL') else 'true healthy'
        else:
            code = 1
    elif args[0] == 'port':
        text = '127.0.0.1:49152'
    elif args[:2] in [['container','inspect'], ['network','inspect']]:
        name = args[-1]
        if name in resources:
            text = json.dumps(resources[name]) if 'json (' in option('--format') else resources[name]
            if '{{.Id}}' in option('--format'):
                text = ('g' * 64 if data.get('bad_id') else ids[name]) + ' ' + text
            if data.get('swap') == args[0] and (args[0] == 'network' or name.endswith('-browser')):
                data.pop('swap')
                displaced = name + '-displaced'
                resources[displaced], ids[displaced] = resources[name], ids[name]
                resources[name] = 'foreign-replacement'
                allocate(name)
        else:
            code = 1
    elif args[:2] in [['container','rm'], ['network','rm']]:
        target = args[-1]
        name = next((name for name, value in ids.items() if value == target), target)
        resources.pop(name, None)
        ids.pop(name, None)
    elif args[:2] in [['container','ls'], ['network','ls']]:
        pattern = option('--filter').removeprefix('name=')
        text = '\n'.join(ids[name] for name in resources
                         if re.search(pattern, '/' + name if args[0] == 'container' else name))
    else:
        code = 1
    stream.seek(0)
    stream.truncate()
    json.dump(data, stream)
print(text)
sys.exit(code)
