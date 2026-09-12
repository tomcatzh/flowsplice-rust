#!/usr/bin/env python3
"""One enrollment across current/future PTY Homes in the disposable fixture only."""
import hashlib
import json
import shutil
import tomllib
from pathlib import Path


def execute(run, alpha, root, primary, secondary, scope, secondary_scope):
    command, require, wait, issuer = run.command, run.require, run.wait, run.issuer
    descriptor = json.loads((run.directory / alpha.removeprefix('/business/')).read_text())
    service_class = {'version': 1, 'approving_home_id': descriptor['approving_home_id'],
                     'application_protocol': 'flowsplice.pty.v1', 'protocol': 'tcp'}
    class_scope = {k: service_class[k] for k in ('application_protocol', 'protocol')} | {'kind': 'service_class'}
    (run.directory / 'service-class.json').write_text(json.dumps(service_class))
    class_path = '/business/service-class.json'
    homes = {scope['home_id']: primary, secondary_scope['home_id']: secondary}

    def probe(label, args, success=True):
        return run.finish(run.start(label, 'flowsplice-service-class-probe', args), success=success, timeout=300)

    def key_hashes(path):
        return {str(p.relative_to(path)): hashlib.sha256(p.read_bytes()).hexdigest()
                for p in path.rglob('*') if p.is_file() and p.suffix in ('.crt', '.key')}

    def enroll(name, validity=None, resume=False):
        travel_id = 'pty-class-' + name + '-' + run.token
        label = 'Class acceptance ' + name + ' · PTY'
        install = '/business/class-' + name
        args = ['enroll', run.relay, class_path, root, install, '/business/password.txt', travel_id, label]
        task = run.start('class-enroll-' + name, 'flowsplice-service-class-probe', args)
        pending = run.pending('/api/enrollment/pending', 'travel_id', travel_id)
        require(pending['service_class'] == service_class and pending['client_label'] == label, 'class pending intent/label changed')
        wait(lambda: pending['verification_code'] in run.logs(task), 'class rendered verification code')
        backup = run.directory / ('class-' + name + '-bootstrap.json')
        shutil.copyfile(run.directory / ('class-' + name) / 'bootstrap-enrollment.json', backup)
        body = {'request_id': pending['request_id'], 'scope': class_scope, 'password': run.password} | (validity or {'valid_days': 365})
        for change in ({'scope': {'kind': 'global'}}, {'scope': scope}, {'scope': class_scope | {'application_protocol': 'other.v1'}}, {'scope': class_scope | {'protocol': 'udp'}}):
            run.reject('/api/enrollment/approve', body | change)
        issuer.request(19084, 'POST', '/api/enrollment/approve', body)
        require('service-class-enrollment-installed' in run.finish(task), 'class enrollment did not finish')
        path = run.directory / ('class-' + name)
        marker = json.loads((path / 'approved-service-class-binding.json').read_text())
        credential = json.loads(bytes.fromhex(marker['response']['response']['signed_credential']['payload_hex']))
        require(credential['scope'] == class_scope, 'class credential widened scope')
        history = wait(lambda: next((item for item in issuer.request(19084, 'GET', '/api/credentials?status=all&page_size=100')['items'] if item['credential_id'] == credential['credential_id']), None), 'class issued history')
        require(history.get('client_label') == label and history['scope'] == class_scope, 'issued class history lost readable identity or scope')
        config = tomllib.loads((path / 'travelagent.toml').read_text())
        require(config['homes'] == [] and not config.get('mappings') and config['service_class'] == service_class, 'class config pins Homes or opens listeners')
        hashes = key_hashes(path)
        require(bool(hashes), 'no class certificate hashes')
        if resume:
            probe('class-resume-fixture', ['resume-fixture', install, '/business/' + backup.name])
            probe('class-incomplete', ['check-only', install + '/travelagent.toml', root, class_path], success=False)
            changed = list(args)
            changed[-1] = 'Changed intent · PTY'
            probe('class-changed-label', changed, success=False)
            require('service-class-enrollment-installed' in probe('class-resume', args), 'class resume incomplete')
            require(key_hashes(path) == hashes, 'class resume rotated keys/certificates')
            after = json.loads((path / 'approved-service-class-binding.json').read_text())
            require(after == marker, 'class resume changed approval/identity')
            require(not (path / 'bootstrap-enrollment.json').exists() and not (path / 'business-installation.pending.json').exists(), 'class resume retained pending state')
            probe('class-binding', ['check-only', install + '/travelagent.toml', root, class_path])
        return [install + '/travelagent.toml', '/business/password.txt', root, class_path, 'home-1', 'tcp-echo', '/business/class-state.json'], credential

    def clean():
        for container in homes.values():
            for domain in ('alpha', 'beta'):
                command(['docker', 'exec', container, '/usr/bin/tmux', '-S', '/tmp/fs-pty/' + domain + '/tmux.sock', 'kill-server'], success=False)

    def surviving_sessions():
        state = json.loads((run.directory / 'class-state.json').read_text())
        require(len(state['sessions']) == 2 and len({s['home_id'] for s in state['sessions']}) == 2, 'class access did not span two Homes')
        for session in state['sessions']:
            domain = session['service_id'].removeprefix('svc-')
            result = command(['docker', 'exec', homes[session['home_id']], '/usr/bin/tmux', '-S', '/tmp/fs-pty/' + domain + '/tmux.sock', 'list-sessions', '-F', '#{session_name}']).stdout
            require(session['session_id'] in result, 'authorization loss killed tmux shell')

    clean()
    base, persistent = enroll('shared', resume=True)
    original = run.directory / 'class-shared'
    relocated = run.directory / 'class shared relocated'
    def all_hashes(path):
        return {str(p.relative_to(path)): hashlib.sha256(p.read_bytes()).hexdigest()
                for p in path.rglob('*') if p.is_file()}
    before_move = all_hashes(original)
    require(bool(before_move), 'relocation fixture is empty')
    original.rename(relocated)
    require(not original.exists() and all_hashes(relocated) == before_move, 'relocation changed installation bytes')
    base[0] = '/business/' + relocated.name + '/travelagent.toml'
    config_path = relocated / 'travelagent.toml'
    config_bytes = config_path.read_bytes()
    require('service-class-binding-verified' in probe('class-relocated-binding', ['check-only', base[0], root, class_path]), 'relocated class binding failed')
    try:
        config_path.write_bytes(config_bytes + b'\n# deliberate digest mismatch\n')
        rejected = probe('class-relocated-digest-rejected', ['check-only', base[0], root, class_path], success=False)
        require('class installation does not match this application and configuration' in rejected, 'altered config did not fail binding digest validation')
    finally:
        config_path.write_bytes(config_bytes)
    require(all_hashes(relocated) == before_move, 'binding checks changed installation bytes')
    require('service-class-binding-verified' in probe('class-relocated-binding-restored', ['check-only', base[0], root, class_path]), 'restored class binding failed')
    # Runtime state/trust may legitimately refresh. Identity/config/root/key bytes may not.
    identity_hashes = {name: digest for name, digest in before_move.items()
                       if name in ('travelagent.toml', 'approved-service-class-binding.json', 'cert/deployment-root.pub')
                       or Path(name).suffix in ('.crt', '.key')}
    require('cert/deployment-root.pub' in identity_hashes and bool(key_hashes(relocated)), 'relocation fixture lacks root/certificate identity')
    run.passed += ['class-relocation-preserves-all-files', 'class-relocation-binding-and-digest-enforcement']
    future_task = run.start('class-future', 'flowsplice-service-class-probe', ['future', *base, '/business/class-future-signal.json'])
    wait(lambda: 'service-class-future-ready' in run.logs(future_task), 'two Home shared runtime ready')
    # This Home did not exist when the application credential was issued.
    services = [{'service_id': 'terminal-future', 'protocol': 'tcp', 'application_protocol': 'flowsplice.pty.v1', 'capabilities': ['read', 'write']}]
    (run.directory / 'future-services.json').write_text(json.dumps(services))
    setup = run.start('future-setup', 'flowsplice-homeagent', ['init', '--server', run.server,
        '--bootstrap-config', '/config/home-bootstrap.toml', '--business-services', '/business/future-services.json', '--install-dir', '/business/home-future'], setup=True)
    directory = run.directory / 'home-future'
    wait(lambda: (directory / 'home-bootstrap.json').exists(), 'future Home bootstrap')
    home_id = json.loads((directory / 'home-bootstrap.json').read_text())['home_id']
    require(home_id not in homes, 'future Home reused old identity')
    pending = run.pending('/api/home-enrollment/pending', 'home_id', home_id)
    wait(lambda: pending['verification_code'] in run.logs(setup), 'future Home code')
    issuer.request(19084, 'POST', '/api/home-enrollment/approve', {'request_id': pending['request_id'], 'profile': 'serving_only', 'valid_days': 365, 'services': ['terminal-future'], 'password': run.password})
    run.finish(setup)
    (run.directory / 'pty-home-future.toml').write_text('home_runtime = "/business/home-future/home-runtime.toml"\n[[domains]]\nservice_id = "terminal-future"\n[domains.tmux]\nbinary = "/usr/bin/tmux"\nsocket = "/tmp/fs-pty/future/tmux.sock"\nshell = "/bin/sh"\nworking_directory = "/business"\n')
    future_home = run.start('future-pty-home', 'flowsplice-pty-home', ['--config', '/business/pty-home-future.toml'])
    (run.directory / 'class-future-signal.json').write_text(json.dumps({'home_id': home_id}))
    require('service-class-future-complete' in run.finish(future_task, timeout=300), 'future Home not automatically discovered')
    command(['docker', 'stop', future_home])
    after_runtime = all_hashes(relocated)
    require(all(after_runtime.get(name) == digest for name, digest in identity_hashes.items()), 'relocated runtime changed config/root/certificates/keys/identity')
    require('service-class-binding-verified' in probe('class-relocated-after-runtime', ['check-only', base[0], root, class_path]), 'relocated runtime invalidated binding')
    run.passed += ['class-relocation-runtime-multi-home', 'class-relocation-identity-unchanged']
    run.passed += ['class-one-identity-two-homes', 'class-future-home-different-service-id', 'class-independent-disconnect-rejoin', 'class-no-listeners', 'class-exact-scope-and-resumable-install']
    clean()
    revoked_base, revoked = enroll('revoked')
    task = run.start('class-revocation', 'flowsplice-service-class-probe', ['access-end', *revoked_base])
    wait(lambda: 'service-class-access-ready' in run.logs(task), 'two active class sockets before revocation')
    issuer.request(19084, 'POST', '/api/revoke', {'credential_id': revoked['credential_id'], 'reason': 'Disposable class acceptance', 'password': run.password})
    require('service-class-access-ended' in run.finish(task, timeout=300), 'revoked class sockets survived')
    surviving_sessions()
    clean()
    expiring_base, _ = enroll('expiring', {'valid_minutes': 1})
    require('service-class-access-ended' in probe('class-expiry', ['access-end', *expiring_base]), 'expired class sockets survived')
    surviving_sessions()
    clean()
    # Keep shared class grant active for the old generic Travel suite after this test.
    run.passed += ['class-revocation-closes-all-preserves-shells', 'class-expiry-closes-all-preserves-shells']
    print(json.dumps({'checkpoint': 'service-class-e2e-complete', 'active_class_credential_id': persistent['credential_id']}), flush=True)
    return service_class
