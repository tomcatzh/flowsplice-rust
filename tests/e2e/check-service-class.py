#!/usr/bin/env python3
"""One service-class enrollment across current/future echo Homes in the disposable fixture only."""
import hashlib
import json
import shutil
import tomllib
from pathlib import Path

APPLICATION_PROTOCOL = 'example.echo.v1'


def execute(run, alpha, root, scope):
    command, require, wait, issuer = run.command, run.require, run.wait, run.issuer
    descriptor = json.loads((run.directory / alpha.removeprefix('/business/')).read_text())
    service_class = {'version': 1, 'approving_home_id': descriptor['approving_home_id'],
                     'application_protocol': APPLICATION_PROTOCOL, 'protocol': 'tcp'}
    class_scope = {k: service_class[k] for k in ('application_protocol', 'protocol')} | {'kind': 'service_class'}
    (run.directory / 'service-class.json').write_text(json.dumps(service_class))
    class_path = '/business/service-class.json'

    def probe(label, args, success=True, timeout=300):
        return run.finish(run.start(label, 'flowsplice-service-class-probe', args), success=success, timeout=timeout)

    def key_hashes(path):
        return {str(p.relative_to(path)): hashlib.sha256(p.read_bytes()).hexdigest()
                for p in path.rglob('*') if p.is_file() and p.suffix in ('.crt', '.key')}

    def state(path):
        return json.loads((run.directory / path.removeprefix('/business/')).read_text())

    def pairs(items):
        return {(item['home_id'], item['service_id']) for item in items}

    def serving_home(label, services_file, approved, install):
        # Same serving-only provisioning as the primary business Home, then the in-process echo.
        setup = run.start(label + '-setup', 'flowsplice-homeagent', [
            'init', '--server', run.server, '--bootstrap-config', '/config/home-bootstrap.toml',
            '--business-services', services_file, '--install-dir', install,
        ], setup=True)
        directory = run.directory / install.removeprefix('/business/')
        wait(lambda: (directory / 'home-bootstrap.json').exists(), label + ' Home bootstrap')
        home_id = json.loads((directory / 'home-bootstrap.json').read_text())['home_id']
        pending = run.pending('/api/home-enrollment/pending', 'home_id', home_id)
        wait(lambda: pending['verification_code'] in run.logs(setup), label + ' Home verification code')
        issuer.request(19084, 'POST', '/api/home-enrollment/approve', {
            'request_id': pending['request_id'], 'profile': 'serving_only', 'valid_days': 365,
            'services': approved, 'password': run.password,
        })
        run.finish(setup)
        config = tomllib.loads((directory / 'home-runtime.toml').read_text())
        require('ui_listen' not in config and 'issuer' not in config, label + ' Home acquired issuer/UI configuration')
        require({s['id'] for s in config['services']} == set(approved), label + ' Home approved service set changed')
        container = run.start(label + '-home', 'flowsplice-socket-probe', ['home', install + '/home-runtime.toml'])
        return home_id, container

    def enroll(name, validity=None, resume=False):
        travel_id = 'class-' + name + '-' + run.token
        label = 'Class acceptance ' + name + ' · Echo'
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
            changed[-1] = 'Changed intent · Echo'
            probe('class-changed-label', changed, success=False)
            require('service-class-enrollment-installed' in probe('class-resume', args), 'class resume incomplete')
            require(key_hashes(path) == hashes, 'class resume rotated keys/certificates')
            after = json.loads((path / 'approved-service-class-binding.json').read_text())
            require(after == marker, 'class resume changed approval/identity')
            require(not (path / 'bootstrap-enrollment.json').exists() and not (path / 'business-installation.pending.json').exists(), 'class resume retained pending state')
            probe('class-binding', ['check-only', install + '/travelagent.toml', root, class_path])
        # WRONG_HOME/WRONG_SERVICE name an ordinary generic service outside the class.
        return [install + '/travelagent.toml', '/business/password.txt', root, class_path, 'home-1', 'tcp-echo',
                '/business/class-' + name + '-state.json'], credential

    secondary_id, _ = serving_home('secondary', '/business/services.json', ['svc-alpha'], '/business/home-secondary')
    require(secondary_id != scope['home_id'], 'secondary Home identity was reused')
    homes = {scope['home_id'], secondary_id}
    # Both approved primary services and the secondary service carry the class application protocol.
    expected = {(scope['home_id'], 'svc-alpha'), (scope['home_id'], 'svc-beta'), (secondary_id, 'svc-alpha')}
    run.passed.append('class-secondary-serving-only-home')

    def homes_keep_serving(label, ended_state):
        # Losing one credential closes only its sockets: a still-valid class identity reaches both Homes.
        ended = state(ended_state)
        require(pairs(ended['discovered']) == expected, label + ' class discovery changed')
        ended_homes = {item['home_id'] for item in ended['connections']}
        require(ended_homes == homes, 'class access did not span both Homes')
        survivor_state = '/business/class-survivor-' + label + '.json'
        survivor = [*base[:-1], survivor_state]
        result = probe('class-' + label + '-survivor', ['exercise', *survivor])
        require('service-class-exercise-complete' in result, 'surviving class identity could not use the Homes')
        survivor_connections = state(survivor_state)['connections']
        require({item['home_id'] for item in survivor_connections} == ended_homes, 'Home stopped serving after another credential lost access')

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
    require(pairs(state(base[-1])['discovered']) == expected, 'class discovery was not exactly the matching approved services')
    # This Home did not exist when the application credential was issued.
    services = [{'service_id': 'echo-future', 'protocol': 'tcp', 'application_protocol': APPLICATION_PROTOCOL, 'capabilities': ['read', 'write']}]
    (run.directory / 'future-services.json').write_text(json.dumps(services))
    future_id, future_home = serving_home('future', '/business/future-services.json', ['echo-future'], '/business/home-future')
    require(future_id not in homes, 'future Home reused old identity')
    (run.directory / 'class-future-signal.json').write_text(json.dumps({'home_id': future_id}))
    future_result = run.finish(future_task, timeout=300)
    require('service-class-future-complete' in future_result, 'future Home not automatically discovered')
    require('service-class-exercise-complete' in future_result, 'shared class runtime did not finish cleanly')
    command(['docker', 'stop', future_home])
    after_runtime = all_hashes(relocated)
    require(all(after_runtime.get(name) == digest for name, digest in identity_hashes.items()), 'relocated runtime changed config/root/certificates/keys/identity')
    require('service-class-binding-verified' in probe('class-relocated-after-runtime', ['check-only', base[0], root, class_path]), 'relocated runtime invalidated binding')
    run.passed += ['class-relocation-runtime-multi-home', 'class-relocation-identity-unchanged']
    run.passed += ['class-one-identity-two-homes', 'class-exact-discovery', 'class-future-home-different-service-id',
                   'class-independent-disconnect-rejoin', 'class-no-listeners', 'class-exact-scope-and-resumable-install']
    revoked_base, revoked = enroll('revoked')
    task = run.start('class-revocation', 'flowsplice-service-class-probe', ['access-end', *revoked_base])
    wait(lambda: 'service-class-access-ready' in run.logs(task), 'two active class sockets before revocation')
    issuer.request(19084, 'POST', '/api/revoke', {'credential_id': revoked['credential_id'], 'reason': 'Disposable class acceptance', 'password': run.password})
    require('service-class-access-ended' in run.finish(task, timeout=480), 'revoked class sockets survived')
    homes_keep_serving('revocation', revoked_base[-1])
    expiring_base, _ = enroll('expiring', {'valid_minutes': 1})
    require('service-class-access-ended' in probe('class-expiry', ['access-end', *expiring_base], timeout=480), 'expired class sockets survived')
    homes_keep_serving('expiry', expiring_base[-1])
    # Keep the shared class grant active; only the revoked and expired identities lost access.
    run.passed += ['class-revocation-closes-all-homes-keep-serving', 'class-expiry-closes-all-homes-keep-serving']
    print(json.dumps({'checkpoint': 'service-class-e2e-complete', 'active_class_credential_id': persistent['credential_id']}), flush=True)
    return service_class
