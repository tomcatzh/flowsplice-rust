#!/usr/bin/env python3
"""Run the actual staging script against a dependency-free, isolated npm fixture."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'pty-apple/scripts/stage-resources.py'


class AppleStagingTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='pty-staging-')
        self.addCleanup(self.temporary.cleanup)
        self.repo = Path(self.temporary.name)
        self.web = self.repo / 'pty-web'
        self.web.mkdir()
        (self.repo / 'pty-apple').mkdir()
        notice = self.repo / 'pty/codec/vendor/snappy/COPYING'
        notice.parent.mkdir(parents=True)
        notice.write_text('fixture license')
        package = {'name': 'staging-fixture', 'version': '1.0.0',
                   'scripts': {'build': 'node build.cjs'}}
        (self.web / 'package.json').write_text(json.dumps(package))
        (self.web / 'package-lock.json').write_text(json.dumps({
            'name': package['name'], 'version': '1.0.0', 'lockfileVersion': 3,
            'packages': {'': {'name': package['name'], 'version': '1.0.0'}}}))
        (self.web / 'build.cjs').write_text("const fs = require('fs'); fs.mkdirSync('dist', {recursive:true}); "
                                           "fs.copyFileSync('source.html', 'dist/index.html');")
        (self.web / 'source.html').write_text('current source')
        (self.web / 'dist').mkdir()
        (self.web / 'dist/index.html').write_text('stale build')
        (self.web / 'dist/stale.js').write_text('stale asset')
        self.output = self.repo / 'output'
        self.env = dict(os.environ, SRCROOT=str(self.repo / 'pty-apple'),
                        TARGET_BUILD_DIR=str(self.output), UNLOCALIZED_RESOURCES_FOLDER_PATH='Resources',
                        FLOWSPLICE_PTY_BOOTSTRAP_DIR='', npm_config_cache=str(self.repo / 'npm-cache'),
                        npm_config_audit='false', npm_config_fund='false')

    def stage(self):
        return subprocess.run(['python3', str(SCRIPT)], env=self.env, capture_output=True, text=True)

    def test_stale_dist_replaced_from_current_source(self):
        result = self.stage()
        self.assertEqual(result.returncode, 0, result.stderr)
        ui = self.output / 'Resources/pty-ui'
        self.assertEqual((ui / 'index.html').read_text(), 'current source')
        self.assertFalse((ui / 'stale.js').exists())
        (self.web / 'source.html').write_text('changed source')
        result = self.stage()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((ui / 'index.html').read_text(), 'changed source')

    def test_failed_build_does_not_stage_stale_dist(self):
        (self.web / 'build.cjs').write_text('process.exit(1)')
        result = self.stage()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.output.exists())
        self.assertFalse((self.web / 'dist').exists())

    def test_missing_lockfile_rejects_stale_dist(self):
        (self.web / 'package-lock.json').unlink()
        result = self.stage()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.output.exists())


if __name__ == '__main__':
    unittest.main()
