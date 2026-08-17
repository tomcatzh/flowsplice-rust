'use strict';
'require dom';
'require form';
'require fs';
'require poll';
'require rpc';
'require ui';
'require uci';
'require view';

var pollAdded = false;
var ADMIN_HELPER = '/usr/libexec/flowsplice/admin';
var ENROLLMENT_REQUEST = '/var/run/flowsplice/luci-enrollment-request.json';
var ENROLLMENT_RESPONSE = '/var/run/flowsplice/luci-enrollment-response.json';
var REVOCATION_REQUEST = '/var/run/flowsplice/luci-revocation.json';

var callServiceList = rpc.declare({
	object: 'service',
	method: 'list',
	params: [ 'name' ],
	expect: { '': {} }
});

function loadStatus() {
	return L.resolveDefault(callServiceList('flowsplice'), {});
}

function runAdmin(action) {
	return fs.exec(ADMIN_HELPER, [ action ]).then(function(result) {
		if (result.code !== 0)
			throw new Error(result.stderr || result.stdout || _('Travel administration failed'));
		try {
			return JSON.parse((result.stdout || '').trim());
		}
		catch (error) {
			throw new Error(_('The Server returned an invalid administration response.'));
		}
	});
}

function loadTravelStatus() {
	return runAdmin('status').catch(function(error) {
		return { ok: false, error: error.message, credentials: [] };
	});
}

function downloadJson(value, filename) {
	var blob = new Blob([ JSON.stringify(value, null, 2) + '\n' ], { type: 'application/json' });
	var url = window.URL.createObjectURL(blob);
	var link = document.createElement('a');
	link.style.display = 'none';
	link.href = url;
	link.download = filename;
	document.body.appendChild(link);
	link.click();
	link.remove();
	window.URL.revokeObjectURL(url);
}

function refreshTravelStatus() {
	return loadTravelStatus().then(function(status) {
		var node = document.getElementById('flowsplice-travel-status');
		if (node)
			dom.content(node, renderTravelStatus(status));
		return status;
	});
}

function prepareTravelEnrollment() {
	return ui.uploadFile(ENROLLMENT_REQUEST, null,
		_('Select the enrollment-request.json generated on the Travel machine.')).then(function() {
		return runAdmin('prepare');
	}).then(function(approval) {
		downloadJson(approval, 'flowsplice-travel-approval.json');
		ui.addNotification(null, E('p', {}, [
			_('The approval file is ready. Move it to the offline issuer; no signing private key is stored on this router.')
		]), 'info');
	}).catch(function(error) {
		ui.addNotification(null, E('p', {}, [ error.message ]), 'error');
	});
}

function importTravelEnrollment() {
	return ui.uploadFile(ENROLLMENT_RESPONSE, null,
		_('Select the signed enrollment response returned by the offline issuer.')).then(function() {
		return runAdmin('import');
	}).then(function() {
		ui.addNotification(null, E('p', {}, [ _('The Travel credential was imported successfully.') ]), 'info');
		return refreshTravelStatus();
	}).catch(function(error) {
		ui.addNotification(null, E('p', {}, [ error.message ]), 'error');
	});
}

function revokeTravelCredential(credential) {
	var reason = window.prompt(_('Revocation reason for %s').format(credential.travel_id));
	if (reason == null)
		return Promise.resolve();
	reason = reason.trim();
	if (!reason) {
		ui.addNotification(null, E('p', {}, [ _('A revocation reason is required.') ]), 'error');
		return Promise.resolve();
	}
	return fs.write(REVOCATION_REQUEST, JSON.stringify({
		credential_id: credential.credential_id,
		reason: reason
	}), 384).then(function() {
		return runAdmin('revoke');
	}).then(function() {
		ui.addNotification(null, E('p', {}, [ _('The Travel credential was revoked immediately.') ]), 'info');
		return refreshTravelStatus();
	}).catch(function(error) {
		ui.addNotification(null, E('p', {}, [ error.message ]), 'error');
	});
}

function travelStateLabel(credential) {
	var label = credential.revoked ? _('Revoked') : (credential.active ? _('Active') : _('Inactive'));
	var color = credential.revoked ? 'red' : (credential.active ? 'green' : '#777');
	return E('span', { 'style': 'font-weight:bold;color:%s'.format(color) }, [ label ]);
}

function renderTravelStatus(status) {
	if (!status || status.ok !== true)
		return E('p', { 'class': 'alert-message warning' }, [
			(status && status.error) || _('The Server administration interface is unavailable.')
		]);
	var rows = (status.credentials || []).map(function(credential) {
		var action = credential.revoked ? '—' : E('button', {
			'class': 'btn cbi-button cbi-button-negative',
			'click': ui.createHandlerFn(null, function() {
				return revokeTravelCredential(credential);
			})
		}, [ _('Revoke') ]);
		return [
			E('span', {}, [ String(credential.travel_id) ]),
			E('code', {}, [ String(credential.credential_id) ]),
			travelStateLabel(credential),
			E('span', {}, [ new Date(credential.not_after_unix_secs * 1000).toLocaleString() ]),
			action
		];
	});
	var table = E('table', { 'class': 'table' }, [
		E('tr', { 'class': 'tr table-titles' }, [
			E('th', { 'class': 'th' }, [ _('Travel ID') ]),
			E('th', { 'class': 'th' }, [ _('Credential ID') ]),
			E('th', { 'class': 'th' }, [ _('Status') ]),
			E('th', { 'class': 'th' }, [ _('Expires') ]),
			E('th', { 'class': 'th' }, [ _('Action') ])
		])
	]);
	cbi_update_table(table, rows, E('em', {}, [ _('No Travel credentials have been issued.') ]));
	return table;
}

function renderTravelAdministration(status) {
	return E('div', { 'class': 'cbi-section' }, [
		E('h3', {}, [ _('Travel credential issuance') ]),
		E('p', { 'class': 'description' }, [
			_('The Travel machine creates encrypted private keys locally. This page approves the requested validity period and imports the offline-signed result; the signing private keys remain offline.')
		]),
		E('div', { 'class': 'cbi-section-actions' }, [
			E('button', {
				'class': 'btn cbi-button cbi-button-action',
				'click': ui.createHandlerFn(null, prepareTravelEnrollment)
			}, [ _('Upload request and download approval') ]), ' ',
			E('button', {
				'class': 'btn cbi-button cbi-button-apply',
				'click': ui.createHandlerFn(null, importTravelEnrollment)
			}, [ _('Upload and import signed result') ]), ' ',
			E('button', {
				'class': 'btn cbi-button cbi-button-neutral',
				'click': ui.createHandlerFn(null, refreshTravelStatus)
			}, [ _('Refresh') ])
		]),
		E('h4', {}, [ _('Issued Travel credentials') ]),
		E('div', { 'id': 'flowsplice-travel-status' }, [ renderTravelStatus(status) ])
	]);
}

function configuredInstances() {
	var instances = [ { name: 'server', label: _('Server') } ];
	uci.sections('flowsplice', 'relay', function(section) {
		instances.push({
			name: 'relay_%s'.format(section['.name']),
			label: _('Relay: %s').format(section.id || section['.name'])
		});
	});
	return instances;
}

function instanceState(services, name) {
	var service = services.flowsplice || {};
	var instance = (service.instances || {})[name] || {};
	return {
		running: instance.running === true,
		pid: instance.pid || null,
		respawn: instance.respawn === true
	};
}

function statusLabel(running) {
	return E('span', {
		'style': 'font-weight:bold;color:%s'.format(running ? 'green' : 'red')
	}, [ running ? _('RUNNING') : _('STOPPED') ]);
}

function runAction(action) {
	return fs.exec('/etc/init.d/flowsplice', [ action ]).then(function(result) {
		if (result.code !== 0)
			throw new Error(result.stderr || result.stdout || _('Service action failed'));
		if (action === 'validate')
			ui.addNotification(null, E('p', {}, [ _('All enabled FlowSplice instances are valid.') ]), 'info');
		return new Promise(function(resolve) { window.setTimeout(resolve, 500); });
	}).then(refreshStatus).catch(function(error) {
		ui.addNotification(null, E('p', {}, [ error.message ]), 'error');
	});
}

function actionButton(label, action, style) {
	return E('button', {
		'class': 'btn cbi-button cbi-button-%s'.format(style),
		'click': ui.createHandlerFn(this, function() { return runAction(action); })
	}, [ label ]);
}

function renderStatusContent(services) {
	var rows = configuredInstances().map(function(instance) {
		var state = instanceState(services, instance.name);
		return [
			instance.label,
			statusLabel(state.running),
			state.pid || '—',
			state.respawn ? _('armed') : '—'
		];
	});
	var table = E('table', { 'class': 'table' }, [
		E('tr', { 'class': 'tr table-titles' }, [
			E('th', { 'class': 'th' }, [ _('Instance') ]),
			E('th', { 'class': 'th' }, [ _('State') ]),
			E('th', { 'class': 'th' }, [ _('PID') ]),
			E('th', { 'class': 'th' }, [ _('Respawn') ])
		])
	]);
	cbi_update_table(table, rows, E('em', {}, [ _('No instances configured.') ]));
	return E('div', {}, [
		table,
		E('p', { 'class': 'description' }, [
			_('Firewall exposure is intentionally managed separately. Installing or starting FlowSplice does not create WAN rules.')
		]),
		E('div', { 'class': 'right' }, [
			actionButton(_('Validate'), 'validate', 'action'), ' ',
			actionButton(_('Start'), 'start', 'apply'), ' ',
			actionButton(_('Stop'), 'stop', 'reset'), ' ',
			actionButton(_('Restart'), 'restart', 'reload')
		])
	]);
}

function refreshStatus() {
	return loadStatus().then(function(services) {
		var node = document.getElementById('flowsplice-service-status');
		if (node)
			dom.content(node, renderStatusContent(services));
	});
}

function required(option) {
	option.rmempty = false;
	return option;
}

function addPathOption(section, tab, name, title, description) {
	var option = section.taboption(tab, form.Value, name, title, description);
	option.modalonly = true;
	return required(option);
}

return view.extend({
	load: function() {
		return Promise.all([ uci.load('flowsplice'), loadStatus(), loadTravelStatus() ]);
	},

	render: function(data) {
		var m, s, o;

		m = new form.Map('flowsplice', _('FlowSplice'),
			_('Manage the OpenWrt Server and multiple logical Relay instances from one page. Use separate LAN and WAN6 Relay identities even though both use the same installed binary.'));

		s = m.section(form.NamedSection, '_status');
		s.anonymous = true;
		s.render = function() {
			if (!pollAdded) {
				poll.add(refreshStatus, 2);
				pollAdded = true;
			}
			return E('div', { 'class': 'cbi-section', 'id': 'flowsplice-service-status' }, [
				E('h3', {}, [ _('Runtime status') ]),
				renderStatusContent(data[1])
			]);
		};

		s = m.section(form.NamedSection, 'global', 'flowsplice', _('Global settings'));
		s.anonymous = true;
		o = s.option(form.ListValue, 'log_level', _('Log level'));
		[ 'error', 'warn', 'info', 'debug', 'trace' ].forEach(function(level) {
			o.value(level, level);
		});
		o.default = 'info';
		o.rmempty = false;

		s = m.section(form.NamedSection, 'server', 'server', _('Server'));
		s.anonymous = true;
		s.tab('general', _('General'));
		s.tab('trust', _('Trust and identity'));
		s.tab('limits', _('Limits'));
		o = s.taboption('general', form.Flag, 'enabled', _('Enabled'));
		o.default = o.disabled;
		o.rmempty = false;
		required(s.taboption('general', form.Value, 'id', _('Server ID'))).placeholder = 'server-1';
		o = required(s.taboption('general', form.Value, 'control_listen', _('Home control listener'),
			_('Use LAN addresses only; do not expose the Server control listener to WAN.')));
		o.placeholder = '192.0.2.1:7443';
		o = required(s.taboption('general', form.DynamicList, 'data_listen', _('Data listeners'),
			_('May contain explicit LAN IPv4 and WAN IPv6 socket addresses.')));
		o.placeholder = '[2001:db8::1]:7444';
		o = required(s.taboption('general', form.Value, 'travel_valid_days', _('Default Travel validity (days)'),
			_('Used when preparing a new Travel enrollment approval.')));
		o.datatype = 'range(1,3650)';
		o.default = '365';
		addPathOption(s, 'trust', 'cert', _('Management certificate'));
		addPathOption(s, 'trust', 'key', _('Management private key'), _('The file must be readable by the flowsplice service user.'));
		addPathOption(s, 'trust', 'management_ca', _('Management CA'));
		required(s.taboption('trust', form.DynamicList, 'relay_spki_pin', _('Relay SPKI pins')));
		o = required(s.taboption('trust', form.Value, 'travel_authority_public_key', _('Travel authorization public key'),
			_('Uncompressed P-256 public key in hexadecimal. The matching offline private key must never be installed on this router.')));
		addPathOption(s, 'trust', 'travel_credentials', _('Signed Travel credentials'));
		addPathOption(s, 'trust', 'travel_revocations', _('Persistent Travel revocations'));
		addPathOption(s, 'trust', 'admin_socket', _('Local administration socket'));
		o = required(s.taboption('limits', form.Value, 'handshake_timeout_secs', _('Handshake timeout (seconds)')));
		o.datatype = 'uinteger';
		o.default = '10';
		o = required(s.taboption('limits', form.Value, 'work_ttl_secs', _('Pending work TTL (seconds)')));
		o.datatype = 'uinteger';
		o.default = '15';
		o = required(s.taboption('limits', form.Value, 'max_pending_work', _('Maximum pending work')));
		o.datatype = 'uinteger';
		o.default = '256';

		s = m.section(form.GridSection, 'home', _('Trusted Home Agents'),
			_('Add every Home Agent that may host a logical service. A Travel mapping selects one Home ID and one service; it never falls back to another Home.'));
		s.addremove = true;
		s.nodescriptions = true;
		s.addbtntitle = _('Add Home Agent');
		required(s.option(form.Value, 'id', _('Home ID')));
		o = required(s.option(form.DynamicList, 'spki_pin', _('Home SPKI pins')));
		o.modalonly = true;

		s = m.section(form.GridSection, 'relay', _('Local Relay instances'),
			_('Create separate named instances for LAN and WAN6. Each instance has its own identity, listeners, advertised data address, logs, and procd lifecycle.'));
		s.addremove = true;
		s.nodescriptions = true;
		s.addbtntitle = _('Add Relay instance');
		o = s.option(form.Flag, 'enabled', _('Enabled'));
		o.default = o.disabled;
		o.rmempty = false;
		required(s.option(form.Value, 'id', _('Relay ID')));
		required(s.option(form.Value, 'management_listen', _('Management listen'))).placeholder = '192.0.2.1:8443';
		o = required(s.option(form.Value, 'data_listen', _('Data listen')));
		o.modalonly = true;
		o = required(s.option(form.Value, 'data_public_addr', _('Advertised data address')));
		o.modalonly = true;
		o = required(s.option(form.Value, 'server_data_addr', _('Server data address')));
		o.modalonly = true;
		o = required(s.option(form.Value, 'server_id', _('Server ID')));
		o.modalonly = true;
		[ [ 'cert', _('Management certificate') ], [ 'key', _('Management private key') ], [ 'management_ca', _('Management CA') ] ].forEach(function(item) {
			var pathOption = required(s.option(form.Value, item[0], item[1]));
			pathOption.modalonly = true;
		});
		[ [ 'server_spki_pin', _('Server SPKI pins') ] ].forEach(function(item) {
			var pinOption = required(s.option(form.DynamicList, item[0], item[1]));
			pinOption.modalonly = true;
		});
		o = required(s.option(form.Value, 'travel_authority_public_key', _('Travel authorization public key')));
		o.modalonly = true;
		o = required(s.option(form.Value, 'travel_authorization_cache', _('Persistent revocation cache')));
		o.modalonly = true;
		[ [ 'handshake_timeout_secs', _('Handshake timeout'), '10' ], [ 'route_ttl_secs', _('Route TTL'), '15' ], [ 'max_pending_routes', _('Maximum pending routes'), '256' ] ].forEach(function(item) {
			var limitOption = required(s.option(form.Value, item[0], item[1]));
			limitOption.datatype = 'uinteger';
			limitOption.default = item[2];
			limitOption.modalonly = true;
		});

		s = m.section(form.GridSection, 'relay_endpoint', _('Server Relay directory'),
			_('These entries are published to Travel Agents. Add every local and VPS Relay with a distinct Relay ID.'));
		s.addremove = true;
		s.nodescriptions = true;
		s.addbtntitle = _('Add directory entry');
		o = s.option(form.Flag, 'enabled', _('Enabled'));
		o.default = o.enabled;
		o.rmempty = false;
		required(s.option(form.Value, 'id', _('Relay ID')));
		required(s.option(form.Value, 'management_addr', _('Management address')));
		required(s.option(form.Value, 'server_name', _('TLS server name')));

		s = m.section(form.NamedSection, '_travel_administration');
		s.anonymous = true;
		s.render = function() {
			return renderTravelAdministration(data[2]);
		};

		return m.render();
	}
});
